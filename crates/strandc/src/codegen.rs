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
    Emitter::new(hir).run()
}

struct Emitter<'hir> {
    hir: &'hir Hir,
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
    /// Host functions this module actually calls. Imports take the lowest
    /// function indices, so everything defined here is offset past them.
    imports: Vec<Builtin>,
}

impl<'hir> Emitter<'hir> {
    fn new(hir: &'hir Hir) -> Self {
        let helpers = hir.funcs.len() as u32;
        let builds_nodes = hir.funcs.iter().any(|func| func.is_view);
        // Global 0 is the bump pointer and global 1, when there is an actor, is
        // the state. The frame's arena takes the next three.
        let first_free = if hir.actor.is_some() { 2 } else { 1 };
        Self {
            hir,
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
            imports: Vec::new(),
        }
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
        self.collect_strings();
        self.collect_imports();

        // Imports shift every defined function, so fix the helper indices here
        // rather than sprinkling the offset through emission.
        let offset = self.imports.len() as u32;
        self.alloc_index += offset;
        self.str_eq_index += offset;
        self.node_push_index += offset;

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
        let actor_recv_ty = self.intern_type(vec![ValType::I32, ValType::I32], Vec::new());
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
            for builtin in &self.imports {
                let (module_name, field) = builtin.import();
                imports.import(module_name, field, EntityType::Function(builtin_type(*builtin, &mut self.types)));
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
        if self.hir.actor.is_some() {
            functions.function(actor_main_ty);
            functions.function(actor_recv_ty);
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
        if let Some(actor) = &self.hir.actor {
            // Global 1 holds the current state. `receive` returns the next one,
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
        for (index, func) in self.hir.funcs.iter().enumerate() {
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
        if self.hir.actor.is_some() {
            let actor_base =
                if self.builds_nodes { self.node_push_index + 1 } else { self.str_eq_index };
            exports.export("strand_main", ExportKind::Func, actor_base + 1);
            exports.export("strand_on_message", ExportKind::Func, actor_base + 2);
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
        if let Some(actor) = &self.hir.actor {
            code.function(&actor_main_body(actor, offset));
            code.function(&actor_receive_body(actor, self.alloc_index, self.hir, offset));
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
                }
                let index = self
                    .imports
                    .iter()
                    .position(|b| b == builtin)
                    .expect("import was collected");
                code.push(Instruction::Call(index as u32));
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
                let payload: u64 = field_tys.iter().map(words).sum();
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

/// Interns the signature of a host import.
fn builtin_type(builtin: Builtin, types: &mut Vec<(Vec<ValType>, Vec<ValType>)>) -> u32 {
    let signature = match builtin {
        // log(ptr, len)
        Builtin::Log => (vec![ValType::I32, ValType::I32], Vec::new()),
    };
    if let Some(index) = types.iter().position(|t| *t == signature) {
        return index as u32;
    }
    types.push(signature);
    (types.len() - 1) as u32
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

/// `strand_on_message`: turn the delivered bytes into the actor's message
/// type, hand it to `receive` with the current state, and keep what comes back.
///
/// For a flat message the bytes already *are* the value: the runtime copied
/// them into this arena with `strand_alloc`, and the checker guaranteed the
/// type holds no pointers needing relocation, so a boxed variant is used
/// in place with no decoding at all. Strings are the one relocated case —
/// codegen knows their layout (`docs/abi.md` §5), so it adds the header.
fn actor_receive_body(actor: &ActorInfo, alloc: u32, hir: &Hir, offset: u32) -> Function {
    let mut f = Function::new([(1, ValType::I32)]);
    let (ptr, len, text) = (0, 1, 2);

    f.instruction(&Instruction::GlobalGet(1));

    match &actor.message {
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
        Ty::Sum(id) if hir.sums[id.0 as usize].variants.iter().any(|v| !v.fields.is_empty()) => {
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

    f.instruction(&Instruction::Call(offset + actor.receive.0));
    f.instruction(&Instruction::GlobalSet(1));
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
    walk_block(block, &mut |expr| {
        if let ExprKind::CallBuiltin { builtin, .. } = &expr.kind {
            out.push(*builtin);
        }
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
        | ExprKind::MakeRecord { fields: args, .. }
        | ExprKind::MakeVariant { fields: args, .. } => {
            for arg in args {
                walk_expr(arg, visit);
            }
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
        ExprKind::Unary { expr, .. } => collect_expr_strings(expr, out),
        ExprKind::Binary { lhs, rhs, .. } => {
            collect_expr_strings(lhs, out);
            collect_expr_strings(rhs, out);
        }
        ExprKind::Call { args, .. } | ExprKind::CallBuiltin { args, .. } => {
            for arg in args {
                collect_expr_strings(arg, out);
            }
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
