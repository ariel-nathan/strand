//! Typed IR produced by the checker and consumed by the WASM emitter.
//!
//! The checker resolves names, assigns local slots, and picks the
//! representation for every value, so codegen never has to ask a question the
//! type system already answered. Layout rules live in `docs/abi.md`.

use std::fmt;

use crate::ui::{NodeKind, Slot};

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
    /// A built UI node (§6.2).
    ///
    /// Zero-width: a node is *emitted* into the frame's array where it is
    /// written, so the value left behind carries nothing. That is not a trick
    /// to save a word — it is why a node cannot be stored, passed around, or
    /// used twice, and therefore why the array is in tree order by
    /// construction rather than by discipline.
    Node,
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

    /// Combines two compatible types into the most-defined one, filling
    /// `Error` holes from the other side. `Ok(1)` types as
    /// `Result<int, Error>` and `Err(Bad)` as `Result<Error, E>`; joining the
    /// branches of an `if` recovers the full `Result<int, E>` that codegen
    /// needs in order to know the payload's representation.
    pub fn join(&self, other: &Ty) -> Ty {
        match (self, other) {
            (Ty::Error | Ty::Never, filled) | (filled, Ty::Error | Ty::Never) => filled.clone(),
            (Ty::List(a), Ty::List(b)) => Ty::List(Box::new(a.join(b))),
            (Ty::Option(a), Ty::Option(b)) => Ty::Option(Box::new(a.join(b))),
            (Ty::Result(a1, e1), Ty::Result(a2, e2)) => {
                Ty::Result(Box::new(a1.join(a2)), Box::new(e1.join(e2)))
            }
            _ => self.clone(),
        }
    }

    /// Whether any part of this type is still unknown. Such a type cannot be
    /// given a representation, so codegen rejects it rather than guessing.
    pub fn has_holes(&self) -> bool {
        match self {
            Ty::Error => true,
            Ty::List(t) | Ty::Option(t) => t.has_holes(),
            Ty::Result(t, e) => t.has_holes() || e.has_holes(),
            _ => false,
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
            Ty::Node => write!(f, "Node"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Hir {
    pub records: Vec<RecordDef>,
    pub sums: Vec<SumDef>,
    pub funcs: Vec<Func>,
    /// At most one per module: the runtime instantiates one module per actor.
    pub actor: Option<ActorInfo>,
}

/// What codegen needs to wire an actor to the host ABI.
#[derive(Debug, Clone, PartialEq)]
pub struct ActorInfo {
    pub name: String,
    pub state: Ty,
    /// The channel's payload type, checked against `receive` (§5.3).
    pub message: Ty,
    pub init: FuncId,
    pub receive: FuncId,
    /// `view fn view(state) -> Node`, when the actor declares one. Its presence
    /// is what makes a module a UI actor (§6.5).
    pub view: Option<FuncId>,
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
    /// Written `view fn` (§6.2): pure, returns a `Node`, and the only kind of
    /// function that may build one.
    pub is_view: bool,
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
    /// A call into the host ABI. Imports occupy the low function indices, so
    /// codegen keeps these separate from ordinary calls.
    CallBuiltin { builtin: Builtin, args: Vec<Expr> },

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

    /// §6.2's builder call: append one node to the frame's array, after
    /// evaluating `children` so that whatever they appended becomes its own.
    ///
    /// `props` keeps source order so evaluation does too; every slot the source
    /// left out takes the builder's default, which the checker has already
    /// folded into `numbers` (the two float slots) or is simply zero.
    MakeNode {
        kind: NodeKind,
        props: Vec<(Slot, Expr)>,
        /// Defaults for `Slot::Number` and `Slot::Number2`.
        numbers: [f32; 2],
        /// Empty for a leaf.
        children: Block,
    },
}

/// Host functions callable from Strand (`docs/abi.md` §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Builtin {
    /// `log(msg: string)` — writes through to the runtime's actor log.
    Log,
}

impl Builtin {
    pub fn name(self) -> &'static str {
        match self {
            Builtin::Log => "log",
        }
    }

    /// The `(module, field)` pair this import is linked against.
    pub fn import(self) -> (&'static str, &'static str) {
        match self {
            Builtin::Log => ("strand", "log"),
        }
    }
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
