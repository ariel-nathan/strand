//! Typed IR produced by the checker and consumed by the WASM emitter.
//!
//! The checker resolves names, assigns local slots, and picks the
//! representation for every value, so codegen never has to ask a question the
//! type system already answered. Layout rules live in `docs/abi.md`.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FuncId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RecordId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SumId(pub u32);

#[derive(Debug, Clone, PartialEq, Default)]
pub enum Ty {
    Int,
    Float,
    Bool,
    Str,
    #[default]
    Unit,
    List(Box<Ty>),
    Option(Box<Ty>),
    Result(Box<Ty>, Box<Ty>),
    Record(RecordId),
    Sum(SumId),
    /// The type of an expression that never yields — a block ending in
    /// `return`. Unifies with anything.
    Never,
    /// Poison, produced where checking already failed. Unifies with anything so
    /// one mistake does not cascade into a page of errors.
    Error,
}

impl Ty {
    /// Structural compatibility. `Never` and `Error` absorb, so a failed check
    /// never produces follow-on noise.
    pub fn unifies(&self, other: &Ty) -> bool {
        match (self, other) {
            (Ty::Never | Ty::Error, _) | (_, Ty::Never | Ty::Error) => true,
            (Ty::List(a), Ty::List(b)) | (Ty::Option(a), Ty::Option(b)) => a.unifies(b),
            (Ty::Result(a1, e1), Ty::Result(a2, e2)) => a1.unifies(a2) && e1.unifies(e2),
            _ => self == other,
        }
    }

    /// Whether values of this type are a pointer into linear memory
    /// (`docs/abi.md` §3-§5) rather than an immediate scalar.
    pub fn is_boxed(&self) -> bool {
        matches!(self, Ty::Str | Ty::List(_) | Ty::Record(_) | Ty::Sum(_))
    }
}

/// Renders types the way they are written in source, for diagnostics.
pub struct TyDisplay<'hir>(pub &'hir Ty, pub &'hir Hir);

impl fmt::Display for TyDisplay<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let hir = self.1;
        match self.0 {
            Ty::Int => write!(f, "int"),
            Ty::Float => write!(f, "float"),
            Ty::Bool => write!(f, "bool"),
            Ty::Str => write!(f, "string"),
            Ty::Unit => write!(f, "unit"),
            Ty::Never => write!(f, "never"),
            Ty::Error => write!(f, "<error>"),
            Ty::List(t) => write!(f, "List<{}>", TyDisplay(t, hir)),
            Ty::Option(t) => write!(f, "Option<{}>", TyDisplay(t, hir)),
            Ty::Result(t, e) => {
                write!(f, "Result<{}, {}>", TyDisplay(t, hir), TyDisplay(e, hir))
            }
            Ty::Record(id) => write!(f, "{}", hir.records[id.0 as usize].name),
            Ty::Sum(id) => write!(f, "{}", hir.sums[id.0 as usize].name),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Hir {
    pub records: Vec<RecordDef>,
    pub sums: Vec<SumDef>,
    pub funcs: Vec<Func>,
}

impl Hir {
    pub fn ty(&self, ty: &Ty) -> String {
        TyDisplay(ty, self).to_string()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RecordDef {
    pub name: String,
    pub fields: Vec<(String, Ty)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SumDef {
    pub name: String,
    pub variants: Vec<Variant>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Variant {
    pub name: String,
    pub fields: Vec<(String, Ty)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Func {
    pub name: String,
    pub ret: Ty,
    /// Slot types, parameters first. Slot `i` is WASM local `i`.
    pub locals: Vec<Ty>,
    pub param_count: usize,
    pub body: Block,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Block {
    pub stmts: Vec<Stmt>,
    pub tail: Option<Box<Expr>>,
    pub ty: Ty,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Stmt {
    Let { slot: u32, value: Expr },
    /// Only `var` locals are assignable in M1; records are immutable (§4.2).
    AssignLocal { slot: u32, value: Expr },
    Return(Option<Expr>),
    Expr(Expr),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Expr {
    pub ty: Ty,
    pub kind: ExprKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ExprKind {
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(String),
    Unit,
    Local(u32),

    Unary { op: UnOp, expr: Box<Expr> },
    Binary { op: BinOp, lhs: Box<Expr>, rhs: Box<Expr> },

    Call { func: FuncId, args: Vec<Expr> },

    MakeRecord { record: RecordId, fields: Vec<Expr> },
    /// Field index is resolved, so codegen just needs the offset.
    FieldGet { base: Box<Expr>, index: u32 },

    MakeVariant { sum: SumId, variant: u32, fields: Vec<Expr> },

    /// `Ok`/`Err`/`Some`. `None` carries no payload.
    MakeOk(Box<Expr>),
    MakeErr(Box<Expr>),
    MakeSome(Box<Expr>),
    MakeNone,

    If { cond: Box<Expr>, then_block: Block, else_block: Option<Box<Expr>> },
    Match { scrutinee: Box<Expr>, arms: Vec<Arm>, scrutinee_slot: u32 },
    Block(Block),

    /// `expr?` — on the error arm, returns from the enclosing function (§4.3).
    Try { expr: Box<Expr>, kind: TryKind },
}

/// Which discriminated shape a `?` is unwrapping. Determines what the early
/// return re-emits: `(1, payload)` for `Result`, `(1, 0)` for `Option`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TryKind {
    Result,
    Option,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Arm {
    pub pattern: Pattern,
    pub body: Expr,
}

/// Patterns keep their tree shape; codegen tests arms in order. Naive but
/// correct, and match arms are short in practice.
#[derive(Debug, Clone, PartialEq)]
pub enum Pattern {
    Wildcard,
    Bind { slot: u32 },
    Int(i64),
    Bool(bool),
    Str(String),
    /// Matches a tag, then its payload against `inner`.
    Tagged { tag: Tag, inner: Vec<Pattern> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tag {
    Ok,
    Err,
    Some,
    None,
    /// A user sum-type variant: `docs/abi.md` §3 boxes these.
    Variant { sum: SumId, index: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    AddInt,
    SubInt,
    MulInt,
    DivInt,
    RemInt,
    AddFloat,
    SubFloat,
    MulFloat,
    DivFloat,
    EqInt,
    NeInt,
    LtInt,
    LeInt,
    GtInt,
    GeInt,
    EqFloat,
    NeFloat,
    LtFloat,
    LeFloat,
    GtFloat,
    GeFloat,
    EqBool,
    NeBool,
    And,
    Or,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    NegInt,
    NegFloat,
    Not,
}
