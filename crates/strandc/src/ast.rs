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
}

/// An actor declaration (§5.1). Elm-shaped, as §6.5 describes: the handler
/// returns the next state rather than mutating the current one.
///
/// ```text
/// actor Counter {
///   state: Count
///   fn init(): Count { ... }
///   fn receive(state: Count, msg: string): Count { ... }
/// }
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct ActorDecl {
    pub name: String,
    pub state: TypeExpr,
    pub init: FnDecl,
    pub receive: FnDecl,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FnDecl {
    pub name: String,
    pub params: Vec<Param>,
    /// `None` means the function returns unit.
    pub ret: Option<TypeExpr>,
    pub body: Block,
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
    Let { name: String, ty: Option<TypeExpr>, value: Expr, mutable: bool, span: Span },
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
    /// `t.title`, and the receiver half of `list.push(x)`.
    Field { base: Box<Expr>, name: String, span: Span },

    /// `Todo { id: ..., title: ... }`
    RecordLit { name: Option<String>, fields: Vec<FieldInit>, span: Span },

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
            | Expr::Field { span, .. }
            | Expr::RecordLit { span, .. }
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
