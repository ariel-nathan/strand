//! Type checker behaviour, driven through the public API.

use strandc::check::{check, CheckError};
use strandc::hir::{Expr, ExprKind, Hir, Pattern, Tag, Ty};

fn check_src(src: &str) -> Result<Hir, Vec<CheckError>> {
    let program = strandc::parser::parse(src).expect("parse failed");
    check(&program)
}

fn expect_ok(src: &str) -> Hir {
    match check_src(src) {
        Ok(hir) => hir,
        Err(errors) => {
            let joined: Vec<String> = errors.iter().map(|e| e.to_string()).collect();
            panic!("expected a clean check, got:\n  {}", joined.join("\n  "));
        }
    }
}

fn expect_error(src: &str) -> String {
    match check_src(src) {
        Ok(_) => panic!("expected a check error, but it passed"),
        Err(errors) => errors.iter().map(|e| e.message.clone()).collect::<Vec<_>>().join(" | "),
    }
}

#[test]
fn checks_arithmetic_and_locals() {
    let hir = expect_ok("fn add(a: int, b: int): int { a + b }");
    let f = &hir.funcs[0];
    assert_eq!(f.ret, Ty::Int);
    assert_eq!(f.param_count, 2);
    assert!(matches!(f.body.tail.as_deref(), Some(Expr { ty: Ty::Int, .. })));
}

#[test]
fn rejects_implicit_coercion() {
    // §4.2: `"1" + 1` is exactly the bug the language exists to prevent.
    let msg = expect_error(r#"fn f(): int { 1 + "one" }"#);
    assert!(msg.contains("matching types"), "message was: {msg}");
}

#[test]
fn rejects_wrong_return_type() {
    let msg = expect_error("fn f(): int { true }");
    assert!(msg.contains("returns int"), "message was: {msg}");
}

#[test]
fn immutable_bindings_cannot_be_assigned() {
    let msg = expect_error("fn f(): int { let a = 1 a = 2 a }");
    assert!(msg.contains("immutable"), "message was: {msg}");
    expect_ok("fn f(): int { var a = 1 a = 2 a }");
}

#[test]
fn records_check_their_fields() {
    let decl = "type Todo = { id: int, title: string, done: bool }";

    expect_ok(&format!(
        r#"{decl} fn f(): Todo {{ Todo {{ id: 1, title: "x", done: false }} }}"#
    ));

    let missing = expect_error(&format!("{decl} fn f(): Todo {{ Todo {{ id: 1 }} }}"));
    assert!(missing.contains("missing field"), "message was: {missing}");

    let unknown = expect_error(&format!(
        r#"{decl} fn f(): Todo {{ Todo {{ id: 1, title: "x", done: false, oops: 1 }} }}"#
    ));
    assert!(unknown.contains("no field `oops`"), "message was: {unknown}");

    let wrong = expect_error(&format!(
        r#"{decl} fn f(): Todo {{ Todo {{ id: "no", title: "x", done: false }} }}"#
    ));
    assert!(wrong.contains("field `id`"), "message was: {wrong}");
}

#[test]
fn field_access_is_typed() {
    let hir = expect_ok(
        "type Todo = { id: int, title: string }
         fn titleOf(t: Todo): string { t.title }",
    );
    assert_eq!(hir.funcs[0].ret, Ty::Str);

    let msg = expect_error(
        "type Todo = { id: int }
         fn f(t: Todo): int { t.nope }",
    );
    assert!(msg.contains("no field `nope`"), "message was: {msg}");
}

#[test]
fn result_constructors_take_their_other_half_from_context() {
    // `Ok(1)` alone cannot know the error type; the signature supplies it.
    let hir = expect_ok(
        "type AddError = | EmptyTitle
         fn f(): Result<int, AddError> { Ok(1) }
         fn g(): Result<int, AddError> { Err(EmptyTitle) }",
    );
    assert_eq!(hir.funcs.len(), 2);

    let msg = expect_error(
        "type AddError = | EmptyTitle
         fn f(): Result<string, AddError> { Ok(1) }",
    );
    assert!(msg.contains("returns Result"), "message was: {msg}");
}

#[test]
fn try_operator_requires_a_result_returning_function() {
    expect_ok(
        "type E = | Bad
         fn inner(): Result<int, E> { Ok(1) }
         fn outer(): Result<int, E> { Ok(inner()? + 1) }",
    );

    let msg = expect_error(
        "type E = | Bad
         fn inner(): Result<int, E> { Ok(1) }
         fn outer(): int { inner()? }",
    );
    assert!(msg.contains("Result or Option"), "message was: {msg}");
}

#[test]
fn try_rejects_mismatched_error_types() {
    // No error conversion in the POC (docs/abi.md §2).
    let msg = expect_error(
        "type A = | Bad
         type B = | Worse
         fn inner(): Result<int, A> { Ok(1) }
         fn outer(): Result<int, B> { Ok(inner()?) }",
    );
    assert!(msg.contains("propagates"), "message was: {msg}");
}

#[test]
fn match_must_be_exhaustive() {
    expect_ok(
        "type AddError = | EmptyTitle | TooLong(max: int)
         fn f(e: AddError): int {
           match e {
             EmptyTitle => 1,
             TooLong(max) => max,
           }
         }",
    );

    let msg = expect_error(
        "type AddError = | EmptyTitle | TooLong(max: int)
         fn f(e: AddError): int {
           match e {
             EmptyTitle => 1,
           }
         }",
    );
    assert!(msg.contains("TooLong"), "message was: {msg}");
}

#[test]
fn match_on_result_needs_both_arms() {
    let msg = expect_error(
        "type E = | Bad
         fn f(r: Result<int, E>): int {
           match r { Ok(v) => v, }
         }",
    );
    assert!(msg.contains("`Err`"), "message was: {msg}");

    // A wildcard closes the match.
    expect_ok(
        "type E = | Bad
         fn f(r: Result<int, E>): int {
           match r { Ok(v) => v, _ => 0, }
         }",
    );
}

#[test]
fn match_arms_must_agree_on_type() {
    let msg = expect_error(
        r#"fn f(b: bool): int {
             match b { true => 1, false => "no", }
           }"#,
    );
    assert!(msg.contains("arms disagree"), "message was: {msg}");
}

#[test]
fn nested_patterns_bind_through_variants() {
    // The §4.5 match, whose Err(TooLong(max)) arm nests two levels.
    let hir = expect_ok(
        "type AddError = | EmptyTitle | TooLong(max: int)
         fn f(r: Result<int, AddError>): int {
           match r {
             Ok(v) => v,
             Err(EmptyTitle) => 0,
             Err(TooLong(max)) => max,
           }
         }",
    );
    let Some(Expr { kind: ExprKind::Match { arms, .. }, .. }) = hir.funcs[0].body.tail.as_deref()
    else {
        panic!("expected a match tail");
    };
    assert_eq!(arms.len(), 3);
    let Pattern::Tagged { tag: Tag::Err, inner } = &arms[2].pattern else {
        panic!("expected an Err(..) pattern");
    };
    assert!(matches!(inner[0], Pattern::Tagged { tag: Tag::Variant { .. }, .. }));
}

#[test]
fn reports_unknown_names_and_types() {
    assert!(expect_error("fn f(): int { nope }").contains("unknown name"));
    assert!(expect_error("fn f(): Nope { 1 }").contains("unknown type"));
    assert!(expect_error("fn f(): int { nope() }").contains("unknown function"));
}

#[test]
fn if_branches_must_agree() {
    expect_ok("fn f(b: bool): int { if b { 1 } else { 2 } }");
    let msg = expect_error(r#"fn f(b: bool): int { if b { 1 } else { "x" } }"#);
    assert!(msg.contains("branches disagree"), "message was: {msg}");
}

#[test]
fn if_without_else_is_unit() {
    // The guard style from §4.5: an `if` that returns early.
    expect_ok(
        "type E = | Bad
         fn f(b: bool): Result<int, E> {
           if b { return Err(Bad) }
           Ok(1)
         }",
    );
    let msg = expect_error("fn f(b: bool): int { if b { 1 } }");
    assert!(msg.contains("must have type unit"), "message was: {msg}");
}

#[test]
fn call_arity_and_argument_types_are_checked() {
    let decl = "fn add(a: int, b: int): int { a + b }";
    expect_ok(&format!("{decl} fn f(): int {{ add(1, 2) }}"));
    assert!(expect_error(&format!("{decl} fn f(): int {{ add(1) }}")).contains("2 argument"));
    assert!(expect_error(&format!("{decl} fn f(): int {{ add(1, true) }}")).contains("argument 2"));
}

#[test]
fn argument_labels_must_match_parameter_names() {
    // §4.5 writes `TooLong(max: 200)`, so a wrong label is worth catching.
    let decl = "fn add(a: int, b: int): int { a + b }";
    expect_ok(&format!("{decl} fn f(): int {{ add(a: 1, b: 2) }}"));
    let msg = expect_error(&format!("{decl} fn f(): int {{ add(a: 1, wrong: 2) }}"));
    assert!(msg.contains("not `wrong`"), "message was: {msg}");
}

#[test]
fn accumulates_multiple_errors() {
    let Err(errors) = check_src("fn f(): int { nope }  fn g(): int { alsoNope }") else {
        panic!("expected errors");
    };
    assert_eq!(errors.len(), 2, "each unknown name should be reported exactly once");
}

#[test]
fn optional_sugar_resolves_to_option() {
    let hir = expect_ok("fn f(a: string?): int { 1 }");
    assert_eq!(hir.funcs[0].locals[0], Ty::Option(Box::new(Ty::Str)));
}

#[test]
fn checks_the_design_doc_add_todo_shape() {
    // §4.5 without the List/string methods, which need the stdlib (§4.6 defers).
    expect_ok(
        r#"
        type Id = int
        type Todo = { id: Id, title: string, done: bool }
        type AddError = | EmptyTitle | TooLong(max: int)

        fn addTodo(title: string, len: int): Result<Todo, AddError> {
          if len == 0   { return Err(EmptyTitle) }
          if len > 200  { return Err(TooLong(max: 200)) }
          Ok(Todo { id: 1, title: title, done: false })
        }

        fn describe(r: Result<Todo, AddError>): int {
          match r {
            Ok(t)             => 0,
            Err(EmptyTitle)   => 1,
            Err(TooLong(max)) => max,
          }
        }
        "#,
    );
}
