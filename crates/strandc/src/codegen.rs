//! WASM emission (§4.6), implementing the layout rules in `docs/abi.md`.
//!
//! Core modules and linear memory — no GC types, no Component Model. The
//! load-bearing decision is §2 of that document: `Result`/`Option` cross a
//! return boundary as two WASM values, `(i32 tag, i64 payload)`, never a heap
//! allocation. `?` is then a tag test and a re-return of the pair unchanged.

use std::collections::HashMap;

use wasm_encoder::{
    BlockType, CodeSection, ConstExpr, DataSection, EntityType, ExportKind, ExportSection,
    Function, FunctionSection, GlobalSection, GlobalType, ImportSection, Instruction, MemArg,
    MemorySection, MemoryType, Module, TypeSection, ValType,
};

use crate::hir::*;
use crate::ui::{self, Slot};

/// Every value occupies whole 8-byte slots in memory, so field offsets are
/// just word counts. Simpler than tight packing and irrelevant at POC scale.
const WORD: u64 = 8;

/// Static string data starts here; offset 0 stays unused so a null pointer is
/// never a valid value.
const DATA_START: u32 = 16;

/// A list is `{ i32 len, <pad>, elements... }`. The header is a whole word so
/// the elements after it stay 8-byte aligned, which is what lets an element be
/// loaded by exactly the code that loads a record field.
const LIST_HEADER: u64 = WORD;

/// Bytes one element of `elem` occupies. Whole words, like a record's fields —
/// a two-word `Result` takes two.
fn stride(elem: &Ty) -> u64 {
    words(elem).max(1) * WORD
}

type Code = Vec<Instruction<'static>>;

/// The WASM representation of a Strand type (`docs/abi.md`).
fn rep(ty: &Ty) -> Vec<ValType> {
    match ty {
        Ty::Int => vec![ValType::I64],
        Ty::Float => vec![ValType::F64],
        Ty::Bool => vec![ValType::I32],
        // Pointers into linear memory, and immediate tags for all-niladic sums.
        Ty::Str | Ty::List(_) | Ty::Record(_) | Ty::Sum(_) => vec![ValType::I32],
        // The multi-value pair. This is the whole point of docs/abi.md §2.
        Ty::Option(_) | Ty::Result(..) => vec![ValType::I32, ValType::I64],
        // A node leaves nothing behind: building it *was* the effect. See
        // `Ty::Node` in the HIR for why that is the point rather than a saving.
        Ty::Unit | Ty::Never | Ty::Error | Ty::Node => vec![],
    }
}

/// How many WASM values a type occupies when returned. The runner needs this
/// to size a dynamic call, and it must agree with `rep`.
pub fn wasm_arity(ty: &Ty) -> usize {
    rep(ty).len()
}

fn words(ty: &Ty) -> u64 {
    rep(ty).len() as u64
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmitError {
    pub message: String,
}

impl std::fmt::Display for EmitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for EmitError {}

type EResult<T> = Result<T, EmitError>;

fn bail<T>(message: impl Into<String>) -> EResult<T> {
    Err(EmitError { message: message.into() })
}

pub fn emit(hir: &Hir) -> EResult<Vec<u8>> {
    match hir.actors.len() {
        0 => Emitter::new(hir, None).run(),
        1 => Emitter::new(hir, Some(0)).run(),
        // A module is one actor's code: the state global, `strand_main` and
        // `strand_on_message` are singular by construction. A file holding
        // several is compiled once per actor, which is also what gives each of
        // them its own arena (§5.1).
        _ => bail("this file declares several actors — emit each one with `emit_actor`"),
    }
}

/// Emits the module for one of the file's actors.
///
/// The actors share the file's functions and types, so the emitted modules are
/// near-identical apart from which `init`, handlers and view the ABI exports
/// point at. That is the cost of having no imports yet, and it is paid in
/// bytes rather than in isolation: the instances that run them are separate
/// Stores either way.
pub fn emit_actor(hir: &Hir, actor: usize) -> EResult<Vec<u8>> {
    if actor >= hir.actors.len() {
        return bail(format!("no actor {actor} in this file"));
    }
    Emitter::new(hir, Some(actor)).run()
}

struct Emitter<'hir> {
    hir: &'hir Hir,
    /// Which actor this module is being emitted for, if any.
    actor: Option<&'hir ActorInfo>,
    /// Interned function types, so multi-value block types can be referenced.
    types: Vec<(Vec<ValType>, Vec<ValType>)>,
    /// Literal text -> byte offset of its `{ len, bytes }` header.
    strings: HashMap<String, u32>,
    data: Vec<u8>,
    heap_start: u32,
    alloc_index: u32,
    str_eq_index: u32,
    /// The generated helper that appends one node to the frame's array.
    node_push_index: u32,
    /// Globals holding the frame's arena. Their indices depend on whether the
    /// module has an actor, since the state global keeps index 1 — it is
    /// exported as `strand_state` and moving it would break the host.
    node_base_global: u32,
    node_count_global: u32,
    pending_global: u32,
    /// Whether anything in this module builds nodes. A module that draws
    /// nothing pays for none of this.
    builds_nodes: bool,
    /// Generated string helpers this module actually calls, in a fixed order.
    /// Emitted last, so adding one shifts no index anything else depends on.
    helpers: Vec<Helper>,
    helpers_base: u32,
    /// Host functions this module actually calls. Imports take the lowest
    /// function indices, so everything defined here is offset past them.
    imports: Vec<Builtin>,
    /// A word of static data `send` writes an immediate into, so that a value
    /// with no address can still be handed to the host as `(ptr, len)`.
    immediate_slot: u32,
}

impl<'hir> Emitter<'hir> {
    fn new(hir: &'hir Hir, actor: Option<usize>) -> Self {
        let actor = actor.map(|index| &hir.actors[index]);
        let helpers = hir.funcs.len() as u32;
        let builds_nodes = hir.funcs.iter().any(|func| func.is_view);
        // Global 0 is the bump pointer and global 1, when there is an actor, is
        // the state. The frame's arena takes the next three.
        let first_free = if actor.is_some() { 2 } else { 1 };
        Self {
            hir,
            actor,
            types: Vec::new(),
            strings: HashMap::new(),
            data: Vec::new(),
            heap_start: DATA_START,
            alloc_index: helpers,
            str_eq_index: helpers + 1,
            node_push_index: helpers + 2,
            node_base_global: first_free,
            node_count_global: first_free + 1,
            pending_global: first_free + 2,
            builds_nodes,
            helpers: Vec::new(),
            // Filled in once the conditional helpers before it are counted.
            helpers_base: 0,
            imports: Vec::new(),
            // Reserved during `collect_strings`, and only when something sends.
            immediate_slot: 0,
        }
    }

    /// The functions that belong to an actor rather than to the file.
    fn actor_owned(&self) -> Vec<u32> {
        let mut owned = Vec::new();
        for actor in &self.hir.actors {
            owned.push(actor.init.0);
            owned.extend(actor.handlers.iter().map(|id| id.0));
            owned.extend(actor.view.map(|id| id.0));
        }
        owned
    }

    /// Where `helper` ended up. Only helpers the module calls are emitted, so
    /// this is a position in that list rather than a fixed slot.
    fn helper_index(&self, helper: Helper) -> u32 {
        let position = self
            .helpers
            .iter()
            .position(|candidate| *candidate == helper)
            .expect("helper was collected");
        self.helpers_base + position as u32
    }

    fn intern_type(&mut self, params: Vec<ValType>, results: Vec<ValType>) -> u32 {
        let key = (params, results);
        if let Some(index) = self.types.iter().position(|t| *t == key) {
            return index as u32;
        }
        self.types.push(key);
        (self.types.len() - 1) as u32
    }

    /// Block type for a value of `ty`. Multi-value needs a real type index.
    fn block_type(&mut self, ty: &Ty) -> BlockType {
        let results = rep(ty);
        match results.len() {
            0 => BlockType::Empty,
            1 => BlockType::Result(results[0]),
            _ => BlockType::FunctionType(self.intern_type(Vec::new(), results)),
        }
    }

    fn run(mut self) -> EResult<Vec<u8>> {
        // Imports first: whether anything sends decides whether the data
        // section reserves a scratch word.
        self.collect_imports();
        self.collect_strings();
        self.collect_helpers();

        // Imports shift every defined function, so fix the helper indices here
        // rather than sprinkling the offset through emission.
        let offset = self.imports.len() as u32;
        self.alloc_index += offset;
        self.str_eq_index += offset;
        self.node_push_index += offset;

        // The generated string helpers sit after everything else, so their
        // presence cannot move an index another part of the emitter computed.
        let mut after = self.str_eq_index + 1;
        if self.builds_nodes {
            after += 2;
        }
        if let Some(actor) = self.actor {
            after += 2;
            if actor.view.is_some() {
                after += 1;
            }
        }
        self.helpers_base = after;

        // Function types, in index order: user functions, then the two helpers.
        let mut signatures = Vec::new();
        for func in &self.hir.funcs {
            let params: Vec<ValType> =
                func.locals[..func.param_count].iter().flat_map(rep).collect();
            let results = rep(&func.ret);
            signatures.push(self.intern_type(params, results));
        }
        let alloc_ty = self.intern_type(vec![ValType::I32], vec![ValType::I32]);
        let str_eq_ty =
            self.intern_type(vec![ValType::I32, ValType::I32], vec![ValType::I32]);
        let actor_main_ty = self.intern_type(Vec::new(), Vec::new());
        // (port, ptr, len): which channel it arrived on, and the bytes.
        let actor_recv_ty =
            self.intern_type(vec![ValType::I32, ValType::I32, ValType::I32], Vec::new());
        // node_push(kind, marker, id, flag, text, text2, number, number2)
        let node_push_ty = self.intern_type(
            vec![
                ValType::I32,
                ValType::I32,
                ValType::I32,
                ValType::I32,
                ValType::I32,
                ValType::I32,
                ValType::F32,
                ValType::F32,
            ],
            Vec::new(),
        );
        let frame_reset_ty = self.intern_type(Vec::new(), Vec::new());
        let actor_view_ty = self.intern_type(Vec::new(), Vec::new());
        let helper_types: Vec<u32> = self
            .helpers
            .clone()
            .into_iter()
            .map(|helper| {
                let (params, results) = helper_signature(helper);
                self.intern_type(params, results)
            })
            .collect();

        // Import signatures are interned here rather than where the import
        // section is written, which is after the type section has been emitted:
        // an import whose type is new at that point would name an index the
        // module does not contain. It went unnoticed while `log`'s `(i32, i32)`
        // happened to be the same shape as `strand_on_message`.
        let import_types: Vec<u32> = self
            .imports
            .clone()
            .into_iter()
            .map(|builtin| {
                let (params, results) = builtin_signature(builtin);
                self.intern_type(params, results)
            })
            .collect();

        // Bodies are emitted before the type section is finalised, because a
        // multi-value block inside a body can intern a new type.
        let mut bodies = Vec::new();
        for func in &self.hir.funcs {
            bodies.push(self.emit_func(func)?);
        }

        let mut module = Module::new();

        let mut types = TypeSection::new();
        for (params, results) in &self.types {
            types.ty().function(params.iter().copied(), results.iter().copied());
        }
        module.section(&types);

        if !self.imports.is_empty() {
            let mut imports = ImportSection::new();
            for (builtin, ty) in self.imports.iter().zip(&import_types) {
                let (module_name, field) = builtin.import();
                imports.import(module_name, field, EntityType::Function(*ty));
            }
            module.section(&imports);
        }

        let mut functions = FunctionSection::new();
        for signature in &signatures {
            functions.function(*signature);
        }
        functions.function(alloc_ty);
        functions.function(str_eq_ty);
        if self.builds_nodes {
            functions.function(node_push_ty);
            functions.function(frame_reset_ty);
        }
        if let Some(actor) = self.actor {
            functions.function(actor_main_ty);
            functions.function(actor_recv_ty);
            if actor.view.is_some() {
                functions.function(actor_view_ty);
            }
        }
        for helper_ty in &helper_types {
            functions.function(*helper_ty);
        }
        module.section(&functions);

        let mut memories = MemorySection::new();
        memories.memory(MemoryType {
            minimum: 17,
            maximum: None,
            memory64: false,
            shared: false,
            page_size_log2: None,
        });
        module.section(&memories);

        // The frame's arena sits between the static data and the bump heap, at
        // a fixed size decided here rather than grown at runtime (see
        // `ui::NODE_CAPACITY`).
        let node_arena = self.heap_start;
        let bump_start =
            if self.builds_nodes { node_arena + ui::NODE_ARENA_BYTES } else { node_arena };

        let mut globals = GlobalSection::new();
        globals.global(
            GlobalType { val_type: ValType::I32, mutable: true, shared: false },
            &ConstExpr::i32_const(bump_start as i32),
        );
        if let Some(actor) = self.actor {
            // Global 1 holds the current state. A handler returns the next one,
            // so a message handler never mutates in place (§6.5).
            let val_type = state_type(&actor.state)?;
            globals.global(
                GlobalType { val_type, mutable: true, shared: false },
                &zero_of(val_type),
            );
        }
        if self.builds_nodes {
            // base, count, and the roots not yet claimed by a parent.
            globals.global(
                GlobalType { val_type: ValType::I32, mutable: false, shared: false },
                &ConstExpr::i32_const(node_arena as i32),
            );
            for _ in 0..2 {
                globals.global(
                    GlobalType { val_type: ValType::I32, mutable: true, shared: false },
                    &ConstExpr::i32_const(0),
                );
            }
        }
        module.section(&globals);

        let mut exports = ExportSection::new();
        exports.export("memory", ExportKind::Memory, 0);
        let owned = self.actor_owned();
        for (index, func) in self.hir.funcs.iter().enumerate() {
            // An actor's own functions are reached through the mailbox and the
            // ABI's entry points, never by name — and two actors in one file
            // both declaring `init` is the ordinary case, so exporting them
            // would be a name clash over a door nobody uses.
            if owned.contains(&(index as u32)) {
                continue;
            }
            exports.export(&func.name, ExportKind::Func, offset + index as u32);
        }
        // The host ABI names from docs/abi.md §6.
        exports.export("strand_alloc", ExportKind::Func, self.alloc_index);
        if self.builds_nodes {
            // What a host needs to read a frame: where the array starts, how
            // many nodes are in it, and how to empty it before the next one.
            exports.export("strand_nodes", ExportKind::Global, self.node_base_global);
            exports.export("strand_node_count", ExportKind::Global, self.node_count_global);
            exports.export("strand_frame_reset", ExportKind::Func, self.node_push_index + 1);
        }
        if let Some(actor) = self.actor {
            let actor_base =
                if self.builds_nodes { self.node_push_index + 1 } else { self.str_eq_index };
            exports.export("strand_main", ExportKind::Func, actor_base + 1);
            exports.export("strand_on_message", ExportKind::Func, actor_base + 2);
            if actor.view.is_some() {
                // Draws the actor as it currently is. The runtime calls this
                // after each message; what it produces is read out of
                // `strand_nodes` (`docs/abi.md` §8).
                exports.export("strand_view", ExportKind::Func, actor_base + 3);
            }
            // Lets a host read the actor's state without the actor logging it.
            exports.export("strand_state", ExportKind::Global, 1);
        }
        module.section(&exports);

        let mut code = CodeSection::new();
        for body in bodies {
            code.function(&body);
        }
        code.function(&alloc_body());
        code.function(&str_eq_body());
        if self.builds_nodes {
            code.function(&node_push_body(
                self.node_base_global,
                self.node_count_global,
                self.pending_global,
            ));
            code.function(&frame_reset_body(self.node_count_global, self.pending_global));
        }
        if let Some(actor) = self.actor {
            code.function(&actor_main_body(actor, offset));
            code.function(&actor_receive_body(actor, self.alloc_index, self.hir, offset));
            if let Some(view) = actor.view {
                code.function(&actor_view_body(view, self.node_push_index + 1, offset));
            }
        }
        for helper in &self.helpers {
            code.function(&helper_body(*helper, self.alloc_index));
        }
        module.section(&code);

        if !self.data.is_empty() {
            let mut data = DataSection::new();
            data.active(0, &ConstExpr::i32_const(DATA_START as i32), self.data.iter().copied());
            module.section(&data);
        }

        Ok(module.finish())
    }

    /// Finds which host functions the program calls, so only those are imported.
    /// Finds the string helpers this module calls.
    ///
    /// A program that never touches a string emits none of them, which is the
    /// same rule imports follow: you pay for what you call.
    fn collect_helpers(&mut self) {
        let mut used = Vec::new();
        for func in &self.hir.funcs {
            walk_block(&func.body, &mut |expr| {
                if let ExprKind::CallHelper { helper, .. } = &expr.kind {
                    used.push(*helper);
                }
            });
        }
        used.sort();
        used.dedup();
        self.helpers = used;
    }

    fn collect_imports(&mut self) {
        let mut used = Vec::new();
        for func in &self.hir.funcs {
            collect_block_builtins(&func.body, &mut used);
        }
        for builtin in used {
            if !self.imports.contains(&builtin) {
                self.imports.push(builtin);
            }
        }
        // Stable order keeps emitted modules reproducible.
        self.imports.sort_by_key(|b| b.name());
    }

    // ---- static data -----------------------------------------------------

    fn collect_strings(&mut self) {
        let mut literals = Vec::new();
        for func in &self.hir.funcs {
            collect_block_strings(&func.body, &mut literals);
        }
        for text in literals {
            if self.strings.contains_key(&text) {
                continue;
            }
            let offset = DATA_START + self.data.len() as u32;
            // Header is the length; the bytes follow immediately (§5).
            self.data.extend_from_slice(&(text.len() as u32).to_le_bytes());
            self.data.extend_from_slice(text.as_bytes());
            while self.data.len() % WORD as usize != 0 {
                self.data.push(0);
            }
            self.strings.insert(text, offset);
        }
        // One word of static scratch, for the immediates `send` needs an
        // address for. Reused by every send: the host copies the bytes out
        // before the call returns, so nothing outlives the call that wrote it.
        if self.imports.contains(&Builtin::Send) {
            self.immediate_slot = DATA_START + self.data.len() as u32;
            self.data.extend_from_slice(&[0; WORD as usize]);
        }
        self.heap_start = DATA_START + self.data.len() as u32;
    }

    // ---- functions -------------------------------------------------------

    fn emit_func(&mut self, func: &Func) -> EResult<Function> {
        // A type with unknown parts has no representation. This is reachable
        // from `let r = Ok(1)`, where nothing ever pins the error type.
        for ty in &func.locals {
            if ty.has_holes() {
                return bail(format!(
                    "in `{}`: could not infer a complete type for a local                      (a `Result`/`Option` whose other half is never determined);                      add a type annotation",
                    func.name
                ));
            }
        }
        let mut ctx = FnCtx::new(func);
        let mut code: Code = Vec::new();

        self.block(&mut ctx, &mut code, &func.body)?;
        code.push(Instruction::End);

        let mut declared: Vec<(u32, ValType)> = Vec::new();
        for ty in &ctx.declared {
            declared.push((1, *ty));
        }
        let mut function = Function::new(declared);
        for instruction in &code {
            function.instruction(instruction);
        }
        Ok(function)
    }

    fn block(&mut self, ctx: &mut FnCtx, code: &mut Code, block: &Block) -> EResult<()> {
        for stmt in &block.stmts {
            self.stmt(ctx, code, stmt)?;
        }
        if let Some(tail) = &block.tail {
            self.expr(ctx, code, tail)?;
        }
        Ok(())
    }

    fn stmt(&mut self, ctx: &mut FnCtx, code: &mut Code, stmt: &Stmt) -> EResult<()> {
        match stmt {
            Stmt::Let { slot, value } | Stmt::AssignLocal { slot, value } => {
                self.expr(ctx, code, value)?;
                store_locals(code, &ctx.slot_locals[*slot as usize]);
            }
            Stmt::Return(value) => {
                if let Some(value) = value {
                    self.expr(ctx, code, value)?;
                }
                code.push(Instruction::Return);
            }
            Stmt::Expr(expr) => {
                self.expr(ctx, code, expr)?;
                for _ in 0..words(&expr.ty) {
                    code.push(Instruction::Drop);
                }
            }
        }
        Ok(())
    }

    fn expr(&mut self, ctx: &mut FnCtx, code: &mut Code, expr: &Expr) -> EResult<()> {
        match &expr.kind {
            ExprKind::Int(v) => code.push(Instruction::I64Const(*v)),
            ExprKind::Float(v) => code.push(Instruction::F64Const((*v).into())),
            ExprKind::Bool(v) => code.push(Instruction::I32Const(i32::from(*v))),
            ExprKind::Str(text) => {
                let offset = *self.strings.get(text).expect("literal was collected");
                code.push(Instruction::I32Const(offset as i32));
            }
            ExprKind::Unit => {}
            ExprKind::Local(slot) => {
                for local in &ctx.slot_locals[*slot as usize] {
                    code.push(Instruction::LocalGet(*local));
                }
            }

            ExprKind::Unary { op, expr: inner } => {
                match op {
                    // No i64.neg in WASM: subtract from zero.
                    UnOp::NegInt => {
                        code.push(Instruction::I64Const(0));
                        self.expr(ctx, code, inner)?;
                        code.push(Instruction::I64Sub);
                    }
                    UnOp::NegFloat => {
                        self.expr(ctx, code, inner)?;
                        code.push(Instruction::F64Neg);
                    }
                    UnOp::Not => {
                        self.expr(ctx, code, inner)?;
                        code.push(Instruction::I32Eqz);
                    }
                }
            }

            ExprKind::Binary { op, lhs, rhs } => self.binary(ctx, code, *op, lhs, rhs)?,

            ExprKind::Call { func, args } => {
                for arg in args {
                    self.expr(ctx, code, arg)?;
                }
                let offset = self.imports.len() as u32;
                code.push(Instruction::Call(offset + func.0));
            }

            // `send(port, value)`: hand the host the bytes that already are
            // the value (docs/abi.md §7). The port is a constant the checker
            // resolved; nothing here knows or can know who receives it.
            ExprKind::Send { port, value } => {
                let ty = value.ty.clone();
                let scratch = ctx.scratch(ValType::I32);

                code.push(Instruction::I32Const(*port as i32));
                match &ty {
                    // The bytes on the wire are the string's, without the
                    // length header — codegen rebuilds that on arrival.
                    Ty::Str => {
                        self.expr(ctx, code, value)?;
                        code.push(Instruction::LocalSet(scratch));
                        code.push(Instruction::LocalGet(scratch));
                        code.push(Instruction::I32Const(4));
                        code.push(Instruction::I32Add);
                        code.push(Instruction::LocalGet(scratch));
                        code.push(Instruction::I32Load(mem_arg(0, 2)));
                    }
                    // A boxed variant is already laid out the way the wire
                    // wants it, so the pointer is the payload.
                    Ty::Sum(id)
                        if self.hir.sums[id.0 as usize]
                            .variants
                            .iter()
                            .any(|v| !v.fields.is_empty()) =>
                    {
                        let def = &self.hir.sums[id.0 as usize];
                        let bytes = (variant_payload_words(def) + 1) * WORD;
                        self.expr(ctx, code, value)?;
                        code.push(Instruction::I32Const(bytes as i32));
                    }
                    // Immediates have no address, so they need one: a word of
                    // scratch, written and read inside this call. The host
                    // copies before returning, so one slot serves every send.
                    other => {
                        let slot = self.immediate_slot;
                        code.push(Instruction::I32Const(slot as i32));
                        self.expr(ctx, code, value)?;
                        let bytes = match other {
                            Ty::Int => {
                                code.push(Instruction::I64Store(mem_arg(0, 3)));
                                8
                            }
                            Ty::Float => {
                                code.push(Instruction::F64Store(mem_arg(0, 3)));
                                8
                            }
                            // A bare tag, and `bool`, are both an i32.
                            _ => {
                                code.push(Instruction::I32Store(mem_arg(0, 2)));
                                4
                            }
                        };
                        code.push(Instruction::I32Const(slot as i32));
                        code.push(Instruction::I32Const(bytes));
                    }
                }

                let index = self
                    .imports
                    .iter()
                    .position(|b| *b == Builtin::Send)
                    .expect("send was collected as an import");
                code.push(Instruction::Call(index as u32));
            }

            ExprKind::CallBuiltin { builtin, args } => {
                match builtin {
                    // log(msg) takes a Strand string; the host ABI takes
                    // (ptr, len), so unpack the header here (docs/abi.md §5).
                    Builtin::Log => {
                        let text = ctx.scratch(ValType::I32);
                        self.expr(ctx, code, &args[0])?;
                        code.push(Instruction::LocalSet(text));
                        code.push(Instruction::LocalGet(text));
                        code.push(Instruction::I32Const(4));
                        code.push(Instruction::I32Add);
                        code.push(Instruction::LocalGet(text));
                        code.push(Instruction::I32Load(mem_arg(0, 2)));
                    }
                    // Emitted through `ExprKind::Send`, which knows the port.
                    Builtin::Send => {
                        return bail("`send` is emitted from its own node, not as a call")
                    }
                    // Same string unpacking as `log`: the host takes a pair.
                    Builtin::Panic => {
                        let text = ctx.scratch(ValType::I32);
                        self.expr(ctx, code, &args[0])?;
                        code.push(Instruction::LocalSet(text));
                        code.push(Instruction::LocalGet(text));
                        code.push(Instruction::I32Const(4));
                        code.push(Instruction::I32Add);
                        code.push(Instruction::LocalGet(text));
                        code.push(Instruction::I32Load(mem_arg(0, 2)));
                    }
                }
                let index = self
                    .imports
                    .iter()
                    .position(|b| b == builtin)
                    .expect("import was collected");
                code.push(Instruction::Call(index as u32));
                // The host call raises rather than returns, but WASM has no way
                // to say so about an import. Without this, everything after a
                // `panic` is still reachable as far as validation is concerned,
                // and a `panic` in tail position would fall off a function that
                // owes its caller a value.
                if *builtin == Builtin::Panic {
                    code.push(Instruction::Unreachable);
                }
            }

            ExprKind::CallHelper { helper, args } => {
                for arg in args {
                    self.expr(ctx, code, arg)?;
                }
                code.push(Instruction::Call(self.helper_index(*helper)));
            }

            ExprKind::MakeList { elems } => {
                let Ty::List(elem) = &expr.ty else {
                    return bail("a list literal reached codegen without a list type");
                };
                let elem = (**elem).clone();
                let step = stride(&elem);

                let ptr = ctx.scratch(ValType::I32);
                code.push(Instruction::I32Const(
                    (LIST_HEADER + step * elems.len() as u64) as i32,
                ));
                code.push(Instruction::Call(self.alloc_index));
                code.push(Instruction::LocalTee(ptr));
                code.push(Instruction::I32Const(elems.len() as i32));
                code.push(Instruction::I32Store(mem_arg(0, 2)));

                for (index, value) in elems.iter().enumerate() {
                    let offset = LIST_HEADER + step * index as u64;
                    self.store_at(ctx, code, ptr, offset, &elem, value)?;
                }
                code.push(Instruction::LocalGet(ptr));
            }

            ExprKind::ListLen { list } => {
                self.expr(ctx, code, list)?;
                code.push(Instruction::I32Load(mem_arg(0, 2)));
                // `int` is 64-bit; a length is not.
                code.push(Instruction::I64ExtendI32U);
            }

            ExprKind::ListPush { list, value } => {
                let Ty::List(elem) = &expr.ty else {
                    return bail("a push reached codegen without a list type");
                };
                let elem = (**elem).clone();
                let step = stride(&elem);

                let source = ctx.scratch(ValType::I32);
                let count = ctx.scratch(ValType::I32);
                let ptr = ctx.scratch(ValType::I32);
                let slot = ctx.scratch(ValType::I32);

                self.expr(ctx, code, list)?;
                code.push(Instruction::LocalTee(source));
                code.push(Instruction::I32Load(mem_arg(0, 2)));
                code.push(Instruction::LocalSet(count));

                // A new list, one longer. The old one is untouched and stays
                // valid for anyone still holding it (§4.2).
                code.push(Instruction::LocalGet(count));
                code.push(Instruction::I32Const(1));
                code.push(Instruction::I32Add);
                code.push(Instruction::I32Const(step as i32));
                code.push(Instruction::I32Mul);
                code.push(Instruction::I32Const(LIST_HEADER as i32));
                code.push(Instruction::I32Add);
                code.push(Instruction::Call(self.alloc_index));
                code.push(Instruction::LocalTee(ptr));
                code.push(Instruction::LocalGet(count));
                code.push(Instruction::I32Const(1));
                code.push(Instruction::I32Add);
                code.push(Instruction::I32Store(mem_arg(0, 2)));

                code.push(Instruction::LocalGet(ptr));
                code.push(Instruction::I32Const(LIST_HEADER as i32));
                code.push(Instruction::I32Add);
                code.push(Instruction::LocalGet(source));
                code.push(Instruction::I32Const(LIST_HEADER as i32));
                code.push(Instruction::I32Add);
                code.push(Instruction::LocalGet(count));
                code.push(Instruction::I32Const(step as i32));
                code.push(Instruction::I32Mul);
                code.push(Instruction::MemoryCopy { src_mem: 0, dst_mem: 0 });

                // The new element goes at the end, through the same typed store
                // a record field uses.
                code.push(Instruction::LocalGet(ptr));
                code.push(Instruction::LocalGet(count));
                code.push(Instruction::I32Const(step as i32));
                code.push(Instruction::I32Mul);
                code.push(Instruction::I32Add);
                code.push(Instruction::LocalSet(slot));
                self.store_at(ctx, code, slot, LIST_HEADER, &elem, value)?;

                code.push(Instruction::LocalGet(ptr));
            }

            ExprKind::For { slot, list, body } => {
                let Ty::List(elem) = &list.ty else {
                    return bail("a `for` reached codegen without a list to walk");
                };
                let elem = (**elem).clone();
                let step = stride(&elem);

                let source = ctx.scratch(ValType::I32);
                let count = ctx.scratch(ValType::I32);
                let index = ctx.scratch(ValType::I32);
                let at = ctx.scratch(ValType::I32);

                self.expr(ctx, code, list)?;
                code.push(Instruction::LocalTee(source));
                code.push(Instruction::I32Load(mem_arg(0, 2)));
                code.push(Instruction::LocalSet(count));
                code.push(Instruction::I32Const(0));
                code.push(Instruction::LocalSet(index));

                code.push(Instruction::Block(BlockType::Empty));
                code.push(Instruction::Loop(BlockType::Empty));
                code.push(Instruction::LocalGet(index));
                code.push(Instruction::LocalGet(count));
                code.push(Instruction::I32GeU);
                code.push(Instruction::BrIf(1));

                // The element's address, then the same load a record field uses.
                code.push(Instruction::LocalGet(source));
                code.push(Instruction::LocalGet(index));
                code.push(Instruction::I32Const(step as i32));
                code.push(Instruction::I32Mul);
                code.push(Instruction::I32Add);
                code.push(Instruction::LocalSet(at));
                load_at(code, at, LIST_HEADER, &elem);
                store_locals(code, &ctx.slot_locals[*slot as usize]);

                self.block(ctx, code, body)?;
                // A body that leaves a value behind would unbalance the loop.
                for _ in rep(&body.ty) {
                    code.push(Instruction::Drop);
                }

                code.push(Instruction::LocalGet(index));
                code.push(Instruction::I32Const(1));
                code.push(Instruction::I32Add);
                code.push(Instruction::LocalSet(index));
                code.push(Instruction::Br(0));
                code.push(Instruction::End);
                code.push(Instruction::End);
            }

            ExprKind::MakeNode { kind, props, numbers, children } => {
                // Slots first, in the order they were written, so a prop that
                // calls something observable happens when the source says.
                let id = ctx.scratch(ValType::I32);
                let flag = ctx.scratch(ValType::I32);
                let text = ctx.scratch(ValType::I32);
                let text2 = ctx.scratch(ValType::I32);
                let number = ctx.scratch(ValType::F32);
                let number2 = ctx.scratch(ValType::F32);
                let marker = ctx.scratch(ValType::I32);

                for local in [id, flag, text, text2] {
                    code.push(Instruction::I32Const(0));
                    code.push(Instruction::LocalSet(local));
                }
                code.push(Instruction::F32Const(numbers[0].into()));
                code.push(Instruction::LocalSet(number));
                code.push(Instruction::F32Const(numbers[1].into()));
                code.push(Instruction::LocalSet(number2));

                for (slot, value) in props {
                    self.expr(ctx, code, value)?;
                    // Slots are 32-bit; Strand ints are i64 and floats f64. A
                    // spacing prop is written as an int (§6.3's unit is the
                    // logical pixel) but stored as a float, because layout
                    // works in fractions of one.
                    match (slot.is_float(), &value.ty) {
                        (true, Ty::Int) => code.push(Instruction::F32ConvertI64S),
                        (true, Ty::Float) => code.push(Instruction::F32DemoteF64),
                        (false, Ty::Int) => code.push(Instruction::I32WrapI64),
                        // bool and string are already i32.
                        _ => {}
                    }
                    let local = match slot {
                        Slot::Id => id,
                        Slot::Flag => flag,
                        Slot::Text => text,
                        Slot::Text2 => text2,
                        Slot::Number => number,
                        Slot::Number2 => number2,
                    };
                    code.push(Instruction::LocalSet(local));
                }

                // Everything appended from here until the push belongs to this
                // node. `node_push` turns the difference into a child count.
                code.push(Instruction::GlobalGet(self.pending_global));
                code.push(Instruction::LocalSet(marker));
                self.block(ctx, code, children)?;

                code.push(Instruction::I32Const(kind.tag()));
                code.push(Instruction::LocalGet(marker));
                code.push(Instruction::LocalGet(id));
                code.push(Instruction::LocalGet(flag));
                code.push(Instruction::LocalGet(text));
                code.push(Instruction::LocalGet(text2));
                code.push(Instruction::LocalGet(number));
                code.push(Instruction::LocalGet(number2));
                code.push(Instruction::Call(self.node_push_index));
            }

            ExprKind::MakeRecord { record, fields } => {
                let def = &self.hir.records[record.0 as usize];
                let total: u64 = def.fields.iter().map(|(_, t)| words(t)).sum();
                let ptr = ctx.scratch(ValType::I32);

                code.push(Instruction::I32Const((total * WORD) as i32));
                code.push(Instruction::Call(self.alloc_index));
                code.push(Instruction::LocalSet(ptr));

                let mut offset = 0u64;
                for (value, (_, ty)) in fields.iter().zip(def.fields.iter()) {
                    let ty = ty.clone();
                    self.store_at(ctx, code, ptr, offset, &ty, value)?;
                    offset += words(&ty) * WORD;
                }
                code.push(Instruction::LocalGet(ptr));
            }

            ExprKind::FieldGet { base, index } => {
                let Ty::Record(id) = &base.ty else {
                    return bail("field access on a non-record reached codegen");
                };
                let def = &self.hir.records[id.0 as usize];
                let offset: u64 =
                    def.fields[..*index as usize].iter().map(|(_, t)| words(t) * WORD).sum();
                let field_ty = def.fields[*index as usize].1.clone();

                let ptr = ctx.scratch(ValType::I32);
                self.expr(ctx, code, base)?;
                code.push(Instruction::LocalSet(ptr));
                load_at(code, ptr, offset, &field_ty);
            }

            ExprKind::MakeVariant { sum, variant, fields } => {
                let def = &self.hir.sums[sum.0 as usize];
                if def.variants.iter().all(|v| v.fields.is_empty()) {
                    // All-niladic sums degrade to a bare tag (docs/abi.md §3).
                    code.push(Instruction::I32Const(*variant as i32));
                    return Ok(());
                }

                let field_tys: Vec<Ty> = def.variants[*variant as usize]
                    .fields
                    .iter()
                    .map(|(_, t)| t.clone())
                    .collect();
                // Every variant of a sum takes the same room: one word for the
                // tag, then one per field of the *widest* variant. A few bytes
                // go unused on the narrow ones, and in exchange the size of a
                // value is a property of its type rather than of its tag —
                // which is what lets `send` put a constant length on the wire
                // instead of computing one from the tag at run time.
                let payload = variant_payload_words(def);
                let ptr = ctx.scratch(ValType::I32);

                code.push(Instruction::I32Const(((payload + 1) * WORD) as i32));
                code.push(Instruction::Call(self.alloc_index));
                code.push(Instruction::LocalSet(ptr));

                // Tag first, then fields.
                code.push(Instruction::LocalGet(ptr));
                code.push(Instruction::I32Const(*variant as i32));
                code.push(Instruction::I32Store(mem_arg(0, 2)));

                let mut offset = WORD;
                for (value, ty) in fields.iter().zip(field_tys.iter()) {
                    self.store_at(ctx, code, ptr, offset, ty, value)?;
                    offset += words(ty) * WORD;
                }
                code.push(Instruction::LocalGet(ptr));
            }

            ExprKind::MakeOk(inner) | ExprKind::MakeSome(inner) => {
                code.push(Instruction::I32Const(0));
                self.encode_payload(ctx, code, inner)?;
            }
            ExprKind::MakeErr(inner) => {
                code.push(Instruction::I32Const(1));
                self.encode_payload(ctx, code, inner)?;
            }
            ExprKind::MakeNone => {
                code.push(Instruction::I32Const(1));
                code.push(Instruction::I64Const(0));
            }

            ExprKind::If { cond, then_block, else_block } => {
                let block_ty = self.block_type(&expr.ty);
                self.expr(ctx, code, cond)?;
                code.push(Instruction::If(block_ty));
                self.block(ctx, code, then_block)?;
                if let Some(else_block) = else_block {
                    code.push(Instruction::Else);
                    self.expr(ctx, code, else_block)?;
                }
                code.push(Instruction::End);
            }

            ExprKind::Block(block) => self.block(ctx, code, block)?,

            ExprKind::Match { scrutinee, arms, scrutinee_slot } => {
                self.match_expr(ctx, code, expr, scrutinee, arms, *scrutinee_slot)?
            }

            ExprKind::Try { expr: inner, kind } => self.try_expr(ctx, code, inner, *kind)?,
        }
        Ok(())
    }

    fn binary(
        &mut self,
        ctx: &mut FnCtx,
        code: &mut Code,
        op: BinOp,
        lhs: &Expr,
        rhs: &Expr,
    ) -> EResult<()> {
        // `&&` and `||` short-circuit, so they cannot evaluate both sides first.
        if matches!(op, BinOp::And | BinOp::Or) {
            self.expr(ctx, code, lhs)?;
            code.push(Instruction::If(BlockType::Result(ValType::I32)));
            match op {
                BinOp::And => {
                    self.expr(ctx, code, rhs)?;
                    code.push(Instruction::Else);
                    code.push(Instruction::I32Const(0));
                }
                _ => {
                    code.push(Instruction::I32Const(1));
                    code.push(Instruction::Else);
                    self.expr(ctx, code, rhs)?;
                }
            }
            code.push(Instruction::End);
            return Ok(());
        }

        self.expr(ctx, code, lhs)?;
        self.expr(ctx, code, rhs)?;
        code.push(match op {
            BinOp::AddInt => Instruction::I64Add,
            BinOp::SubInt => Instruction::I64Sub,
            BinOp::MulInt => Instruction::I64Mul,
            BinOp::DivInt => Instruction::I64DivS,
            BinOp::RemInt => Instruction::I64RemS,
            BinOp::AddFloat => Instruction::F64Add,
            BinOp::SubFloat => Instruction::F64Sub,
            BinOp::MulFloat => Instruction::F64Mul,
            BinOp::DivFloat => Instruction::F64Div,
            BinOp::EqInt => Instruction::I64Eq,
            BinOp::NeInt => Instruction::I64Ne,
            BinOp::LtInt => Instruction::I64LtS,
            BinOp::LeInt => Instruction::I64LeS,
            BinOp::GtInt => Instruction::I64GtS,
            BinOp::GeInt => Instruction::I64GeS,
            BinOp::EqFloat => Instruction::F64Eq,
            BinOp::NeFloat => Instruction::F64Ne,
            BinOp::LtFloat => Instruction::F64Lt,
            BinOp::LeFloat => Instruction::F64Le,
            BinOp::GtFloat => Instruction::F64Gt,
            BinOp::GeFloat => Instruction::F64Ge,
            BinOp::EqBool => Instruction::I32Eq,
            BinOp::NeBool => Instruction::I32Ne,
            BinOp::And | BinOp::Or => unreachable!("handled above"),
        });
        Ok(())
    }

    /// Emits a value and widens it to the single 64-bit payload slot.
    fn encode_payload(&mut self, ctx: &mut FnCtx, code: &mut Code, inner: &Expr) -> EResult<()> {
        let representation = rep(&inner.ty);
        match representation.len() {
            0 => {
                self.expr(ctx, code, inner)?;
                code.push(Instruction::I64Const(0));
            }
            1 => {
                self.expr(ctx, code, inner)?;
                code.push(widen(representation[0]));
            }
            _ => {
                return bail(
                    "a Result/Option payload that is itself Result/Option is not supported yet",
                )
            }
        }
        Ok(())
    }

    fn store_at(
        &mut self,
        ctx: &mut FnCtx,
        code: &mut Code,
        ptr: u32,
        offset: u64,
        ty: &Ty,
        value: &Expr,
    ) -> EResult<()> {
        let representation = rep(ty);
        if representation.is_empty() {
            self.expr(ctx, code, value)?;
            return Ok(());
        }
        if representation.len() == 1 {
            code.push(Instruction::LocalGet(ptr));
            self.expr(ctx, code, value)?;
            code.push(store_instruction(representation[0], offset));
            return Ok(());
        }

        // Two-word values (Result/Option) need their parts split off the stack.
        let payload = ctx.scratch(ValType::I64);
        let tag = ctx.scratch(ValType::I32);
        self.expr(ctx, code, value)?;
        code.push(Instruction::LocalSet(payload));
        code.push(Instruction::LocalSet(tag));
        code.push(Instruction::LocalGet(ptr));
        code.push(Instruction::LocalGet(tag));
        code.push(store_instruction(ValType::I32, offset));
        code.push(Instruction::LocalGet(ptr));
        code.push(Instruction::LocalGet(payload));
        code.push(store_instruction(ValType::I64, offset + WORD));
        Ok(())
    }

    fn try_expr(
        &mut self,
        ctx: &mut FnCtx,
        code: &mut Code,
        inner: &Expr,
        kind: TryKind,
    ) -> EResult<()> {
        let tag = ctx.scratch(ValType::I32);
        let payload = ctx.scratch(ValType::I64);

        self.expr(ctx, code, inner)?;
        code.push(Instruction::LocalSet(payload));
        code.push(Instruction::LocalSet(tag));

        // Non-zero tag is Err/None: re-return the pair and leave. This is the
        // allocation-free propagation docs/abi.md §2 promises.
        code.push(Instruction::LocalGet(tag));
        code.push(Instruction::If(BlockType::Empty));
        code.push(Instruction::I32Const(1));
        match kind {
            TryKind::Result => code.push(Instruction::LocalGet(payload)),
            TryKind::Option => code.push(Instruction::I64Const(0)),
        }
        code.push(Instruction::Return);
        code.push(Instruction::End);

        // Ok/Some: narrow the payload back to the value type.
        let inner_ty = match &inner.ty {
            Ty::Result(ok, _) => (**ok).clone(),
            Ty::Option(some) => (**some).clone(),
            _ => return bail("`?` on a non-Result reached codegen"),
        };
        let representation = rep(&inner_ty);
        if representation.len() == 1 {
            code.push(Instruction::LocalGet(payload));
            code.push(narrow(representation[0]));
        } else if representation.len() > 1 {
            return bail("a Result/Option payload that is itself Result/Option is not supported yet");
        }
        Ok(())
    }

    fn match_expr(
        &mut self,
        ctx: &mut FnCtx,
        code: &mut Code,
        whole: &Expr,
        scrutinee: &Expr,
        arms: &[Arm],
        scrutinee_slot: u32,
    ) -> EResult<()> {
        // Evaluate once into the slot the checker reserved.
        self.expr(ctx, code, scrutinee)?;
        let holder = ctx.slot_locals[scrutinee_slot as usize].clone();
        store_locals(code, &holder);

        let block_ty = self.block_type(&whole.ty);
        code.push(Instruction::Block(block_ty));
        for arm in arms {
            code.push(Instruction::Block(BlockType::Empty));
            // A failed test branches to depth 0, i.e. the next arm.
            self.pattern(ctx, code, &arm.pattern, &scrutinee.ty, &holder)?;
            self.expr(ctx, code, &arm.body)?;
            code.push(Instruction::Br(1));
            code.push(Instruction::End);
        }
        // The checker proved exhaustiveness, so falling through is impossible.
        code.push(Instruction::Unreachable);
        code.push(Instruction::End);
        Ok(())
    }

    /// Emits the test for one pattern. On mismatch it branches to depth 0.
    /// Patterns never open blocks, so nested tests keep the same depth.
    fn pattern(
        &mut self,
        ctx: &mut FnCtx,
        code: &mut Code,
        pattern: &Pattern,
        ty: &Ty,
        value: &[u32],
    ) -> EResult<()> {
        match pattern {
            Pattern::Wildcard => {}
            Pattern::Bind { slot } => {
                let target = ctx.slot_locals[*slot as usize].clone();
                for (from, to) in value.iter().zip(target.iter()) {
                    code.push(Instruction::LocalGet(*from));
                    code.push(Instruction::LocalSet(*to));
                }
            }
            Pattern::Int(v) => {
                code.push(Instruction::LocalGet(value[0]));
                code.push(Instruction::I64Const(*v));
                code.push(Instruction::I64Ne);
                code.push(Instruction::BrIf(0));
            }
            Pattern::Bool(v) => {
                code.push(Instruction::LocalGet(value[0]));
                code.push(Instruction::I32Const(i32::from(*v)));
                code.push(Instruction::I32Ne);
                code.push(Instruction::BrIf(0));
            }
            Pattern::Str(text) => {
                let offset = *self.strings.get(text).expect("literal was collected");
                code.push(Instruction::LocalGet(value[0]));
                code.push(Instruction::I32Const(offset as i32));
                code.push(Instruction::Call(self.str_eq_index));
                code.push(Instruction::I32Eqz);
                code.push(Instruction::BrIf(0));
            }
            Pattern::Tagged { tag, inner } => {
                self.tagged_pattern(ctx, code, *tag, inner, ty, value)?
            }
        }
        Ok(())
    }

    fn tagged_pattern(
        &mut self,
        ctx: &mut FnCtx,
        code: &mut Code,
        tag: Tag,
        inner: &[Pattern],
        ty: &Ty,
        value: &[u32],
    ) -> EResult<()> {
        match tag {
            Tag::Ok | Tag::Some | Tag::Err | Tag::None => {
                let want = if matches!(tag, Tag::Ok | Tag::Some) { 0 } else { 1 };
                code.push(Instruction::LocalGet(value[0]));
                code.push(Instruction::I32Const(want));
                code.push(Instruction::I32Ne);
                code.push(Instruction::BrIf(0));

                if matches!(tag, Tag::None) || inner.is_empty() {
                    return Ok(());
                }
                let payload_ty = match (&tag, ty) {
                    (Tag::Ok | Tag::Some, Ty::Result(ok, _)) => (**ok).clone(),
                    (Tag::Ok | Tag::Some, Ty::Option(some)) => (**some).clone(),
                    (Tag::Err, Ty::Result(_, err)) => (**err).clone(),
                    _ => return bail("tagged pattern on an unexpected type"),
                };
                if payload_ty.has_holes() {
                    return bail("could not infer the payload type of this pattern");
                }
                let representation = rep(&payload_ty);
                if representation.is_empty() {
                    // A unit payload carries nothing to bind.
                    return Ok(());
                }
                if representation.len() > 1 {
                    return bail("a nested Result/Option payload is not supported yet");
                }
                let holder = ctx.scratch(representation[0]);
                code.push(Instruction::LocalGet(value[1]));
                code.push(narrow(representation[0]));
                code.push(Instruction::LocalSet(holder));
                self.pattern(ctx, code, &inner[0], &payload_ty, &[holder])?;
            }

            Tag::Variant { sum, index } => {
                let def = &self.hir.sums[sum.0 as usize];
                let niladic = def.variants.iter().all(|v| v.fields.is_empty());

                if niladic {
                    code.push(Instruction::LocalGet(value[0]));
                    code.push(Instruction::I32Const(index as i32));
                    code.push(Instruction::I32Ne);
                    code.push(Instruction::BrIf(0));
                    return Ok(());
                }

                // Boxed: the tag is the first word of the allocation.
                code.push(Instruction::LocalGet(value[0]));
                code.push(Instruction::I32Load(mem_arg(0, 2)));
                code.push(Instruction::I32Const(index as i32));
                code.push(Instruction::I32Ne);
                code.push(Instruction::BrIf(0));

                let field_tys: Vec<Ty> = def.variants[index as usize]
                    .fields
                    .iter()
                    .map(|(_, t)| t.clone())
                    .collect();
                let mut offset = WORD;
                for (sub, field_ty) in inner.iter().zip(field_tys.iter()) {
                    let representation = rep(field_ty);
                    if representation.len() == 1 {
                        let holder = ctx.scratch(representation[0]);
                        load_at(code, value[0], offset, field_ty);
                        code.push(Instruction::LocalSet(holder));
                        self.pattern(ctx, code, sub, field_ty, &[holder])?;
                    }
                    offset += words(field_ty) * WORD;
                }
            }
        }
        Ok(())
    }
}

/// Per-function local bookkeeping. One HIR slot can span several WASM locals,
/// since `Result`/`Option` occupy two.
struct FnCtx {
    slot_locals: Vec<Vec<u32>>,
    declared: Vec<ValType>,
    next: u32,
}

impl FnCtx {
    fn new(func: &Func) -> Self {
        let mut slot_locals = Vec::new();
        let mut declared = Vec::new();
        let mut next = 0;

        for (index, ty) in func.locals.iter().enumerate() {
            let representation = rep(ty);
            let ids: Vec<u32> = (0..representation.len()).map(|i| next + i as u32).collect();
            next += representation.len() as u32;
            slot_locals.push(ids);
            // Parameters are already WASM locals; the rest must be declared.
            if index >= func.param_count {
                declared.extend(representation);
            }
        }
        Self { slot_locals, declared, next }
    }

    /// Allocates a fresh local. Never reused — wasteful but trivially correct,
    /// and WASM engines fold these away.
    fn scratch(&mut self, ty: ValType) -> u32 {
        let index = self.next;
        self.next += 1;
        self.declared.push(ty);
        index
    }
}

fn store_locals(code: &mut Code, locals: &[u32]) {
    // The stack holds them in order, so pop back to front.
    for local in locals.iter().rev() {
        code.push(Instruction::LocalSet(*local));
    }
}

fn mem_arg(offset: u64, align: u32) -> MemArg {
    MemArg { offset, align, memory_index: 0 }
}

fn store_instruction(ty: ValType, offset: u64) -> Instruction<'static> {
    match ty {
        ValType::I32 => Instruction::I32Store(mem_arg(offset, 2)),
        ValType::I64 => Instruction::I64Store(mem_arg(offset, 3)),
        ValType::F64 => Instruction::F64Store(mem_arg(offset, 3)),
        other => unreachable!("unexpected representation {other:?}"),
    }
}

fn load_at(code: &mut Code, ptr: u32, offset: u64, ty: &Ty) {
    let representation = rep(ty);
    if representation.is_empty() {
        return;
    }
    code.push(Instruction::LocalGet(ptr));
    code.push(match representation[0] {
        ValType::I32 => Instruction::I32Load(mem_arg(offset, 2)),
        ValType::I64 => Instruction::I64Load(mem_arg(offset, 3)),
        ValType::F64 => Instruction::F64Load(mem_arg(offset, 3)),
        other => unreachable!("unexpected representation {other:?}"),
    });
    if representation.len() > 1 {
        code.push(Instruction::LocalGet(ptr));
        code.push(Instruction::I64Load(mem_arg(offset + WORD, 3)));
    }
}

/// Widens a value into the 64-bit payload slot.
fn widen(ty: ValType) -> Instruction<'static> {
    match ty {
        ValType::I64 => Instruction::Nop,
        ValType::I32 => Instruction::I64ExtendI32U,
        ValType::F64 => Instruction::I64ReinterpretF64,
        other => unreachable!("unexpected representation {other:?}"),
    }
}

/// Narrows a payload slot back to its value type.
fn narrow(ty: ValType) -> Instruction<'static> {
    match ty {
        ValType::I64 => Instruction::Nop,
        ValType::I32 => Instruction::I32WrapI64,
        ValType::F64 => Instruction::F64ReinterpretI64,
        other => unreachable!("unexpected representation {other:?}"),
    }
}

/// The signature of a host import.
fn builtin_signature(builtin: Builtin) -> (Vec<ValType>, Vec<ValType>) {
    match builtin {
        // log(ptr, len)
        Builtin::Log => (vec![ValType::I32, ValType::I32], Vec::new()),
        // send(port, ptr, len)
        Builtin::Send => (vec![ValType::I32, ValType::I32, ValType::I32], Vec::new()),
        // panic(ptr, len). Declared as returning nothing even though it never
        // returns: WASM has no bottom type for an import, so the emitter puts
        // an `unreachable` after the call instead.
        Builtin::Panic => (vec![ValType::I32, ValType::I32], Vec::new()),
    }
}

/// How many words a sum's payload occupies — the widest variant's, so that
/// every value of the type is the same size (see `MakeVariant`).
fn variant_payload_words(def: &SumDef) -> u64 {
    def.variants
        .iter()
        .map(|variant| variant.fields.iter().map(|(_, ty)| words(ty)).sum::<u64>())
        .max()
        .unwrap_or(0)
}

/// The WASM type of an actor's state global.
fn state_type(state: &Ty) -> EResult<ValType> {
    match rep(state).as_slice() {
        [single] => Ok(*single),
        _ => bail("an actor's state must be a single-word type, such as a record"),
    }
}

fn zero_of(ty: ValType) -> ConstExpr {
    match ty {
        ValType::I64 => ConstExpr::i64_const(0),
        ValType::F64 => ConstExpr::f64_const(0.0.into()),
        _ => ConstExpr::i32_const(0),
    }
}

/// `strand_main`: build the starting state and park it in the global.
fn actor_main_body(actor: &ActorInfo, offset: u32) -> Function {
    let mut f = Function::new([]);
    f.instruction(&Instruction::Call(offset + actor.init.0));
    f.instruction(&Instruction::GlobalSet(1));
    f.instruction(&Instruction::End);
    f
}

/// `strand_on_message`: turn the delivered bytes into the port's message type,
/// hand them to that port's handler with the current state, and keep what comes
/// back.
///
/// For a flat message the bytes already *are* the value: the runtime copied
/// them into this arena with `strand_alloc`, and the checker guaranteed the
/// type holds no pointers needing relocation, so a boxed variant is used
/// in place with no decoding at all. Strings are the one relocated case —
/// codegen knows their layout (`docs/abi.md` §5), so it adds the header.
///
/// The port decides which handler runs and how the bytes are read, so the two
/// come from one table and cannot drift apart. A port number the actor does
/// not have traps: it can only come from a host that disagrees with this
/// module about the actor's shape, and a crash report naming the actor is a
/// better account of that than a message quietly going nowhere.
fn actor_receive_body(actor: &ActorInfo, alloc: u32, hir: &Hir, offset: u32) -> Function {
    let mut f = Function::new([(1, ValType::I32)]);
    let (port, ptr, len, text) = (0, 1, 2, 3);

    for (index, info) in actor.inbox.iter().enumerate() {
        f.instruction(&Instruction::LocalGet(port));
        f.instruction(&Instruction::I32Const(index as i32));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Empty));

        f.instruction(&Instruction::GlobalGet(1));

        match &info.ty {
            Ty::Str => {
                f.instruction(&Instruction::LocalGet(len));
                f.instruction(&Instruction::I32Const(4));
                f.instruction(&Instruction::I32Add);
                f.instruction(&Instruction::Call(alloc));
                f.instruction(&Instruction::LocalTee(text));

                // header: the length
                f.instruction(&Instruction::LocalGet(len));
                f.instruction(&Instruction::I32Store(mem_arg(0, 2)));

                // body: the delivered bytes, after it
                f.instruction(&Instruction::LocalGet(text));
                f.instruction(&Instruction::I32Const(4));
                f.instruction(&Instruction::I32Add);
                f.instruction(&Instruction::LocalGet(ptr));
                f.instruction(&Instruction::LocalGet(len));
                f.instruction(&Instruction::MemoryCopy { src_mem: 0, dst_mem: 0 });
                f.instruction(&Instruction::LocalGet(text));
            }
            Ty::Sum(id)
                if hir.sums[id.0 as usize].variants.iter().any(|v| !v.fields.is_empty()) =>
            {
                // Boxed variant: the delivered block is already a valid value here.
                f.instruction(&Instruction::LocalGet(ptr));
            }
            // All-niladic sums, and scalars, arrive as their bare value.
            Ty::Sum(_) | Ty::Bool => {
                f.instruction(&Instruction::LocalGet(ptr));
                f.instruction(&Instruction::I32Load(mem_arg(0, 2)));
            }
            Ty::Int => {
                f.instruction(&Instruction::LocalGet(ptr));
                f.instruction(&Instruction::I64Load(mem_arg(0, 3)));
            }
            Ty::Float => {
                f.instruction(&Instruction::LocalGet(ptr));
                f.instruction(&Instruction::F64Load(mem_arg(0, 3)));
            }
            // The checker rejects anything else before codegen sees it.
            _ => {
                f.instruction(&Instruction::LocalGet(ptr));
            }
        }

        f.instruction(&Instruction::Call(offset + actor.handlers[index].0));
        f.instruction(&Instruction::GlobalSet(1));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
    }

    f.instruction(&Instruction::Unreachable);
    f.instruction(&Instruction::End);
    f
}

/// `strand_view`: empty last frame's array, then draw the actor as it is.
///
/// The state global is the only argument, because §6.5 makes a view a pure
/// function of state — there is nothing else it could need, and nothing else it
/// is allowed to see.
fn actor_view_body(view: FuncId, frame_reset: u32, offset: u32) -> Function {
    let mut f = Function::new([]);
    f.instruction(&Instruction::Call(frame_reset));
    f.instruction(&Instruction::GlobalGet(1));
    f.instruction(&Instruction::Call(offset + view.0));
    // A view returns nothing: building the nodes was the result.
    f.instruction(&Instruction::End);
    f
}

// ---- generated string helpers (`stdlib`) ---------------------------------
//
// Every one of these works on `docs/abi.md` §5's layout: a pointer to
// `{ i32 len, bytes... }`, UTF-8, immutable. Immutable is what makes them
// cheap to reason about — a helper never edits its argument, it allocates a
// new string, and the old one stays valid for anyone still holding it.
//
// Characters, not bytes, wherever a count is user-visible. A UTF-8 continuation
// byte is `0b10xxxxxx`, so `b & 0xC0 != 0x80` marks the start of a character,
// and counting or stepping back over those is all any of this needs.

/// The WASM signature of a helper. Needed before any body is emitted, because
/// the type section is written first.
fn helper_signature(helper: Helper) -> (Vec<ValType>, Vec<ValType>) {
    match helper {
        Helper::StrConcat => (vec![ValType::I32, ValType::I32], vec![ValType::I32]),
        Helper::StrFromInt => (vec![ValType::I64], vec![ValType::I32]),
        Helper::StrFromChar => (vec![ValType::I64], vec![ValType::I32]),
        Helper::StrCharCount => (vec![ValType::I32], vec![ValType::I64]),
        Helper::StrDropLast | Helper::StrTrim => (vec![ValType::I32], vec![ValType::I32]),
    }
}

fn helper_body(helper: Helper, alloc: u32) -> Function {
    match helper {
        Helper::StrConcat => str_concat_body(alloc),
        Helper::StrFromInt => str_from_int_body(alloc),
        Helper::StrFromChar => str_from_char_body(alloc),
        Helper::StrCharCount => str_char_count_body(),
        Helper::StrDropLast => str_drop_last_body(alloc),
        Helper::StrTrim => str_trim_body(alloc),
    }
}

/// Pushes `bytes = header + len`, allocates, and writes the length header.
/// Leaves the new string's pointer in `out`.
fn begin_string(f: &mut Function, alloc: u32, len: u32, out: u32) {
    f.instruction(&Instruction::LocalGet(len));
    f.instruction(&Instruction::I32Const(4));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::Call(alloc));
    f.instruction(&Instruction::LocalTee(out));
    f.instruction(&Instruction::LocalGet(len));
    f.instruction(&Instruction::I32Store(mem_arg(0, 2)));
}

/// `a + b`. One allocation, two copies, and neither argument is touched.
fn str_concat_body(alloc: u32) -> Function {
    let mut f = Function::new([(3, ValType::I32)]);
    let (a, b, la, lb, out) = (0, 1, 2, 3, 4);

    for (src, len) in [(a, la), (b, lb)] {
        f.instruction(&Instruction::LocalGet(src));
        f.instruction(&Instruction::I32Load(mem_arg(0, 2)));
        f.instruction(&Instruction::LocalSet(len));
    }

    // total = la + lb, reusing `la`'s neighbour slot for the sum.
    f.instruction(&Instruction::LocalGet(la));
    f.instruction(&Instruction::LocalGet(lb));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::I32Const(4));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::Call(alloc));
    f.instruction(&Instruction::LocalTee(out));
    f.instruction(&Instruction::LocalGet(la));
    f.instruction(&Instruction::LocalGet(lb));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::I32Store(mem_arg(0, 2)));

    // out[4..] = a[4..], then out[4 + la..] = b[4..].
    f.instruction(&Instruction::LocalGet(out));
    f.instruction(&Instruction::I32Const(4));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalGet(a));
    f.instruction(&Instruction::I32Const(4));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalGet(la));
    f.instruction(&Instruction::MemoryCopy { src_mem: 0, dst_mem: 0 });

    f.instruction(&Instruction::LocalGet(out));
    f.instruction(&Instruction::I32Const(4));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalGet(la));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalGet(b));
    f.instruction(&Instruction::I32Const(4));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalGet(lb));
    f.instruction(&Instruction::MemoryCopy { src_mem: 0, dst_mem: 0 });

    f.instruction(&Instruction::LocalGet(out));
    f.instruction(&Instruction::End);
    f
}

/// Characters, not bytes: everything whose top bits are not `10` starts one.
fn str_char_count_body() -> Function {
    let mut f = Function::new([(3, ValType::I32)]);
    let (s, n, i, count) = (0, 1, 2, 3);

    f.instruction(&Instruction::LocalGet(s));
    f.instruction(&Instruction::I32Load(mem_arg(0, 2)));
    f.instruction(&Instruction::LocalSet(n));

    f.instruction(&Instruction::Block(BlockType::Empty));
    f.instruction(&Instruction::Loop(BlockType::Empty));
    f.instruction(&Instruction::LocalGet(i));
    f.instruction(&Instruction::LocalGet(n));
    f.instruction(&Instruction::I32GeU);
    f.instruction(&Instruction::BrIf(1));

    f.instruction(&Instruction::LocalGet(s));
    f.instruction(&Instruction::LocalGet(i));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::I32Load8U(mem_arg(4, 0)));
    f.instruction(&Instruction::I32Const(0xC0));
    f.instruction(&Instruction::I32And);
    f.instruction(&Instruction::I32Const(0x80));
    f.instruction(&Instruction::I32Ne);
    f.instruction(&Instruction::If(BlockType::Empty));
    f.instruction(&Instruction::LocalGet(count));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalSet(count));
    f.instruction(&Instruction::End);

    f.instruction(&Instruction::LocalGet(i));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalSet(i));
    f.instruction(&Instruction::Br(0));
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);

    f.instruction(&Instruction::LocalGet(count));
    f.instruction(&Instruction::I64ExtendI32U);
    f.instruction(&Instruction::End);
    f
}

/// What Backspace does: step back over any continuation bytes so a multi-byte
/// character goes as one thing rather than leaving a broken tail.
fn str_drop_last_body(alloc: u32) -> Function {
    let mut f = Function::new([(3, ValType::I32)]);
    let (s, n, cut, out) = (0, 1, 2, 3);

    f.instruction(&Instruction::LocalGet(s));
    f.instruction(&Instruction::I32Load(mem_arg(0, 2)));
    f.instruction(&Instruction::LocalTee(n));

    // Nothing to drop: hand back the same string. Immutability makes that safe.
    f.instruction(&Instruction::I32Eqz);
    f.instruction(&Instruction::If(BlockType::Empty));
    f.instruction(&Instruction::LocalGet(s));
    f.instruction(&Instruction::Return);
    f.instruction(&Instruction::End);

    f.instruction(&Instruction::LocalGet(n));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::I32Sub);
    f.instruction(&Instruction::LocalSet(cut));

    f.instruction(&Instruction::Block(BlockType::Empty));
    f.instruction(&Instruction::Loop(BlockType::Empty));
    f.instruction(&Instruction::LocalGet(cut));
    f.instruction(&Instruction::I32Eqz);
    f.instruction(&Instruction::BrIf(1));

    f.instruction(&Instruction::LocalGet(s));
    f.instruction(&Instruction::LocalGet(cut));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::I32Load8U(mem_arg(4, 0)));
    f.instruction(&Instruction::I32Const(0xC0));
    f.instruction(&Instruction::I32And);
    f.instruction(&Instruction::I32Const(0x80));
    f.instruction(&Instruction::I32Ne);
    f.instruction(&Instruction::BrIf(1));

    f.instruction(&Instruction::LocalGet(cut));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::I32Sub);
    f.instruction(&Instruction::LocalSet(cut));
    f.instruction(&Instruction::Br(0));
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);

    begin_string(&mut f, alloc, cut, out);
    f.instruction(&Instruction::LocalGet(out));
    f.instruction(&Instruction::I32Const(4));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalGet(s));
    f.instruction(&Instruction::I32Const(4));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalGet(cut));
    f.instruction(&Instruction::MemoryCopy { src_mem: 0, dst_mem: 0 });

    f.instruction(&Instruction::LocalGet(out));
    f.instruction(&Instruction::End);
    f
}

/// ASCII whitespace off both ends. §12's scope discipline: Unicode whitespace
/// is a table, and nothing in the POC needs one.
fn str_trim_body(alloc: u32) -> Function {
    // Six locals, not five: `is_ascii_space` borrows the last one.
    let mut f = Function::new([(6, ValType::I32)]);
    let (s, n, start, end, len, out) = (0, 1, 2, 3, 4, 5);

    f.instruction(&Instruction::LocalGet(s));
    f.instruction(&Instruction::I32Load(mem_arg(0, 2)));
    f.instruction(&Instruction::LocalTee(n));
    f.instruction(&Instruction::LocalSet(end));

    // Forwards past leading whitespace.
    f.instruction(&Instruction::Block(BlockType::Empty));
    f.instruction(&Instruction::Loop(BlockType::Empty));
    f.instruction(&Instruction::LocalGet(start));
    f.instruction(&Instruction::LocalGet(end));
    f.instruction(&Instruction::I32GeU);
    f.instruction(&Instruction::BrIf(1));
    f.instruction(&Instruction::LocalGet(s));
    f.instruction(&Instruction::LocalGet(start));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::I32Load8U(mem_arg(4, 0)));
    is_ascii_space(&mut f);
    f.instruction(&Instruction::I32Eqz);
    f.instruction(&Instruction::BrIf(1));
    f.instruction(&Instruction::LocalGet(start));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalSet(start));
    f.instruction(&Instruction::Br(0));
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);

    // Backwards past trailing whitespace.
    f.instruction(&Instruction::Block(BlockType::Empty));
    f.instruction(&Instruction::Loop(BlockType::Empty));
    f.instruction(&Instruction::LocalGet(end));
    f.instruction(&Instruction::LocalGet(start));
    f.instruction(&Instruction::I32LeU);
    f.instruction(&Instruction::BrIf(1));
    f.instruction(&Instruction::LocalGet(s));
    f.instruction(&Instruction::LocalGet(end));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::I32Load8U(mem_arg(3, 0)));
    is_ascii_space(&mut f);
    f.instruction(&Instruction::I32Eqz);
    f.instruction(&Instruction::BrIf(1));
    f.instruction(&Instruction::LocalGet(end));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::I32Sub);
    f.instruction(&Instruction::LocalSet(end));
    f.instruction(&Instruction::Br(0));
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);

    f.instruction(&Instruction::LocalGet(end));
    f.instruction(&Instruction::LocalGet(start));
    f.instruction(&Instruction::I32Sub);
    f.instruction(&Instruction::LocalSet(len));

    begin_string(&mut f, alloc, len, out);
    f.instruction(&Instruction::LocalGet(out));
    f.instruction(&Instruction::I32Const(4));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalGet(s));
    f.instruction(&Instruction::I32Const(4));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalGet(start));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalGet(len));
    f.instruction(&Instruction::MemoryCopy { src_mem: 0, dst_mem: 0 });

    f.instruction(&Instruction::LocalGet(out));
    f.instruction(&Instruction::End);
    f
}

/// Replaces the byte on the stack with whether it is space, tab, CR or LF.
fn is_ascii_space(f: &mut Function) {
    // b == 32 || b == 9 || b == 10 || b == 13, without a local to hold `b`:
    // `(b == 32) | (b == 9) | (b == 10) | (b == 13)` needs `b` four times, so
    // subtract-and-compare against the small set instead.
    f.instruction(&Instruction::LocalSet(SPACE_SCRATCH));
    let mut first = true;
    for byte in [32, 9, 10, 13] {
        f.instruction(&Instruction::LocalGet(SPACE_SCRATCH));
        f.instruction(&Instruction::I32Const(byte));
        f.instruction(&Instruction::I32Eq);
        if !first {
            f.instruction(&Instruction::I32Or);
        }
        first = false;
    }
}

/// The local `is_ascii_space` borrows. Both callers declare it as their last
/// i32, so the index is the same in each.
const SPACE_SCRATCH: u32 = 6;

/// Decimal. Two passes: count the digits, then fill backwards from the end.
///
/// The magnitude is taken as *unsigned*, which is what makes `int`'s most
/// negative value work: negating it wraps to itself, and read without a sign
/// that bit pattern is exactly the magnitude wanted.
fn str_from_int_body(alloc: u32) -> Function {
    let mut f = Function::new([(2, ValType::I64), (4, ValType::I32)]);
    let (value, mag, scratch) = (0, 1, 2);
    let (digits, total, out, at) = (3, 4, 5, 6);

    f.instruction(&Instruction::LocalGet(value));
    f.instruction(&Instruction::I64Const(0));
    f.instruction(&Instruction::I64LtS);
    f.instruction(&Instruction::If(BlockType::Empty));
    f.instruction(&Instruction::I64Const(0));
    f.instruction(&Instruction::LocalGet(value));
    f.instruction(&Instruction::I64Sub);
    f.instruction(&Instruction::LocalSet(mag));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::LocalSet(total));
    f.instruction(&Instruction::Else);
    f.instruction(&Instruction::LocalGet(value));
    f.instruction(&Instruction::LocalSet(mag));
    f.instruction(&Instruction::End);

    // At least one digit, so zero prints as "0" rather than as nothing.
    f.instruction(&Instruction::LocalGet(mag));
    f.instruction(&Instruction::LocalSet(scratch));
    f.instruction(&Instruction::Loop(BlockType::Empty));
    f.instruction(&Instruction::LocalGet(digits));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalSet(digits));
    f.instruction(&Instruction::LocalGet(scratch));
    f.instruction(&Instruction::I64Const(10));
    f.instruction(&Instruction::I64DivU);
    f.instruction(&Instruction::LocalTee(scratch));
    f.instruction(&Instruction::I64Const(0));
    f.instruction(&Instruction::I64Ne);
    f.instruction(&Instruction::BrIf(0));
    f.instruction(&Instruction::End);

    f.instruction(&Instruction::LocalGet(total));
    f.instruction(&Instruction::LocalGet(digits));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalSet(total));

    begin_string(&mut f, alloc, total, out);

    // The sign, if there is one: `total` was seeded with 1 for it.
    f.instruction(&Instruction::LocalGet(value));
    f.instruction(&Instruction::I64Const(0));
    f.instruction(&Instruction::I64LtS);
    f.instruction(&Instruction::If(BlockType::Empty));
    f.instruction(&Instruction::LocalGet(out));
    f.instruction(&Instruction::I32Const(b'-' as i32));
    f.instruction(&Instruction::I32Store8(mem_arg(4, 0)));
    f.instruction(&Instruction::End);

    // Digits, least significant first, written from the far end backwards.
    f.instruction(&Instruction::LocalGet(total));
    f.instruction(&Instruction::LocalSet(at));
    f.instruction(&Instruction::LocalGet(mag));
    f.instruction(&Instruction::LocalSet(scratch));
    f.instruction(&Instruction::Loop(BlockType::Empty));
    f.instruction(&Instruction::LocalGet(at));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::I32Sub);
    f.instruction(&Instruction::LocalTee(at));
    f.instruction(&Instruction::LocalGet(out));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalGet(scratch));
    f.instruction(&Instruction::I64Const(10));
    f.instruction(&Instruction::I64RemU);
    f.instruction(&Instruction::I32WrapI64);
    f.instruction(&Instruction::I32Const(b'0' as i32));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::I32Store8(mem_arg(4, 0)));
    f.instruction(&Instruction::LocalGet(scratch));
    f.instruction(&Instruction::I64Const(10));
    f.instruction(&Instruction::I64DivU);
    f.instruction(&Instruction::LocalTee(scratch));
    f.instruction(&Instruction::I64Const(0));
    f.instruction(&Instruction::I64Ne);
    f.instruction(&Instruction::BrIf(0));
    f.instruction(&Instruction::End);

    f.instruction(&Instruction::LocalGet(out));
    f.instruction(&Instruction::End);
    f
}

/// One UTF-8 width: how many bytes, the lead byte's mask, and where each
/// trailing byte goes and which six bits it carries.
type Width = (i32, i32, &'static [(u64, i32)]);

/// One character, UTF-8 encoded, from a Unicode scalar value.
///
/// The encoding is written out rather than looped, because there are only four
/// widths and each writes a different number of bytes.
fn str_from_char_body(alloc: u32) -> Function {
    let mut f = Function::new([(4, ValType::I32)]);
    let code = 0;
    let (scalar, len, out) = (1, 2, 3);

    f.instruction(&Instruction::LocalGet(code));
    f.instruction(&Instruction::I32WrapI64);
    f.instruction(&Instruction::LocalSet(scalar));

    // Width, by range.
    for (limit, width) in [(0x80, 1), (0x800, 2), (0x1_0000, 3)] {
        f.instruction(&Instruction::LocalGet(len));
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::LocalGet(scalar));
        f.instruction(&Instruction::I32Const(limit));
        f.instruction(&Instruction::I32LtU);
        f.instruction(&Instruction::I32And);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::I32Const(width));
        f.instruction(&Instruction::LocalSet(len));
        f.instruction(&Instruction::End);
    }
    f.instruction(&Instruction::LocalGet(len));
    f.instruction(&Instruction::I32Eqz);
    f.instruction(&Instruction::If(BlockType::Empty));
    f.instruction(&Instruction::I32Const(4));
    f.instruction(&Instruction::LocalSet(len));
    f.instruction(&Instruction::End);

    begin_string(&mut f, alloc, len, out);

    let lead = |f: &mut Function, mask: i32, shift: i32| {
        f.instruction(&Instruction::LocalGet(out));
        f.instruction(&Instruction::LocalGet(scalar));
        f.instruction(&Instruction::I32Const(shift));
        f.instruction(&Instruction::I32ShrU);
        f.instruction(&Instruction::I32Const(mask));
        f.instruction(&Instruction::I32Or);
        f.instruction(&Instruction::I32Store8(mem_arg(4, 0)));
    };
    let trail = |f: &mut Function, at: u64, shift: i32| {
        f.instruction(&Instruction::LocalGet(out));
        f.instruction(&Instruction::LocalGet(scalar));
        f.instruction(&Instruction::I32Const(shift));
        f.instruction(&Instruction::I32ShrU);
        f.instruction(&Instruction::I32Const(0x3F));
        f.instruction(&Instruction::I32And);
        f.instruction(&Instruction::I32Const(0x80));
        f.instruction(&Instruction::I32Or);
        f.instruction(&Instruction::I32Store8(mem_arg(at, 0)));
    };

    // (width, lead-byte mask, trailing bytes as (offset, shift)).
    let widths: [Width; 4] = [
        (1, 0x00, &[]),
        (2, 0xC0, &[(5, 0)]),
        (3, 0xE0, &[(5, 6), (6, 0)]),
        (4, 0xF0, &[(5, 12), (6, 6), (7, 0)]),
    ];
    for (width, mask, trailers) in widths {
        f.instruction(&Instruction::LocalGet(len));
        f.instruction(&Instruction::I32Const(width));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Empty));
        lead(&mut f, mask, (6 * trailers.len()) as i32);
        for (at, shift) in trailers {
            trail(&mut f, *at, *shift);
        }
        f.instruction(&Instruction::End);
    }

    f.instruction(&Instruction::LocalGet(out));
    f.instruction(&Instruction::End);
    f
}

/// Bump allocator in the guest arena (`docs/abi.md` §6). Never frees: §5.1
/// reclaims the whole arena when the actor dies.
fn alloc_body() -> Function {
    let mut f = Function::new([(2, ValType::I32)]);
    let (size, ptr, next) = (0, 1, 2);

    f.instruction(&Instruction::GlobalGet(0));
    f.instruction(&Instruction::LocalTee(ptr));
    f.instruction(&Instruction::LocalGet(size));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::I32Const(7));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::I32Const(-8));
    f.instruction(&Instruction::I32And);
    f.instruction(&Instruction::LocalSet(next));

    // Grow a page at a time until the bump fits.
    f.instruction(&Instruction::Block(BlockType::Empty));
    f.instruction(&Instruction::Loop(BlockType::Empty));
    f.instruction(&Instruction::LocalGet(next));
    f.instruction(&Instruction::MemorySize(0));
    f.instruction(&Instruction::I32Const(16));
    f.instruction(&Instruction::I32Shl);
    f.instruction(&Instruction::I32LeU);
    f.instruction(&Instruction::BrIf(1));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::MemoryGrow(0));
    f.instruction(&Instruction::Drop);
    f.instruction(&Instruction::Br(0));
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);

    f.instruction(&Instruction::LocalGet(next));
    f.instruction(&Instruction::GlobalSet(0));
    f.instruction(&Instruction::LocalGet(ptr));
    f.instruction(&Instruction::End);
    f
}

/// Appends one node to the frame's array (`ui`'s layout).
///
/// `marker` is the pending count from before this node's children ran, so
/// `pending - marker` is exactly how many of the finished roots are its own.
/// One subtraction replaces the child-tracking stack a tree builder would
/// otherwise need, and it is what makes the array post-order by construction.
///
/// A frame that exceeds `NODE_CAPACITY` traps. That is deliberate: the arena is
/// fixed, so the alternative is a silent truncation, and a trap arrives as a
/// crash report naming the actor (§8.4) instead of as a view that quietly
/// stopped drawing halfway down.
fn node_push_body(base_global: u32, count_global: u32, pending_global: u32) -> Function {
    let mut f = Function::new([(1, ValType::I32)]);
    let (kind, marker, id, flag, text, text2, number, number2) = (0, 1, 2, 3, 4, 5, 6, 7);
    let addr = 8;

    // if node_count >= NODE_CAPACITY { unreachable }
    f.instruction(&Instruction::GlobalGet(count_global));
    f.instruction(&Instruction::I32Const(ui::NODE_CAPACITY as i32));
    f.instruction(&Instruction::I32GeU);
    f.instruction(&Instruction::If(BlockType::Empty));
    f.instruction(&Instruction::Unreachable);
    f.instruction(&Instruction::End);

    // addr = node_base + node_count * NODE_SIZE
    f.instruction(&Instruction::GlobalGet(base_global));
    f.instruction(&Instruction::GlobalGet(count_global));
    f.instruction(&Instruction::I32Const(ui::NODE_SIZE as i32));
    f.instruction(&Instruction::I32Mul);
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalSet(addr));

    let store_i32 = |f: &mut Function, offset: u32, local: u32| {
        f.instruction(&Instruction::LocalGet(addr));
        f.instruction(&Instruction::LocalGet(local));
        f.instruction(&Instruction::I32Store(mem_arg(offset as u64, 2)));
    };
    store_i32(&mut f, ui::KIND_OFFSET, kind);
    store_i32(&mut f, Slot::Id.offset(), id);
    store_i32(&mut f, Slot::Flag.offset(), flag);
    store_i32(&mut f, Slot::Text.offset(), text);
    store_i32(&mut f, Slot::Text2.offset(), text2);

    for (offset, local) in [(Slot::Number.offset(), number), (Slot::Number2.offset(), number2)] {
        f.instruction(&Instruction::LocalGet(addr));
        f.instruction(&Instruction::LocalGet(local));
        f.instruction(&Instruction::F32Store(mem_arg(offset as u64, 2)));
    }

    // child_count = pending - marker
    f.instruction(&Instruction::LocalGet(addr));
    f.instruction(&Instruction::GlobalGet(pending_global));
    f.instruction(&Instruction::LocalGet(marker));
    f.instruction(&Instruction::I32Sub);
    f.instruction(&Instruction::I32Store(mem_arg(ui::CHILD_COUNT_OFFSET as u64, 2)));

    f.instruction(&Instruction::GlobalGet(count_global));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::GlobalSet(count_global));

    // This node has claimed its children and is now one pending root itself.
    f.instruction(&Instruction::LocalGet(marker));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::GlobalSet(pending_global));

    f.instruction(&Instruction::End);
    f
}

/// Empties the frame's array. §6.1's per-frame arena reset, which at this
/// layout is two stores.
fn frame_reset_body(count_global: u32, pending_global: u32) -> Function {
    let mut f = Function::new([]);
    for global in [count_global, pending_global] {
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::GlobalSet(global));
    }
    f.instruction(&Instruction::End);
    f
}

/// Byte-wise string equality, needed by string patterns (§5 layout).
fn str_eq_body() -> Function {
    let mut f = Function::new([(2, ValType::I32)]);
    let (a, b, n, i) = (0, 1, 2, 3);

    f.instruction(&Instruction::LocalGet(a));
    f.instruction(&Instruction::I32Load(mem_arg(0, 2)));
    f.instruction(&Instruction::LocalSet(n));

    f.instruction(&Instruction::LocalGet(n));
    f.instruction(&Instruction::LocalGet(b));
    f.instruction(&Instruction::I32Load(mem_arg(0, 2)));
    f.instruction(&Instruction::I32Ne);
    f.instruction(&Instruction::If(BlockType::Empty));
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::Return);
    f.instruction(&Instruction::End);

    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::LocalSet(i));
    f.instruction(&Instruction::Block(BlockType::Empty));
    f.instruction(&Instruction::Loop(BlockType::Empty));
    f.instruction(&Instruction::LocalGet(i));
    f.instruction(&Instruction::LocalGet(n));
    f.instruction(&Instruction::I32GeU);
    f.instruction(&Instruction::BrIf(1));

    f.instruction(&Instruction::LocalGet(a));
    f.instruction(&Instruction::LocalGet(i));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::I32Load8U(mem_arg(4, 0)));
    f.instruction(&Instruction::LocalGet(b));
    f.instruction(&Instruction::LocalGet(i));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::I32Load8U(mem_arg(4, 0)));
    f.instruction(&Instruction::I32Ne);
    f.instruction(&Instruction::If(BlockType::Empty));
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::Return);
    f.instruction(&Instruction::End);

    f.instruction(&Instruction::LocalGet(i));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalSet(i));
    f.instruction(&Instruction::Br(0));
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);

    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::End);
    f
}

// ---- literal collection --------------------------------------------------

fn collect_block_strings(block: &Block, out: &mut Vec<String>) {
    for stmt in &block.stmts {
        match stmt {
            Stmt::Let { value, .. } | Stmt::AssignLocal { value, .. } | Stmt::Expr(value) => {
                collect_expr_strings(value, out)
            }
            Stmt::Return(value) => {
                if let Some(value) = value {
                    collect_expr_strings(value, out);
                }
            }
        }
    }
    if let Some(tail) = &block.tail {
        collect_expr_strings(tail, out);
    }
}

fn collect_block_builtins(block: &Block, out: &mut Vec<Builtin>) {
    walk_block(block, &mut |expr| match &expr.kind {
        ExprKind::CallBuiltin { builtin, .. } => out.push(*builtin),
        // `send` is its own node rather than a builtin call, because the port
        // is a compile-time number and not an argument. It still imports the
        // host function, so it has to be counted here too.
        ExprKind::Send { .. } => out.push(Builtin::Send),
        _ => {}
    });
}

/// Visits every expression in a block. Used by the builtin collector; the
/// string collector predates it and keeps its own traversal.
fn walk_block(block: &Block, visit: &mut impl FnMut(&Expr)) {
    for stmt in &block.stmts {
        match stmt {
            Stmt::Let { value, .. } | Stmt::AssignLocal { value, .. } | Stmt::Expr(value) => {
                walk_expr(value, visit)
            }
            Stmt::Return(value) => {
                if let Some(value) = value {
                    walk_expr(value, visit);
                }
            }
        }
    }
    if let Some(tail) = &block.tail {
        walk_expr(tail, visit);
    }
}

fn walk_expr(expr: &Expr, visit: &mut impl FnMut(&Expr)) {
    visit(expr);
    match &expr.kind {
        ExprKind::Unary { expr, .. } => walk_expr(expr, visit),
        ExprKind::Binary { lhs, rhs, .. } => {
            walk_expr(lhs, visit);
            walk_expr(rhs, visit);
        }
        ExprKind::Call { args, .. }
        | ExprKind::CallBuiltin { args, .. }
        | ExprKind::CallHelper { args, .. }
        | ExprKind::MakeRecord { fields: args, .. }
        | ExprKind::MakeVariant { fields: args, .. }
        | ExprKind::MakeList { elems: args } => {
            for arg in args {
                walk_expr(arg, visit);
            }
        }
        ExprKind::Send { value, .. } => walk_expr(value, visit),
        ExprKind::ListLen { list } => walk_expr(list, visit),
        ExprKind::ListPush { list, value } => {
            walk_expr(list, visit);
            walk_expr(value, visit);
        }
        ExprKind::For { list, body, .. } => {
            walk_expr(list, visit);
            walk_block(body, visit);
        }
        // A view's props and children hold ordinary expressions, and anything
        // in them can call `log` or a helper. Missing this meant a `log` inside
        // a builder's block compiled to a call to an import nobody collected.
        ExprKind::MakeNode { props, children, .. } => {
            for (_, value) in props {
                walk_expr(value, visit);
            }
            walk_block(children, visit);
        }
        ExprKind::FieldGet { base, .. } => walk_expr(base, visit),
        ExprKind::MakeOk(inner)
        | ExprKind::MakeErr(inner)
        | ExprKind::MakeSome(inner)
        | ExprKind::Try { expr: inner, .. } => walk_expr(inner, visit),
        ExprKind::If { cond, then_block, else_block } => {
            walk_expr(cond, visit);
            walk_block(then_block, visit);
            if let Some(else_block) = else_block {
                walk_expr(else_block, visit);
            }
        }
        ExprKind::Match { scrutinee, arms, .. } => {
            walk_expr(scrutinee, visit);
            for arm in arms {
                walk_expr(&arm.body, visit);
            }
        }
        ExprKind::Block(block) => walk_block(block, visit),
        _ => {}
    }
}

fn collect_pattern_strings(pattern: &Pattern, out: &mut Vec<String>) {
    match pattern {
        Pattern::Str(text) => out.push(text.clone()),
        Pattern::Tagged { inner, .. } => {
            for sub in inner {
                collect_pattern_strings(sub, out);
            }
        }
        _ => {}
    }
}

fn collect_expr_strings(expr: &Expr, out: &mut Vec<String>) {
    match &expr.kind {
        ExprKind::Str(text) => out.push(text.clone()),
        ExprKind::Send { value, .. } => collect_expr_strings(value, out),
        ExprKind::Unary { expr, .. } => collect_expr_strings(expr, out),
        ExprKind::Binary { lhs, rhs, .. } => {
            collect_expr_strings(lhs, out);
            collect_expr_strings(rhs, out);
        }
        ExprKind::Call { args, .. }
        | ExprKind::CallBuiltin { args, .. }
        | ExprKind::CallHelper { args, .. }
        | ExprKind::MakeList { elems: args } => {
            for arg in args {
                collect_expr_strings(arg, out);
            }
        }
        ExprKind::ListLen { list } => collect_expr_strings(list, out),
        ExprKind::ListPush { list, value } => {
            collect_expr_strings(list, out);
            collect_expr_strings(value, out);
        }
        ExprKind::For { list, body, .. } => {
            collect_expr_strings(list, out);
            collect_block_strings(body, out);
        }
        ExprKind::MakeRecord { fields, .. } | ExprKind::MakeVariant { fields, .. } => {
            for field in fields {
                collect_expr_strings(field, out);
            }
        }
        ExprKind::FieldGet { base, .. } => collect_expr_strings(base, out),
        ExprKind::MakeOk(inner)
        | ExprKind::MakeErr(inner)
        | ExprKind::MakeSome(inner)
        | ExprKind::Try { expr: inner, .. } => collect_expr_strings(inner, out),
        ExprKind::If { cond, then_block, else_block } => {
            collect_expr_strings(cond, out);
            collect_block_strings(then_block, out);
            if let Some(else_block) = else_block {
                collect_expr_strings(else_block, out);
            }
        }
        ExprKind::Match { scrutinee, arms, .. } => {
            collect_expr_strings(scrutinee, out);
            for arm in arms {
                collect_pattern_strings(&arm.pattern, out);
                collect_expr_strings(&arm.body, out);
            }
        }
        ExprKind::Block(block) => collect_block_strings(block, out),
        ExprKind::MakeNode { props, children, .. } => {
            for (_, value) in props {
                collect_expr_strings(value, out);
            }
            collect_block_strings(children, out);
        }
        ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Bool(_)
        | ExprKind::Unit
        | ExprKind::Local(_)
        | ExprKind::MakeNone => {}
    }
}
