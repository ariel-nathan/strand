//! Bidirectional-ish type checker (§4.6): signatures are annotated, locals are
//! inferred. Full inference is future work (§9).
//!
//! Errors are accumulated rather than thrown one at a time. Anything that fails
//! to check becomes `Ty::Error`, which unifies with everything, so a single
//! mistake yields a single message.

use std::collections::HashMap;

use crate::analysis::Analysis;
use crate::ast;
use crate::diag::Diagnostic;
use crate::hir::*;
use crate::lexer::Span;
use crate::input;
use crate::lifecycle;
use crate::stdlib;
use crate::ui::{self, PropTy, Slot};

pub fn check(program: &ast::Program) -> Result<Hir, Vec<Diagnostic>> {
    let (hir, errors) = check_recovering(program);
    if errors.is_empty() {
        Ok(hir)
    } else {
        Err(errors)
    }
}

/// Checks the whole module and hands back what it built alongside whatever went
/// wrong, instead of discarding one for the other.
///
/// `check` throws the partial module away on any error, which is right for a
/// batch compile — there is nothing downstream to do with a module that will not
/// run. An editor is in the opposite position: the file is mid-edit almost all
/// the time, and the types checked so far are exactly what hover needs. The
/// checker already never bails partway, so the partial `Hir` is simply the work
/// it had already done.
pub fn check_recovering(program: &ast::Program) -> (Hir, Vec<Diagnostic>) {
    let (hir, _, errors) = analyze(program);
    (hir, errors)
}

/// `check_recovering`, plus the position-indexed facts an editor needs.
///
/// Hover and go-to-definition are answered from `Analysis` rather than from the
/// `Hir`, which carries no spans.
pub fn analyze(program: &ast::Program) -> (Hir, Analysis, Vec<Diagnostic>) {
    let mut cx = Checker::default();
    cx.collect_types(program);
    cx.collect_signatures(program);
    cx.collect_actors(program);
    cx.check_bodies(program);
    // Last: a wire names ports, and ports are only known once every actor has
    // been collected.
    cx.collect_app(program);
    (cx.hir, cx.analysis, cx.errors)
}

/// Every function in the module, paired with the key it is known by.
///
/// An actor's own functions are qualified with the actor's name, because two
/// actors in one file both declaring `init` — or both handling a port called
/// `input` — is the ordinary case rather than a clash. The qualification is
/// internal: `Func::name` keeps the name as written, since that is the one a
/// diagnostic should say back.
fn fn_decls(program: &ast::Program) -> Vec<(String, &ast::FnDecl, Option<&str>)> {
    let mut out = Vec::new();
    for item in &program.items {
        match item {
            ast::Item::Fn(decl) => out.push((decl.name.clone(), decl, None)),
            ast::Item::Actor(decl) => {
                let owner = decl.name.as_str();
                out.push((qualified(owner, &decl.init.name), &decl.init, Some(owner)));
                for handler in &decl.handlers {
                    out.push((qualified(owner, &handler.name), handler, Some(owner)));
                }
                if let Some(view) = &decl.view {
                    out.push((qualified(owner, &view.name), view, Some(owner)));
                }
            }
            ast::Item::Type(_) | ast::Item::App(_) => {}
        }
    }
    out
}

/// How an actor's own functions are keyed in the signature table.
fn qualified(actor: &str, name: &str) -> String {
    format!("{actor}.{name}")
}

/// Which end of a channel a name is being looked up on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    In,
    Out,
}

impl Direction {
    fn word(self) -> &'static str {
        match self {
            Direction::In => "in",
            Direction::Out => "out",
        }
    }

    fn verb(self) -> &'static str {
        match self {
            Direction::In => "receives",
            Direction::Out => "sends",
        }
    }
}

/// The Strand type a prop argument must have.
fn prop_ty(ty: PropTy) -> Ty {
    match ty {
        PropTy::Int => Ty::Int,
        PropTy::Float => Ty::Float,
        PropTy::Bool => Ty::Bool,
        PropTy::Str => Ty::Str,
    }
}

fn number_default(builder: &ui::Builder, slot: Slot) -> f32 {
    builder
        .props
        .iter()
        .find(|prop| prop.slot == slot)
        .and_then(|prop| prop.default)
        .unwrap_or(0.0)
}

#[derive(Debug, Clone)]
struct Signature {
    id: FuncId,
    params: Vec<(String, Ty)>,
    ret: Ty,
    is_view: bool,
    /// Where the function's name was written, for go-to-definition.
    def_span: Span,
}

#[derive(Debug, Clone)]
struct Local {
    slot: u32,
    ty: Ty,
    mutable: bool,
    /// Where this binding's name was written, for go-to-definition.
    def_span: Span,
}

#[derive(Default)]
struct Checker {
    hir: Hir,
    errors: Vec<Diagnostic>,
    record_ids: HashMap<String, RecordId>,
    sum_ids: HashMap<String, SumId>,
    /// Constructor name -> owning sum and variant index.
    ctors: HashMap<String, (SumId, u32)>,
    aliases: HashMap<String, Ty>,
    signatures: HashMap<String, Signature>,
    /// Per-function state, reset for each body.
    scopes: Vec<HashMap<String, Local>>,
    locals: Vec<Ty>,
    param_count: usize,
    ret_ty: Ty,
    /// Whether the body being checked was written `view fn`. Builders are legal
    /// only here, which is what confines a node's lifetime to the expression
    /// that built it.
    in_view: bool,
    /// Which actor owns the body being checked, when one does. `send` names a
    /// port, and only this actor's ports are in scope.
    sender: Option<usize>,
    /// Position-indexed facts for editors. Codegen ignores these; they exist so
    /// hover and go-to-definition do not need a second resolver.
    analysis: Analysis,
    /// Declaration site of each named type, keyed the same way `record_ids`,
    /// `sum_ids` and `aliases` are.
    type_defs: HashMap<String, Span>,
    /// Declaration site of each sum-type constructor.
    ctor_defs: HashMap<String, Span>,
}

impl Default for Hir {
    fn default() -> Self {
        Hir {
            records: Vec::new(),
            sums: Vec::new(),
            funcs: Vec::new(),
            actors: Vec::new(),
            app: None,
        }
    }
}

impl Checker {
    fn error(&mut self, span: Span, message: impl Into<String>) {
        self.errors.push(Diagnostic::new(span, message));
    }

    /// Reports with a labelled underline and a suggested fix. Used only where
    /// the fix is unambiguous — §8.2 wants suggestions, not guesses.
    fn error_labeled(
        &mut self,
        span: Span,
        message: impl Into<String>,
        label: impl Into<String>,
        help: impl Into<String>,
    ) {
        self.errors
            .push(Diagnostic::new(span, message).with_label(label).with_help(help));
    }

    fn show(&self, ty: &Ty) -> String {
        self.hir.ty(ty)
    }

    // ---- declarations ----------------------------------------------------

    fn collect_types(&mut self, program: &ast::Program) {
        self.collect_platform_types(program);

        // Pass 1: reserve ids so types may refer to each other in any order.
        for item in &program.items {
            let ast::Item::Type(decl) = item else { continue };
            if self.record_ids.contains_key(&decl.name)
                || self.sum_ids.contains_key(&decl.name)
                || self.aliases.contains_key(&decl.name)
            {
                self.error(decl.span, format!("type `{}` is declared twice", decl.name));
                continue;
            }
            // One place for all three kinds, so a type reference can find its
            // declaration whether it is a record, a sum or an alias.
            self.type_defs.insert(decl.name.clone(), decl.name_span);
            match &decl.def {
                ast::TypeDef::Record(_) => {
                    let id = RecordId(self.hir.records.len() as u32);
                    self.hir.records.push(RecordDef {
                        name: decl.name.clone(),
                        fields: Vec::new(),
                    });
                    self.record_ids.insert(decl.name.clone(), id);
                }
                ast::TypeDef::Sum(_) => {
                    let id = SumId(self.hir.sums.len() as u32);
                    self.hir.sums.push(SumDef {
                        name: decl.name.clone(),
                        variants: Vec::new(),
                    });
                    self.sum_ids.insert(decl.name.clone(), id);
                }
                ast::TypeDef::Alias(_) => {}
            }
        }

        // Pass 2: resolve field and variant types now that all names exist.
        for item in &program.items {
            let ast::Item::Type(decl) = item else { continue };
            match &decl.def {
                ast::TypeDef::Record(fields) => {
                    let Some(id) = self.record_ids.get(&decl.name).copied() else { continue };
                    let mut resolved = Vec::new();
                    for field in fields {
                        let ty = self.resolve_ty(&field.ty);
                        if resolved.iter().any(|(n, _): &(String, Ty)| *n == field.name) {
                            self.error(
                                field.span,
                                format!("field `{}` is declared twice", field.name),
                            );
                        }
                        resolved.push((field.name.clone(), ty));
                    }
                    self.hir.records[id.0 as usize].fields = resolved;
                }
                ast::TypeDef::Sum(variants) => {
                    let Some(id) = self.sum_ids.get(&decl.name).copied() else { continue };
                    let mut resolved = Vec::new();
                    for (index, variant) in variants.iter().enumerate() {
                        if let Some((owner, _)) = self.ctors.get(&variant.name) {
                            let owner = self.hir.sums[owner.0 as usize].name.clone();
                            self.error(
                                variant.span,
                                format!(
                                    "constructor `{}` is already declared by `{owner}`",
                                    variant.name
                                ),
                            );
                        }
                        self.ctors.insert(variant.name.clone(), (id, index as u32));
                        self.ctor_defs.insert(variant.name.clone(), variant.span);
                        let fields = variant
                            .fields
                            .iter()
                            .map(|f| (f.name.clone(), self.resolve_ty(&f.ty)))
                            .collect();
                        resolved.push(Variant { name: variant.name.clone(), fields });
                    }
                    self.hir.sums[id.0 as usize].variants = resolved;
                }
                ast::TypeDef::Alias(target) => {
                    let ty = self.resolve_ty(target);
                    self.aliases.insert(decl.name.clone(), ty);
                }
            }
        }
    }

    /// Declares the platform's own types, for the modules that asked for them.
    ///
    /// Asking means naming one as the type of a port. That is the whole opt-in,
    /// and it matters because registering `Input` also registers `Click`,
    /// `Enter` and the rest as constructors — ordinary names a UI program might
    /// want for itself. A file that never mentions a platform type reserves
    /// nothing from it; a file that declares its own type of that name keeps
    /// its own, and is left alone here rather than told it clashes with
    /// something it never asked for.
    fn collect_platform_types(&mut self, program: &ast::Program) {
        for (name, variants) in
            [(input::TYPE_NAME, input::VARIANTS), (lifecycle::TYPE_NAME, lifecycle::VARIANTS)]
        {
            self.collect_platform_type(program, name, variants);
        }
    }

    fn collect_platform_type(
        &mut self,
        program: &ast::Program,
        type_name: &str,
        variants: &[input::Variant],
    ) {
        let asked = program.items.iter().any(|item| match item {
            ast::Item::Actor(decl) => decl.inbox.iter().chain(&decl.outbox).any(|port| {
                matches!(&port.ty, ast::TypeExpr::Named { name, args, .. }
                    if name == type_name && args.is_empty())
            }),
            _ => false,
        });
        let declared_here = program
            .items
            .iter()
            .any(|item| matches!(item, ast::Item::Type(decl) if decl.name == type_name));
        if !asked || declared_here {
            return;
        }

        let id = SumId(self.hir.sums.len() as u32);
        self.hir.sums.push(SumDef {
            name: type_name.to_string(),
            variants: variants
                .iter()
                .map(|variant| Variant {
                    name: variant.name.to_string(),
                    fields: variant
                        .fields
                        .iter()
                        .map(|(name, field)| {
                            let ty = match field {
                                input::Field::Int => Ty::Int,
                                input::Field::Float => Ty::Float,
                            };
                            (name.to_string(), ty)
                        })
                        .collect(),
                })
                .collect(),
        });
        self.sum_ids.insert(type_name.to_string(), id);
        for (index, variant) in variants.iter().enumerate() {
            self.ctors.insert(variant.name.to_string(), (id, index as u32));
        }
        // No `type_defs` or `ctor_defs` entry: there is no declaration in this
        // file for go-to-definition to land on.
    }

    fn collect_signatures(&mut self, program: &ast::Program) {
        for (key, decl, _) in fn_decls(program) {
            if self.signatures.contains_key(&key) {
                self.error(decl.span, format!("function `{}` is declared twice", decl.name));
                continue;
            }
            let params: Vec<(String, Ty)> = decl
                .params
                .iter()
                .map(|p| (p.name.clone(), self.resolve_ty(&p.ty)))
                .collect();
            let ret = decl.ret.as_ref().map_or(Ty::Unit, |t| self.resolve_ty(t));
            self.check_view_signature(decl, &params, &ret);
            let id = FuncId(self.signatures.len() as u32);
            self.signatures.insert(
                key,
                Signature { id, params, ret, is_view: decl.is_view, def_span: decl.name_span },
            );
        }
    }

    /// Notes what to say when the cursor lands on `span`.
    ///
    /// For anything the platform provides rather than the file declares, since
    /// there is no declaration for go-to-definition to land on and be read.
    fn describe(&mut self, span: Span, signature: &str, doc: &str) {
        self.analysis.descriptions.push((span, format!("{signature}\n{doc}")));
    }

    /// `Node` is a value you may build and hand back, and nothing else.
    ///
    /// A node is emitted into the frame's array at the point it is written, so
    /// storing one would mean a node that appears somewhere other than where it
    /// was built. Rather than let that be a subtle ordering bug, the type
    /// system makes it unsayable — the same move as §5.3's flat message rule.
    fn check_view_signature(&mut self, decl: &ast::FnDecl, params: &[(String, Ty)], ret: &Ty) {
        for (name, ty) in params {
            if *ty == Ty::Node {
                self.error_labeled(
                    decl.span,
                    format!("parameter `{name}` cannot be a Node"),
                    "nodes are not values you can pass",
                    "a node is emitted where it is written, so passing one would \
                     put it somewhere else in the tree — pass the data and build \
                     the node in place, or call a `view fn` as a child",
                );
            }
        }

        if decl.is_view && *ret != Ty::Node {
            let found = self.show(ret);
            self.error_labeled(
                decl.span,
                format!("`view fn {}` must return Node, found {found}", decl.name),
                "not a view",
                "a view function is `view fn name(...) -> Node`",
            );
        }
        if !decl.is_view && *ret == Ty::Node {
            self.error_labeled(
                decl.span,
                format!("`{}` returns Node but is not a view", decl.name),
                "missing `view`",
                format!("write `view fn {}` — only a view may build nodes (§6.2)", decl.name),
            );
        }
    }

    /// A view returns one node, so a node built as a *statement* is built and
    /// then dropped on the floor — it never joins the tree.
    ///
    /// Left unchecked this is only caught when the host reads a frame with two
    /// roots in it, which is a long way from the line that wrote them.
    fn reject_orphan_nodes(&mut self, body: &Block, source: &ast::Block) {
        let spans: Vec<Span> = source
            .stmts
            .iter()
            .filter_map(|stmt| match stmt {
                ast::Stmt::Expr(expr) => Some(expr.span()),
                _ => None,
            })
            .collect();

        let mut seen = 0;
        let mut orphans = Vec::new();
        for stmt in &body.stmts {
            if let Stmt::Expr(expr) = stmt {
                if expr.ty == Ty::Node {
                    if let Some(span) = spans.get(seen) {
                        orphans.push(*span);
                    }
                }
                seen += 1;
            }
        }

        for span in orphans {
            self.error_labeled(
                span,
                "this node is built but never placed",
                "nowhere to go",
                "a view returns one node — put this inside a container's block, \
                 or make it the value the view returns",
            );
        }
    }

    /// A message crosses into another arena, where any pointer it carries
    /// would be meaningless. Flat payloads let the wire format *be* the memory
    /// format — the Cap'n Proto lesson in §17 — so the
    /// receiver never parses anything. Anything needing relocation is rejected.
    fn check_message_is_flat(&mut self, message: &Ty, span: Span) {
        let offender = match message {
            // Strings are relocated by codegen, which knows their one layout.
            Ty::Str | Ty::Int | Ty::Float | Ty::Bool | Ty::Error => None,
            Ty::Sum(id) => {
                let def = &self.hir.sums[id.0 as usize];
                def.variants
                    .iter()
                    .flat_map(|variant| variant.fields.iter())
                    .find(|(_, ty)| !matches!(ty, Ty::Int | Ty::Float | Ty::Bool))
                    .map(|(name, ty)| format!("field `{name}` is {}", self.show(ty)))
            }
            other => Some(format!("{} cannot be sent", self.show(other))),
        };

        if let Some(offender) = offender {
            self.error_labeled(
                span,
                format!("message types must be flat: {offender}"),
                "not sendable",
                "a message may only carry int, float, bool — anything holding a \
                 pointer would arrive in an arena where that pointer means nothing",
            );
        }
    }

    /// Validates every actor's shape and records what codegen needs (§5.1).
    fn collect_actors(&mut self, program: &ast::Program) {
        for item in &program.items {
            let ast::Item::Actor(decl) = item else { continue };
            if self.hir.actors.iter().any(|a| a.name == decl.name) {
                self.error(
                    decl.name_span,
                    format!("actor `{}` is declared twice", decl.name),
                );
                continue;
            }

            let state = self.resolve_ty(&decl.state);
            let inbox = self.collect_ports(&decl.inbox);
            let outbox = self.collect_ports(&decl.outbox);

            // One namespace for both directions. They are different channels,
            // but `send(counts, ...)` and `on counts(...)` sitting in one actor
            // and meaning different ports is a reading trap with nothing on the
            // other side of it.
            let mut names: Vec<&str> = Vec::new();
            for port in decl.inbox.iter().chain(&decl.outbox) {
                if names.contains(&port.name.as_str()) {
                    self.error(
                        port.name_span,
                        format!("port `{}` is declared twice", port.name),
                    );
                }
                names.push(&port.name);
            }

            let Some(init) = self.signatures.get(&qualified(&decl.name, "init")).cloned() else {
                continue;
            };

            if !init.params.is_empty() {
                self.error(decl.init.span, "`init` takes no parameters");
            }
            if !init.ret.unifies(&state) {
                let (found, want) = (self.show(&init.ret), self.show(&state));
                self.error(
                    decl.init.span,
                    format!("`init` must return the actor state {want}, found {found}"),
                );
            }

            // Every `in` port needs its handler, and every handler needs its
            // port. Either half alone is a channel that silently does nothing.
            let mut handlers = Vec::new();
            for (index, port) in decl.inbox.iter().enumerate() {
                let Some(handler) = decl.handlers.iter().find(|h| h.name == port.name) else {
                    self.error_labeled(
                        port.span,
                        format!("port `{}` has no handler", port.name),
                        "nothing receives on this port",
                        format!(
                            "add `on {}(state: {}, msg: {}): {}` — a port without a \
                             handler is a channel whose messages go nowhere",
                            port.name,
                            self.show(&state),
                            self.show(&inbox[index].ty),
                            self.show(&state),
                        ),
                    );
                    continue;
                };
                let Some(signature) =
                    self.signatures.get(&qualified(&decl.name, &handler.name)).cloned()
                else {
                    continue;
                };
                self.check_handler(handler, &signature, &state, &inbox[index].ty, &port.name);
                handlers.push(signature.id);
            }
            for handler in &decl.handlers {
                if !decl.inbox.iter().any(|port| port.name == handler.name) {
                    let known: Vec<&str> =
                        decl.inbox.iter().map(|port| port.name.as_str()).collect();
                    let help = if known.is_empty() {
                        format!(
                            "this actor declares no `in` ports — add `in {}: SomeType` \
                             above",
                            handler.name
                        )
                    } else {
                        format!("this actor receives on: {}", known.join(", "))
                    };
                    self.error_labeled(
                        handler.name_span,
                        format!("`{}` is not a port on this actor", handler.name),
                        "no such port",
                        help,
                    );
                }
            }
            // A handler is only reached through its port, so one that failed
            // the check above would leave `handlers` short and misalign every
            // port after it. Refusing to record the actor at all is better than
            // recording one whose port indices lie.
            if handlers.len() != decl.inbox.len() {
                continue;
            }

            // A UI actor is one that declares how to draw itself. Everything
            // else about it — mailbox, state, supervision — is unchanged.
            let view = decl.view.as_ref().and_then(|view_decl| {
                let signature =
                    self.signatures.get(&qualified(&decl.name, &view_decl.name)).cloned()?;
                match signature.params.as_slice() {
                    [(_, only)] if only.unifies(&state) => {}
                    _ => {
                        let want = self.show(&state);
                        self.error_labeled(
                            view_decl.span,
                            format!("`view` takes the actor state {want} and nothing else"),
                            "wrong parameters",
                            "a view is a pure function of state (§6.5), so the state \
                             is all it gets",
                        );
                    }
                }
                Some(signature.id)
            });

            self.hir.actors.push(ActorInfo {
                name: decl.name.clone(),
                state,
                inbox,
                outbox,
                init: init.id,
                handlers,
                view,
            });
        }
    }

    /// Resolves a run of port declarations, enforcing §7's flatness rule on
    /// each: what crosses a channel is copied into another arena, so a payload
    /// holding a pointer would arrive meaning nothing.
    fn collect_ports(&mut self, ports: &[ast::Port]) -> Vec<PortInfo> {
        ports
            .iter()
            .map(|port| {
                let ty = self.resolve_ty(&port.ty);
                self.check_message_is_flat(&ty, port.span);
                PortInfo { name: port.name.clone(), ty }
            })
            .collect()
    }

    /// `on <port>(state, msg): State` — the shape §6.5 asks for, checked
    /// against the port it is named after rather than against a convention.
    fn check_handler(
        &mut self,
        decl: &ast::FnDecl,
        signature: &Signature,
        state: &Ty,
        message: &Ty,
        port: &str,
    ) {
        match signature.params.as_slice() {
            [(_, first), (_, second)] => {
                if !first.unifies(state) {
                    let (found, want) = (self.show(first), self.show(state));
                    self.error(
                        decl.span,
                        format!("`on {port}` takes the state {want} first, found {found}"),
                    );
                }
                if !second.unifies(message) {
                    let (found, want) = (self.show(second), self.show(message));
                    self.error(
                        decl.span,
                        format!(
                            "`on {port}` receives {want}, which is what the port \
                             carries, found {found}"
                        ),
                    );
                }
            }
            _ => self.error_labeled(
                decl.span,
                format!("`on {port}` takes exactly the state and the message"),
                "wrong parameters",
                format!(
                    "write `on {port}(state: {}, msg: {}): {}`",
                    self.show(state),
                    self.show(message),
                    self.show(state),
                ),
            ),
        }

        if !signature.ret.unifies(state) {
            let (found, want) = (self.show(&signature.ret), self.show(state));
            self.error(
                decl.span,
                format!("`on {port}` must return the next state {want}, found {found}"),
            );
        }
    }

    /// Resolves `app Name { ... }` into instances and wires (§7).
    ///
    /// Everything here is a name that has to mean something: an instance names
    /// an actor the file declares, and each half of a wire names a port that
    /// actor has. None of it survives into the running program as a name — the
    /// runtime is handed indices — so this is the only place the mistakes are
    /// catchable.
    fn collect_app(&mut self, program: &ast::Program) {
        let mut seen: Option<&ast::AppDecl> = None;
        for item in &program.items {
            let ast::Item::App(decl) = item else { continue };
            if let Some(first) = seen {
                self.error(
                    decl.name_span,
                    format!("a file declares at most one app; `{}` is already declared", first.name),
                );
                continue;
            }
            seen = Some(decl);

            let mut instances: Vec<InstanceInfo> = Vec::new();
            for instance in &decl.instances {
                if instances.iter().any(|i| i.name == instance.name) {
                    self.error(
                        instance.name_span,
                        format!("`{}` is already running in this app", instance.name),
                    );
                    continue;
                }
                let Some(actor) =
                    self.hir.actors.iter().position(|a| a.name == instance.actor)
                else {
                    let known: Vec<&str> =
                        self.hir.actors.iter().map(|a| a.name.as_str()).collect();
                    let help = if known.is_empty() {
                        "this file declares no actors".to_string()
                    } else {
                        format!("this file declares: {}", known.join(", "))
                    };
                    self.error_labeled(
                        instance.actor_span,
                        format!("unknown actor `{}`", instance.actor),
                        "no such actor",
                        help,
                    );
                    continue;
                };
                instances.push(InstanceInfo { name: instance.name.clone(), actor });
            }

            let mut wires: Vec<Wire> = Vec::new();
            for wire in &decl.wires {
                let Some(from) = self.resolve_port_ref(&instances, &wire.from, Direction::Out)
                else {
                    continue;
                };
                let Some(to) = self.resolve_port_ref(&instances, &wire.to, Direction::In) else {
                    continue;
                };

                let out_ty = self.hir.actors[instances[from.0].actor].outbox[from.1].ty.clone();
                let in_ty = self.hir.actors[instances[to.0].actor].inbox[to.1].ty.clone();
                if !out_ty.unifies(&in_ty) {
                    let (sent, taken) = (self.show(&out_ty), self.show(&in_ty));
                    self.error_labeled(
                        wire.span,
                        format!("this wire carries {sent} into a port that takes {taken}"),
                        "the two ends disagree",
                        "both halves of a channel are declared, so the compiler can \
                         say so here rather than the receiver reading nonsense (§5.3)",
                    );
                    continue;
                }
                if wires.iter().any(|w| w.from == from.0 && w.from_port == from.1) {
                    self.error_labeled(
                        wire.from.span,
                        format!("`{}.{}` is already wired", wire.from.instance, wire.from.port),
                        "a second destination",
                        "an out port is one channel with one far end; give the actor \
                         a second `out` port to send two places",
                    );
                    continue;
                }
                wires.push(Wire { from: from.0, from_port: from.1, to: to.0, to_port: to.1 });
            }

            // An out port nobody wired is a `send` that vanishes. §8.2 says a
            // diagnostic is a product surface, and "your messages went nowhere"
            // discovered at run time is the opposite of one.
            let mut dangling: Vec<(String, String)> = Vec::new();
            for (index, instance) in instances.iter().enumerate() {
                let actor = &self.hir.actors[instance.actor];
                for (port, info) in actor.outbox.iter().enumerate() {
                    if wires.iter().any(|w| w.from == index && w.from_port == port) {
                        continue;
                    }
                    dangling.push((instance.name.clone(), info.name.clone()));
                }
            }
            for (name, port_name) in dangling {
                self.error_labeled(
                    decl.name_span,
                    format!("`{name}.{port_name}` is not wired to anything"),
                    "an out port with no far end",
                    format!(
                        "add `{name}.{port_name} -> someone.somePort`, or drop the \
                         `out {port_name}` declaration — as written, everything \
                         sent on it would be discarded"
                    ),
                );
            }

            self.hir.app =
                Some(AppInfo { name: decl.name.clone(), instances, wires });
        }
    }

    /// One half of a wire: which instance, and which of its ports.
    fn resolve_port_ref(
        &mut self,
        instances: &[InstanceInfo],
        reference: &ast::PortRef,
        direction: Direction,
    ) -> Option<(usize, usize)> {
        let Some(index) = instances.iter().position(|i| i.name == reference.instance) else {
            let known: Vec<&str> = instances.iter().map(|i| i.name.as_str()).collect();
            let help = if known.is_empty() {
                "this app runs no actors yet — write `name = SomeActor` first".to_string()
            } else {
                format!("this app runs: {}", known.join(", "))
            };
            self.error_labeled(
                reference.instance_span,
                format!("`{}` is not running in this app", reference.instance),
                "unknown name",
                help,
            );
            return None;
        };
        let actor = &self.hir.actors[instances[index].actor];
        let (found, known) = match direction {
            Direction::Out => (
                actor.out_port(&reference.port),
                actor.outbox.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
            ),
            Direction::In => (
                actor.in_port(&reference.port),
                actor.inbox.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
            ),
        };
        let Some(port) = found else {
            let (actor_name, word) = (actor.name.clone(), direction.word());
            let help = if known.is_empty() {
                format!("`{actor_name}` declares no `{word}` ports")
            } else {
                format!("`{actor_name}` {} on: {}", direction.verb(), known.join(", "))
            };
            self.error_labeled(
                reference.port_span,
                format!("`{actor_name}` has no `{word}` port called `{}`", reference.port),
                "no such port",
                help,
            );
            return None;
        };
        Some((index, port))
    }

    /// Resolves a written type, recording what it resolved to.
    ///
    /// A type annotation is not an expression, so nothing else records one —
    /// and an annotation is exactly where someone asks what a name means. The
    /// spans nest (`List<Todo>` contains `Todo`), and hover takes the narrowest,
    /// so both answer for themselves.
    fn resolve_ty(&mut self, ty: &ast::TypeExpr) -> Ty {
        let resolved = self.resolve_ty_inner(ty);
        self.analysis.types.push((ty.span(), resolved.clone()));
        resolved
    }

    fn resolve_ty_inner(&mut self, ty: &ast::TypeExpr) -> Ty {
        match ty {
            ast::TypeExpr::Optional { inner, .. } => Ty::Option(Box::new(self.resolve_ty(inner))),
            ast::TypeExpr::Fn { span, .. } => {
                self.error(*span, "function types are not supported yet");
                Ty::Error
            }
            ast::TypeExpr::Named { name, args, span } => {
                // Covers records, sums and aliases alike, before the branches
                // below split on which kind this is.
                if let Some(declared_at) = self.type_defs.get(name).copied() {
                    self.record_use(*span, declared_at);
                }
                let arity = args.len();
                let mut arg_tys: Vec<Ty> = args.iter().map(|a| self.resolve_ty(a)).collect();

                let expect = |cx: &mut Self, want: usize| {
                    if arity != want {
                        cx.error(
                            *span,
                            format!("`{name}` takes {want} type argument(s), found {arity}"),
                        );
                        false
                    } else {
                        true
                    }
                };

                match name.as_str() {
                    "Node" => {
                        if arity != 0 {
                            self.error(*span, "`Node` takes no type arguments");
                        }
                        Ty::Node
                    }
                    "int" | "float" | "bool" | "string" | "unit" => {
                        if arity != 0 {
                            self.error(*span, format!("`{name}` takes no type arguments"));
                        }
                        match name.as_str() {
                            "int" => Ty::Int,
                            "float" => Ty::Float,
                            "bool" => Ty::Bool,
                            "string" => Ty::Str,
                            _ => Ty::Unit,
                        }
                    }
                    "List" => {
                        if !expect(self, 1) {
                            return Ty::Error;
                        }
                        Ty::List(Box::new(arg_tys.remove(0)))
                    }
                    "Option" => {
                        if !expect(self, 1) {
                            return Ty::Error;
                        }
                        Ty::Option(Box::new(arg_tys.remove(0)))
                    }
                    "Result" => {
                        if !expect(self, 2) {
                            return Ty::Error;
                        }
                        let err = arg_tys.remove(1);
                        Ty::Result(Box::new(arg_tys.remove(0)), Box::new(err))
                    }
                    _ => {
                        if let Some(id) = self.record_ids.get(name) {
                            Ty::Record(*id)
                        } else if let Some(id) = self.sum_ids.get(name) {
                            Ty::Sum(*id)
                        } else if let Some(ty) = self.aliases.get(name) {
                            ty.clone()
                        } else {
                            self.error(*span, format!("unknown type `{name}`"));
                            Ty::Error
                        }
                    }
                }
            }
        }
    }

    // ---- bodies ----------------------------------------------------------

    fn check_bodies(&mut self, program: &ast::Program) {
        // Emit functions in signature id order so FuncId indexes `hir.funcs`.
        let mut decls: Vec<(String, &ast::FnDecl, Option<&str>)> = Vec::new();
        for (key, decl, owner) in fn_decls(program) {
            // Only the first declaration of a name owns the id; a duplicate has
            // already been reported and emits no body.
            let is_first = !decls.iter().any(|(seen, _, _)| *seen == key);
            if is_first && self.signatures.contains_key(&key) {
                decls.push((key, decl, owner));
            }
        }
        decls.sort_by_key(|(key, _, _)| self.signatures[key].id.0);

        for (key, decl, owner) in decls {
            let signature = self.signatures[&key].clone();
            self.scopes.clear();
            self.locals.clear();
            self.ret_ty = signature.ret.clone();
            self.in_view = signature.is_view;
            // `send` names a port, and a port belongs to an actor. Only the
            // actor's own functions are inside one, which is exactly the set
            // that may send.
            self.sender = owner
                .and_then(|name| self.hir.actors.iter().position(|a| a.name == name));

            self.scopes.push(HashMap::new());
            // `signature.params` carries no spans; the declaration it was built
            // from does, in the same order.
            for (index, (name, ty)) in signature.params.iter().enumerate() {
                let def_span =
                    decl.params.get(index).map(|param| param.span).unwrap_or(decl.span);
                self.declare(name.clone(), ty.clone(), false, def_span);
            }
            self.param_count = self.locals.len();

            let body = self.check_block(&decl.body, Some(&signature.ret));
            if signature.is_view {
                self.reject_orphan_nodes(&body, &decl.body);
            }
            if !body.ty.unifies(&signature.ret) {
                let (found, want) = (self.show(&body.ty), self.show(&signature.ret));
                self.error(
                    decl.body.span,
                    format!("function `{}` returns {want}, but its body has type {found}", decl.name),
                );
            }

            self.hir.funcs.push(Func {
                name: decl.name.clone(),
                is_view: signature.is_view,
                ret: signature.ret,
                locals: std::mem::take(&mut self.locals),
                param_count: self.param_count,
                body,
            });
        }
    }

    fn declare(&mut self, name: String, ty: Ty, mutable: bool, def_span: Span) -> u32 {
        // Every binding in the language comes through here — parameters, lets,
        // a `for` variable, a pattern binding — so recording the type once here
        // is what makes hover work on all four.
        self.analysis.types.push((def_span, ty.clone()));
        let slot = self.locals.len() as u32;
        self.locals.push(ty.clone());
        self.scopes
            .last_mut()
            .expect("a scope is always open")
            .insert(name, Local { slot, ty, mutable, def_span });
        slot
    }

    fn lookup(&self, name: &str) -> Option<&Local> {
        self.scopes.iter().rev().find_map(|scope| scope.get(name))
    }

    /// Notes that the name written at `use_site` refers to a declaration made at
    /// `declared_at`.
    fn record_use(&mut self, use_site: Span, declared_at: Span) {
        self.analysis.definitions.push((use_site, declared_at));
    }

    fn check_block(&mut self, block: &ast::Block, expected: Option<&Ty>) -> Block {
        self.scopes.push(HashMap::new());

        let mut stmts = Vec::new();
        let mut diverges = false;
        for stmt in &block.stmts {
            match stmt {
                ast::Stmt::Let { name, name_span, ty, value, mutable, span } => {
                    let want = ty.as_ref().map(|t| self.resolve_ty(t));
                    let value = self.check_expr(value, want.as_ref());
                    let ty = match want {
                        Some(want) => {
                            if !value.ty.unifies(&want) {
                                let (found, want_s) = (self.show(&value.ty), self.show(&want));
                                self.error(
                                    *span,
                                    format!("`{name}` is declared {want_s} but initialised with {found}"),
                                );
                            }
                            want
                        }
                        None => {
                            if matches!(value.ty, Ty::Unit | Ty::Never) {
                                self.error(*span, format!("`{name}` cannot bind a unit value"));
                            }
                            value.ty.clone()
                        }
                    };
                    if ty == Ty::Node {
                        self.error_labeled(
                            *span,
                            format!("`{name}` cannot hold a Node"),
                            "nodes are emitted, not stored",
                            "a node joins the tree where it is written, so binding one                              would separate the two — write the builder call where the                              node belongs, or wrap it in a `view fn` and call that",
                        );
                    }
                    let slot = self.declare(name.clone(), ty, *mutable, *name_span);
                    stmts.push(Stmt::Let { slot, value });
                }
                ast::Stmt::Assign { target, value, span } => {
                    let ast::Expr::Ident { name, .. } = target else {
                        // §4.2 makes data immutable; only `var` locals are places.
                        self.error(
                            *span,
                            "only `var` locals can be assigned; records are immutable",
                        );
                        let value = self.check_expr(value, None);
                        stmts.push(Stmt::Expr(value));
                        continue;
                    };
                    let Some(local) = self.lookup(name).cloned() else {
                        self.error(*span, format!("unknown variable `{name}`"));
                        let value = self.check_expr(value, None);
                        stmts.push(Stmt::Expr(value));
                        continue;
                    };
                    if !local.mutable {
                        self.error_labeled(
                            *span,
                            format!("`{name}` is immutable"),
                            "assignment to a `let` binding",
                            format!("declare it as `var {name}` to allow assignment"),
                        );
                    }
                    let value = self.check_expr(value, Some(&local.ty));
                    if !value.ty.unifies(&local.ty) {
                        let (found, want) = (self.show(&value.ty), self.show(&local.ty));
                        self.error(*span, format!("cannot assign {found} to {name}: {want}"));
                    }
                    stmts.push(Stmt::AssignLocal { slot: local.slot, value });
                }
                ast::Stmt::Return { value, span } => {
                    let ret = self.ret_ty.clone();
                    let value = match value {
                        Some(expr) => {
                            let expr = self.check_expr(expr, Some(&ret));
                            if !expr.ty.unifies(&ret) {
                                let (found, want) = (self.show(&expr.ty), self.show(&ret));
                                self.error(*span, format!("returns {found}, expected {want}"));
                            }
                            Some(expr)
                        }
                        None => {
                            if !ret.unifies(&Ty::Unit) {
                                let want = self.show(&ret);
                                self.error(*span, format!("returns nothing, expected {want}"));
                            }
                            None
                        }
                    };
                    diverges = true;
                    stmts.push(Stmt::Return(value));
                }
                ast::Stmt::Expr(expr) => {
                    let expr = self.check_expr(expr, None);
                    stmts.push(Stmt::Expr(expr));
                }
            }
        }

        let tail = block.tail.as_ref().map(|e| Box::new(self.check_expr(e, expected)));
        self.scopes.pop();

        let ty = match (&tail, diverges) {
            (Some(expr), _) => expr.ty.clone(),
            (None, true) => Ty::Never,
            (None, false) => Ty::Unit,
        };
        Block { stmts, tail, ty }
    }

    /// Records every expression's range and type on the way back out.
    ///
    /// Inner expressions finish first, so the narrower spans land in the table
    /// before the wider ones that contain them; `Analysis::type_at` picks the
    /// narrowest either way.
    fn check_expr(&mut self, expr: &ast::Expr, expected: Option<&Ty>) -> Expr {
        let checked = self.check_expr_inner(expr, expected);
        self.analysis.types.push((expr.span(), checked.ty.clone()));
        checked
    }

    fn check_expr_inner(&mut self, expr: &ast::Expr, expected: Option<&Ty>) -> Expr {
        match expr {
            ast::Expr::Int { value, .. } => Expr { ty: Ty::Int, kind: ExprKind::Int(*value) },
            ast::Expr::Float { value, .. } => {
                Expr { ty: Ty::Float, kind: ExprKind::Float(*value) }
            }
            ast::Expr::Bool { value, .. } => Expr { ty: Ty::Bool, kind: ExprKind::Bool(*value) },
            ast::Expr::Str { value, .. } => {
                Expr { ty: Ty::Str, kind: ExprKind::Str(value.clone()) }
            }

            ast::Expr::Ident { name, span } => {
                if let Some(local) = self.lookup(name) {
                    let (ty, slot, def_span) =
                        (local.ty.clone(), local.slot, local.def_span);
                    self.record_use(*span, def_span);
                    return Expr { ty, kind: ExprKind::Local(slot) };
                }
                // A niladic constructor used as a value: `None`, `EmptyTitle`.
                if name == "None" {
                    let ty = match expected {
                        Some(Ty::Option(inner)) => Ty::Option(inner.clone()),
                        _ => {
                            self.error(*span, "cannot tell what `None` is an Option of here");
                            Ty::Error
                        }
                    };
                    return Expr { ty, kind: ExprKind::MakeNone };
                }
                if let Some((sum, index)) = self.ctors.get(name).copied() {
                    if let Some(declared_at) = self.ctor_defs.get(name).copied() {
                        self.record_use(*span, declared_at);
                    }
                    let variant = &self.hir.sums[sum.0 as usize].variants[index as usize];
                    if !variant.fields.is_empty() {
                        let arity = variant.fields.len();
                        self.error(*span, format!("`{name}` needs {arity} argument(s)"));
                    }
                    return Expr {
                        ty: Ty::Sum(sum),
                        kind: ExprKind::MakeVariant { sum, variant: index, fields: Vec::new() },
                    };
                }
                self.errors
                    .push(Diagnostic::new(*span, format!("unknown name `{name}`"))
                        .with_label("not in scope"));
                Expr { ty: Ty::Error, kind: ExprKind::Unit }
            }

            ast::Expr::Unary { op, expr, span } => {
                let inner = self.check_expr(expr, None);
                let (ty, op) = match (op, &inner.ty) {
                    (ast::UnOp::Neg, Ty::Int) => (Ty::Int, UnOp::NegInt),
                    (ast::UnOp::Neg, Ty::Float) => (Ty::Float, UnOp::NegFloat),
                    (ast::UnOp::Not, Ty::Bool) => (Ty::Bool, UnOp::Not),
                    (_, Ty::Error | Ty::Never) => (Ty::Error, UnOp::Not),
                    (ast::UnOp::Neg, other) => {
                        let found = self.show(other);
                        self.error(*span, format!("cannot negate {found}"));
                        (Ty::Error, UnOp::NegInt)
                    }
                    (ast::UnOp::Not, other) => {
                        let found = self.show(other);
                        self.error(*span, format!("`!` needs a bool, found {found}"));
                        (Ty::Error, UnOp::Not)
                    }
                };
                Expr { ty, kind: ExprKind::Unary { op, expr: Box::new(inner) } }
            }

            ast::Expr::Binary { op, lhs, rhs, span } => self.check_binary(*op, lhs, rhs, *span),

            ast::Expr::Field { base, name, span } => {
                let base = self.check_expr(base, None);
                match &base.ty {
                    Ty::Record(id) => {
                        let def = &self.hir.records[id.0 as usize];
                        match def.fields.iter().position(|(n, _)| n == name) {
                            Some(index) => {
                                let ty = def.fields[index].1.clone();
                                Expr {
                                    ty,
                                    kind: ExprKind::FieldGet {
                                        base: Box::new(base),
                                        index: index as u32,
                                    },
                                }
                            }
                            None => {
                                let record = def.name.clone();
                                self.error(*span, format!("`{record}` has no field `{name}`"));
                                Expr { ty: Ty::Error, kind: ExprKind::Unit }
                            }
                        }
                    }
                    Ty::Error | Ty::Never => Expr { ty: Ty::Error, kind: ExprKind::Unit },
                    other => {
                        let found = self.show(other);
                        self.error(*span, format!("cannot read field `{name}` of {found}"));
                        Expr { ty: Ty::Error, kind: ExprKind::Unit }
                    }
                }
            }

            ast::Expr::Call { callee, args, span } => self.check_call(callee, args, *span, expected),

            ast::Expr::Build { name, name_span, args, children, span } => {
                self.check_build(name, *name_span, args, children.as_ref(), *span)
            }

            ast::Expr::RecordLit { name, base, fields, span } => {
                self.check_record_lit(name.as_deref(), base.as_deref(), fields, *span, expected)
            }

            ast::Expr::ListLit { items, span } => self.check_list_lit(items, *span, expected),

            ast::Expr::For { name, name_span, iter, body, span } => {
                self.check_for(name, *name_span, iter, body, *span)
            }

            ast::Expr::If { cond, then_block, else_block, span } => {
                let cond = self.check_expr(cond, Some(&Ty::Bool));
                if !cond.ty.unifies(&Ty::Bool) {
                    let found = self.show(&cond.ty);
                    self.error(*span, format!("`if` needs a bool condition, found {found}"));
                }
                let then_block = self.check_block(then_block, expected);
                let else_block = else_block.as_ref().map(|e| self.check_expr(e, expected));

                let ty = match &else_block {
                    Some(else_expr) => {
                        if !then_block.ty.unifies(&else_expr.ty) {
                            let (t, e) = (self.show(&then_block.ty), self.show(&else_expr.ty));
                            self.error(
                                *span,
                                format!("`if` branches disagree: {t} and {e}"),
                            );
                            Ty::Error
                        } else {
                            then_block.ty.join(&else_expr.ty)
                        }
                    }
                    // A view's `if` needs no `else`: §6.2 writes conditional
                    // children, and "no node" is a perfectly good result. It
                    // costs nothing to represent, because the frame's array
                    // simply has one fewer entry and its parent counts one
                    // fewer child.
                    None if then_block.ty == Ty::Node => Ty::Node,
                    None => {
                        // Without `else` the branch cannot produce a value.
                        if !then_block.ty.unifies(&Ty::Unit) {
                            let found = self.show(&then_block.ty);
                            self.error(
                                *span,
                                format!("`if` without `else` must have type unit, found {found}"),
                            );
                        }
                        Ty::Unit
                    }
                };
                Expr {
                    ty,
                    kind: ExprKind::If {
                        cond: Box::new(cond),
                        then_block,
                        else_block: else_block.map(Box::new),
                    },
                }
            }

            ast::Expr::Block(block) => {
                let block = self.check_block(block, expected);
                Expr { ty: block.ty.clone(), kind: ExprKind::Block(block) }
            }

            ast::Expr::Match { scrutinee, arms, span } => {
                self.check_match(scrutinee, arms, *span, expected)
            }

            ast::Expr::Try { expr, span } => self.check_try(expr, *span),
        }
    }

    fn check_binary(
        &mut self,
        op: ast::BinOp,
        lhs: &ast::Expr,
        rhs: &ast::Expr,
        span: Span,
    ) -> Expr {
        use ast::BinOp as B;

        let lhs = self.check_expr(lhs, None);
        let rhs = self.check_expr(rhs, Some(&lhs.ty));

        let poisoned = matches!(lhs.ty, Ty::Error | Ty::Never)
            || matches!(rhs.ty, Ty::Error | Ty::Never);

        if !poisoned && !lhs.ty.unifies(&rhs.ty) {
            // §4.2: no implicit coercion, ever.
            let (l, r) = (self.show(&lhs.ty), self.show(&rhs.ty));
            self.error_labeled(
                span,
                format!("`{}` needs matching types, found {l} and {r}", op.as_str()),
                format!("{l} vs {r}"),
                "Strand never coerces (§4.2); convert one side explicitly",
            );
            return Expr { ty: Ty::Error, kind: ExprKind::Unit };
        }

        let operand = if matches!(lhs.ty, Ty::Error | Ty::Never) { rhs.ty.clone() } else { lhs.ty.clone() };

        let (ty, hir_op) = match (op, &operand) {
            // §4.2's complaint about JS is `"1" + 1`, not `"a" + "b"`. Mixed
            // operands are still rejected above, so this cannot coerce.
            (B::Add, Ty::Str) => {
                return Expr {
                    ty: Ty::Str,
                    kind: ExprKind::CallHelper {
                        helper: Helper::StrConcat,
                        args: vec![lhs, rhs],
                    },
                }
            }

            (B::Add, Ty::Int) => (Ty::Int, BinOp::AddInt),
            (B::Sub, Ty::Int) => (Ty::Int, BinOp::SubInt),
            (B::Mul, Ty::Int) => (Ty::Int, BinOp::MulInt),
            (B::Div, Ty::Int) => (Ty::Int, BinOp::DivInt),
            (B::Rem, Ty::Int) => (Ty::Int, BinOp::RemInt),
            (B::Add, Ty::Float) => (Ty::Float, BinOp::AddFloat),
            (B::Sub, Ty::Float) => (Ty::Float, BinOp::SubFloat),
            (B::Mul, Ty::Float) => (Ty::Float, BinOp::MulFloat),
            (B::Div, Ty::Float) => (Ty::Float, BinOp::DivFloat),

            (B::Eq, Ty::Int) => (Ty::Bool, BinOp::EqInt),
            (B::Ne, Ty::Int) => (Ty::Bool, BinOp::NeInt),
            (B::Lt, Ty::Int) => (Ty::Bool, BinOp::LtInt),
            (B::Le, Ty::Int) => (Ty::Bool, BinOp::LeInt),
            (B::Gt, Ty::Int) => (Ty::Bool, BinOp::GtInt),
            (B::Ge, Ty::Int) => (Ty::Bool, BinOp::GeInt),

            (B::Eq, Ty::Float) => (Ty::Bool, BinOp::EqFloat),
            (B::Ne, Ty::Float) => (Ty::Bool, BinOp::NeFloat),
            (B::Lt, Ty::Float) => (Ty::Bool, BinOp::LtFloat),
            (B::Le, Ty::Float) => (Ty::Bool, BinOp::LeFloat),
            (B::Gt, Ty::Float) => (Ty::Bool, BinOp::GtFloat),
            (B::Ge, Ty::Float) => (Ty::Bool, BinOp::GeFloat),

            (B::Eq, Ty::Bool) => (Ty::Bool, BinOp::EqBool),
            (B::Ne, Ty::Bool) => (Ty::Bool, BinOp::NeBool),
            (B::And, Ty::Bool) => (Ty::Bool, BinOp::And),
            (B::Or, Ty::Bool) => (Ty::Bool, BinOp::Or),

            (_, Ty::Error | Ty::Never) => (Ty::Error, BinOp::AddInt),
            (_, other) => {
                let found = self.show(other);
                self.error(span, format!("`{}` does not apply to {found}", op.as_str()));
                (Ty::Error, BinOp::AddInt)
            }
        };

        Expr {
            ty,
            kind: ExprKind::Binary { op: hir_op, lhs: Box::new(lhs), rhs: Box::new(rhs) },
        }
    }

    /// `[a, b, c]`.
    ///
    /// The element type comes from the items, or — when there are none — from
    /// whatever the surrounding code was expecting. An empty list with nothing
    /// to learn from is refused rather than guessed at, because a `List<?>`
    /// has no representation and the error would surface much later.
    fn check_list_lit(&mut self, items: &[ast::Expr], span: Span, expected: Option<&Ty>) -> Expr {
        let wanted = match expected {
            Some(Ty::List(elem)) => Some((**elem).clone()),
            _ => None,
        };

        let mut checked = Vec::new();
        let mut elem = wanted.clone().unwrap_or(Ty::Error);
        for item in items {
            let value = self.check_expr(item, Some(&elem));
            if !value.ty.unifies(&elem) {
                let (found, want) = (self.show(&value.ty), self.show(&elem));
                self.error_labeled(
                    item.span(),
                    format!("this list holds {want}, but this item is {found}"),
                    "mismatched element",
                    "every element of a list has the same type (§4.2)",
                );
            } else {
                elem = elem.join(&value.ty);
            }
            checked.push(value);
        }

        if items.is_empty() && wanted.is_none() {
            self.error_labeled(
                span,
                "an empty list needs to be told what it holds",
                "element type unknown",
                "annotate it — `let todos: List<Todo> = []`",
            );
        }

        Expr { ty: Ty::List(Box::new(elem)), kind: ExprKind::MakeList { elems: checked } }
    }

    /// `for x in list { ... }`.
    fn check_for(
        &mut self,
        name: &str,
        name_span: Span,
        iter: &ast::Expr,
        body: &ast::Block,
        span: Span,
    ) -> Expr {
        let list = self.check_expr(iter, None);
        let elem = match &list.ty {
            Ty::List(elem) => (**elem).clone(),
            Ty::Error | Ty::Never => Ty::Error,
            other => {
                let found = self.show(other);
                self.error_labeled(
                    span,
                    format!("`for` needs a list, found {found}"),
                    "not a list",
                    "only `List<T>` can be walked over in the POC (§4.6)",
                );
                Ty::Error
            }
        };

        // The loop variable is scoped to the body, like any other binding.
        self.scopes.push(HashMap::new());
        let slot = self.declare(name.to_string(), elem, false, name_span);
        let body = self.check_block(body, None);
        self.scopes.pop();

        Expr {
            ty: Ty::Unit,
            kind: ExprKind::For { slot, list: Box::new(list), body },
        }
    }

    /// `push` and the list half of `len`/`isEmpty`.
    ///
    /// `len` reads naturally on both a string and a list, so it means both, and
    /// which one is decided by the argument rather than by the name. Two names
    /// for one question would be the worse trade.
    fn check_list_call(
        &mut self,
        name: &str,
        name_span: Span,
        args: &[ast::Arg],
        span: Span,
    ) -> Option<Expr> {
        if name == "push" {
            self.describe(
                name_span,
                "fn push(list: List<T>, value: T): List<T>",
                "A new list one longer. The original is untouched (§4.2).",
            );
            if args.len() != 2 {
                self.error_labeled(
                    span,
                    format!("`push` takes a list and a value, found {} argument(s)", args.len()),
                    "wrong number of arguments",
                    "fn push(list: List<T>, value: T): List<T>",
                );
                return Some(Expr { ty: Ty::Error, kind: ExprKind::Unit });
            }
            let list = self.check_expr(&args[0].value, None);
            let elem = match &list.ty {
                Ty::List(elem) => (**elem).clone(),
                Ty::Error | Ty::Never => Ty::Error,
                other => {
                    let found = self.show(other);
                    self.error_labeled(
                        args[0].span,
                        format!("`push` needs a list, found {found}"),
                        "not a list",
                        "fn push(list: List<T>, value: T): List<T>",
                    );
                    Ty::Error
                }
            };
            let value = self.check_expr(&args[1].value, Some(&elem));
            if !value.ty.unifies(&elem) {
                let (found, want) = (self.show(&value.ty), self.show(&elem));
                self.error_labeled(
                    args[1].span,
                    format!("this list holds {want}, but this value is {found}"),
                    "mismatched element",
                    "push adds an element, so it must be one",
                );
            }
            let ty = Ty::List(Box::new(elem.join(&value.ty)));
            return Some(Expr {
                ty,
                kind: ExprKind::ListPush { list: Box::new(list), value: Box::new(value) },
            });
        }

        if (name != "len" && name != "isEmpty") || args.len() != 1 {
            return None;
        }
        // Peek at the argument to decide which `len` this is. Checking it here
        // and again in the string path would report every error inside it twice.
        let checked = self.check_expr(&args[0].value, None);
        if !matches!(checked.ty, Ty::List(_)) {
            return Some(self.finish_stdlib(name, name_span, checked, args[0].span));
        }

        if name == "len" {
            self.describe(name_span, "fn len(list: List<T>): int", "How many elements.");
        } else {
            self.describe(
                name_span,
                "fn isEmpty(list: List<T>): bool",
                "Whether there is nothing in it.",
            );
        }

        let length = Expr { ty: Ty::Int, kind: ExprKind::ListLen { list: Box::new(checked) } };
        if name == "len" {
            return Some(length);
        }
        Some(Expr {
            ty: Ty::Bool,
            kind: ExprKind::Binary {
                op: BinOp::EqInt,
                lhs: Box::new(length),
                rhs: Box::new(Expr { ty: Ty::Int, kind: ExprKind::Int(0) }),
            },
        })
    }

    /// The string half of an already-checked `len`/`isEmpty` argument.
    fn finish_stdlib(&mut self, name: &str, name_span: Span, arg: Expr, span: Span) -> Expr {
        let fun = stdlib::lookup(name).expect("only called for stdlib names");
        self.describe(name_span, &fun.signature(), fun.doc);
        if !arg.ty.unifies(&Ty::Str) {
            let found = self.show(&arg.ty);
            self.error_labeled(
                span,
                format!("`{name}` takes a string or a list, found {found}"),
                "wrong type",
                fun.signature(),
            );
            return Expr { ty: if name == "len" { Ty::Int } else { Ty::Bool }, kind: ExprKind::Unit };
        }

        let count = Expr {
            ty: Ty::Int,
            kind: ExprKind::CallHelper { helper: Helper::StrCharCount, args: vec![arg] },
        };
        if name == "len" {
            return count;
        }
        Expr {
            ty: Ty::Bool,
            kind: ExprKind::Binary {
                op: BinOp::EqInt,
                lhs: Box::new(count),
                rhs: Box::new(Expr { ty: Ty::Int, kind: ExprKind::Int(0) }),
            },
        }
    }

    /// A call to one of `stdlib`'s functions.
    ///
    /// Checked exactly like a user function — the argument count and every type
    /// — because from the caller's side that is what it is. The only difference
    /// is where the body comes from.
    fn check_stdlib_call(
        &mut self,
        fun: &stdlib::Fun,
        name_span: Span,
        args: &[ast::Arg],
        span: Span,
    ) -> Expr {
        let kind_ty = |kind: stdlib::Kind| match kind {
            stdlib::Kind::Int => Ty::Int,
            stdlib::Kind::Str => Ty::Str,
            stdlib::Kind::Bool => Ty::Bool,
        };
        let ret = kind_ty(fun.ret);

        if args.len() != fun.params.len() {
            self.error_labeled(
                span,
                format!(
                    "`{}` takes {} argument(s), found {}",
                    fun.name,
                    fun.params.len(),
                    args.len()
                ),
                "wrong number of arguments",
                fun.signature(),
            );
        }

        let mut checked = Vec::new();
        for (index, arg) in args.iter().enumerate() {
            let want = fun.params.get(index).copied().map(kind_ty);
            let value = self.check_expr(&arg.value, want.as_ref());
            if let Some(want) = want {
                if !value.ty.unifies(&want) {
                    let (found, want) = (self.show(&value.ty), self.show(&want));
                    self.error_labeled(
                        arg.span,
                        format!("argument {} of `{}` is {want}, found {found}", index + 1, fun.name),
                        "wrong type",
                        fun.signature(),
                    );
                }
            }
            checked.push(value);
        }
        if checked.len() != fun.params.len() {
            return Expr { ty: ret, kind: ExprKind::Unit };
        }

        // These have no declaration to go to either, so the signature is what
        // hover has to say.
        self.analysis
            .descriptions
            .push((name_span, format!("{}\n{}", fun.signature(), fun.doc)));

        let kind = match fun.body {
            stdlib::Body::Helper(helper) => ExprKind::CallHelper { helper, args: checked },
            // `len(s) == 0`, out of pieces that already exist.
            stdlib::Body::LengthIsZero => ExprKind::Binary {
                op: BinOp::EqInt,
                lhs: Box::new(Expr {
                    ty: Ty::Int,
                    kind: ExprKind::CallHelper {
                        helper: Helper::StrCharCount,
                        args: checked,
                    },
                }),
                rhs: Box::new(Expr { ty: Ty::Int, kind: ExprKind::Int(0) }),
            },
        };
        Expr { ty: ret, kind }
    }

    /// §6.2's builder call: `column(gap: 4) { ... }`.
    ///
    /// Props are ordinary type-checked arguments, which is the whole argument
    /// for the builder DSL over a JSX-shaped syntax — there is no second mode
    /// with its own escape hatches, and a mistyped prop is a compile error like
    /// any other.
    fn check_build(
        &mut self,
        name: &str,
        name_span: Span,
        args: &[ast::Arg],
        children: Option<&ast::Block>,
        span: Span,
    ) -> Expr {
        let Some(builder) = ui::lookup(name) else {
            self.error(span, format!("unknown builder `{name}`"));
            return Expr { ty: Ty::Error, kind: ExprKind::Unit };
        };

        // There is no declaration to send go-to-definition at, so the signature
        // is the only way to find out what a builder takes.
        self.analysis.descriptions.push((name_span, builder.signature()));

        if !self.in_view {
            self.error_labeled(
                span,
                format!("`{name}` builds a node, so it belongs in a view"),
                "not inside a `view fn`",
                "mark the enclosing function `view fn name(...) -> Node` (§6.2)",
            );
        }

        // Props bind by label where one is written and by position otherwise —
        // the same rule ordinary calls use, so there is nothing new to learn.
        let mut props: Vec<(Slot, Expr)> = Vec::new();
        let mut filled: Vec<&'static str> = Vec::new();
        for (index, arg) in args.iter().enumerate() {
            let found = match &arg.name {
                Some(label) => builder.props.iter().find(|prop| prop.name == *label),
                None => builder.props.get(index),
            };
            let Some(prop) = found else {
                match &arg.name {
                    Some(label) => self.error_labeled(
                        arg.span,
                        format!("`{name}` has no prop `{label}`"),
                        "unknown prop",
                        builder.signature(),
                    ),
                    None => self.error(
                        arg.span,
                        format!("`{name}` takes {} argument(s), found {}", builder.props.len(), args.len()),
                    ),
                }
                self.check_expr(&arg.value, None);
                continue;
            };

            if filled.contains(&prop.name) {
                self.error(arg.span, format!("`{}` is given twice", prop.name));
            }
            filled.push(prop.name);

            let want = prop_ty(prop.ty);
            let value = self.check_expr(&arg.value, Some(&want));
            if !value.ty.unifies(&want) {
                let (found, want) = (self.show(&value.ty), self.show(&want));
                self.error_labeled(
                    arg.span,
                    format!("`{}` on `{name}` is {want}, found {found}", prop.name),
                    "wrong type",
                    "props are type-checked like any other argument (§6.2)",
                );
            }
            props.push((prop.slot, value));
        }

        for prop in builder.props {
            if prop.default.is_none() && !filled.contains(&prop.name) {
                self.error_labeled(
                    span,
                    format!("`{name}` needs `{}`", prop.name),
                    "missing prop",
                    builder.signature(),
                );
            }
        }

        let numbers = [
            number_default(builder, Slot::Number),
            number_default(builder, Slot::Number2),
        ];

        let children = match children {
            Some(block) => {
                if !builder.container {
                    self.error_labeled(
                        block.span,
                        format!("`{name}` has no children"),
                        "unexpected block",
                        format!("`{name}` is a leaf — it draws itself and nothing under it"),
                    );
                }
                self.check_children(block)
            }
            None => Block { stmts: Vec::new(), tail: None, ty: Ty::Unit },
        };

        Expr { ty: Ty::Node, kind: ExprKind::MakeNode { kind: builder.kind, props, numbers, children } }
    }

    /// A builder's trailing block. Every statement in it is a child, so unlike
    /// an ordinary block it has no tail value — the last node is a child like
    /// the others, not a result.
    fn check_children(&mut self, block: &ast::Block) -> Block {
        let mut checked = self.check_block(block, Some(&Ty::Node));

        let spans: Vec<Span> = block
            .stmts
            .iter()
            .filter_map(|stmt| match stmt {
                ast::Stmt::Expr(expr) => Some(expr.span()),
                _ => None,
            })
            .collect();
        let mut seen = 0;
        let mut complaints = Vec::new();
        for stmt in &checked.stmts {
            if let Stmt::Expr(expr) = stmt {
                if let Some(span) = spans.get(seen) {
                    complaints.push((expr.ty.clone(), *span));
                }
                seen += 1;
            }
        }
        for (ty, span) in complaints {
            self.expect_child_ty(&ty, span);
        }

        // The last child is a child like the others, not the block's value.
        if let Some(tail) = checked.tail.take() {
            if let Some(source) = &block.tail {
                self.expect_child_ty(&tail.ty, source.span());
            }
            checked.stmts.push(Stmt::Expr(*tail));
        }
        checked.ty = Ty::Unit;
        checked
    }

    /// Anything in a children block must either be a node or do nothing
    /// visible. A stray value would be silently dropped, which is exactly the
    /// class of mistake `{count && <Badge/>}` makes in JSX.
    fn expect_child_ty(&mut self, ty: &Ty, span: Span) {
        if matches!(ty, Ty::Node | Ty::Unit | Ty::Never | Ty::Error) {
            return;
        }
        let found = self.show(ty);
        self.error_labeled(
            span,
            format!("a child must be a Node, found {found}"),
            "not a node",
            "children are nodes — a bare value here would be silently dropped",
        );
    }

    fn check_call(
        &mut self,
        callee: &ast::Expr,
        args: &[ast::Arg],
        span: Span,
        expected: Option<&Ty>,
    ) -> Expr {
        let ast::Expr::Ident { name, .. } = callee else {
            // Method calls need a stdlib; §4.6 defers that past M1.
            self.error(span, "method calls are not supported yet");
            for arg in args {
                self.check_expr(&arg.value, None);
            }
            return Expr { ty: Ty::Error, kind: ExprKind::Unit };
        };

        // Built-in constructors first: they are not user functions.
        match name.as_str() {
            "Ok" | "Err" | "Some" => {
                if args.len() != 1 {
                    self.error(span, format!("`{name}` takes exactly one argument"));
                }
                // Context supplies the half the constructor cannot know: the
                // error type of `Ok(..)`, the ok type of `Err(..)`.
                let (want_ok, want_err) = match expected {
                    Some(Ty::Result(ok, err)) => (Some((**ok).clone()), Some((**err).clone())),
                    Some(Ty::Option(some)) => (Some((**some).clone()), None),
                    _ => (None, None),
                };
                let hint = if name == "Err" { want_err.clone() } else { want_ok.clone() };
                let inner = args
                    .first()
                    .map(|a| self.check_expr(&a.value, hint.as_ref()))
                    .unwrap_or(Expr { ty: Ty::Error, kind: ExprKind::Unit });

                let (ty, kind) = match name.as_str() {
                    "Ok" => (
                        Ty::Result(
                            Box::new(inner.ty.clone()),
                            Box::new(want_err.unwrap_or(Ty::Error)),
                        ),
                        ExprKind::MakeOk(Box::new(inner)),
                    ),
                    "Err" => (
                        Ty::Result(
                            Box::new(want_ok.unwrap_or(Ty::Error)),
                            Box::new(inner.ty.clone()),
                        ),
                        ExprKind::MakeErr(Box::new(inner)),
                    ),
                    _ => (
                        Ty::Option(Box::new(inner.ty.clone())),
                        ExprKind::MakeSome(Box::new(inner)),
                    ),
                };
                return Expr { ty, kind };
            }
            _ => {}
        }

        if let Some((sum, index)) = self.ctors.get(name).copied() {
            if let Some(declared_at) = self.ctor_defs.get(name).copied() {
                self.record_use(span, declared_at);
            }
            return self.check_variant_call(sum, index, name, args, span);
        }

        if !self.signatures.contains_key(name) {
            // A user function of the same name wins, so nothing here takes a
            // name out of circulation.
            // The *name's* span, not the call's: a description covering the
            // whole call would answer for every argument inside it too, so
            // hovering `title` in `trim(title)` would report `trim`.
            let name_span = callee.span();
            if let Some(expr) = self.check_list_call(name, name_span, args, span) {
                return expr;
            }
            if let Some(fun) = stdlib::lookup(name) {
                return self.check_stdlib_call(fun, name_span, args, span);
            }
        }

        // Host builtins are not user functions and cannot be shadowed.
        if name == "send" {
            return self.check_send(args, span);
        }

        // §4.3's second tier: a bug ends the actor. There is no catch, so the
        // type is `Never` — it unifies with whatever the surrounding
        // expression owes, which is what lets `panic` stand as a match arm or
        // as a function's tail without a value to return.
        if name == "panic" {
            if args.len() != 1 {
                self.error_labeled(
                    span,
                    "`panic` takes one message",
                    "wrong arguments",
                    "write `panic(\"what went wrong\")` — the message becomes the                      crash report's reason (§8.4)",
                );
            }
            let arg = args
                .first()
                .map(|a| self.check_expr(&a.value, Some(&Ty::Str)))
                .unwrap_or(Expr { ty: Ty::Error, kind: ExprKind::Unit });
            if !arg.ty.unifies(&Ty::Str) {
                let found = self.show(&arg.ty);
                self.error(span, format!("`panic` takes a string, found {found}"));
            }
            return Expr {
                ty: Ty::Never,
                kind: ExprKind::CallBuiltin { builtin: Builtin::Panic, args: vec![arg] },
            };
        }

        if name == "log" {
            if args.len() != 1 {
                self.error(span, "`log` takes exactly one argument");
            }
            let arg = args
                .first()
                .map(|a| self.check_expr(&a.value, Some(&Ty::Str)))
                .unwrap_or(Expr { ty: Ty::Error, kind: ExprKind::Unit });
            if !arg.ty.unifies(&Ty::Str) {
                let found = self.show(&arg.ty);
                self.error(span, format!("`log` takes a string, found {found}"));
            }
            return Expr {
                ty: Ty::Unit,
                kind: ExprKind::CallBuiltin { builtin: Builtin::Log, args: vec![arg] },
            };
        }

        let Some(signature) = self.signatures.get(name).cloned() else {
            self.error(span, format!("unknown function `{name}`"));
            for arg in args {
                self.check_expr(&arg.value, None);
            }
            return Expr { ty: Ty::Error, kind: ExprKind::Unit };
        };
        self.record_use(span, signature.def_span);

        if args.len() != signature.params.len() {
            self.error(
                span,
                format!(
                    "`{name}` takes {} argument(s), found {}",
                    signature.params.len(),
                    args.len()
                ),
            );
        }

        let mut checked = Vec::new();
        for (index, arg) in args.iter().enumerate() {
            let want = signature.params.get(index).map(|(_, t)| t.clone());
            // A label, if written, must match the parameter name.
            if let (Some(label), Some((param, _))) =
                (arg.name.as_ref(), signature.params.get(index))
            {
                if label != param {
                    self.error(
                        arg.span,
                        format!("argument {} of `{name}` is `{param}`, not `{label}`", index + 1),
                    );
                }
            }
            let value = self.check_expr(&arg.value, want.as_ref());
            if let Some(want) = want {
                if !value.ty.unifies(&want) {
                    let (found, want_s) = (self.show(&value.ty), self.show(&want));
                    self.error(
                        arg.span,
                        format!("argument {} of `{name}` is {want_s}, found {found}", index + 1),
                    );
                }
            }
            checked.push(value);
        }

        Expr {
            ty: signature.ret.clone(),
            kind: ExprKind::Call { func: signature.id, args: checked },
        }
    }

    fn check_variant_call(
        &mut self,
        sum: SumId,
        index: u32,
        name: &str,
        args: &[ast::Arg],
        span: Span,
    ) -> Expr {
        let fields = self.hir.sums[sum.0 as usize].variants[index as usize].fields.clone();
        if args.len() != fields.len() {
            self.error(
                span,
                format!("`{name}` takes {} argument(s), found {}", fields.len(), args.len()),
            );
        }

        let mut checked = Vec::new();
        for (position, arg) in args.iter().enumerate() {
            let want = fields.get(position).map(|(_, t)| t.clone());
            if let (Some(label), Some((field, _))) = (arg.name.as_ref(), fields.get(position)) {
                if label != field {
                    self.error(
                        arg.span,
                        format!("field {} of `{name}` is `{field}`, not `{label}`", position + 1),
                    );
                }
            }
            let value = self.check_expr(&arg.value, want.as_ref());
            if let Some(want) = want {
                if !value.ty.unifies(&want) {
                    let (found, want_s) = (self.show(&value.ty), self.show(&want));
                    self.error(
                        arg.span,
                        format!("field `{}` of `{name}` is {want_s}, found {found}",
                            fields[position].0),
                    );
                }
            }
            checked.push(value);
        }

        Expr {
            ty: Ty::Sum(sum),
            kind: ExprKind::MakeVariant { sum, variant: index, fields: checked },
        }
    }

    fn check_record_lit(
        &mut self,
        name: Option<&str>,
        base: Option<&ast::Expr>,
        fields: &[ast::FieldInit],
        span: Span,
        expected: Option<&Ty>,
    ) -> Expr {
        let id = match name.and_then(|n| self.record_ids.get(n).copied()) {
            Some(id) => id,
            None => match expected {
                // Anonymous `{ ... }` takes its type from context.
                Some(Ty::Record(id)) => *id,
                _ => {
                    let what = name.unwrap_or("record literal");
                    self.error(span, format!("unknown record type `{what}`"));
                    if let Some(base) = base {
                        self.check_expr(base, None);
                    }
                    for field in fields {
                        if let Some(value) = &field.value {
                            self.check_expr(value, None);
                        }
                    }
                    return Expr { ty: Ty::Error, kind: ExprKind::Unit };
                }
            },
        };

        let def = self.hir.records[id.0 as usize].clone();
        let mut slots: Vec<Option<Expr>> = vec![None; def.fields.len()];

        // `Model { ...state, draft: x }`. The spread is evaluated once into a
        // local, and every field the literal leaves out becomes an ordinary
        // read of that local — which is why this needs no new HIR node and no
        // codegen at all. The result is still a whole new record (§4.2); the
        // sugar removes the restating, not the copy.
        let spread = base.map(|base| {
            let value = self.check_expr(base, Some(&Ty::Record(id)));
            if !value.ty.unifies(&Ty::Record(id)) {
                let (found, want) = (self.show(&value.ty), self.show(&Ty::Record(id)));
                self.error(
                    base.span(),
                    format!("`...` here spreads {want}, found {found}"),
                );
            }
            let slot = self.locals.len() as u32;
            self.locals.push(Ty::Record(id));
            (slot, value)
        });

        for field in fields {
            let Some(index) = def.fields.iter().position(|(n, _)| *n == field.name) else {
                self.error(
                    field.span,
                    format!("`{}` has no field `{}`", def.name, field.name),
                );
                if let Some(value) = &field.value {
                    self.check_expr(value, None);
                }
                continue;
            };
            if slots[index].is_some() {
                self.error(field.span, format!("field `{}` is set twice", field.name));
            }

            let want = def.fields[index].1.clone();
            // Shorthand `Todo { title }` reads the binding of the same name.
            let value = match &field.value {
                Some(value) => self.check_expr(value, Some(&want)),
                None => self.check_expr(
                    &ast::Expr::Ident { name: field.name.clone(), span: field.span },
                    Some(&want),
                ),
            };
            if !value.ty.unifies(&want) {
                let (found, want_s) = (self.show(&value.ty), self.show(&want));
                self.error(
                    field.span,
                    format!("field `{}` is {want_s}, found {found}", field.name),
                );
            }
            slots[index] = Some(value);
        }

        let mut values = Vec::new();
        for (index, slot) in slots.into_iter().enumerate() {
            match slot {
                Some(value) => values.push(value),
                // Unset, and there is a spread to take it from.
                None if spread.is_some() => {
                    let (base_slot, _) = spread.as_ref().expect("just checked");
                    let ty = def.fields[index].1.clone();
                    values.push(Expr {
                        ty: ty.clone(),
                        kind: ExprKind::FieldGet {
                            base: Box::new(Expr {
                                ty: Ty::Record(id),
                                kind: ExprKind::Local(*base_slot),
                            }),
                            index: index as u32,
                        },
                    });
                }
                None => {
                    self.error(
                        span,
                        format!("`{}` is missing field `{}`", def.name, def.fields[index].0),
                    );
                    values.push(Expr { ty: Ty::Error, kind: ExprKind::Unit });
                }
            }
        }

        let made =
            Expr { ty: Ty::Record(id), kind: ExprKind::MakeRecord { record: id, fields: values } };

        match spread {
            None => made,
            // The spread is bound before the record is built, so an expression
            // with a side effect runs once rather than once per field taken.
            Some((slot, value)) => Expr {
                ty: Ty::Record(id),
                kind: ExprKind::Block(Block {
                    stmts: vec![Stmt::Let { slot, value }],
                    tail: Some(Box::new(made)),
                    ty: Ty::Record(id),
                }),
            },
        }
    }

    /// `send(port, value)` — put a value on one of this actor's out ports.
    ///
    /// The port is a name rather than an address. Nothing in the language can
    /// name another actor, so an actor cannot reach one it was not wired to,
    /// and "who is on the other end" stays the `app` block's question (§9.5).
    fn check_send(&mut self, args: &[ast::Arg], span: Span) -> Expr {
        let unit = Expr { ty: Ty::Unit, kind: ExprKind::Unit };
        if args.len() != 2 {
            self.error_labeled(
                span,
                "`send` takes a port and a value",
                "wrong arguments",
                "write `send(somePort, SomeMessage(...))`, naming one of the actor's \n                 `out` ports",
            );
            for arg in args {
                self.check_expr(&arg.value, None);
            }
            return unit;
        }

        let ast::Expr::Ident { name: port, span: port_span } = &args[0].value else {
            self.error_labeled(
                args[0].value.span(),
                "the first argument to `send` is a port name",
                "not a port",
                "ports are named where the actor declares them, and the name is \n                 written literally — there is no value that stands for a channel",
            );
            self.check_expr(&args[1].value, None);
            return unit;
        };

        // A view is a pure function of state (§6.5), and Tier-1 hot reload
        // (§8.3) rests on re-running one being free of consequences.
        if self.in_view {
            self.error_labeled(
                span,
                "a view cannot send",
                "not allowed here",
                "a view is a pure function of state (§6.5) — the platform re-runs \n                 it whenever it likes, so sending from one would send again each \n                 time. Send from the handler that changed the state.",
            );
            self.check_expr(&args[1].value, None);
            return unit;
        }

        let Some(actor) = self.sender else {
            self.error_labeled(
                span,
                "`send` only works inside an actor",
                "no ports in scope",
                "a port belongs to the actor that declares it, so a plain `fn` has \n                 none to name — send from the actor's `on` handler and pass this \n                 function whatever it needs to compute",
            );
            self.check_expr(&args[1].value, None);
            return unit;
        };

        let Some(index) = self.hir.actors[actor].out_port(port) else {
            let known: Vec<&str> =
                self.hir.actors[actor].outbox.iter().map(|p| p.name.as_str()).collect();
            let actor_name = self.hir.actors[actor].name.clone();
            let help = if known.is_empty() {
                format!("`{actor_name}` declares no `out` ports — add `out {port}: SomeType`")
            } else {
                format!("`{actor_name}` sends on: {}", known.join(", "))
            };
            self.error_labeled(
                *port_span,
                format!("`{actor_name}` has no `out` port called `{port}`"),
                "no such port",
                help,
            );
            self.check_expr(&args[1].value, None);
            return unit;
        };

        let want = self.hir.actors[actor].outbox[index].ty.clone();
        let value = self.check_expr(&args[1].value, Some(&want));
        if !value.ty.unifies(&want) {
            let (found, want) = (self.show(&value.ty), self.show(&want));
            self.error(
                args[1].value.span(),
                format!("`{port}` carries {want}, found {found}"),
            );
        }

        Expr {
            ty: Ty::Unit,
            kind: ExprKind::Send { port: index as u32, value: Box::new(value) },
        }
    }

    fn check_try(&mut self, inner: &ast::Expr, span: Span) -> Expr {
        let inner = self.check_expr(inner, None);
        let ret = self.ret_ty.clone();

        match (&inner.ty, &ret) {
            (Ty::Result(ok, err), Ty::Result(_, ret_err)) => {
                // No error conversion in the POC: the error types must agree.
                if !err.unifies(ret_err) {
                    let (found, want) = (self.show(err), self.show(ret_err));
                    self.error(
                        span,
                        format!("`?` propagates {found}, but this function returns {want}"),
                    );
                }
                Expr {
                    ty: (**ok).clone(),
                    kind: ExprKind::Try { expr: Box::new(inner), kind: TryKind::Result },
                }
            }
            (Ty::Option(some), Ty::Option(_)) => Expr {
                ty: (**some).clone(),
                kind: ExprKind::Try { expr: Box::new(inner), kind: TryKind::Option },
            },
            (Ty::Result(..) | Ty::Option(..), other) => {
                let want = self.show(other);
                self.error(
                    span,
                    format!("`?` needs the function to return Result or Option, found {want}"),
                );
                Expr { ty: Ty::Error, kind: ExprKind::Unit }
            }
            (Ty::Error | Ty::Never, _) => Expr { ty: Ty::Error, kind: ExprKind::Unit },
            (other, _) => {
                let found = self.show(other);
                self.error(span, format!("`?` applies to Result or Option, found {found}"));
                Expr { ty: Ty::Error, kind: ExprKind::Unit }
            }
        }
    }

    fn check_match(
        &mut self,
        scrutinee: &ast::Expr,
        arms: &[ast::MatchArm],
        span: Span,
        expected: Option<&Ty>,
    ) -> Expr {
        let scrutinee = self.check_expr(scrutinee, None);
        // The scrutinee is evaluated once into a slot the arms read from.
        let scrutinee_slot = self.locals.len() as u32;
        self.locals.push(scrutinee.ty.clone());

        let mut checked = Vec::new();
        let mut result_ty: Option<Ty> = None;
        let mut covered = Coverage::default();

        for arm in arms {
            self.scopes.push(HashMap::new());
            let pattern = self.check_pattern(&arm.pattern, &scrutinee.ty, &mut covered);
            let body = self.check_expr(&arm.body, expected);
            self.scopes.pop();

            match &result_ty {
                None => result_ty = Some(body.ty.clone()),
                Some(want) => {
                    if !body.ty.unifies(want) {
                        let (found, want_s) = (self.show(&body.ty), self.show(want));
                        self.error(
                            arm.span,
                            format!("match arms disagree: {want_s} and {found}"),
                        );
                    } else {
                        result_ty = Some(want.join(&body.ty));
                    }
                }
            }
            checked.push(Arm { pattern, body });
        }

        if let Some(missing) = covered.missing(&scrutinee.ty, &self.hir) {
            self.error_labeled(
                span,
                format!("`match` does not cover {missing}"),
                format!("{missing} not handled"),
                "add the missing arm, or a `_` arm to catch the rest",
            );
        }

        Expr {
            ty: result_ty.unwrap_or(Ty::Error),
            kind: ExprKind::Match {
                scrutinee: Box::new(scrutinee),
                arms: checked,
                scrutinee_slot,
            },
        }
    }

    fn check_pattern(
        &mut self,
        pattern: &ast::Pattern,
        scrutinee: &Ty,
        covered: &mut Coverage,
    ) -> Pattern {
        match pattern {
            ast::Pattern::Wildcard { .. } => {
                covered.irrefutable = true;
                Pattern::Wildcard
            }
            ast::Pattern::Binding { name, span } => {
                covered.irrefutable = true;
                let slot = self.declare(name.clone(), scrutinee.clone(), false, *span);
                Pattern::Bind { slot }
            }
            ast::Pattern::Int { value, span } => {
                self.expect_pattern_ty(scrutinee, &Ty::Int, *span);
                Pattern::Int(*value)
            }
            ast::Pattern::Bool { value, span } => {
                self.expect_pattern_ty(scrutinee, &Ty::Bool, *span);
                Pattern::Bool(*value)
            }
            ast::Pattern::Str { value, span } => {
                self.expect_pattern_ty(scrutinee, &Ty::Str, *span);
                Pattern::Str(value.clone())
            }
            ast::Pattern::Ctor { name, args, span } => {
                self.check_ctor_pattern(name, args, *span, scrutinee, covered)
            }
        }
    }

    fn check_ctor_pattern(
        &mut self,
        name: &str,
        args: &[ast::Pattern],
        span: Span,
        scrutinee: &Ty,
        covered: &mut Coverage,
    ) -> Pattern {
        let sub = |cx: &mut Self, ty: &Ty| {
            // Nested patterns get their own coverage, which we do not track:
            // exhaustiveness is checked at the top level only in the POC.
            let mut inner_cov = Coverage::default();
            match args.first() {
                Some(pattern) => vec![cx.check_pattern(pattern, ty, &mut inner_cov)],
                None => {
                    cx.error(span, format!("`{name}` binds one value"));
                    vec![Pattern::Wildcard]
                }
            }
        };

        match (name, scrutinee) {
            ("Ok", Ty::Result(ok, _)) => {
                covered.ok = true;
                let inner = sub(self, &ok.clone());
                Pattern::Tagged { tag: Tag::Ok, inner }
            }
            ("Err", Ty::Result(_, err)) => {
                covered.err = true;
                let inner = sub(self, &err.clone());
                Pattern::Tagged { tag: Tag::Err, inner }
            }
            ("Some", Ty::Option(some)) => {
                covered.ok = true;
                let inner = sub(self, &some.clone());
                Pattern::Tagged { tag: Tag::Some, inner }
            }
            ("None", Ty::Option(_)) => {
                covered.err = true;
                if !args.is_empty() {
                    self.error(span, "`None` takes no arguments");
                }
                Pattern::Tagged { tag: Tag::None, inner: Vec::new() }
            }
            (_, Ty::Sum(id)) => {
                let sum = *id;
                let Some((owner, index)) = self.ctors.get(name).copied() else {
                    self.error(span, format!("unknown constructor `{name}`"));
                    return Pattern::Wildcard;
                };
                if let Some(declared_at) = self.ctor_defs.get(name).copied() {
                    self.record_use(span, declared_at);
                }
                if owner != sum {
                    let want = self.hir.sums[sum.0 as usize].name.clone();
                    self.error(span, format!("`{name}` is not a variant of `{want}`"));
                    return Pattern::Wildcard;
                }
                covered.variants.push(index);

                let fields = self.hir.sums[sum.0 as usize].variants[index as usize].fields.clone();
                if args.len() != fields.len() {
                    self.error(
                        span,
                        format!("`{name}` binds {} value(s), found {}", fields.len(), args.len()),
                    );
                }
                let mut inner = Vec::new();
                for (position, (_, ty)) in fields.iter().enumerate() {
                    match args.get(position) {
                        Some(pattern) => {
                            let mut nested = Coverage::default();
                            inner.push(self.check_pattern(pattern, ty, &mut nested));
                        }
                        None => inner.push(Pattern::Wildcard),
                    }
                }
                Pattern::Tagged { tag: Tag::Variant { sum, index }, inner }
            }
            (_, Ty::Error | Ty::Never) => Pattern::Wildcard,
            (_, other) => {
                let found = self.show(other);
                self.error(span, format!("`{name}` does not match {found}"));
                Pattern::Wildcard
            }
        }
    }

    fn expect_pattern_ty(&mut self, scrutinee: &Ty, want: &Ty, span: Span) {
        if !scrutinee.unifies(want) {
            let (found, want_s) = (self.show(scrutinee), self.show(want));
            self.error(span, format!("pattern is {want_s}, but the value is {found}"));
        }
    }
}

/// Top-level match coverage. Nested patterns are not tracked: the POC checks
/// exhaustiveness one level deep, which covers Result/Option/sum scrutinees.
#[derive(Default)]
struct Coverage {
    irrefutable: bool,
    ok: bool,
    err: bool,
    variants: Vec<u32>,
}

impl Coverage {
    fn missing(&self, scrutinee: &Ty, hir: &Hir) -> Option<String> {
        if self.irrefutable {
            return None;
        }
        match scrutinee {
            Ty::Result(..) => match (self.ok, self.err) {
                (true, true) => None,
                (false, true) => Some("`Ok`".into()),
                (true, false) => Some("`Err`".into()),
                (false, false) => Some("`Ok` or `Err`".into()),
            },
            Ty::Option(_) => match (self.ok, self.err) {
                (true, true) => None,
                (false, true) => Some("`Some`".into()),
                (true, false) => Some("`None`".into()),
                (false, false) => Some("`Some` or `None`".into()),
            },
            Ty::Sum(id) => {
                let def = &hir.sums[id.0 as usize];
                let missing: Vec<&str> = def
                    .variants
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| !self.variants.contains(&(*index as u32)))
                    .map(|(_, v)| v.name.as_str())
                    .collect();
                if missing.is_empty() {
                    None
                } else {
                    Some(missing.iter().map(|n| format!("`{n}`")).collect::<Vec<_>>().join(", "))
                }
            }
            // Int/string scrutinees can never be covered by listing cases.
            Ty::Int | Ty::Str | Ty::Float => Some("every value; add a `_` arm".into()),
            Ty::Bool => None,
            _ => None,
        }
    }
}
