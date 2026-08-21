//! Diagnostics (§8.2).
//!
//! Every compiler phase reports the same shape: a message, the source span it
//! points at, a short label rendered under the underline, and — where one
//! genuinely exists — a suggested fix. §8.2 makes this a product surface from
//! M1 rather than a retrofit, so the phases build `Diagnostic` values directly
//! instead of formatting strings.

use miette::{Diagnostic as Diag, NamedSource, SourceSpan};
use thiserror::Error;

use crate::lexer::Span;

/// A single problem, independent of how it will be rendered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub message: String,
    pub span: Span,
    /// Text under the underline. Defaults to "here" when a phase has nothing
    /// more specific to say.
    pub label: Option<String>,
    /// A suggested fix. `None` unless the fix is genuinely unambiguous —
    /// a wrong guess is worse than no guess.
    pub help: Option<String>,
}

impl Diagnostic {
    pub fn new(span: Span, message: impl Into<String>) -> Self {
        Self { message: message.into(), span, label: None, help: None }
    }

    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    pub fn with_help(mut self, help: impl Into<String>) -> Self {
        self.help = Some(help.into());
        self
    }

    pub fn line(&self) -> u32 {
        self.span.line
    }

    pub fn col(&self) -> u32 {
        self.span.col
    }
}

impl std::fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}: {}", self.span.line, self.span.col, self.message)
    }
}

impl std::error::Error for Diagnostic {}

/// One rendered entry. miette draws the underline from `span` and attaches the
/// help line when present.
#[derive(Debug, Error, Diag)]
#[error("{message}")]
struct Entry {
    message: String,
    #[label("{label}")]
    span: SourceSpan,
    label: String,
    #[help]
    help: Option<String>,
}

/// A batch of diagnostics sharing one source file. The checker accumulates, so
/// the common case is several at once.
#[derive(Debug, Error, Diag)]
#[error("{}", summary(.entries.len()))]
pub struct Report {
    #[source_code]
    src: NamedSource<String>,
    #[related]
    entries: Vec<Entry>,
}

fn summary(count: usize) -> String {
    if count == 1 {
        "could not compile: 1 error".to_string()
    } else {
        format!("could not compile: {count} errors")
    }
}

impl Report {
    pub fn new(path: &str, source: &str, diagnostics: Vec<Diagnostic>) -> Self {
        let entries = diagnostics
            .into_iter()
            .map(|d| {
                // Guard against a span that runs past the source: a zero-width
                // span at EOF would otherwise panic the renderer.
                let start = d.span.start.min(source.len());
                let end = d.span.end.clamp(start, source.len());
                let len = (end - start).max(1).min(source.len().saturating_sub(start));
                Entry {
                    message: d.message,
                    span: SourceSpan::from((start, len)),
                    label: d.label.unwrap_or_else(|| "here".to_string()),
                    help: d.help,
                }
            })
            .collect();

        Self {
            src: NamedSource::new(path, source.to_string()).with_language("Strand"),
            entries,
        }
    }

    pub fn count(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(start: usize, end: usize) -> Span {
        Span { start, end, line: 1, col: (start + 1) as u32 }
    }

    #[test]
    fn renders_a_labelled_underline_with_help() {
        let source = "fn f(): int { 1 + true }";
        let report = Report::new(
            "demo.str",
            source,
            vec![Diagnostic::new(span(14, 22), "`+` needs matching types, found int and bool")
                .with_label("bool")
                .with_help("convert the operand explicitly; Strand never coerces")],
        );

        let rendered = format!("{:?}", miette::Report::new(report));
        assert!(rendered.contains("matching types"), "rendered: {rendered}");
        assert!(rendered.contains("demo.str"), "rendered: {rendered}");
        // The help line is what §8.2 calls a suggested fix.
        assert!(rendered.contains("never coerces"), "rendered: {rendered}");
    }

    #[test]
    fn summarises_multiple_errors() {
        let report = Report::new(
            "demo.str",
            "fn f(): int { a }",
            vec![
                Diagnostic::new(span(14, 15), "unknown name `a`"),
                Diagnostic::new(span(0, 2), "second problem"),
            ],
        );
        assert_eq!(report.count(), 2);
        assert!(report.to_string().contains("2 errors"), "was: {report}");
    }

    #[test]
    fn a_span_at_end_of_file_does_not_panic() {
        let source = "fn f(): int {";
        let report = Report::new(
            "demo.str",
            source,
            vec![Diagnostic::new(span(source.len(), source.len()), "unexpected end of file")],
        );
        let rendered = format!("{:?}", miette::Report::new(report));
        assert!(rendered.contains("end of file"), "rendered: {rendered}");
    }
}
