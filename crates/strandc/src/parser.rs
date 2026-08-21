//! Recursive-descent parser (§4.6).
//!
//! Semicolons are optional: §4.5 writes none. A block's last expression is its
//! tail value unless it is followed by `;`.

use crate::ast::*;
use crate::diag::Diagnostic;
use crate::lexer::{lex, Span, Tok, Token};
use crate::ui::{is_builder, takes_children};

pub fn parse(src: &str) -> Result<Program, Diagnostic> {
    let tokens = lex(src)?;
    Parser { tokens, pos: 0, no_record_literal: false }.program()
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    /// Set while parsing an `if`/`match` head. Without it, `if a > b { a }`
    /// reads `b { a }` as a record literal with a shorthand field — the same
    /// ambiguity Rust resolves with a no-struct-literal restriction.
    no_record_literal: bool,
}

type PResult<T> = Result<T, Diagnostic>;

impl Parser {
    fn peek(&self) -> &Tok {
        &self.tokens[self.pos].tok
    }

    fn peek_at(&self, n: usize) -> &Tok {
        let i = (self.pos + n).min(self.tokens.len() - 1);
        &self.tokens[i].tok
    }

    fn span(&self) -> Span {
        self.tokens[self.pos].span
    }

    fn prev_span(&self) -> Span {
        self.tokens[self.pos.saturating_sub(1)].span
    }

    fn at(&self, tok: &Tok) -> bool {
        self.peek() == tok
    }

    fn advance(&mut self) -> Tok {
        let tok = self.tokens[self.pos].tok.clone();
        if self.pos + 1 < self.tokens.len() {
            self.pos += 1;
        }
        tok
    }

    fn eat(&mut self, tok: &Tok) -> bool {
        if self.at(tok) {
            self.advance();
            true
        } else {
            false
        }
    }

    /// Parses `inner` with record literals allowed again — inside parentheses
    /// or braces the ambiguity cannot arise.
    fn allowing_records<T>(&mut self, inner: impl FnOnce(&mut Self) -> PResult<T>) -> PResult<T> {
        let saved = std::mem::replace(&mut self.no_record_literal, false);
        let result = inner(self);
        self.no_record_literal = saved;
        result
    }

    /// Parses the head of an `if`/`match`, where a `{` always opens a block.
    fn head_expr(&mut self) -> PResult<Expr> {
        let saved = std::mem::replace(&mut self.no_record_literal, true);
        let result = self.expr();
        self.no_record_literal = saved;
        result
    }

    fn error<T>(&self, message: impl Into<String>) -> PResult<T> {
        Err(Diagnostic::new(self.span(), message).with_label("unexpected"))
    }

    fn expect(&mut self, tok: Tok) -> PResult<Span> {
        if self.at(&tok) {
            let span = self.span();
            self.advance();
            Ok(span)
        } else {
            let found = self.peek().clone();
            Err(Diagnostic::new(self.span(), format!("expected {tok}, found {found}"))
                .with_label(format!("expected {tok}"))
                .with_help(format!("insert {tok} here")))
        }
    }

    fn expect_ident(&mut self) -> PResult<(String, Span)> {
        let span = self.span();
        match self.peek().clone() {
            Tok::Ident(name) => {
                self.advance();
                Ok((name, span))
            }
            other => self.error(format!("expected an identifier, found {other}")),
        }
    }

    /// Joins two spans so a node covers its full source range.
    fn join(start: Span, end: Span) -> Span {
        Span { start: start.start, end: end.end, line: start.line, col: start.col }
    }

    // ---- items -----------------------------------------------------------

    fn program(mut self) -> PResult<Program> {
        let mut items = Vec::new();
        while !self.at(&Tok::Eof) {
            items.push(self.item()?);
        }
        Ok(Program { items })
    }

    fn item(&mut self) -> PResult<Item> {
        match self.peek() {
            Tok::Fn | Tok::View => Ok(Item::Fn(self.fn_decl()?)),
            Tok::Type => Ok(Item::Type(self.type_decl()?)),
            Tok::Actor => Ok(Item::Actor(self.actor_decl()?)),
            other => self.error(format!("expected `fn`, `type` or `actor`, found {other}")),
        }
    }

    fn fn_decl(&mut self) -> PResult<FnDecl> {
        // `view fn name(...)` (§6.2). The keyword leads, so the reader knows
        // what kind of function this is before reading its name.
        let is_view = self.at(&Tok::View);
        let start = if is_view {
            let span = self.expect(Tok::View)?;
            self.expect(Tok::Fn)?;
            span
        } else {
            self.expect(Tok::Fn)?
        };

        // `view fn view(...)` is the obvious thing to reach for and does not
        // work, because `view` is a keyword now. Saying which name is taken
        // beats "expected an identifier".
        if self.at(&Tok::View) {
            return Err(Diagnostic::new(self.span(), "`view` is a keyword, not a name")
                .with_label("reserved")
                .with_help(
                    "name it for what it draws — `view fn todoList(...)`. Inside \
                     an actor, the `view fn` is the actor's view whatever it is \
                     called, so the name is free.",
                ));
        }
        let (name, _) = self.expect_ident()?;

        self.expect(Tok::LParen)?;
        let mut params = Vec::new();
        while !self.at(&Tok::RParen) {
            let (pname, pspan) = self.expect_ident()?;
            self.expect(Tok::Colon)?;
            let ty = self.type_expr()?;
            params.push(Param { name: pname, span: Self::join(pspan, ty.span()), ty });
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.expect(Tok::RParen)?;

        // `: T` per §4.5; `-> T` also accepted since §6.2 writes views that way.
        let ret = if self.eat(&Tok::Colon) || self.eat(&Tok::Arrow) {
            Some(self.type_expr()?)
        } else {
            None
        };

        let body = self.block()?;
        Ok(FnDecl { name, params, ret, is_view, span: Self::join(start, body.span), body })
    }

    fn actor_decl(&mut self) -> PResult<ActorDecl> {
        let start = self.expect(Tok::Actor)?;
        let (name, _) = self.expect_ident()?;
        self.expect(Tok::LBrace)?;

        // `state: T` — the record this actor owns.
        let (keyword, keyword_span) = self.expect_ident()?;
        if keyword != "state" {
            return Err(Diagnostic::new(keyword_span, format!("expected `state`, found `{keyword}`"))
                .with_label("actors declare their state first")
                .with_help("write `state: SomeRecord` as the first line of the actor"));
        }
        self.expect(Tok::Colon)?;
        let state = self.type_expr()?;

        // Optional `message: T`; without it the channel carries strings.
        let mut message = None;
        if let (Tok::Ident(word), Tok::Colon) = (self.peek(), self.peek_at(1)) {
            if word == "message" {
                self.advance();
                self.advance();
                message = Some(self.type_expr()?);
            }
        }

        let mut init = None;
        let mut receive = None;
        let mut view = None;
        while self.at(&Tok::Fn) || self.at(&Tok::View) {
            let decl = self.fn_decl()?;
            // A `view fn` is the actor's view whatever it is called: the
            // keyword already says what it is, so the name is free to say what
            // it draws.
            if decl.is_view {
                if let Some(first) = &view {
                    let first: &FnDecl = first;
                    return Err(Diagnostic::new(
                        decl.span,
                        format!("`{}` already draws this actor", first.name),
                    )
                    .with_label("a second view")
                    .with_help(
                        "an actor has one view; break the rest out as `view fn`                          items outside the actor and call them as children",
                    ));
                }
                view = Some(decl);
                continue;
            }
            match decl.name.as_str() {
                "init" => init = Some(decl),
                "receive" => receive = Some(decl),
                other => {
                    return Err(Diagnostic::new(decl.span, format!("unexpected function `{other}`"))
                        .with_label("not part of an actor")
                        .with_help(
                            "an actor declares `init` and `receive`, and optionally a                              `view fn` to draw itself",
                        ));
                }
            }
        }
        let end = self.expect(Tok::RBrace)?;

        let (Some(init), Some(receive)) = (init, receive) else {
            return Err(Diagnostic::new(Self::join(start, end), format!("actor `{name}` is incomplete"))
                .with_label("missing `init` or `receive`")
                .with_help("an actor needs `fn init()` for its starting state and `fn receive(state, msg)` for each message"));
        };

        Ok(ActorDecl { name, state, message, init, receive, view, span: Self::join(start, end) })
    }

    fn type_decl(&mut self) -> PResult<TypeDecl> {
        let start = self.expect(Tok::Type)?;
        let (name, _) = self.expect_ident()?;
        self.expect(Tok::Eq)?;

        let def = if self.at(&Tok::LBrace) {
            TypeDef::Record(self.field_defs()?)
        } else if self.at(&Tok::Pipe) {
            TypeDef::Sum(self.variant_defs()?)
        } else {
            TypeDef::Alias(self.type_expr()?)
        };

        Ok(TypeDecl { name, def, span: Self::join(start, self.prev_span()) })
    }

    fn field_defs(&mut self) -> PResult<Vec<FieldDef>> {
        self.expect(Tok::LBrace)?;
        let mut fields = Vec::new();
        while !self.at(&Tok::RBrace) {
            let (name, span) = self.expect_ident()?;
            self.expect(Tok::Colon)?;
            let ty = self.type_expr()?;
            fields.push(FieldDef { name, span: Self::join(span, ty.span()), ty });
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.expect(Tok::RBrace)?;
        Ok(fields)
    }

    fn variant_defs(&mut self) -> PResult<Vec<VariantDef>> {
        let mut variants = Vec::new();
        while self.eat(&Tok::Pipe) {
            let (name, span) = self.expect_ident()?;
            let mut fields = Vec::new();
            if self.eat(&Tok::LParen) {
                while !self.at(&Tok::RParen) {
                    let (fname, fspan) = self.expect_ident()?;
                    self.expect(Tok::Colon)?;
                    let ty = self.type_expr()?;
                    fields.push(FieldDef { name: fname, span: Self::join(fspan, ty.span()), ty });
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
                self.expect(Tok::RParen)?;
            }
            variants.push(VariantDef { name, fields, span: Self::join(span, self.prev_span()) });
        }
        if variants.is_empty() {
            return self.error("expected at least one `| Variant`");
        }
        Ok(variants)
    }

    // ---- types -----------------------------------------------------------

    fn type_expr(&mut self) -> PResult<TypeExpr> {
        let start = self.span();

        let mut ty = if self.eat(&Tok::Fn) {
            self.expect(Tok::LParen)?;
            let mut params = Vec::new();
            while !self.at(&Tok::RParen) {
                params.push(self.type_expr()?);
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
            self.expect(Tok::RParen)?;
            let ret = if self.eat(&Tok::Arrow) || self.eat(&Tok::Colon) {
                Some(Box::new(self.type_expr()?))
            } else {
                None
            };
            TypeExpr::Fn { params, ret, span: Self::join(start, self.prev_span()) }
        } else {
            let (name, span) = self.expect_ident()?;
            let mut args = Vec::new();
            if self.eat(&Tok::Lt) {
                while !self.at(&Tok::Gt) {
                    args.push(self.type_expr()?);
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
                self.expect(Tok::Gt)?;
            }
            TypeExpr::Named { name, args, span: Self::join(span, self.prev_span()) }
        };

        // `string?` ≡ `Option<string>` (§4.2), and it may stack.
        while self.at(&Tok::Question) {
            let q = self.span();
            self.advance();
            ty = TypeExpr::Optional { inner: Box::new(ty), span: Self::join(start, q) };
        }
        Ok(ty)
    }

    // ---- statements ------------------------------------------------------

    fn block(&mut self) -> PResult<Block> {
        let saved = std::mem::replace(&mut self.no_record_literal, false);
        let block = self.block_inner();
        self.no_record_literal = saved;
        block
    }

    fn block_inner(&mut self) -> PResult<Block> {
        let start = self.expect(Tok::LBrace)?;
        let mut stmts = Vec::new();
        let mut tail = None;

        while !self.at(&Tok::RBrace) && !self.at(&Tok::Eof) {
            // A bare expression is the block's tail only if it is last and not
            // terminated by `;`.
            let stmt = self.stmt()?;
            if let Stmt::Expr(expr) = stmt {
                let terminated = self.eat(&Tok::Semi);
                if !terminated && self.at(&Tok::RBrace) {
                    tail = Some(Box::new(expr));
                    break;
                }
                stmts.push(Stmt::Expr(expr));
            } else {
                self.eat(&Tok::Semi);
                stmts.push(stmt);
            }
        }

        let end = self.expect(Tok::RBrace)?;
        Ok(Block { stmts, tail, span: Self::join(start, end) })
    }

    fn stmt(&mut self) -> PResult<Stmt> {
        match self.peek() {
            Tok::Let | Tok::Var => {
                let mutable = matches!(self.peek(), Tok::Var);
                let start = self.span();
                self.advance();
                let (name, _) = self.expect_ident()?;
                let ty = if self.eat(&Tok::Colon) { Some(self.type_expr()?) } else { None };
                self.expect(Tok::Eq)?;
                let value = self.expr()?;
                Ok(Stmt::Let {
                    name,
                    ty,
                    span: Self::join(start, value.span()),
                    value,
                    mutable,
                })
            }
            Tok::Return => {
                let start = self.span();
                self.advance();
                // `return` with no value, when the block ends or a `;` follows.
                let value = if self.at(&Tok::RBrace) || self.at(&Tok::Semi) {
                    None
                } else {
                    Some(self.expr()?)
                };
                let end = value.as_ref().map_or(start, |v| v.span());
                Ok(Stmt::Return { value, span: Self::join(start, end) })
            }
            _ => {
                let expr = self.expr()?;
                if self.at(&Tok::Eq) {
                    self.advance();
                    let value = self.expr()?;
                    return Ok(Stmt::Assign {
                        span: Self::join(expr.span(), value.span()),
                        target: expr,
                        value,
                    });
                }
                Ok(Stmt::Expr(expr))
            }
        }
    }

    // ---- expressions -----------------------------------------------------

    fn expr(&mut self) -> PResult<Expr> {
        self.binary(0)
    }

    /// Precedence climbing. Lower binding power binds looser.
    fn binary(&mut self, min_bp: u8) -> PResult<Expr> {
        let mut lhs = self.unary()?;

        loop {
            let Some((op, bp)) = binop_of(self.peek()) else { break };
            if bp < min_bp {
                break;
            }
            self.advance();
            // All binary operators here are left-associative.
            let rhs = self.binary(bp + 1)?;
            lhs = Expr::Binary {
                op,
                span: Self::join(lhs.span(), rhs.span()),
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    fn unary(&mut self) -> PResult<Expr> {
        let start = self.span();
        let op = match self.peek() {
            Tok::Minus => Some(UnOp::Neg),
            Tok::Bang => Some(UnOp::Not),
            _ => None,
        };
        if let Some(op) = op {
            self.advance();
            let expr = self.unary()?;
            return Ok(Expr::Unary {
                op,
                span: Self::join(start, expr.span()),
                expr: Box::new(expr),
            });
        }
        self.postfix()
    }

    fn postfix(&mut self) -> PResult<Expr> {
        let mut expr = self.primary()?;
        loop {
            match self.peek() {
                Tok::LParen => {
                    self.advance();
                    let args = self.allowing_records(|p| p.call_args())?;
                    let end = self.expect(Tok::RParen)?;

                    // §6.2's trailing block. A block attaches only to a name
                    // this parser already knows is a builder, which is what
                    // keeps `foo()` followed by a block unambiguous without a
                    // no-block-here restriction of the kind `if` needs.
                    if let Expr::Ident { name, .. } = &expr {
                        if is_builder(name) {
                            let name = name.clone();
                            let children = if takes_children(&name) && self.at(&Tok::LBrace) {
                                Some(self.block()?)
                            } else {
                                None
                            };
                            let end = children.as_ref().map_or(end, |b| b.span);
                            expr = Expr::Build {
                                span: Self::join(expr.span(), end),
                                name,
                                args,
                                children,
                            };
                            continue;
                        }
                    }

                    expr = Expr::Call {
                        span: Self::join(expr.span(), end),
                        callee: Box::new(expr),
                        args,
                    };
                }
                Tok::Dot => {
                    self.advance();
                    let (name, span) = self.expect_ident()?;
                    expr = Expr::Field {
                        span: Self::join(expr.span(), span),
                        base: Box::new(expr),
                        name,
                    };
                }
                Tok::Question => {
                    let q = self.span();
                    self.advance();
                    expr = Expr::Try {
                        span: Self::join(expr.span(), q),
                        expr: Box::new(expr),
                    };
                }
                _ => return Ok(expr),
            }
        }
    }

    fn call_args(&mut self) -> PResult<Vec<Arg>> {
        let mut args = Vec::new();
        while !self.at(&Tok::RParen) {
            let start = self.span();
            // `name: value` is a labelled argument; a bare `name` is a value.
            let name = match (self.peek(), self.peek_at(1)) {
                (Tok::Ident(n), Tok::Colon) => {
                    let n = n.clone();
                    self.advance();
                    self.advance();
                    Some(n)
                }
                _ => None,
            };
            let value = self.expr()?;
            args.push(Arg { name, span: Self::join(start, value.span()), value });
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        Ok(args)
    }

    fn primary(&mut self) -> PResult<Expr> {
        let span = self.span();
        match self.peek().clone() {
            Tok::Int(value) => {
                self.advance();
                Ok(Expr::Int { value, span })
            }
            Tok::Float(value) => {
                self.advance();
                Ok(Expr::Float { value, span })
            }
            Tok::Str(value) => {
                self.advance();
                Ok(Expr::Str { value, span })
            }
            Tok::True => {
                self.advance();
                Ok(Expr::Bool { value: true, span })
            }
            Tok::False => {
                self.advance();
                Ok(Expr::Bool { value: false, span })
            }
            Tok::LParen => {
                self.advance();
                let inner = self.allowing_records(|p| p.expr())?;
                self.expect(Tok::RParen)?;
                Ok(inner)
            }
            Tok::LBrace => Ok(Expr::Block(self.block()?)),
            Tok::If => self.if_expr(),
            Tok::Match => self.match_expr(),
            Tok::Ident(name) => {
                self.advance();
                // `Todo { ... }` is a record literal; a bare `{` after an
                // identifier is never a block in expression position.
                if !self.no_record_literal && self.at(&Tok::LBrace) && starts_record_literal(self) {
                    let fields = self.field_inits()?;
                    return Ok(Expr::RecordLit {
                        name: Some(name),
                        fields,
                        span: Self::join(span, self.prev_span()),
                    });
                }
                Ok(Expr::Ident { name, span })
            }
            other => self.error(format!("expected an expression, found {other}")),
        }
    }

    fn field_inits(&mut self) -> PResult<Vec<FieldInit>> {
        self.expect(Tok::LBrace)?;
        let mut fields = Vec::new();
        while !self.at(&Tok::RBrace) {
            let (name, span) = self.expect_ident()?;
            // Shorthand `Todo { title }` reuses the binding of the same name.
            let value = if self.eat(&Tok::Colon) { Some(self.expr()?) } else { None };
            let end = value.as_ref().map_or(span, |v| v.span());
            fields.push(FieldInit { name, value, span: Self::join(span, end) });
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.expect(Tok::RBrace)?;
        Ok(fields)
    }

    fn if_expr(&mut self) -> PResult<Expr> {
        let start = self.expect(Tok::If)?;
        let cond = self.head_expr()?;
        let then_block = self.block()?;
        let else_block = if self.eat(&Tok::Else) {
            if self.at(&Tok::If) {
                Some(Box::new(self.if_expr()?))
            } else {
                Some(Box::new(Expr::Block(self.block()?)))
            }
        } else {
            None
        };
        let end = else_block.as_ref().map_or(then_block.span, |e| e.span());
        Ok(Expr::If {
            cond: Box::new(cond),
            then_block,
            else_block,
            span: Self::join(start, end),
        })
    }

    fn match_expr(&mut self) -> PResult<Expr> {
        let start = self.expect(Tok::Match)?;
        let scrutinee = self.head_expr()?;
        self.expect(Tok::LBrace)?;

        let mut arms = Vec::new();
        while !self.at(&Tok::RBrace) && !self.at(&Tok::Eof) {
            let pattern = self.pattern()?;
            self.expect(Tok::FatArrow)?;
            let body = self.expr()?;
            arms.push(MatchArm {
                span: Self::join(pattern.span(), body.span()),
                pattern,
                body,
            });
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        let end = self.expect(Tok::RBrace)?;

        if arms.is_empty() {
            return self.error("`match` needs at least one arm");
        }
        Ok(Expr::Match {
            scrutinee: Box::new(scrutinee),
            arms,
            span: Self::join(start, end),
        })
    }

    fn pattern(&mut self) -> PResult<Pattern> {
        let span = self.span();
        match self.peek().clone() {
            Tok::Int(value) => {
                self.advance();
                Ok(Pattern::Int { value, span })
            }
            Tok::Str(value) => {
                self.advance();
                Ok(Pattern::Str { value, span })
            }
            Tok::True => {
                self.advance();
                Ok(Pattern::Bool { value: true, span })
            }
            Tok::False => {
                self.advance();
                Ok(Pattern::Bool { value: false, span })
            }
            Tok::Ident(name) => {
                self.advance();
                if name == "_" {
                    return Ok(Pattern::Wildcard { span });
                }
                if self.eat(&Tok::LParen) {
                    let mut args = Vec::new();
                    while !self.at(&Tok::RParen) {
                        // Field labels inside patterns are accepted and dropped:
                        // `TooLong(max)` and `TooLong(max: m)` both bind.
                        if let (Tok::Ident(_), Tok::Colon) = (self.peek(), self.peek_at(1)) {
                            self.advance();
                            self.advance();
                        }
                        args.push(self.pattern()?);
                        if !self.eat(&Tok::Comma) {
                            break;
                        }
                    }
                    let end = self.expect(Tok::RParen)?;
                    return Ok(Pattern::Ctor { name, args, span: Self::join(span, end) });
                }
                // Capitalised bare names are niladic constructors (`EmptyTitle`),
                // lowercase ones bind (`next`). The checker verifies either way.
                if name.starts_with(|c: char| c.is_uppercase()) {
                    Ok(Pattern::Ctor { name, args: Vec::new(), span })
                } else {
                    Ok(Pattern::Binding { name, span })
                }
            }
            other => self.error(format!("expected a pattern, found {other}")),
        }
    }
}

/// Distinguishes `Todo { id: 1 }` from `if cond { ... }`, where the `{` opens a
/// block rather than a literal. Looks for `ident:` or an immediate `}`.
fn starts_record_literal(p: &Parser) -> bool {
    match (p.peek_at(1), p.peek_at(2)) {
        (Tok::RBrace, _) => true,
        (Tok::Ident(_), Tok::Colon | Tok::Comma | Tok::RBrace) => true,
        _ => false,
    }
}

fn binop_of(tok: &Tok) -> Option<(BinOp, u8)> {
    let pair = match tok {
        Tok::PipePipe => (BinOp::Or, 1),
        Tok::AmpAmp => (BinOp::And, 2),
        Tok::EqEq => (BinOp::Eq, 3),
        Tok::BangEq => (BinOp::Ne, 3),
        Tok::Lt => (BinOp::Lt, 4),
        Tok::LtEq => (BinOp::Le, 4),
        Tok::Gt => (BinOp::Gt, 4),
        Tok::GtEq => (BinOp::Ge, 4),
        Tok::Plus => (BinOp::Add, 5),
        Tok::Minus => (BinOp::Sub, 5),
        Tok::Star => (BinOp::Mul, 6),
        Tok::Slash => (BinOp::Div, 6),
        Tok::Percent => (BinOp::Rem, 6),
        _ => return None,
    };
    Some(pair)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_ok(src: &str) -> Program {
        match parse(src) {
            Ok(p) => p,
            Err(e) => panic!("parse failed: {e}"),
        }
    }

    fn only_fn(src: &str) -> FnDecl {
        match parse_ok(src).items.into_iter().next().expect("no items") {
            Item::Fn(f) => f,
            other => panic!("expected a fn, got {other:?}"),
        }
    }

    #[test]
    fn parses_fn_signature_and_tail_expression() {
        let f = only_fn("fn add(a: int, b: int): int { a + b }");
        assert_eq!(f.name, "add");
        assert_eq!(f.params.len(), 2);
        assert_eq!(f.params[0].name, "a");
        assert!(f.body.stmts.is_empty());
        assert!(matches!(f.body.tail.as_deref(), Some(Expr::Binary { op: BinOp::Add, .. })));
    }

    #[test]
    fn arithmetic_precedence_binds_tighter_than_comparison() {
        let f = only_fn("fn f(): bool { 1 + 2 * 3 < 10 }");
        let Some(Expr::Binary { op: BinOp::Lt, lhs, .. }) = f.body.tail.as_deref() else {
            panic!("expected a comparison at the root, got {:?}", f.body.tail);
        };
        // lhs must be (1 + (2 * 3)), not ((1 + 2) * 3).
        let Expr::Binary { op: BinOp::Add, rhs, .. } = lhs.as_ref() else {
            panic!("expected + under the comparison");
        };
        assert!(matches!(rhs.as_ref(), Expr::Binary { op: BinOp::Mul, .. }));
    }

    #[test]
    fn logical_operators_bind_loosest() {
        let f = only_fn("fn f(): bool { a == 1 && b == 2 || c }");
        assert!(matches!(f.body.tail.as_deref(), Some(Expr::Binary { op: BinOp::Or, .. })));
    }

    #[test]
    fn parses_the_design_doc_add_todo() {
        // Verbatim from §4.5, minus the method calls the checker handles later.
        let f = only_fn(
            r#"
            fn addTodo(list: List<Todo>, title: string): Result<List<Todo>, AddError> {
              if title.trim().isEmpty() { return Err(EmptyTitle) }
              if title.len() > 200      { return Err(TooLong(max: 200)) }
              Ok(list.push(Todo { id: Id.new(), title, done: false }))
            }
            "#,
        );
        assert_eq!(f.name, "addTodo");
        assert_eq!(f.body.stmts.len(), 2, "two guard statements");
        assert!(f.body.tail.is_some(), "Ok(...) is the tail expression");

        let Some(TypeExpr::Named { name, args, .. }) = f.ret.as_ref() else {
            panic!("expected a named return type");
        };
        assert_eq!(name, "Result");
        assert_eq!(args.len(), 2);
    }

    #[test]
    fn parses_the_design_doc_match() {
        let f = only_fn(
            r#"
            fn f(): int {
              match addTodo(todos, input) {
                Ok(next)          => 1,
                Err(EmptyTitle)   => 2,
                Err(TooLong(max)) => 3,
              }
            }
            "#,
        );
        let Some(Expr::Match { arms, .. }) = f.body.tail.as_deref() else {
            panic!("expected a match tail");
        };
        assert_eq!(arms.len(), 3);
        assert!(matches!(&arms[0].pattern, Pattern::Ctor { name, args, .. }
            if name == "Ok" && matches!(args[0], Pattern::Binding { .. })));
        // Err(EmptyTitle): a capitalised bare name is a niladic constructor.
        assert!(matches!(&arms[1].pattern, Pattern::Ctor { name, args, .. }
            if name == "Err" && matches!(&args[0], Pattern::Ctor { name, args, .. }
                if name == "EmptyTitle" && args.is_empty())));
        // Err(TooLong(max)): lowercase binds.
        assert!(matches!(&arms[2].pattern, Pattern::Ctor { args, .. }
            if matches!(&args[0], Pattern::Ctor { args, .. }
                if matches!(args[0], Pattern::Binding { .. }))));
    }

    #[test]
    fn parses_record_and_sum_type_declarations() {
        let program = parse_ok(
            r#"
            type Todo = { id: Id, title: string, done: bool }
            type AddError = | EmptyTitle | TooLong(max: int)
            "#,
        );
        let [Item::Type(todo), Item::Type(err)] = program.items.as_slice() else {
            panic!("expected two type declarations");
        };
        let TypeDef::Record(fields) = &todo.def else { panic!("Todo must be a record") };
        assert_eq!(fields.len(), 3);
        assert_eq!(fields[1].name, "title");

        let TypeDef::Sum(variants) = &err.def else { panic!("AddError must be a sum") };
        assert_eq!(variants.len(), 2);
        assert!(variants[0].fields.is_empty());
        assert_eq!(variants[1].fields[0].name, "max");
    }

    #[test]
    fn optional_type_sugar_nests() {
        let f = only_fn("fn f(a: string?): int { 1 }");
        let TypeExpr::Optional { inner, .. } = &f.params[0].ty else {
            panic!("expected Optional");
        };
        assert!(matches!(inner.as_ref(), TypeExpr::Named { name, .. } if name == "string"));
    }

    #[test]
    fn try_operator_is_postfix() {
        let f = only_fn("fn f(): int { g()? + 1 }");
        let Some(Expr::Binary { lhs, .. }) = f.body.tail.as_deref() else {
            panic!("expected a binary tail");
        };
        assert!(matches!(lhs.as_ref(), Expr::Try { .. }), "`?` must bind tighter than `+`");
    }

    #[test]
    fn record_literal_is_not_confused_with_a_block() {
        // `if cond { ... }` must parse the brace as a block, not a literal.
        let f = only_fn("fn f(): int { if ready { 1 } else { 2 } }");
        assert!(matches!(f.body.tail.as_deref(), Some(Expr::If { .. })));

        let g = only_fn("fn g(): Todo { Todo { done: false } }");
        assert!(matches!(g.body.tail.as_deref(), Some(Expr::RecordLit { .. })));
    }

    #[test]
    fn shorthand_field_init_has_no_value() {
        let f = only_fn("fn f(): Todo { Todo { title } }");
        let Some(Expr::RecordLit { fields, .. }) = f.body.tail.as_deref() else {
            panic!("expected a record literal");
        };
        assert_eq!(fields[0].name, "title");
        assert!(fields[0].value.is_none());
    }

    #[test]
    fn let_and_var_differ_in_mutability() {
        let f = only_fn("fn f(): int { let a = 1 var b = 2 b = 3 b }");
        assert!(matches!(&f.body.stmts[0], Stmt::Let { mutable: false, name, .. } if name == "a"));
        assert!(matches!(&f.body.stmts[1], Stmt::Let { mutable: true, name, .. } if name == "b"));
        assert!(matches!(&f.body.stmts[2], Stmt::Assign { .. }));
    }

    #[test]
    fn semicolon_suppresses_the_tail() {
        let f = only_fn("fn f() { 1; }");
        assert!(f.body.tail.is_none());
        assert_eq!(f.body.stmts.len(), 1);
    }

    #[test]
    fn reports_position_of_syntax_errors() {
        let err = parse("fn f(): int {\n  let = 1\n}").unwrap_err();
        assert_eq!(err.line(), 2);
        assert!(err.message.contains("identifier"), "message was: {}", err.message);
    }

    #[test]
    fn rejects_a_stray_top_level_expression() {
        let err = parse("1 + 1").unwrap_err();
        assert!(err.message.contains("`fn`"), "message was: {}", err.message);
    }
}
