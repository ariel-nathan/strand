//! What the server answers, checked by calling the pure layer directly.
//!
//! No client, no connection, no async: `Document` is a plain function of the
//! source, which is the whole reason the analysis lives outside the transport.

use strand_lsp::features::Document;
use tower_lsp_server::ls_types::{DiagnosticSeverity, HoverContents, Position, SymbolKind};

/// The position just before the `nth` occurrence (0-based) of `needle`.
///
/// Only whole-word matches count, so asking for `n` finds the variable rather
/// than the `n` inside `fn` or `int`.
fn position_of(src: &str, needle: &str, n: usize) -> Position {
    let is_word = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let offset = src
        .match_indices(needle)
        .filter(|(at, _)| {
            let before_ok = src[..*at].chars().next_back().is_none_or(|c| !is_word(c));
            let after_ok =
                src[at + needle.len()..].chars().next().is_none_or(|c| !is_word(c));
            before_ok && after_ok
        })
        .nth(n)
        .unwrap_or_else(|| panic!("no whole-word {needle:?} #{n}"))
        .0;
    let before = &src[..offset];
    let line = before.matches('\n').count() as u32;
    let line_start = before.rfind('\n').map(|i| i + 1).unwrap_or(0);
    let character = src[line_start..offset].chars().map(char::len_utf16).sum::<usize>() as u32;
    Position { line, character }
}

fn hover_text(document: &Document, position: Position) -> Option<String> {
    match document.hover(position)?.contents {
        HoverContents::Markup(markup) => Some(markup.value),
        _ => None,
    }
}

// ---- diagnostics ---------------------------------------------------------

#[test]
fn a_clean_file_reports_nothing() {
    let src = "fn main(): int {\n  1\n}\n";
    assert!(Document::new(src).diagnostics().is_empty());
}

#[test]
fn a_type_error_is_reported_at_its_span() {
    let src = "fn main(): int {\n  \"not an int\"\n}\n";
    let diagnostics = Document::new(src).diagnostics();

    assert_eq!(diagnostics.len(), 1, "got: {diagnostics:?}");
    assert_eq!(diagnostics[0].severity, Some(DiagnosticSeverity::ERROR));
    assert_eq!(diagnostics[0].source.as_deref(), Some("strandc"));
    assert_eq!(diagnostics[0].range.start.line, 0, "the signature is what disagrees");
}

#[test]
fn every_type_error_is_reported_not_just_the_first() {
    let src = "fn a(): int {\n  true\n}\nfn b(): int {\n  \"s\"\n}\nfn c(): int {\n  1.5\n}\n";
    let diagnostics = Document::new(src).diagnostics();
    assert_eq!(diagnostics.len(), 3, "got: {diagnostics:?}");
}

#[test]
fn syntax_errors_survive_into_diagnostics() {
    let src = "fn ok(): int { 1 }\nfn broken(: int { 2 }\nfn also_ok(): int { 3 }\n";
    let diagnostics = Document::new(src).diagnostics();
    assert!(!diagnostics.is_empty(), "the broken item should be reported");
}

#[test]
fn diagnostics_come_back_in_source_order() {
    // The checker reports in phase order — signatures before bodies — which is
    // not the order the file reads in.
    let src = "\
fn first(): int {
  true
}
fn second(): int {
  \"s\"
}
fn third(): int {
  1.5
}
";
    let diagnostics = Document::new(src).diagnostics();
    let lines: Vec<u32> = diagnostics.iter().map(|d| d.range.start.line).collect();
    let mut sorted = lines.clone();
    sorted.sort();
    assert_eq!(lines, sorted, "should be ordered by position: {lines:?}");
}

#[test]
fn the_compilers_help_text_reaches_the_editor() {
    // §8.2 only writes `help` when the fix is unambiguous, so it is worth
    // surfacing rather than dropping.
    let src = "fn main(): int {\n  let x = 1\n  x = 2\n  x\n}\n";
    let diagnostics = Document::new(src).diagnostics();

    let immutable = diagnostics
        .iter()
        .find(|d| d.message.contains("immutable"))
        .unwrap_or_else(|| panic!("expected an immutability error, got: {diagnostics:?}"));
    assert!(
        immutable.message.contains("help:") && immutable.message.contains("var"),
        "the suggested fix should be carried across: {}",
        immutable.message
    );
}

#[test]
fn a_ranges_end_is_never_before_its_start() {
    // Zero-width spans occur at end of input; a backwards range makes clients
    // misbehave.
    for src in ["fn", "fn main(", "type", "fn main(): int {", ""] {
        for diagnostic in Document::new(src).diagnostics() {
            let (start, end) = (diagnostic.range.start, diagnostic.range.end);
            assert!(
                (start.line, start.character) <= (end.line, end.character),
                "backwards range in {src:?}: {diagnostic:?}"
            );
        }
    }
}

#[test]
fn a_file_that_is_only_junk_does_not_hang() {
    let diagnostics = Document::new("@@@@@\n#####\n").diagnostics();
    assert!(!diagnostics.is_empty());
}

// ---- hover ---------------------------------------------------------------

#[test]
fn hover_reports_the_type_under_the_cursor() {
    let src = "fn main(): int {\n  let tally = 1 + 2\n  tally\n}\n";
    let document = Document::new(src);

    let text = hover_text(&document, position_of(src, "tally", 1)).expect("should hover");
    assert!(text.contains("int"), "got: {text}");
    assert!(text.contains("```strand"), "should be a fenced code block: {text}");
}

#[test]
fn hover_names_user_types() {
    let src = "\
type Todo = { title: string, done: bool }
fn make(): Todo {
  let item = Todo { title: \"x\", done: false }
  item
}
";
    let document = Document::new(src);
    let text = hover_text(&document, position_of(src, "item", 1)).expect("should hover");
    assert!(text.contains("Todo"), "got: {text}");
}

#[test]
fn hover_on_empty_space_is_none() {
    let src = "fn main(): int {\n  1\n}\n";
    let document = Document::new(src);
    assert!(document.hover(Position { line: 99, character: 0 }).is_none());
}

#[test]
fn hover_works_on_a_line_containing_non_ascii() {
    // The byte column the lexer records would be wrong here; positions are
    // derived from byte offsets through the line index instead.
    let src = "fn main(): int {\n  let greeting = \"héllo→\"\n  let n = 7\n  n\n}\n";
    let document = Document::new(src);

    // The tail `n`, on the line after the one holding multi-byte characters.
    let text = hover_text(&document, position_of(src, "n", 1));
    assert_eq!(text.as_deref().map(|t| t.contains("int")), Some(true), "got: {text:?}");
}

// ---- definition and references -------------------------------------------

#[test]
fn definition_points_at_the_declaration() {
    let src = "fn helper(): int { 1 }\nfn main(): int { helper() }\n";
    let document = Document::new(src);

    let found = document.definition(position_of(src, "helper", 1)).expect("should resolve");
    assert_eq!(found.start, position_of(src, "helper", 0));
}

#[test]
fn definition_on_nothing_is_none() {
    let src = "fn main(): int { 1 }\n";
    assert!(Document::new(src).definition(Position { line: 0, character: 0 }).is_none());
}

#[test]
fn references_include_the_declaration_and_uses() {
    let src = "fn main(): int {\n  let tally = 1\n  tally + tally\n}\n";
    let document = Document::new(src);

    let references = document.references(position_of(src, "tally", 1));
    assert_eq!(references.len(), 3, "declaration plus two uses: {references:?}");
    assert_eq!(references[0].start, position_of(src, "tally", 0));
}

// ---- document symbols ----------------------------------------------------

#[test]
fn symbols_list_the_top_level_declarations() {
    let src = "type Id = int\nfn one(): int { 1 }\nfn two(): int { 2 }\n";
    let symbols = Document::new(src).symbols();

    let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["Id", "one", "two"]);
    assert_eq!(symbols[1].kind, SymbolKind::FUNCTION);
}

#[test]
fn a_record_lists_its_fields() {
    let src = "type Todo = { title: string, done: bool }\n";
    let symbols = Document::new(src).symbols();

    assert_eq!(symbols[0].kind, SymbolKind::STRUCT);
    let fields = symbols[0].children.as_ref().expect("fields");
    let names: Vec<&str> = fields.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["title", "done"]);
}

#[test]
fn a_sum_lists_its_variants() {
    let src = "type Shape = | Dot | Rect(w: int)\n";
    let symbols = Document::new(src).symbols();

    assert_eq!(symbols[0].kind, SymbolKind::ENUM);
    let variants = symbols[0].children.as_ref().expect("variants");
    assert_eq!(variants.len(), 2);
    assert_eq!(variants[0].kind, SymbolKind::ENUM_MEMBER);
}

#[test]
fn an_actor_nests_its_members() {
    let src = "\
type Count = { total: int }
actor Counter {
  state: Count
  fn init(): Count { Count { total: 0 } }
  fn receive(state: Count, msg: string): Count { state }
}
";
    let symbols = Document::new(src).symbols();
    let actor = symbols.iter().find(|s| s.name == "Counter").expect("the actor");

    let members = actor.children.as_ref().expect("members");
    let names: Vec<&str> = members.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["init", "receive"]);
}

#[test]
fn a_view_function_says_so_in_its_detail() {
    let src = "view fn panelView(): Node {\n  text(\"hi\")\n}\n";
    let symbols = Document::new(src).symbols();
    assert_eq!(symbols[0].detail.as_deref(), Some("view fn()"));
}

#[test]
fn the_selection_range_covers_only_the_name() {
    let src = "fn helper(): int { 1 }\n";
    let symbols = Document::new(src).symbols();

    assert_eq!(symbols[0].selection_range.start, position_of(src, "helper", 0));
    assert_eq!(
        symbols[0].selection_range.end.character,
        position_of(src, "helper", 0).character + "helper".len() as u32
    );
}

#[test]
fn a_broken_item_does_not_hide_its_neighbours_from_the_outline() {
    // The reason the parser recovers: the declaration being typed must not take
    // the rest of the outline with it.
    let src = "fn before(): int { 1 }\nfn broken(: int { 2 }\nfn after(): int { 3 }\n";
    let symbols = Document::new(src).symbols();

    let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["before", "after"]);
}
