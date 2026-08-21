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
  in inbox: string
  fn init(): Count { Count { total: 0 } }
  on inbox(state: Count, msg: string): Count { state }
}
";
    let symbols = Document::new(src).symbols();
    let actor = symbols.iter().find(|s| s.name == "Counter").expect("the actor");

    let members = actor.children.as_ref().expect("members");
    let names: Vec<&str> = members.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["init", "inbox"], "a handler is listed under its port's name");
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

#[test]
fn hover_on_a_builder_says_what_it_takes() {
    // Every builder call has type `Node`, so the type alone answers "what is
    // this" with the one fact that is the same everywhere. A builder also has
    // no declaration to go and read, which makes hover the only way to find
    // out what it takes.
    let src = "\
view fn main(): Node {
  column(gap: 4) {
    button(id: 1, label: \"Add\")
  }
}
";
    let document = Document::new(src);

    let column = hover_text(&document, position_of(src, "column", 0)).expect("should hover");
    assert!(column.contains("gap: int = 0"), "props and their defaults: {column}");
    assert!(column.contains("padding: int = 0"), "got: {column}");
    assert!(column.contains("{ … }"), "a container says it takes children: {column}");

    let button = hover_text(&document, position_of(src, "button", 0)).expect("should hover");
    assert!(button.contains("id: int"), "got: {button}");
    assert!(button.contains("label: string"), "got: {button}");
    assert!(!button.contains("{ … }"), "a leaf takes no children: {button}");
}

#[test]
fn hover_inside_a_builder_still_reports_the_type_there() {
    // The description covers the builder's name, not its whole call, so an
    // argument inside it hovers as itself.
    let src = "\
view fn main(): Node {
  column(gap: 4) {
    text(\"hello\")
  }
}
";
    let document = Document::new(src);
    let text = hover_text(&document, position_of(src, "hello", 0)).expect("should hover");
    assert!(text.contains("string"), "got: {text}");
}

#[test]
fn a_view_hovers_as_a_node() {
    let src = "view fn main(): Node {\n  spacer()\n}\n";
    let document = Document::new(src);
    let text = hover_text(&document, position_of(src, "spacer", 0)).expect("should hover");
    assert!(text.contains("spacer()"), "got: {text}");
    assert!(text.contains("-> Node"), "got: {text}");
}

// ---- hover on things the file does not declare ---------------------------

#[test]
fn hover_on_a_type_annotation_says_what_it_resolved_to() {
    // Reported from the editor: nothing at all came back on `List`, `Result`
    // or a user type in an annotation. A type is not an expression, so nothing
    // was recording one — and an annotation is exactly where someone asks what
    // a name means.
    let src = "\
type Todo = { id: int, title: string }

fn add(todos: List<Todo>, title: string): Result<int, string> {
  Ok(len(todos))
}
";
    let document = Document::new(src);

    let list = hover_text(&document, position_of(src, "List", 0)).expect("should hover");
    assert!(list.contains("List<Todo>"), "got: {list}");

    let result = hover_text(&document, position_of(src, "Result", 0)).expect("should hover");
    assert!(result.contains("Result<int, string>"), "got: {result}");

    let primitive = hover_text(&document, position_of(src, "int", 0)).expect("should hover");
    assert!(primitive.contains("int"), "got: {primitive}");
}

#[test]
fn the_narrowest_type_name_answers_for_itself() {
    // `List<Todo>` contains `Todo`, and both are recorded. Hover takes the
    // narrower one, so each name reports what it is rather than what encloses
    // it.
    let src = "type Todo = { id: int }\nfn f(xs: List<Todo>): int { 1 }\n";
    let document = Document::new(src);
    let inner = hover_text(&document, position_of(src, "Todo", 1)).expect("should hover");
    assert_eq!(inner.trim(), "```strand\nTodo\n```", "got: {inner}");
}

#[test]
fn hover_on_a_binding_reports_its_type() {
    // Every binding goes through one place in the checker, so this covers a
    // parameter, a `let`, a `for` variable and a pattern binding alike.
    let src = "\
fn f(count: int): int {
  let doubled = count * 2
  var total = 0
  for n in [doubled] {
    total = total + n
  }
  match total {
    other => other,
  }
}
";
    let document = Document::new(src);
    for (name, want) in
        [("count", "int"), ("doubled", "int"), ("total", "int"), ("n", "int"), ("other", "int")]
    {
        let text = hover_text(&document, position_of(src, name, 0))
            .unwrap_or_else(|| panic!("no hover on `{name}`"));
        assert!(text.contains(want), "`{name}` should hover as {want}, got: {text}");
    }
}

#[test]
fn a_function_signature_does_not_answer_for_its_arguments() {
    // A description covering the whole call would report `trim` for everything
    // written inside it. It covers the name only.
    let src = "fn f(title: string): string {\n  trim(title)\n}\n";
    let document = Document::new(src);

    let name = hover_text(&document, position_of(src, "trim", 0)).expect("should hover");
    assert!(name.contains("fn trim(s: string): string"), "got: {name}");

    let argument = hover_text(&document, position_of(src, "title", 1)).expect("should hover");
    assert!(argument.contains("string"), "got: {argument}");
    assert!(!argument.contains("fn trim"), "the argument is not the function: {argument}");
}

#[test]
fn hover_describes_the_list_operations() {
    let src = "fn f(xs: List<int>): List<int> {\n  push(xs, len(xs))\n}\n";
    let document = Document::new(src);

    let push = hover_text(&document, position_of(src, "push", 0)).expect("should hover");
    assert!(push.contains("fn push(list: List<T>, value: T)"), "got: {push}");

    // `len` means both a string and a list, and hover says which one this is.
    let len = hover_text(&document, position_of(src, "len", 0)).expect("should hover");
    assert!(len.contains("fn len(list: List<T>): int"), "got: {len}");
}

#[test]
fn len_on_a_string_still_describes_the_string_one() {
    let src = "fn f(s: string): int {\n  len(s)\n}\n";
    let document = Document::new(src);
    let len = hover_text(&document, position_of(src, "len", 0)).expect("should hover");
    assert!(len.contains("fn len(s: string): int"), "got: {len}");
}

#[test]
fn a_list_mistake_is_reported_like_any_other() {
    let src = "fn main(): int { len([]) }\n";
    let diagnostics = Document::new(src).diagnostics();
    assert_eq!(diagnostics.len(), 1, "got: {diagnostics:?}");
    assert!(diagnostics[0].message.contains("told what it holds"), "{diagnostics:?}");
}

#[test]
fn a_type_name_still_goes_to_its_declaration() {
    // Recording a type for hover must not have displaced the definition.
    let src = "type Todo = { id: int }\nfn f(xs: List<Todo>): int { 1 }\n";
    let document = Document::new(src);
    let target = document.definition(position_of(src, "Todo", 1)).expect("should resolve");
    assert_eq!(target.start.line, 0, "the declaration is on line 0: {target:?}");
}

// ---- the grammar's fixture -----------------------------------------------

/// The half of `highlight-fixture.str` above the "NOT VALID" line.
fn valid_fixture() -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("editors")
        .join("vscode")
        .join("test")
        .join("highlight-fixture.str");
    let source = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    let marker = "// NOT VALID STRAND BELOW THIS LINE";
    let (valid, _) = source
        .split_once(marker)
        .unwrap_or_else(|| panic!("{} lost its {marker} divider", path.display()));
    valid.to_string()
}

#[test]
fn the_grammar_fixture_parses_above_the_line() {
    // What the divider claims: below it is rejected by the *parser* or the
    // lexer, above it is not. Deliberately not "above it type-checks" —
    // section 1 exists to show the lexer's dot rule and writes `1.max(2)`,
    // which lexes and parses exactly as intended and then fails to check.
    //
    // Nothing enforced even this much, and the file drifted once: it listed
    // `for` and `in` as reserved-but-unparsed after they became real.
    let (_, errors) = strandc::parser::parse_recovering(&valid_fixture());
    assert!(
        errors.is_empty(),
        "the fixture stopped parsing above the divider:\n{}",
        errors
            .iter()
            .map(|error| format!("  line {}: {}", error.span.line, error.message))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn the_new_sections_of_the_fixture_also_check() {
    // Sections 9b-9d demonstrate lists, the generated string functions and the
    // platform's `Input` type. Unlike section 1 they are meant to be real
    // Strand, so they are checked as well as parsed — cut out on their own,
    // since the sections above them are not.
    let fixture = valid_fixture();
    let start = fixture.find("// 9b.").expect("section 9b");
    let diagnostics = Document::new(&fixture[start..]).diagnostics();
    assert!(
        diagnostics.is_empty(),
        "sections 9b-9d no longer compile:\n{}",
        diagnostics
            .iter()
            .map(|d| format!("  line {}: {}", d.range.start.line + 1, d.message))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn the_fixture_covers_every_widget_and_builtin_the_grammar_claims() {
    // The grammar lists closed tables from `ui.rs` and `stdlib.rs`. If one
    // gains a name and the fixture does not, nobody ever looks at its colour.
    let fixture = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("editors")
            .join("vscode")
            .join("test")
            .join("highlight-fixture.str"),
    )
    .expect("the fixture should be readable");

    let missing: Vec<&str> = strandc::ui::BUILDERS
        .iter()
        .map(|builder| builder.name)
        .chain(strandc::stdlib::FUNCTIONS.iter().map(|fun| fun.name))
        .filter(|name| !fixture.contains(&format!("{name}(")))
        .collect();
    assert!(missing.is_empty(), "the fixture never exercises: {missing:?}");
}
