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
    cx.collect_actor(program);
    cx.check_bodies(program);
    (cx.hir, cx.analysis, cx.errors)
}

/// Every function in the module, whether top-level or inside an actor.
fn fn_decls(program: &ast::Program) -> Vec<&ast::FnDecl> {
    let mut out = Vec::new();
    for item in &program.items {
        match item {
            ast::Item::Fn(decl) => out.push(decl),
            ast::Item::Actor(decl) => {
                out.push(&decl.init);
                out.push(&decl.receive);
                out.extend(decl.view.as_ref());
            }
            ast::Item::Type(_) => {}
        }
    }
    out
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
        Hir { records: Vec::new(), sums: Vec::new(), funcs: Vec::new(), actor: None }
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
        self.collect_platform_input(program);

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

    /// Declares the platform's `Input` type, if this module asked for it.
    ///
    /// Asking means writing `message: Input` on an actor. That is the whole
    /// opt-in, and it matters because registering the type also registers
    /// `Click`, `Enter` and the rest as constructors — ordinary names a UI
    /// program might want for itself. A file that never mentions `Input`
    /// reserves nothing; a file that declares its own `type Input` keeps it,
    /// and is left alone here rather than being told it clashes with something
    /// it never asked for.
    fn collect_platform_input(&mut self, program: &ast::Program) {
        let asked = program.items.iter().any(|item| match item {
            ast::Item::Actor(decl) => matches!(
                &decl.message,
                Some(ast::TypeExpr::Named { name, args, .. })
                    if name == input::TYPE_NAME && args.is_empty()
            ),
            _ => false,
        });
        let declared_here = program.items.iter().any(|item| {
            matches!(item, ast::Item::Type(decl) if decl.name == input::TYPE_NAME)
        });
        if !asked || declared_here {
            return;
        }

        let id = SumId(self.hir.sums.len() as u32);
        self.hir.sums.push(SumDef {
            name: input::TYPE_NAME.to_string(),
            variants: input::VARIANTS
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
        self.sum_ids.insert(input::TYPE_NAME.to_string(), id);
        for (index, variant) in input::VARIANTS.iter().enumerate() {
            self.ctors.insert(variant.name.to_string(), (id, index as u32));
        }
        // No `type_defs` or `ctor_defs` entry: there is no declaration in this
        // file for go-to-definition to land on.
    }

    fn collect_signatures(&mut self, program: &ast::Program) {
        for decl in fn_decls(program) {
            if self.signatures.contains_key(&decl.name) {
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
                decl.name.clone(),
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
    /// format — the Cap'n Proto lesson in `docs/inspiration-canon.md` — so the
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

    /// Validates the actor shape and records what codegen needs (§5.1).
    fn collect_actor(&mut self, program: &ast::Program) {
        let mut seen: Option<&ast::ActorDecl> = None;
        for item in &program.items {
            let ast::Item::Actor(decl) = item else { continue };
            if let Some(first) = seen {
                self.error(
                    decl.span,
                    format!(
                        "a module declares at most one actor; `{}` is already declared",
                        first.name
                    ),
                );
                continue;
            }
            seen = Some(decl);

            let state = self.resolve_ty(&decl.state);
            let message = match &decl.message {
                Some(ty) => self.resolve_ty(ty),
                None => Ty::Str,
            };
            self.check_message_is_flat(&message, decl.span);
            let Some(init) = self.signatures.get("init").cloned() else { continue };
            let Some(receive) = self.signatures.get("receive").cloned() else { continue };

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

            match receive.params.as_slice() {
                [(_, first), (_, second)] => {
                    if !first.unifies(&state) {
                        let (found, want) = (self.show(first), self.show(&state));
                        self.error(
                            decl.receive.span,
                            format!("`receive` takes the state {want} first, found {found}"),
                        );
                    }
                    if !second.unifies(&message) {
                        let (found, want) = (self.show(second), self.show(&message));
                        self.error(
                            decl.receive.span,
                            format!("`receive` takes the message as {want}, found {found}"),
                        );
                    }
                }
                _ => self.error(
                    decl.receive.span,
                    "`receive` takes exactly the state and the message",
                ),
            }

            if !receive.ret.unifies(&state) {
                let (found, want) = (self.show(&receive.ret), self.show(&state));
                self.error(
                    decl.receive.span,
                    format!("`receive` must return the next state {want}, found {found}"),
                );
            }

            // A UI actor is one that declares how to draw itself. Everything
            // else about it — mailbox, state, supervision — is unchanged.
            let view = decl.view.as_ref().and_then(|view_decl| {
                let signature = self.signatures.get(&view_decl.name).cloned()?;
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

            self.hir.actor = Some(ActorInfo {
                name: decl.name.clone(),
                state,
                message,
                init: init.id,
                receive: receive.id,
                view,
            });
        }
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
        let mut decls: Vec<&ast::FnDecl> = Vec::new();
        for decl in fn_decls(program) {
            // Only the first declaration of a name owns the id; a duplicate has
            // already been reported and emits no body.
            let is_first = !decls.iter().any(|d: &&ast::FnDecl| d.name == decl.name);
            if is_first && self.signatures.contains_key(&decl.name) {
                decls.push(decl);
            }
        }
        decls.sort_by_key(|d| self.signatures[&d.name].id.0);

        for decl in decls {
            let signature = self.signatures[&decl.name].clone();
            self.scopes.clear();
            self.locals.clear();
            self.ret_ty = signature.ret.clone();
            self.in_view = signature.is_view;

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

            ast::Expr::RecordLit { name, fields, span } => {
                self.check_record_lit(name.as_deref(), fields, *span, expected)
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
                None => {
                    self.error(
                        span,
                        format!("`{}` is missing field `{}`", def.name, def.fields[index].0),
                    );
                    values.push(Expr { ty: Ty::Error, kind: ExprKind::Unit });
                }
            }
        }

        Expr { ty: Ty::Record(id), kind: ExprKind::MakeRecord { record: id, fields: values } }
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
