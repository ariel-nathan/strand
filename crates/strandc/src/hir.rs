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
    /// Every actor the file declares. The runtime instantiates one Store per
    /// actor, so an app's actors are separate arenas whether or not they were
    /// written in the same file — the file is a unit of *source*, and §5.1's
    /// unit of isolation is the instance.
    pub actors: Vec<ActorInfo>,
    /// `app Name { ... }`: which actors run, and which port meets which.
    pub app: Option<AppInfo>,
}

/// What codegen needs to wire an actor to the host ABI.
#[derive(Debug, Clone, PartialEq)]
pub struct ActorInfo {
    pub name: String,
    pub state: Ty,
    /// Channels this actor receives on. The index *is* the port number on the
    /// wire, which is what `strand_on_message` dispatches on.
    pub inbox: Vec<PortInfo>,
    /// Channels it sends on, indexed the same way for `strand.send`.
    pub outbox: Vec<PortInfo>,
    pub init: FuncId,
    /// One handler per inbox port, in the same order.
    pub handlers: Vec<FuncId>,
    /// `view fn view(state) -> Node`, when the actor declares one. Its presence
    /// is what makes a module a UI actor (§6.5).
    pub view: Option<FuncId>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PortInfo {
    pub name: String,
    /// The payload type. Flat, per `docs/abi.md` §7.
    pub ty: Ty,
}

/// The supervision tree (§7), as the compiler resolved it.
#[derive(Debug, Clone, PartialEq)]
pub struct AppInfo {
    pub name: String,
    pub instances: Vec<InstanceInfo>,
    pub wires: Vec<Wire>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct InstanceInfo {
    /// What the wires call it.
    pub name: String,
    /// Index into `Hir::actors`.
    pub actor: usize,
}

/// One connection: an out port on one instance feeding an in port on another.
///
/// Indices rather than names, because by this point both ends have been
/// resolved and checked — a wire that survives here cannot be dangling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Wire {
    pub from: usize,
    pub from_port: usize,
    pub to: usize,
    pub to_port: usize,
}

impl Hir {
    pub fn ty(&self, ty: &Ty) -> String {
        TyDisplay(ty, self).to_string()
    }

    /// The actor, for the paths that only ever deal with one — `strand view`
    /// on a single-actor file, and the checks in front of it.
    ///
    /// Deliberately `None` rather than "the first" when a file declares
    /// several: a caller that has not said which one it means is a caller
    /// about to run the wrong actor.
    pub fn lone_actor(&self) -> Option<&ActorInfo> {
        match self.actors.as_slice() {
            [only] => Some(only),
            _ => None,
        }
    }
}

impl ActorInfo {
    pub fn in_port(&self, name: &str) -> Option<usize> {
        self.inbox.iter().position(|p| p.name == name)
    }

    pub fn out_port(&self, name: &str) -> Option<usize> {
        self.outbox.iter().position(|p| p.name == name)
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
    /// A call into a generated WASM helper (`stdlib`). Unlike a builtin these
    /// never leave the actor, so a module full of them still imports nothing.
    CallHelper { helper: Helper, args: Vec<Expr> },

    MakeRecord { record: RecordId, fields: Vec<Expr> },
    /// Field index is resolved, so codegen just needs the offset.
    FieldGet { base: Box<Expr>, index: u32 },

    MakeVariant { sum: SumId, variant: u32, fields: Vec<Expr> },

    /// `[a, b, c]`. The element type is on the expression itself.
    MakeList { elems: Vec<Expr> },
    /// How many elements. Read straight out of the header.
    ListLen { list: Box<Expr> },
    /// `push(list, value)` — a *new* list one longer. §4.2 makes data
    /// immutable, so appending copies; at POC sizes that is the honest trade,
    /// and the alternative is a growable buffer with a capacity nobody can see.
    ListPush { list: Box<Expr>, value: Box<Expr> },
    /// `for x in list { ... }`. Yields unit: what it produces, it produces by
    /// running — which is exactly what lets it stand among a view's children.
    For { slot: u32, list: Box<Expr>, body: Block },

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

    /// `send(port, value)`. `port` is an index into the actor's `outbox`,
    /// resolved by the checker — the emitted call carries a number, and the
    /// name never leaves the compiler.
    Send { port: u32, value: Box<Expr> },

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

/// A function the emitter generates into the module itself.
///
/// Emitted only where used, so a program that never touches a string carries
/// none of them. Ordering here is the order they are laid out in, which is why
/// it is an enum rather than a name: an index has to be computable before any
/// body is emitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Helper {
    /// `a + b` on strings.
    StrConcat,
    /// The decimal form of an `int`.
    StrFromInt,
    /// One character, UTF-8 encoded, from a Unicode scalar value.
    StrFromChar,
    /// Characters, not bytes.
    StrCharCount,
    /// The string without its last character.
    StrDropLast,
    /// The string without surrounding whitespace.
    StrTrim,
}

impl Helper {
    pub fn name(self) -> &'static str {
        match self {
            Helper::StrConcat => "str_concat",
            Helper::StrFromInt => "str_from_int",
            Helper::StrFromChar => "str_from_char",
            Helper::StrCharCount => "str_char_count",
            Helper::StrDropLast => "str_drop_last",
            Helper::StrTrim => "str_trim",
        }
    }
}

/// Host functions callable from Strand (`docs/abi.md` §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Builtin {
    /// `log(msg: string)` — writes through to the runtime's actor log.
    Log,
    /// `send(port, ptr, len)` — hands bytes to whatever the `app` block wired
    /// this port to. The guest names a port; the host knows the destination.
    Send,
    /// `panic(msg: string)` — §4.3's second tier. Ends this actor and nothing
    /// else, with the message as the crash report's reason.
    Panic,
}

impl Builtin {
    pub fn name(self) -> &'static str {
        match self {
            Builtin::Log => "log",
            Builtin::Send => "send",
            Builtin::Panic => "panic",
        }
    }

    /// The `(module, field)` pair this import is linked against.
    pub fn import(self) -> (&'static str, &'static str) {
        match self {
            Builtin::Log => ("strand", "log"),
            Builtin::Send => ("strand", "send"),
            Builtin::Panic => ("strand", "panic"),
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
