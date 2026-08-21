//! The position-indexed facts an editor asks for, driven through the public API.
//!
//! Each test locates a real offset in the source with `find`, so the assertions
//! read as "hover here" rather than as magic numbers.

use strandc::check::analyze;
use strandc::parser::parse;

/// Byte offset of the `nth` occurrence (0-based) of `needle`.
fn nth(src: &str, needle: &str, n: usize) -> usize {
    src.match_indices(needle).nth(n).unwrap_or_else(|| panic!("no {needle:?} #{n}")).0
}

fn first(src: &str, needle: &str) -> usize {
    nth(src, needle, 0)
}

struct Fixture {
    hir: strandc::hir::Hir,
    analysis: strandc::analysis::Analysis,
}

fn analyse(src: &str) -> Fixture {
    let program = parse(src).expect("fixture should parse");
    let (hir, analysis, errors) = analyze(&program);
    assert!(errors.is_empty(), "fixture should check cleanly: {errors:?}");
    Fixture { hir, analysis }
}

impl Fixture {
    fn type_at(&self, offset: usize) -> Option<String> {
        self.analysis.type_label_at(offset, &self.hir)
    }
}

// ---- hover ---------------------------------------------------------------

#[test]
fn primitive_types_are_reported_at_their_literals() {
    let src = "fn main(): int {\n  let n = 42\n  let f = 1.5\n  let s = \"hi\"\n  let b = true\n  n\n}\n";
    let fixture = analyse(src);

    assert_eq!(fixture.type_at(first(src, "42")).as_deref(), Some("int"));
    assert_eq!(fixture.type_at(first(src, "1.5")).as_deref(), Some("float"));
    assert_eq!(fixture.type_at(first(src, "\"hi\"")).as_deref(), Some("string"));
    assert_eq!(fixture.type_at(first(src, "true")).as_deref(), Some("bool"));
}

#[test]
fn an_inferred_local_reports_its_type_where_it_is_used() {
    let src = "fn main(): int {\n  let total = 1 + 2\n  total\n}\n";
    let fixture = analyse(src);

    // The use on the tail line, not the declaration.
    let use_site = nth(src, "total", 1);
    assert_eq!(fixture.type_at(use_site).as_deref(), Some("int"));
}

#[test]
fn a_record_type_is_named_not_numbered() {
    let src = "\
type Todo = { title: string, done: bool }
fn make(): Todo {
  let t = Todo { title: \"write\", done: false }
  t
}
";
    let fixture = analyse(src);
    let use_site = nth(src, "t\n", 0);
    assert_eq!(
        fixture.type_at(use_site).as_deref(),
        Some("Todo"),
        "hover should show the declared name, not a RecordId"
    );
}

#[test]
fn generic_types_render_the_way_they_are_written() {
    let src = "\
type Bad = | Nope
fn lookup(n: int): Result<int, Bad> {
  if n < 0 { return Err(Nope) }
  Ok(n)
}
";
    let fixture = analyse(src);
    let call = first(src, "Ok(n)");
    assert_eq!(fixture.type_at(call).as_deref(), Some("Result<int, Bad>"));
}

#[test]
fn the_innermost_expression_wins_over_its_container() {
    // `flag` is bool; the `if` around it yields int. Hovering the condition must
    // report the condition's type.
    let src = "fn main(): int {\n  let flag = true\n  if flag { 1 } else { 2 }\n}\n";
    let fixture = analyse(src);

    let condition = nth(src, "flag", 1);
    assert_eq!(fixture.type_at(condition).as_deref(), Some("bool"));
}

#[test]
fn a_field_access_reports_the_field_type() {
    let src = "\
type Todo = { title: string, done: bool }
fn describe(t: Todo): bool {
  t.done
}
";
    let fixture = analyse(src);
    assert_eq!(fixture.type_at(first(src, "t.done")).as_deref(), Some("Todo"), "the base");
    assert_eq!(fixture.type_at(first(src, "done\n")).as_deref(), Some("bool"), "the field");
}

// ---- go to definition ----------------------------------------------------

#[test]
fn a_local_use_points_at_its_let() {
    let src = "fn main(): int {\n  let total = 7\n  total\n}\n";
    let fixture = analyse(src);

    let declaration = nth(src, "total", 0);
    let use_site = nth(src, "total", 1);

    let found = fixture.analysis.definition_at(use_site).expect("should resolve");
    assert_eq!(found.start, declaration, "should point at the name in the `let`");
    assert_eq!(found.end, declaration + "total".len(), "and cover only the name");
}

#[test]
fn a_parameter_use_points_at_the_parameter() {
    let src = "fn double(value: int): int {\n  value * 2\n}\n";
    let fixture = analyse(src);

    let found = fixture.analysis.definition_at(nth(src, "value", 1)).expect("should resolve");
    assert_eq!(found.start, nth(src, "value", 0));
}

#[test]
fn a_call_points_at_the_function_name() {
    let src = "fn helper(): int { 1 }\nfn main(): int { helper() }\n";
    let fixture = analyse(src);

    let found = fixture.analysis.definition_at(nth(src, "helper", 1)).expect("should resolve");
    assert_eq!(found.start, nth(src, "helper", 0));
    assert_eq!(found.end, nth(src, "helper", 0) + "helper".len(), "the name alone");
}

#[test]
fn a_type_annotation_points_at_the_type_declaration() {
    let src = "type Id = int\nfn get(id: Id): Id {\n  id\n}\n";
    let fixture = analyse(src);

    // The `Id` in the parameter list, not the declaration.
    let found = fixture.analysis.definition_at(nth(src, "Id", 1)).expect("should resolve");
    assert_eq!(found.start, nth(src, "Id", 0));
}

#[test]
fn a_constructor_points_at_its_variant() {
    let src = "\
type Shape = | Dot | Rect(w: int)
fn main(): Shape {
  Dot
}
";
    let fixture = analyse(src);

    let found = fixture.analysis.definition_at(nth(src, "Dot", 1)).expect("should resolve");
    assert_eq!(found.start, nth(src, "Dot", 0));
}

#[test]
fn a_match_binding_points_at_the_pattern() {
    let src = "\
type Shape = | Rect(w: int)
fn area(s: Shape): int {
  match s {
    Rect(w) => w,
  }
}
";
    let fixture = analyse(src);

    // `w` bound in the pattern, then used in the arm body.
    let bound_at = nth(src, "w)", 0);
    let found = fixture.analysis.definition_at(nth(src, "=> w", 0) + 3).expect("should resolve");
    assert_eq!(found.start, bound_at);
}

#[test]
fn shadowing_resolves_to_the_inner_binding() {
    let src = "\
fn main(): int {
  let x = 1
  if true {
    let x = 2
    x
  } else {
    0
  }
}
";
    let fixture = analyse(src);

    let inner_declaration = nth(src, "x", 1); // `let x = 2`
    let inner_use = nth(src, "x", 2);
    let found = fixture.analysis.definition_at(inner_use).expect("should resolve");
    assert_eq!(found.start, inner_declaration, "the inner `let`, not the outer one");
}

#[test]
fn an_offset_on_nothing_resolves_to_nothing() {
    let src = "fn main(): int { 1 }\n";
    let fixture = analyse(src);
    assert_eq!(fixture.analysis.definition_at(src.len() - 1), None);
}

// ---- find references -----------------------------------------------------

#[test]
fn references_include_the_declaration_and_every_use() {
    // `tally` rather than a short name: single letters also occur inside `fn`
    // and `int`, which would make the offsets below meaningless.
    let src = "fn main(): int {\n  let tally = 1\n  tally + tally\n}\n";
    let fixture = analyse(src);

    let references = fixture.analysis.references_at(nth(src, "tally", 1));
    let starts: Vec<usize> = references.iter().map(|span| span.start).collect();
    assert_eq!(
        starts,
        vec![nth(src, "tally", 0), nth(src, "tally", 1), nth(src, "tally", 2)],
        "declaration plus both uses, in source order"
    );
}

#[test]
fn references_work_from_the_declaration_as_well() {
    let src = "fn main(): int {\n  let tally = 1\n  tally + tally\n}\n";
    let fixture = analyse(src);

    let from_declaration = fixture.analysis.references_at(nth(src, "tally", 0));
    assert_eq!(from_declaration.len(), 3, "should find all three");
    assert_eq!(
        from_declaration,
        fixture.analysis.references_at(nth(src, "tally", 1)),
        "asking from the `let` and from a use should agree"
    );
}

#[test]
fn separate_bindings_do_not_share_references() {
    let src = "fn main(): int {\n  let alpha = 1\n  let bravo = 2\n  alpha + bravo\n}\n";
    let fixture = analyse(src);

    let alpha_refs = fixture.analysis.references_at(nth(src, "alpha", 0));
    let bravo_refs = fixture.analysis.references_at(nth(src, "bravo", 0));
    assert_eq!(alpha_refs.len(), 2, "declaration + one use");
    assert_eq!(bravo_refs.len(), 2);
    assert!(alpha_refs.iter().all(|span| !bravo_refs.contains(span)), "no overlap");
}

// ---- partial results -----------------------------------------------------

#[test]
fn analysis_survives_a_type_error_elsewhere_in_the_file() {
    // The whole point of the partial path: the file is broken, but the parts
    // that checked still answer hover.
    let src =
        "fn good(): int {\n  let tally = 1\n  tally\n}\nfn bad(): int {\n  \"not an int\"\n}\n";
    let program = parse(src).expect("parses");
    let (hir, analysis, errors) = analyze(&program);

    assert!(!errors.is_empty(), "the fixture should have a type error");
    assert_eq!(
        analysis.type_label_at(nth(src, "tally", 1), &hir).as_deref(),
        Some("int"),
        "the good function still has types"
    );
}
