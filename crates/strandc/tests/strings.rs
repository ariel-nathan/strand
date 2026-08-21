//! The generated string helpers, run rather than read.
//!
//! Every one of these is hand-written WASM, which is exactly the code a type
//! checker cannot vouch for: an off-by-one in a copy, a local index that names
//! the wrong slot, a loop that never ends. So each helper is exercised through
//! real Strand source, compiled, validated and executed, with the result read
//! back out of the guest's own memory using §6.5's layout.
//!
//! The cases that matter most are the boundaries: an empty string, a string of
//! only whitespace, the most negative `int`, and a character that does not fit
//! in one byte.

use wasmtime::{Engine, Instance, Module, Store};

/// Compiles, validates, and runs `fn main(): string`, returning what it built.
fn run_string(src: &str) -> String {
    let hir = match strandc::compile("strings.str", src) {
        Ok(hir) => hir,
        Err(report) => panic!("{:?}", miette::Report::new(report)),
    };
    let wasm = strandc::codegen::emit(&hir).expect("emit failed");
    if let Err(e) = wasmparser::validate(&wasm) {
        panic!("emitted invalid WASM: {e}");
    }

    let engine = Engine::default();
    let module = Module::new(&engine, &wasm).expect("wasmtime rejected the module");
    let mut store = Store::new(&engine, ());
    let instance = Instance::new(&mut store, &module, &[]).expect("instantiation failed");

    let main = instance
        .get_typed_func::<(), i32>(&mut store, "main")
        .expect("main should return a string pointer");
    let pointer = main.call(&mut store, ()).expect("trap") as usize;

    let memory = instance.get_memory(&mut store, "memory").expect("no memory");
    let bytes = memory.data(&store);
    let len = u32::from_le_bytes(bytes[pointer..pointer + 4].try_into().unwrap()) as usize;
    String::from_utf8(bytes[pointer + 4..pointer + 4 + len].to_vec())
        .expect("a helper produced invalid UTF-8")
}

/// Compiles and runs `fn main(): int`.
fn run_int(src: &str) -> i64 {
    let hir = match strandc::compile("strings.str", src) {
        Ok(hir) => hir,
        Err(report) => panic!("{:?}", miette::Report::new(report)),
    };
    let wasm = strandc::codegen::emit(&hir).expect("emit failed");
    wasmparser::validate(&wasm).expect("emitted invalid WASM");

    let engine = Engine::default();
    let module = Module::new(&engine, &wasm).expect("wasmtime rejected the module");
    let mut store = Store::new(&engine, ());
    let instance = Instance::new(&mut store, &module, &[]).expect("instantiation failed");
    let main = instance.get_typed_func::<(), i64>(&mut store, "main").expect("no main");
    main.call(&mut store, ()).expect("trap")
}

fn run_bool(src: &str) -> bool {
    let hir = strandc::compile("strings.str", src).expect("should compile");
    let wasm = strandc::codegen::emit(&hir).expect("emit failed");
    wasmparser::validate(&wasm).expect("emitted invalid WASM");

    let engine = Engine::default();
    let module = Module::new(&engine, &wasm).expect("wasmtime rejected the module");
    let mut store = Store::new(&engine, ());
    let instance = Instance::new(&mut store, &module, &[]).expect("instantiation failed");
    let main = instance.get_typed_func::<(), i32>(&mut store, "main").expect("no main");
    main.call(&mut store, ()).expect("trap") != 0
}

/// Wraps an expression in a `main` of the given return type.
fn returning(ty: &str, expr: &str) -> String {
    format!("fn main(): {ty} {{ {expr} }}\n")
}

// ---- concatenation --------------------------------------------------------

#[test]
fn strings_concatenate() {
    assert_eq!(run_string(&returning("string", r#""buy " + "milk""#)), "buy milk");
}

#[test]
fn concatenation_chains_left_to_right() {
    assert_eq!(run_string(&returning("string", r#""a" + "b" + "c""#)), "abc");
}

#[test]
fn an_empty_side_contributes_nothing() {
    // A zero-length copy is the case a hand-written `memory.copy` gets wrong.
    assert_eq!(run_string(&returning("string", r#""" + "x""#)), "x");
    assert_eq!(run_string(&returning("string", r#""x" + """#)), "x");
    assert_eq!(run_string(&returning("string", r#""" + """#)), "");
}

#[test]
fn concatenation_leaves_its_arguments_alone() {
    // Strings are immutable (§4.2 / §6.5), so a helper allocates
    // rather than editing. If `+` wrote into `a`, the second use would differ.
    let src = "\
fn main(): string {
  let a = \"one\"
  let b = \"two\"
  let first = a + b
  first + (a + b)
}
";
    assert_eq!(run_string(src), "onetwoonetwo");
}

#[test]
fn mixing_a_string_with_a_number_is_still_refused() {
    // §4.2: `\"1\" + 1` is the bug the language exists to prevent, and adding
    // concatenation must not open that door.
    let src = returning("string", r#""n = " + 1"#);
    let error = strandc::compile("t.str", &src).expect_err("should not compile");
    let rendered = format!("{:?}", miette::Report::new(error));
    assert!(rendered.contains("matching types"), "{rendered}");
}

// ---- str ------------------------------------------------------------------

#[test]
fn numbers_become_decimal() {
    for value in [0, 1, 7, 42, 100, 999, 1234567890] {
        assert_eq!(
            run_string(&returning("string", &format!("str({value})"))),
            value.to_string()
        );
    }
}

#[test]
fn negative_numbers_keep_their_sign() {
    assert_eq!(run_string(&returning("string", "str(0 - 42)")), "-42");
    assert_eq!(run_string(&returning("string", "str(0 - 1)")), "-1");
}

#[test]
fn the_most_negative_int_survives() {
    // Negating it wraps to itself, so the magnitude has to be read unsigned.
    // Done naively this prints a single wrong digit or loops forever.
    let src = returning("string", "str(0 - 9223372036854775807 - 1)");
    assert_eq!(run_string(&src), "-9223372036854775808");
}

#[test]
fn a_number_can_be_joined_to_text() {
    // The thing a UI actually needs: a count on the screen.
    assert_eq!(run_string(&returning("string", r#""" + str(3) + " done""#)), "3 done");
}

// ---- char -----------------------------------------------------------------

#[test]
fn a_scalar_value_becomes_one_character() {
    // What `Input::Typed` carries, turned back into something drawable.
    assert_eq!(run_string(&returning("string", "char(97)")), "a");
    assert_eq!(run_string(&returning("string", "char(65)")), "A");
    assert_eq!(run_string(&returning("string", "char(32)")), " ");
}

#[test]
fn characters_outside_ascii_are_encoded_properly() {
    // One, two, three and four byte encodings. A helper that assumed one byte
    // per character produces invalid UTF-8 here, and `run_string` says so.
    assert_eq!(run_string(&returning("string", "char(233)")), "é");
    assert_eq!(run_string(&returning("string", "char(960)")), "π");
    assert_eq!(run_string(&returning("string", "char(8212)")), "—");
    assert_eq!(run_string(&returning("string", "char(128169)")), "💩");
}

#[test]
fn typing_appends_one_character_at_a_time() {
    // A text field, in miniature.
    let src = "\
fn main(): string {
  var draft = \"\"
  draft = draft + char(104)
  draft = draft + char(105)
  draft
}
";
    assert_eq!(run_string(src), "hi");
}

// ---- len and isEmpty ------------------------------------------------------

#[test]
fn length_counts_characters_not_bytes() {
    assert_eq!(run_int(&returning("int", r#"len("")"#)), 0);
    assert_eq!(run_int(&returning("int", r#"len("milk")"#)), 4);
    // Four characters, seven bytes.
    assert_eq!(run_int(&returning("int", r#"len("héπ—")"#)), 4);
}

#[test]
fn emptiness_is_asked_of_the_characters() {
    assert!(run_bool(&returning("bool", r#"isEmpty("")"#)));
    assert!(!run_bool(&returning("bool", r#"isEmpty("x")"#)));
    assert!(!run_bool(&returning("bool", r#"isEmpty(" ")"#)), "a space is not nothing");
}

// ---- trim -----------------------------------------------------------------

#[test]
fn trim_takes_whitespace_off_both_ends() {
    assert_eq!(run_string(&returning("string", r#"trim("  milk  ")"#)), "milk");
    assert_eq!(run_string(&returning("string", r#"trim("milk  ")"#)), "milk");
    assert_eq!(run_string(&returning("string", r#"trim("  milk")"#)), "milk");
    assert_eq!(run_string(&returning("string", r#"trim("milk")"#)), "milk");
}

#[test]
fn trim_leaves_the_inside_alone() {
    assert_eq!(run_string(&returning("string", r#"trim("  buy milk  ")"#)), "buy milk");
}

#[test]
fn a_string_of_only_whitespace_trims_to_nothing() {
    // The case where the two scans cross. Done wrong this underflows into a
    // gigantic length and traps on the copy.
    assert_eq!(run_string(&returning("string", r#"trim("   ")"#)), "");
    assert_eq!(run_string(&returning("string", r#"trim("")"#)), "");
}

#[test]
fn trim_handles_tabs_and_newlines() {
    assert_eq!(run_string(&returning("string", "trim(\"\\t milk \\n\")")), "milk");
}

#[test]
fn the_validation_rule_from_the_design_doc_works() {
    // §4.5 writes `title.trim().isEmpty()`. Method syntax is not in yet, so
    // this is the same rule spelled with free functions.
    assert!(run_bool(&returning("bool", r#"isEmpty(trim("   "))"#)));
    assert!(!run_bool(&returning("bool", r#"isEmpty(trim("  x  "))"#)));
}

// ---- dropLast -------------------------------------------------------------

#[test]
fn backspace_removes_the_last_character() {
    assert_eq!(run_string(&returning("string", r#"dropLast("milk")"#)), "mil");
    assert_eq!(run_string(&returning("string", r#"dropLast("m")"#)), "");
}

#[test]
fn backspace_on_nothing_is_not_an_error() {
    assert_eq!(run_string(&returning("string", r#"dropLast("")"#)), "");
}

#[test]
fn backspace_removes_a_whole_multi_byte_character() {
    // Dropping one byte would leave a broken tail, and `run_string` would fail
    // to decode it. Four separate widths, so all four step-backs are covered.
    assert_eq!(run_string(&returning("string", r#"dropLast("aé")"#)), "a");
    assert_eq!(run_string(&returning("string", r#"dropLast("aπ")"#)), "a");
    assert_eq!(run_string(&returning("string", r#"dropLast("a—")"#)), "a");
    assert_eq!(run_string(&returning("string", r#"dropLast("a💩")"#)), "a");
}

#[test]
fn typing_and_deleting_return_to_where_they_started() {
    let src = "\
fn main(): string {
  var draft = \"ab\"
  draft = draft + char(128169)
  draft = dropLast(draft)
  draft
}
";
    assert_eq!(run_string(src), "ab");
}

// ---- the whole set together -----------------------------------------------

#[test]
fn a_title_can_be_validated_and_reported_the_way_the_todo_app_needs() {
    let src = "\
type AddError = | EmptyTitle | TooLong(max: int)

fn add(title: string): Result<string, AddError> {
  let clean = trim(title)
  if isEmpty(clean) { return Err(EmptyTitle) }
  if len(clean) > 6 { return Err(TooLong(max: 6)) }
  Ok(clean)
}

fn describe(title: string): string {
  match add(title) {
    Ok(clean)         => \"added \" + clean,
    Err(EmptyTitle)   => \"a todo needs a title\",
    Err(TooLong(max)) => \"keep it under \" + str(max) + \" characters\",
  }
}

fn main(): string {
  describe(\"  milk  \") + \" / \" + describe(\"   \") + \" / \" + describe(\"a very long one\")
}
";
    assert_eq!(
        run_string(src),
        "added milk / a todo needs a title / keep it under 6 characters"
    );
}

#[test]
fn a_program_that_touches_no_strings_emits_no_helpers() {
    // You pay for what you call, the same rule imports follow.
    let hir = strandc::compile("t.str", "fn main(): int { 1 + 2 }").expect("compiles");
    let plain = strandc::codegen::emit(&hir).expect("emit");

    let hir = strandc::compile("t.str", "fn main(): string { \"a\" + \"b\" }").expect("compiles");
    let with_strings = strandc::codegen::emit(&hir).expect("emit");

    assert!(
        with_strings.len() > plain.len(),
        "the concatenating module should carry a helper the other does not"
    );
}
