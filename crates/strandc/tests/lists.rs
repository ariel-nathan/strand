//! `List<T>` and `for`, run rather than read.
//!
//! Lists are the first thing in the language with a *dynamic* shape, so the
//! bugs available here are new: a stride computed from the wrong type, an
//! element loaded from one slot short, a loop that runs `n+1` times or none.
//! Every case below compiles real Strand, validates the module and executes it.
//!
//! The element types are deliberately varied. A `List<int>` is one word,
//! a `List<Todo>` is a pointer, and a `List<Result<int, E>>` is two words —
//! and a stride that assumed one of those would pass the other's tests.

use wasmtime::{Engine, Instance, Module, Store};

fn build(src: &str) -> Vec<u8> {
    let hir = match strandc::compile("lists.str", src) {
        Ok(hir) => hir,
        Err(report) => panic!("{:?}", miette::Report::new(report)),
    };
    let wasm = strandc::codegen::emit(&hir).expect("emit failed");
    if let Err(e) = wasmparser::validate(&wasm) {
        panic!("emitted invalid WASM: {e}");
    }
    wasm
}

fn run_int(src: &str) -> i64 {
    let wasm = build(src);
    let engine = Engine::default();
    let module = Module::new(&engine, &wasm).expect("wasmtime rejected the module");
    let mut store = Store::new(&engine, ());
    let instance = Instance::new(&mut store, &module, &[]).expect("instantiation failed");
    let main = instance.get_typed_func::<(), i64>(&mut store, "main").expect("no main");
    main.call(&mut store, ()).expect("trap")
}

fn run_string(src: &str) -> String {
    let wasm = build(src);
    let engine = Engine::default();
    let module = Module::new(&engine, &wasm).expect("wasmtime rejected the module");
    let mut store = Store::new(&engine, ());
    let instance = Instance::new(&mut store, &module, &[]).expect("instantiation failed");
    let main = instance.get_typed_func::<(), i32>(&mut store, "main").expect("no main");
    let pointer = main.call(&mut store, ()).expect("trap") as usize;

    let memory = instance.get_memory(&mut store, "memory").expect("no memory");
    let bytes = memory.data(&store);
    let len = u32::from_le_bytes(bytes[pointer..pointer + 4].try_into().unwrap()) as usize;
    String::from_utf8(bytes[pointer + 4..pointer + 4 + len].to_vec()).expect("invalid UTF-8")
}

fn expect_error(src: &str) -> String {
    let report = strandc::compile("lists.str", src).expect_err("should not compile");
    format!("{:?}", miette::Report::new(report))
}

// ---- literals and length --------------------------------------------------

#[test]
fn a_list_knows_how_long_it_is() {
    assert_eq!(run_int("fn main(): int { len([1, 2, 3]) }"), 3);
    assert_eq!(run_int("fn main(): int { len([7]) }"), 1);
}

#[test]
fn an_empty_list_is_empty() {
    assert_eq!(run_int("fn main(): int { let xs: List<int> = []\n  len(xs) }"), 0);
}

#[test]
fn emptiness_reads_the_same_on_a_list_as_on_a_string() {
    // One name, decided by the argument. Two names for one question would be
    // the worse trade.
    let src = "\
fn main(): int {
  let xs: List<int> = []
  if isEmpty(xs) && isEmpty(\"\") && !isEmpty([1]) { 1 } else { 0 }
}
";
    assert_eq!(run_int(src), 1);
}

#[test]
fn an_empty_list_with_nothing_to_learn_from_is_refused() {
    // A `List<?>` has no representation, so guessing would surface as a
    // confusing failure much later.
    let error = expect_error("fn main(): int { len([]) }");
    assert!(error.contains("told what it holds"), "{error}");
}

#[test]
fn elements_must_agree() {
    let error = expect_error(r#"fn main(): int { len([1, "two"]) }"#);
    assert!(error.contains("mismatched element"), "{error}");
}

// ---- for ------------------------------------------------------------------

#[test]
fn for_visits_every_element_once() {
    let src = "\
fn main(): int {
  var total = 0
  for n in [1, 2, 3, 4] {
    total = total + n
  }
  total
}
";
    assert_eq!(run_int(src), 10);
}

#[test]
fn for_over_an_empty_list_runs_nothing() {
    // The loop condition is checked before the body, not after.
    let src = "\
fn main(): int {
  var count = 0
  let xs: List<int> = []
  for n in xs {
    count = count + 1
  }
  count
}
";
    assert_eq!(run_int(src), 0);
}

#[test]
fn for_visits_in_order() {
    let src = "\
fn main(): string {
  var out = \"\"
  for word in [\"a\", \"b\", \"c\"] {
    out = out + word
  }
  out
}
";
    assert_eq!(run_string(src), "abc");
}

#[test]
fn the_loop_variable_is_scoped_to_the_body() {
    let error = expect_error(
        "fn main(): int {\n  for n in [1] { }\n  n\n}",
    );
    assert!(error.contains("unknown name `n`"), "{error}");
}

#[test]
fn loops_nest() {
    let src = "\
fn main(): int {
  var total = 0
  for a in [1, 2, 3] {
    for b in [10, 20] {
      total = total + a * b
    }
  }
  total
}
";
    assert_eq!(run_int(src), 180);
}

#[test]
fn for_needs_a_list() {
    let error = expect_error("fn main(): int {\n  for c in \"abc\" { }\n  1\n}");
    assert!(error.contains("`for` needs a list"), "{error}");
}

// ---- push -----------------------------------------------------------------

#[test]
fn push_returns_a_longer_list() {
    let src = "\
fn main(): int {
  let one = [1, 2]
  let two = push(one, 3)
  len(two)
}
";
    assert_eq!(run_int(src), 3);
}

#[test]
fn push_leaves_the_original_alone() {
    // §4.2 makes data immutable, so appending copies. If `push` wrote into its
    // argument, the first list would have grown too.
    let src = "\
fn main(): int {
  let one = [1, 2]
  let two = push(one, 3)
  len(one) * 10 + len(two)
}
";
    assert_eq!(run_int(src), 23);
}

#[test]
fn push_puts_the_new_element_last() {
    let src = "\
fn main(): string {
  var out = \"\"
  for word in push([\"a\", \"b\"], \"c\") {
    out = out + word
  }
  out
}
";
    assert_eq!(run_string(src), "abc");
}

#[test]
fn a_list_can_be_built_up_one_element_at_a_time() {
    // The shape every list-transforming function takes without closures.
    let src = "\
fn main(): int {
  var doubled: List<int> = []
  for n in [1, 2, 3] {
    doubled = push(doubled, n * 2)
  }
  var total = 0
  for n in doubled {
    total = total + n
  }
  total
}
";
    assert_eq!(run_int(src), 12);
}

#[test]
fn pushing_the_wrong_type_is_refused() {
    let error = expect_error(r#"fn main(): int { len(push([1], "two")) }"#);
    assert!(error.contains("mismatched element"), "{error}");
}

// ---- element types wider and narrower than one word -----------------------

#[test]
fn a_list_of_records_keeps_each_one_whole() {
    // Elements are pointers here. A stride that counted the record's fields
    // instead of its representation would read past the end.
    let src = "\
type Todo = { title: string, done: bool }

fn main(): string {
  let todos = [
    Todo { title: \"a\", done: true },
    Todo { title: \"b\", done: false },
    Todo { title: \"c\", done: true },
  ]
  var out = \"\"
  for todo in todos {
    out = out + todo.title
    if todo.done { out = out + \"!\" }
  }
  out
}
";
    assert_eq!(run_string(src), "a!bc!");
}

#[test]
fn a_list_of_floats_survives_its_own_width() {
    let src = "\
fn main(): int {
  var total = 0.0
  for x in [1.5, 2.25, 0.25] {
    total = total + x
  }
  if total == 4.0 { 1 } else { 0 }
}
";
    assert_eq!(run_int(src), 1);
}

#[test]
fn a_list_of_results_takes_two_words_each() {
    // `Result` crosses as `(tag, payload)` — two words — so its stride is
    // double an int's. A single-word stride would read each element's tag from
    // the previous element's payload.
    let src = "\
type Bad = | Nope

fn main(): int {
  let attempts: List<Result<int, Bad>> = [Ok(1), Err(Nope), Ok(41)]
  var total = 0
  for attempt in attempts {
    // An arm is an expression, not a statement, so the assignment is outside.
    total = total + match attempt {
      Ok(n)  => n,
      Err(e) => 0,
    }
  }
  total
}
";
    assert_eq!(run_int(src), 42);
}

#[test]
fn a_list_of_sums_round_trips() {
    let src = "\
type Colour = | Red | Green | Blue

fn name(c: Colour): string {
  match c {
    Red => \"r\",
    Green => \"g\",
    Blue => \"b\",
  }
}

fn main(): string {
  var out = \"\"
  for c in [Red, Blue, Green] {
    out = out + name(c)
  }
  out
}
";
    assert_eq!(run_string(src), "rbg");
}

#[test]
fn a_list_of_lists_is_just_a_list_of_pointers() {
    let src = "\
fn main(): int {
  var total = 0
  for inner in [[1, 2], [3], [4, 5, 6]] {
    total = total + len(inner)
  }
  total
}
";
    assert_eq!(run_int(src), 6);
}

// ---- the todo app's shape -------------------------------------------------

#[test]
fn a_list_can_be_rebuilt_with_one_element_changed() {
    // Toggling an item, without closures or mutation: walk the list and build
    // a new one, swapping the element that matches.
    let src = "\
type Todo = { id: int, title: string, done: bool }

fn toggle(todos: List<Todo>, id: int): List<Todo> {
  var out: List<Todo> = []
  for todo in todos {
    if todo.id == id {
      out = push(out, Todo { id: todo.id, title: todo.title, done: !todo.done })
    } else {
      out = push(out, todo)
    }
  }
  out
}

fn summary(todos: List<Todo>): string {
  var out = \"\"
  for todo in todos {
    out = out + match todo.done {
      true  => \"[x]\",
      false => \"[ ]\",
    }
  }
  out
}

fn main(): string {
  let todos = [
    Todo { id: 1, title: \"a\", done: false },
    Todo { id: 2, title: \"b\", done: false },
  ]
  summary(todos) + \" -> \" + summary(toggle(todos, 2))
}
";
    assert_eq!(run_string(src), "[ ][ ] -> [ ][x]");
}

#[test]
fn a_list_can_be_rebuilt_with_one_element_removed() {
    let src = "\
type Todo = { id: int, title: string }

fn without(todos: List<Todo>, id: int): List<Todo> {
  var out: List<Todo> = []
  for todo in todos {
    if todo.id != id { out = push(out, todo) }
  }
  out
}

fn titles(todos: List<Todo>): string {
  var out = \"\"
  for todo in todos { out = out + todo.title }
  out
}

fn main(): string {
  let todos = [
    Todo { id: 1, title: \"a\" },
    Todo { id: 2, title: \"b\" },
    Todo { id: 3, title: \"c\" },
  ]
  titles(without(todos, 2))
}
";
    assert_eq!(run_string(src), "ac");
}

#[test]
fn a_long_list_grows_the_arena_rather_than_overrunning_it() {
    // Every `push` allocates a whole new list, so this asks the bump allocator
    // for around 200 allocations and several pages of memory.
    let src = "\
fn main(): int {
  var xs: List<int> = []
  var i = 0
  for n in [1, 2, 3, 4, 5, 6, 7, 8, 9, 10] {
    for m in [1, 2, 3, 4, 5, 6, 7, 8, 9, 10] {
      xs = push(xs, n * m)
    }
  }
  var total = 0
  for x in xs { total = total + x }
  total
}
";
    // (1+..+10) squared.
    assert_eq!(run_int(src), 55 * 55);
}
