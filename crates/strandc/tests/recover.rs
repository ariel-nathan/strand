//! Error recovery, driven through the public API.
//!
//! An editor reparses on every keystroke, so the buffer is usually mid-edit.
//! What matters is that a broken declaration reports itself and then gets out of
//! the way, leaving its neighbours intact — that is what keeps the outline and
//! hover alive while you type.

use strandc::lexer::{lex, lex_recovering, Tok};
use strandc::parser::{parse, parse_recovering};

fn item_names(program: &strandc::ast::Program) -> Vec<String> {
    use strandc::ast::Item;
    program
        .items
        .iter()
        .map(|item| match item {
            Item::Fn(f) => f.name.clone(),
            Item::Type(t) => t.name.clone(),
            Item::Actor(a) => a.name.clone(),
            Item::App(a) => a.name.clone(),
        })
        .collect()
}

#[test]
fn a_clean_program_recovers_to_exactly_what_parse_returns() {
    let src = "type Id = int\nfn one(): int { 1 }\nfn two(): int { 2 }\n";
    let (program, errors) = parse_recovering(src);
    assert!(errors.is_empty(), "unexpected errors: {errors:?}");
    assert_eq!(item_names(&program), ["Id", "one", "two"]);
    assert_eq!(program, parse(src).expect("valid program"));
}

#[test]
fn a_broken_item_does_not_take_its_neighbours_with_it() {
    // `fn broken(` never closes its parameter list.
    let src = "fn before(): int { 1 }\nfn broken(: int { 2 }\nfn after(): int { 3 }\n";
    let (program, errors) = parse_recovering(src);

    assert_eq!(errors.len(), 1, "expected one error, got: {errors:?}");
    assert_eq!(
        item_names(&program),
        ["before", "after"],
        "the items either side of the broken one should survive"
    );
}

#[test]
fn every_broken_item_is_reported_not_just_the_first() {
    let src = "\
fn good_one(): int { 1 }
fn bad_one(: int { 2 }
fn good_two(): int { 3 }
fn bad_two(: int { 4 }
fn good_three(): int { 5 }
fn bad_three(: int { 6 }
";
    let (program, errors) = parse_recovering(src);

    assert_eq!(errors.len(), 3, "expected three errors, got: {errors:?}");
    assert_eq!(item_names(&program), ["good_one", "good_two", "good_three"]);
}

#[test]
fn errors_carry_distinct_positions() {
    let src = "fn a(: int { 1 }\nfn b(): int { 2 }\nfn c(: int { 3 }\n";
    let (_, errors) = parse_recovering(src);
    assert_eq!(errors.len(), 2, "got: {errors:?}");
    assert_ne!(
        errors[0].span.start, errors[1].span.start,
        "each error should point at its own item"
    );
    assert!(errors[0].span.start < errors[1].span.start, "errors should be in source order");
}

#[test]
fn a_leading_error_still_yields_the_rest_of_the_file() {
    // Junk before any declaration: `item` fails without consuming a keyword,
    // which is the case that would spin forever without the progress guard.
    let src = "]]]\nfn after(): int { 1 }\n";
    let (program, errors) = parse_recovering(src);
    assert!(!errors.is_empty());
    assert_eq!(item_names(&program), ["after"]);
}

#[test]
fn trailing_junk_terminates() {
    // Nothing to resync to after the error — the loop must still reach Eof.
    let src = "fn a(): int { 1 }\n@@@\n";
    let (program, errors) = parse_recovering(src);
    assert!(!errors.is_empty());
    assert_eq!(item_names(&program), ["a"]);
}

#[test]
fn types_and_actors_are_resync_points_too() {
    let src = "\
fn bad(: int { 1 }
type Colour = | Red | Green
actor Counter {
  state: Colour
  in inbox: string
  fn init(): Colour { Red }
  on inbox(state: Colour, msg: string): Colour { state }
}
";
    let (program, errors) = parse_recovering(src);
    assert_eq!(errors.len(), 1, "got: {errors:?}");
    assert_eq!(item_names(&program), ["Colour", "Counter"]);
}

// ---- lexer ---------------------------------------------------------------

#[test]
fn the_lexer_steps_over_bad_bytes_and_keeps_going() {
    let src = "fn a() # int $ { 1 }";
    let (tokens, errors) = lex_recovering(src);
    assert_eq!(errors.len(), 2, "both stray characters should be reported: {errors:?}");

    // The surrounding tokens are all still there.
    assert!(tokens.iter().any(|t| t.tok == Tok::Fn));
    assert!(tokens.iter().any(|t| matches!(&t.tok, Tok::Ident(n) if n == "int")));
    assert!(tokens.iter().any(|t| t.tok == Tok::Int(1)));
    assert_eq!(tokens.last().map(|t| &t.tok), Some(&Tok::Eof));
}

#[test]
fn a_lone_ampersand_is_recovered_like_any_other_stray_byte() {
    let (tokens, errors) = lex_recovering("let x = 1 & 2");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains('&'));
    assert!(tokens.iter().any(|t| t.tok == Tok::Int(2)), "lexing continues past the `&`");
}

#[test]
fn an_unterminated_string_still_reaches_eof() {
    let (tokens, errors) = lex_recovering("fn a(): string { \"oops\n}");
    assert!(!errors.is_empty());
    assert_eq!(tokens.last().map(|t| &t.tok), Some(&Tok::Eof), "must terminate");
}

#[test]
fn lex_keeps_its_stop_at_the_first_error_contract() {
    // The batch path is unchanged: one error, no tokens.
    assert!(lex("fn a() # int $ { 1 }").is_err());
}

#[test]
fn parse_keeps_its_all_or_nothing_contract() {
    assert!(parse("fn bad(: int { 1 }").is_err());
}

// ---- depth guard ---------------------------------------------------------

#[test]
fn deeply_nested_input_is_a_diagnostic_not_a_crash() {
    // A server parses whatever is in the buffer. Without a bound this is a
    // stack overflow, which takes the whole process down rather than one file.
    let src = format!("fn deep(): int {{ {}1{} }}", "(".repeat(5000), ")".repeat(5000));
    let (_, errors) = parse_recovering(&src);
    assert!(!errors.is_empty(), "should report rather than overflow");
    assert!(
        errors.iter().any(|e| e.message.contains("too deeply")),
        "expected a depth diagnostic, got: {errors:?}"
    );
}

#[test]
fn ordinary_nesting_is_still_accepted() {
    // Well within the bound — this must not trip the guard.
    let src = format!("fn ok(): int {{ {}1{} }}", "(".repeat(32), ")".repeat(32));
    let (_, errors) = parse_recovering(&src);
    assert!(errors.is_empty(), "unexpected errors: {errors:?}");
}
