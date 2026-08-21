//! Abstract syntax tree for the language subset built so far (§4.6).
//!
//! Every node carries a `Span` so the checker can point at source. Nodes for
//! later milestones (`scope`/`spawn`, `view`) are deliberately absent rather
//! than stubbed.

use crate::lexer::Span;

#[derive(Debug, Clone, PartialEq)]
pub struct Program {
    pub items: Vec<Item>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    Fn(FnDecl),
    Type(TypeDecl),
    Actor(ActorDecl),
    App(AppDecl),
}

/// An actor declaration (§5.1). Elm-shaped, as §6.5 describes: the handler
/// returns the next state rather than mutating the current one.
///
/// ```text
/// actor Counter {
///   state: Count
///   in  bumps: Bump
///   out total: Total
///   fn init(): Count { ... }
///   on bumps(state: Count, msg: Bump): Count { ... }
/// }
/// ```
///
/// An actor names its channels rather than holding addresses: `in` is what it
/// can be told, `out` is what it can say, and the `app` block decides which
/// out meets which in. Nothing in an actor knows who is on the other end,
/// which is what §9.5 means by location transparency — and why there is no
/// actor address anywhere in the language.
#[derive(Debug, Clone, PartialEq)]
pub struct ActorDecl {
    pub name: String,
    /// Just the name, where `span` covers the whole declaration.
    pub name_span: Span,
    pub state: TypeExpr,
    /// `in <name>: T` — a channel this actor receives on. Each needs an `on`
    /// handler of the same name.
    pub inbox: Vec<Port>,
    /// `out <name>: T` — a channel this actor sends on, via `send(<name>, v)`.
    pub outbox: Vec<Port>,
    pub init: FnDecl,
    /// `on <port>(state, msg): State`, one per `in` port. The `FnDecl`'s name
    /// is the port's.
    pub handlers: Vec<FnDecl>,
    /// `view fn view(state) -> Node`, when the actor draws itself (§6.2).
    pub view: Option<FnDecl>,
    pub span: Span,
}

/// One `in` or `out` declaration.
#[derive(Debug, Clone, PartialEq)]
pub struct Port {
    pub name: String,
    pub name_span: Span,
    pub ty: TypeExpr,
    pub span: Span,
}

/// `app Name { ... }` — the supervision tree (§7), written down.
///
/// ```text
/// app Todo {
///   ui    = TodoUi
///   stats = Stats
///   ui.stats     -> stats.commands
///   stats.tally  -> ui.tally
/// }
/// ```
///
/// It is in the source rather than in a config file because the wiring is
/// typed and the compiler is the thing that can check it (§8.1 asks for zero
/// config files, and a wiring the compiler cannot see is a wiring that fails
/// at run time).
#[derive(Debug, Clone, PartialEq)]
pub struct AppDecl {
    pub name: String,
    pub name_span: Span,
    pub instances: Vec<Instance>,
    pub wires: Vec<WireDecl>,
    pub span: Span,
}

/// `ui = TodoUi` — one running actor, and the name the wires call it by.
#[derive(Debug, Clone, PartialEq)]
pub struct Instance {
    pub name: String,
    pub name_span: Span,
    pub actor: String,
    pub actor_span: Span,
    pub span: Span,
}

/// `ui.stats -> stats.commands`.
#[derive(Debug, Clone, PartialEq)]
pub struct WireDecl {
    pub from: PortRef,
    pub to: PortRef,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PortRef {
    pub instance: String,
    pub instance_span: Span,
    pub port: String,
    pub port_span: Span,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FnDecl {
    pub name: String,
    /// Just the name, where `span` covers the whole declaration.
    pub name_span: Span,
    pub params: Vec<Param>,
    /// `None` means the function returns unit.
    pub ret: Option<TypeExpr>,
    pub body: Block,
    /// Written `view fn` (§6.2). A view returns a `Node`, may use the builder
    /// DSL, and is the only place that may — which is what lets a node be
    /// emitted exactly where it is written.
    pub is_view: bool,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Param {
    pub name: String,
    pub ty: TypeExpr,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TypeDecl {
    pub name: String,
    /// Just the name, where `span` covers the whole declaration.
    pub name_span: Span,
    pub def: TypeDef,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TypeDef {
    /// `type Todo = { id: Id, title: string }`
    Record(Vec<FieldDef>),
    /// `type AddError = | EmptyTitle | TooLong(max: int)`
    Sum(Vec<VariantDef>),
    /// `type Id = int`
    Alias(TypeExpr),
}

#[derive(Debug, Clone, PartialEq)]
pub struct FieldDef {
    pub name: String,
    pub ty: TypeExpr,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct VariantDef {
    pub name: String,
    /// Empty for niladic variants like `EmptyTitle`.
    pub fields: Vec<FieldDef>,
    pub span: Span,
}

/// A type as written in source, before resolution.
#[derive(Debug, Clone, PartialEq)]
pub enum TypeExpr {
    /// `int`, `List<Todo>`, `Result<T, E>`
    Named { name: String, args: Vec<TypeExpr>, span: Span },
    /// `string?` — sugar for `Option<string>` (§4.2).
    Optional { inner: Box<TypeExpr>, span: Span },
    /// `fn(Id)` — used by callback props.
    Fn { params: Vec<TypeExpr>, ret: Option<Box<TypeExpr>>, span: Span },
}

impl TypeExpr {
    pub fn span(&self) -> Span {
        match self {
            TypeExpr::Named { span, .. }
            | TypeExpr::Optional { span, .. }
            | TypeExpr::Fn { span, .. } => *span,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Block {
    pub stmts: Vec<Stmt>,
    /// Trailing expression, if the block ends in one without a `;`.
    pub tail: Option<Box<Expr>>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Stmt {
    /// `let x = e` (immutable) or `var x = e` (mutable), per §4.2.
    Let {
        name: String,
        /// Just the name, where `span` covers the whole statement. Editors jump
        /// to and rename the name alone.
        name_span: Span,
        ty: Option<TypeExpr>,
        value: Expr,
        mutable: bool,
        span: Span,
    },
    /// `state.todos = next`
    Assign { target: Expr, value: Expr, span: Span },
    Return { value: Option<Expr>, span: Span },
    Expr(Expr),
}

#[derive(Debug, Clone, PartialEq, Eq, Copy)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
}

impl BinOp {
    pub fn as_str(self) -> &'static str {
        match self {
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
            BinOp::Rem => "%",
            BinOp::Eq => "==",
            BinOp::Ne => "!=",
            BinOp::Lt => "<",
            BinOp::Le => "<=",
            BinOp::Gt => ">",
            BinOp::Ge => ">=",
            BinOp::And => "&&",
            BinOp::Or => "||",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Copy)]
pub enum UnOp {
    Neg,
    Not,
}

/// A call argument, optionally labelled: `TooLong(max: 200)`, `row(gap: 8)`.
#[derive(Debug, Clone, PartialEq)]
pub struct Arg {
    pub name: Option<String>,
    pub value: Expr,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Int { value: i64, span: Span },
    Float { value: f64, span: Span },
    Bool { value: bool, span: Span },
    Str { value: String, span: Span },
    Ident { name: String, span: Span },

    Unary { op: UnOp, expr: Box<Expr>, span: Span },
    Binary { op: BinOp, lhs: Box<Expr>, rhs: Box<Expr>, span: Span },

    Call { callee: Box<Expr>, args: Vec<Arg>, span: Span },
    /// §6.2's builder call: `column(gap: 4) { ... }`.
    ///
    /// A separate node rather than a `Call` with a block argument, because the
    /// children are not an argument: they are a scope whose contents become
    /// this node's children in the order they are written.
    Build {
        name: String,
        /// Where the builder's name was written. A builder has no declaration
        /// to point at, so this is what hover attaches its signature to.
        name_span: Span,
        args: Vec<Arg>,
        children: Option<Block>,
        span: Span,
    },
    /// `t.title`, and the receiver half of `list.push(x)`.
    Field { base: Box<Expr>, name: String, span: Span },

    /// `Todo { id: ..., title: ... }`, and `Todo { ...t, done: true }`.
    ///
    /// `base` is the spread: every field the literal does not set is taken
    /// from it. §4.2 makes records immutable, so an update *is* a whole new
    /// record — the sugar says which fields differ instead of restating the
    /// ones that do not.
    RecordLit {
        name: Option<String>,
        base: Option<Box<Expr>>,
        fields: Vec<FieldInit>,
        span: Span,
    },

    /// `[a, b, c]`. An empty one takes its element type from context.
    ListLit { items: Vec<Expr>, span: Span },

    /// `for t in todos { ... }`.
    ///
    /// An expression rather than a statement, so it can stand among a
    /// container's children (§6.2) exactly the way an `if` does. Its value is
    /// unit; what it produces, it produces by running.
    For {
        name: String,
        name_span: Span,
        iter: Box<Expr>,
        body: Block,
        span: Span,
    },

    If { cond: Box<Expr>, then_block: Block, else_block: Option<Box<Expr>>, span: Span },
    Match { scrutinee: Box<Expr>, arms: Vec<MatchArm>, span: Span },
    Block(Block),

    /// `expr?` — propagates Err/None to the caller (§4.3).
    Try { expr: Box<Expr>, span: Span },
}

impl Expr {
    pub fn span(&self) -> Span {
        match self {
            Expr::Int { span, .. }
            | Expr::Float { span, .. }
            | Expr::Bool { span, .. }
            | Expr::Str { span, .. }
            | Expr::Ident { span, .. }
            | Expr::Unary { span, .. }
            | Expr::Binary { span, .. }
            | Expr::Call { span, .. }
            | Expr::Build { span, .. }
            | Expr::Field { span, .. }
            | Expr::RecordLit { span, .. }
            | Expr::ListLit { span, .. }
            | Expr::For { span, .. }
            | Expr::If { span, .. }
            | Expr::Match { span, .. }
            | Expr::Try { span, .. } => *span,
            Expr::Block(block) => block.span,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct FieldInit {
    pub name: String,
    /// `None` for shorthand `Todo { title }`, where the value is `title`.
    pub value: Option<Expr>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MatchArm {
    pub pattern: Pattern,
    pub body: Expr,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Pattern {
    /// `_`
    Wildcard { span: Span },
    /// `next` — binds the whole scrutinee.
    Binding { name: String, span: Span },
    /// `Ok(next)`, `Err(TooLong(max))`, `EmptyTitle`
    Ctor { name: String, args: Vec<Pattern>, span: Span },
    Int { value: i64, span: Span },
    Bool { value: bool, span: Span },
    Str { value: String, span: Span },
}

impl Pattern {
    pub fn span(&self) -> Span {
        match self {
            Pattern::Wildcard { span }
            | Pattern::Binding { span, .. }
            | Pattern::Ctor { span, .. }
            | Pattern::Int { span, .. }
            | Pattern::Bool { span, .. }
            | Pattern::Str { span, .. } => *span,
        }
    }
}
