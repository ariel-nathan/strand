//! The answers, as plain functions of the source text.
//!
//! Everything the server can say is computed here, with no client, no
//! connection and no async. The transport in `server` is a shell over these, so
//! the interesting behaviour can be tested by calling a function and looking at
//! what comes back.

use strandc::analysis::Analysis;
use strandc::ast;
use strandc::check::analyze;
use strandc::hir::Hir;
use strandc::lexer::Span;
use strandc::line_index::{LineIndex, Position as BytePosition};
use strandc::parser::parse_recovering;

use tower_lsp_server::ls_types::{
    Diagnostic, DiagnosticSeverity, DocumentSymbol, Hover, HoverContents, MarkupContent,
    MarkupKind, Position, Range, SymbolKind,
};

/// One file, parsed and checked once so every request can be answered from it.
pub struct Document<'src> {
    src: &'src str,
    lines: LineIndex<'src>,
    program: ast::Program,
    hir: Hir,
    analysis: Analysis,
    diagnostics: Vec<strandc::diag::Diagnostic>,
}

impl<'src> Document<'src> {
    /// Runs the front end over `src`. Never fails: both phases recover, so a
    /// file that is mid-edit still yields whatever was understood.
    pub fn new(src: &'src str) -> Self {
        let (program, parse_errors) = parse_recovering(src);
        let (hir, analysis, check_errors) = analyze(&program);

        let mut diagnostics = parse_errors;
        diagnostics.extend(check_errors);

        Self { src, lines: LineIndex::new(src), program, hir, analysis, diagnostics }
    }

    fn position(&self, offset: usize) -> Position {
        let BytePosition { line, character } = self.lines.position(offset);
        Position { line, character }
    }

    fn offset(&self, position: Position) -> usize {
        self.lines.offset(BytePosition {
            line: position.line,
            character: position.character,
        })
    }

    fn range(&self, span: Span) -> Range {
        // A zero-width span — the parser produces them at end of input — would
        // render as an invisible marker, so widen it onto the next character
        // the way the terminal renderer does.
        let end = if span.end == span.start {
            (span.start + 1).min(self.src.len())
        } else {
            span.end
        };
        Range { start: self.position(span.start), end: self.position(end) }
    }

    /// Every problem in the file, in source order.
    ///
    /// The compiler reports in phase order — types, then signatures, then
    /// bodies — which does not match how the file reads.
    pub fn diagnostics(&self) -> Vec<Diagnostic> {
        let mut out: Vec<Diagnostic> = self
            .diagnostics
            .iter()
            .map(|diagnostic| Diagnostic {
                range: self.range(diagnostic.span),
                severity: Some(DiagnosticSeverity::ERROR),
                source: Some("strandc".to_string()),
                message: match &diagnostic.help {
                    // The compiler only writes `help` when the fix is
                    // unambiguous, so it is worth showing rather than hiding
                    // behind a hover.
                    Some(help) => format!("{}\n\nhelp: {help}", diagnostic.message),
                    None => diagnostic.message.clone(),
                },
                ..Default::default()
            })
            .collect();
        out.sort_by_key(|diagnostic| (diagnostic.range.start.line, diagnostic.range.start.character));
        out
    }

    /// The type under the cursor, rendered as it would be written in source.
    pub fn hover(&self, position: Position) -> Option<Hover> {
        let offset = self.offset(position);
        let label = self.analysis.type_label_at(offset, &self.hir)?;
        Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: format!("```strand\n{label}\n```"),
            }),
            range: None,
        })
    }

    /// Where the name under the cursor was declared.
    pub fn definition(&self, position: Position) -> Option<Range> {
        let offset = self.offset(position);
        self.analysis.definition_at(offset).map(|span| self.range(span))
    }

    /// Every mention of whatever the cursor is on, declaration included.
    pub fn references(&self, position: Position) -> Vec<Range> {
        let offset = self.offset(position);
        self.analysis.references_at(offset).into_iter().map(|span| self.range(span)).collect()
    }

    /// The file's declarations, for the outline and breadcrumbs.
    pub fn symbols(&self) -> Vec<DocumentSymbol> {
        self.program.items.iter().map(|item| self.item_symbol(item)).collect()
    }

    fn item_symbol(&self, item: &ast::Item) -> DocumentSymbol {
        match item {
            ast::Item::Fn(decl) => self.fn_symbol(decl),
            ast::Item::Type(decl) => {
                let (kind, children) = match &decl.def {
                    ast::TypeDef::Record(fields) => (
                        SymbolKind::STRUCT,
                        fields
                            .iter()
                            .map(|field| {
                                self.symbol(
                                    &field.name,
                                    None,
                                    SymbolKind::FIELD,
                                    field.span,
                                    field.span,
                                    vec![],
                                )
                            })
                            .collect(),
                    ),
                    ast::TypeDef::Sum(variants) => (
                        SymbolKind::ENUM,
                        variants
                            .iter()
                            .map(|variant| {
                                self.symbol(
                                    &variant.name,
                                    None,
                                    SymbolKind::ENUM_MEMBER,
                                    variant.span,
                                    variant.span,
                                    vec![],
                                )
                            })
                            .collect(),
                    ),
                    // LSP has no alias kind; a plain type name reads best as a
                    // class in every client's icon set.
                    ast::TypeDef::Alias(_) => (SymbolKind::CLASS, vec![]),
                };
                self.symbol(&decl.name, None, kind, decl.span, decl.name_span, children)
            }
            ast::Item::Actor(decl) => {
                let mut children = vec![self.fn_symbol(&decl.init), self.fn_symbol(&decl.receive)];
                children.extend(decl.view.as_ref().map(|view| self.fn_symbol(view)));
                self.symbol(
                    &decl.name,
                    Some("actor".to_string()),
                    SymbolKind::CLASS,
                    decl.span,
                    decl.name_span,
                    children,
                )
            }
        }
    }

    fn fn_symbol(&self, decl: &ast::FnDecl) -> DocumentSymbol {
        let params: Vec<String> =
            decl.params.iter().map(|param| param.name.clone()).collect();
        let detail = format!(
            "{}fn({})",
            if decl.is_view { "view " } else { "" },
            params.join(", ")
        );
        self.symbol(
            &decl.name,
            Some(detail),
            SymbolKind::FUNCTION,
            decl.span,
            decl.name_span,
            vec![],
        )
    }

    fn symbol(
        &self,
        name: &str,
        detail: Option<String>,
        kind: SymbolKind,
        span: Span,
        name_span: Span,
        children: Vec<DocumentSymbol>,
    ) -> DocumentSymbol {
        #[allow(deprecated)] // `deprecated` is required by the struct literal.
        DocumentSymbol {
            name: name.to_string(),
            detail,
            kind,
            tags: None,
            deprecated: None,
            range: self.range(span),
            // What the editor highlights when you pick the symbol: the name,
            // not the whole declaration.
            selection_range: self.range(name_span),
            children: if children.is_empty() { None } else { Some(children) },
        }
    }
}
