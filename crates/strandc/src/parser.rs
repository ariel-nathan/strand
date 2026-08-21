//! Recursive-descent parser (§4.6).
//!
//! Semicolons are optional: §4.5 writes none. A block's last expression is its
//! tail value unless it is followed by `;`.

use crate::ast::*;
use crate::diag::Diagnostic;
use crate::lexer::{lex, lex_recovering, Span, Tok, Token};
use crate::ui::{is_builder, takes_children};

pub fn parse(src: &str) -> Result<Program, Diagnostic> {
    let tokens = lex(src)?;
    Parser { tokens, pos: 0, no_record_literal: false, depth: 0 }.program()
}

/// Parses as much as it can, reporting every item it could not read instead of
/// stopping at the first one.
///
/// An editor holds a file that is briefly invalid on most keystrokes, and the
/// item being typed must not take the rest of the file's declarations down with
/// it. An item that fails to parse is dropped and the parser resynchronises at
/// the next `fn`/`view`/`type`/`actor`, so its neighbours survive. `parse` keeps
/// its all-or-nothing contract for batch compilation.
pub fn parse_recovering(src: &str) -> (Program, Vec<Diagnostic>) {
    let (tokens, mut errors) = lex_recovering(src);
    let mut parser = Parser { tokens, pos: 0, no_record_literal: false, depth: 0 };
    let program = parser.program_recovering(&mut errors);
    (program, errors)
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    /// Set while parsing an `if`/`match` head. Without it, `if a > b { a }`
    /// reads `b { a }` as a record literal with a shorthand field — the same
    /// ambiguity Rust resolves with a no-struct-literal restriction.
    no_record_literal: bool,
    /// Expression nesting, to keep deeply nested input from overflowing the
    /// stack. A batch compile only ever sees files someone wrote, but a server
    /// parses whatever is in the buffer, and `((((…` should be a diagnostic
    /// rather than a crashed process.
    depth: u32,
}

/// Deeper than any real Strand expression, shallow enough to unwind safely.
///
/// The bound counts frames, but what actually has to fit is bytes, and the two
/// drift apart: a level of descent costs several frames holding an `ast::Expr`
/// or a `PResult<Expr>`, and in an unoptimised build a `match` arm's locals get
/// their own stack slots — so *adding an arm to `primary`* makes every level
/// more expensive without touching this number.
///
/// That is not hypothetical. At 128 the guard stopped firing before the stack
/// ran out on a 2 MB test thread, once `primary` grew the arms for list
/// literals and `for`: measured, the real ceiling had fallen to somewhere
/// between 100 and 120 levels. 64 leaves roughly double the headroom, and is
/// still far past anything a person writes — the deepest expression in this
/// repository nests four.
const MAX_DEPTH: u32 = 64;

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

    /// `program`, but a failed item is reported and skipped rather than ending
    /// the parse.
    fn program_recovering(&mut self, errors: &mut Vec<Diagnostic>) -> Program {
        let mut items = Vec::new();
        while !self.at(&Tok::Eof) {
            let before = self.pos;
            match self.item() {
                Ok(item) => items.push(item),
                Err(diagnostic) => {
                    errors.push(diagnostic);
                    self.resync(before);
                }
            }
        }
        Program { items }
    }

    /// Skips to the next item keyword after a failed item.
    ///
    /// Strand has no nested functions, so any `fn`/`view`/`type`/`actor` token
    /// begins a new declaration and is a safe place to start reading again.
    /// `failed_at` guarantees forward progress even when `item` consumed
    /// nothing, which would otherwise loop forever.
    fn resync(&mut self, failed_at: usize) {
        if self.pos == failed_at {
            self.advance();
        }
        while !self.at(&Tok::Eof) {
            if matches!(self.peek(), Tok::Fn | Tok::View | Tok::Type | Tok::Actor) || self.at_app()
            {
                return;
            }
            self.advance();
        }
    }

    /// `app Name {` — contextual, so `app` stays an ordinary name everywhere
    /// else. Three tokens is enough to be sure, and nothing else in the
    /// language reads that way at item level.
    fn at_app(&self) -> bool {
        matches!((self.peek(), self.peek_at(1), self.peek_at(2)),
            (Tok::Ident(word), Tok::Ident(_), Tok::LBrace) if word == "app")
    }

    fn item(&mut self) -> PResult<Item> {
        if self.at_app() {
            return Ok(Item::App(self.app_decl()?));
        }
        match self.peek() {
            Tok::Fn | Tok::View => Ok(Item::Fn(self.fn_decl()?)),
            Tok::Type => Ok(Item::Type(self.type_decl()?)),
            Tok::Actor => Ok(Item::Actor(self.actor_decl()?)),
            other => {
                self.error(format!("expected `fn`, `type`, `actor` or `app`, found {other}"))
            }
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
        let (name, name_span) = self.expect_ident()?;

        let params = self.params()?;

        // `: T` per §4.5; `-> T` also accepted since §6.2 writes views that way.
        let ret = if self.eat(&Tok::Colon) || self.eat(&Tok::Arrow) {
            Some(self.type_expr()?)
        } else {
            None
        };

        let body = self.block()?;
        Ok(FnDecl {
            name,
            name_span,
            params,
            ret,
            is_view,
            span: Self::join(start, body.span),
            body,
        })
    }

    /// `(a: int, b: string)` — shared by `fn` and by an `on` handler, which
    /// differ in how they are reached rather than in how they are written.
    fn params(&mut self) -> PResult<Vec<Param>> {
        self.expect(Tok::LParen)?;
        let mut params = Vec::new();
        while !self.at(&Tok::RParen) {
            let (name, span) = self.expect_ident()?;
            self.expect(Tok::Colon)?;
            let ty = self.type_expr()?;
            params.push(Param { name, span: Self::join(span, ty.span()), ty });
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.expect(Tok::RParen)?;
        Ok(params)
    }

    /// `on <port>(state, msg): State` — the handler for an `in` port.
    ///
    /// Contextual like `state` and `in`: `on` is only a keyword directly in
    /// front of `name(`, so it stays an ordinary identifier everywhere else.
    fn at_handler(&self) -> bool {
        matches!((self.peek(), self.peek_at(1), self.peek_at(2)),
            (Tok::Ident(word), Tok::Ident(_), Tok::LParen) if word == "on")
    }

    /// `in name: T` / `out name: T`.
    fn port_decl(&mut self) -> PResult<Port> {
        let start = self.span();
        self.advance();
        let (name, name_span) = self.expect_ident()?;
        self.expect(Tok::Colon)?;
        let ty = self.type_expr()?;
        Ok(Port { name, name_span, ty, span: Self::join(start, self.prev_span()) })
    }

    fn actor_decl(&mut self) -> PResult<ActorDecl> {
        let start = self.expect(Tok::Actor)?;
        let (name, name_span) = self.expect_ident()?;
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

        // Then the channels, in either order and any number: `in` is what this
        // actor can be told, `out` is what it can say.
        let mut inbox = Vec::new();
        let mut outbox = Vec::new();
        loop {
            if self.at(&Tok::In) {
                inbox.push(self.port_decl()?);
                continue;
            }
            let is_out = matches!((self.peek(), self.peek_at(1)),
                (Tok::Ident(word), Tok::Ident(_)) if word == "out");
            if is_out {
                outbox.push(self.port_decl()?);
                continue;
            }
            break;
        }

        let mut init = None;
        let mut handlers: Vec<FnDecl> = Vec::new();
        let mut view = None;
        while self.at(&Tok::Fn) || self.at(&Tok::View) || self.at_handler() {
            if self.at_handler() {
                let decl = self.handler_decl()?;
                if handlers.iter().any(|h| h.name == decl.name) {
                    let port = decl.name.clone();
                    return Err(Diagnostic::new(
                        decl.span,
                        format!("port `{port}` is already handled"),
                    )
                    .with_label("a second handler for one port")
                    .with_help(format!(
                        "a port is one channel, so it has one handler — fold this into \
                         the other `on {port}`",
                    )));
                }
                handlers.push(decl);
                continue;
            }
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
                        "an actor has one view; break the rest out as `view fn` items \
                         outside the actor and call them as children",
                    ));
                }
                view = Some(decl);
                continue;
            }
            match decl.name.as_str() {
                "init" => init = Some(decl),
                other => {
                    return Err(Diagnostic::new(decl.span, format!("unexpected function `{other}`"))
                        .with_label("not part of an actor")
                        .with_help(
                            "an actor declares `fn init()`, an `on <port>` handler for each \
                             `in` port, and optionally a `view fn` to draw itself",
                        ));
                }
            }
        }
        let end = self.expect(Tok::RBrace)?;

        let Some(init) = init else {
            return Err(Diagnostic::new(
                Self::join(start, end),
                format!("actor `{name}` has no `init`"),
            )
            .with_label("no starting state")
            .with_help("an actor needs `fn init()` returning the state it starts with"));
        };

        Ok(ActorDecl {
            name,
            name_span,
            state,
            inbox,
            outbox,
            init,
            handlers,
            view,
            span: Self::join(start, end),
        })
    }

    /// The body of `on name(...)`, which is a `fn` in every way except how it
    /// is reached: nothing calls a handler, the mailbox does.
    fn handler_decl(&mut self) -> PResult<FnDecl> {
        let start = self.span();
        self.advance();
        let (name, name_span) = self.expect_ident()?;
        let params = self.params()?;
        let ret = if self.eat(&Tok::Colon) || self.eat(&Tok::Arrow) {
            Some(self.type_expr()?)
        } else {
            None
        };
        let body = self.block()?;
        Ok(FnDecl {
            name,
            name_span,
            params,
            ret,
            body,
            is_view: false,
            span: Self::join(start, self.prev_span()),
        })
    }

    /// `app Name { ui = TodoUi ... ui.out -> stats.in }`.
    fn app_decl(&mut self) -> PResult<AppDecl> {
        let start = self.span();
        self.advance();
        let (name, name_span) = self.expect_ident()?;
        self.expect(Tok::LBrace)?;

        let mut instances = Vec::new();
        let mut wires = Vec::new();
        while !self.at(&Tok::RBrace) {
            let (first, first_span) = self.expect_ident()?;
            if self.eat(&Tok::Eq) {
                let (actor, actor_span) = self.expect_ident()?;
                instances.push(Instance {
                    name: first,
                    name_span: first_span,
                    actor,
                    actor_span,
                    span: Self::join(first_span, actor_span),
                });
                continue;
            }
            let from = self.port_ref(first, first_span)?;
            self.expect(Tok::Arrow)?;
            let (to_instance, to_span) = self.expect_ident()?;
            let to = self.port_ref(to_instance, to_span)?;
            wires.push(WireDecl { span: Self::join(from.span, to.span), from, to });
        }
        let end = self.expect(Tok::RBrace)?;

        Ok(AppDecl { name, name_span, instances, wires, span: Self::join(start, end) })
    }

    /// The `ui.stats` half of a wire, given the instance name already read.
    fn port_ref(&mut self, instance: String, instance_span: Span) -> PResult<PortRef> {
        self.expect(Tok::Dot)?;
        let (port, port_span) = self.expect_ident()?;
        Ok(PortRef {
            instance,
            instance_span,
            port,
            port_span,
            span: Self::join(instance_span, port_span),
        })
    }

    fn type_decl(&mut self) -> PResult<TypeDecl> {
        let start = self.expect(Tok::Type)?;
        let (name, name_span) = self.expect_ident()?;
        self.expect(Tok::Eq)?;

        let def = if self.at(&Tok::LBrace) {
            TypeDef::Record(self.field_defs()?)
        } else if self.at(&Tok::Pipe) {
            TypeDef::Sum(self.variant_defs()?)
        } else {
            TypeDef::Alias(self.type_expr()?)
        };

        Ok(TypeDecl { name, name_span, def, span: Self::join(start, self.prev_span()) })
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
                let (name, name_span) = self.expect_ident()?;
                let ty = if self.eat(&Tok::Colon) { Some(self.type_expr()?) } else { None };
                self.expect(Tok::Eq)?;
                let value = self.expr()?;
                Ok(Stmt::Let {
                    name,
                    name_span,
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
        // Every nested expression funnels through here, so this is the one
        // place the descent has to be bounded.
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            self.depth -= 1;
            return Err(Diagnostic::new(self.span(), "expression nests too deeply")
                .with_label("too deep")
                .with_help("name the inner parts with `let` to flatten this"));
        }
        let result = self.binary(0);
        self.depth -= 1;
        result
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
                    if let Expr::Ident { name, span: name_span } = &expr {
                        if is_builder(name) {
                            let (name, name_span) = (name.clone(), *name_span);
                            let children = if takes_children(&name) && self.at(&Tok::LBrace) {
                                Some(self.block()?)
                            } else {
                                None
                            };
                            let end = children.as_ref().map_or(end, |b| b.span);
                            expr = Expr::Build {
                                span: Self::join(expr.span(), end),
                                name,
                                name_span,
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
            Tok::LBracket => {
                self.advance();
                let items = self.allowing_records(|p| {
                    let mut items = Vec::new();
                    while !p.at(&Tok::RBracket) {
                        items.push(p.expr()?);
                        if !p.eat(&Tok::Comma) {
                            break;
                        }
                    }
                    Ok(items)
                })?;
                let end = self.expect(Tok::RBracket)?;
                Ok(Expr::ListLit { items, span: Self::join(span, end) })
            }

            Tok::For => {
                self.advance();
                let (name, name_span) = self.expect_ident()?;
                self.expect(Tok::In)?;
                // Same restriction `if` needs: without it, `for t in todos {`
                // reads `todos { ... }` as a record literal.
                let iter = self.head_expr()?;
                let body = self.block()?;
                Ok(Expr::For {
                    span: Self::join(span, body.span),
                    name,
                    name_span,
                    iter: Box::new(iter),
                    body,
                })
            }

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
                    let (base, fields) = self.field_inits()?;
                    return Ok(Expr::RecordLit {
                        name: Some(name),
                        base,
                        fields,
                        span: Self::join(span, self.prev_span()),
                    });
                }
                Ok(Expr::Ident { name, span })
            }
            other => self.error(format!("expected an expression, found {other}")),
        }
    }

    /// The body of a record literal: an optional leading `...base`, then the
    /// fields that differ from it.
    ///
    /// The spread has to come first. Letting it appear anywhere would mean the
    /// reader has to scan the whole literal to know whether a field they can
    /// see is the one that wins, and there is no case the freedom buys.
    fn field_inits(&mut self) -> PResult<(Option<Box<Expr>>, Vec<FieldInit>)> {
        self.expect(Tok::LBrace)?;
        let mut base = None;
        if self.at(&Tok::DotDotDot) {
            self.advance();
            base = Some(Box::new(self.expr()?));
            // A spread on its own is a copy, so the comma is only needed when
            // something follows it.
            if !self.at(&Tok::RBrace) {
                self.expect(Tok::Comma)?;
            }
        }
        let mut fields = Vec::new();
        while !self.at(&Tok::RBrace) {
            if self.at(&Tok::DotDotDot) {
                let span = self.span();
                return Err(Diagnostic::new(span, "`...` has to come first")
                    .with_label("a spread after a field")
                    .with_help(
                        "put the spread first: `Model { ...state, draft: x }`, so the \n                         fields that differ are the ones you can see",
                    ));
            }
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
        Ok((base, fields))
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
        // `Model { ...state, ... }`. Nothing else in the language opens a
        // brace with a spread, so one token is enough to tell.
        (Tok::DotDotDot, _) => true,
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

    #[test]
    fn nesting_reports_rather_than_overflowing_on_both_sides_of_the_limit() {
        // The guard has to fire *before* the stack does, so this walks from
        // well inside the bound to well past it rather than testing one depth.
        // A run that overflows takes the process down instead of failing, so
        // the useful signal is this finishing at all.
        for depth in [1usize, 32, 63, 64, 65, 256, 5000] {
            let src = format!("fn deep(): int {{ {}1{} }}", "(".repeat(depth), ")".repeat(depth));
            let (_, errors) = parse_recovering(&src);
            if depth < MAX_DEPTH as usize {
                assert!(errors.is_empty(), "depth {depth} should still parse: {errors:?}");
            } else {
                assert!(
                    errors.iter().any(|e| e.message.contains("too deeply")),
                    "depth {depth} should report rather than overflow: {errors:?}"
                );
            }
        }
    }
}
