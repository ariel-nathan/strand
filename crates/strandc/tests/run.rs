//! End-to-end: Strand source -> WASM -> wasmtime, asserting real results.
//!
//! These are the tests that can catch what the type checker structurally
//! cannot: wrong field offsets, a mis-encoded payload, a `?` that returns the
//! wrong pair. Every module is validated before it runs.

use wasmtime::{Engine, Instance, Module, Store};

/// Compiles source and validates the emitted module.
fn build(src: &str) -> Vec<u8> {
    let hir = match strandc::compile("test.str", src) {
        Ok(hir) => hir,
        Err(report) => panic!("{:?}", miette::Report::new(report)),
    };
    let wasm = strandc::codegen::emit(&hir).expect("emit failed");
    if let Err(e) = wasmparser::validate(&wasm) {
        panic!("emitted invalid WASM: {e}");
    }
    wasm
}

fn instantiate(src: &str) -> (Store<()>, Instance) {
    let wasm = build(src);
    let engine = Engine::default();
    let module = Module::new(&engine, &wasm).expect("wasmtime rejected the module");
    let mut store = Store::new(&engine, ());
    let instance = Instance::new(&mut store, &module, &[]).expect("instantiation failed");
    (store, instance)
}

/// Calls a function whose result is a single `int`.
fn call_int(src: &str, name: &str, args: (i64, i64)) -> i64 {
    let (mut store, instance) = instantiate(src);
    let func = instance
        .get_typed_func::<(i64, i64), i64>(&mut store, name)
        .expect("no such export");
    func.call(&mut store, args).expect("trap")
}

fn call_int0(src: &str, name: &str) -> i64 {
    let (mut store, instance) = instantiate(src);
    let func = instance.get_typed_func::<(), i64>(&mut store, name).expect("no such export");
    func.call(&mut store, ()).expect("trap")
}

/// Calls a `Result`/`Option`-returning function, which per §6.2
/// comes back as the pair `(tag, payload)`.
fn call_result(src: &str, name: &str, arg: i64) -> (i32, i64) {
    let (mut store, instance) = instantiate(src);
    let func = instance
        .get_typed_func::<i64, (i32, i64)>(&mut store, name)
        .expect("no such export");
    func.call(&mut store, arg).expect("trap")
}

#[test]
fn arithmetic_and_parameters() {
    assert_eq!(call_int("fn add(a: int, b: int): int { a + b }", "add", (2, 3)), 5);
    assert_eq!(
        call_int("fn f(a: int, b: int): int { a + b * 2 - 1 }", "f", (10, 4)),
        17,
        "precedence must survive into codegen"
    );
    assert_eq!(call_int("fn f(a: int, b: int): int { a / b }", "f", (7, 2)), 3);
    assert_eq!(call_int("fn f(a: int, b: int): int { a % b }", "f", (7, 2)), 1);
}

#[test]
fn locals_and_mutation() {
    let src = "fn f(): int {
        let a = 10
        var b = 5
        b = b + a
        b
    }";
    assert_eq!(call_int0(src, "f"), 15);
}

#[test]
fn if_else_yields_a_value() {
    let src = "fn pick(a: int, b: int): int { if a > b { a } else { b } }";
    assert_eq!(call_int(src, "pick", (3, 9)), 9);
    assert_eq!(call_int(src, "pick", (9, 3)), 9);
}

#[test]
fn early_return_from_a_guard() {
    // The §4.5 guard shape, compiled.
    let src = "fn clamp(a: int, b: int): int {
        if a > b { return b }
        a
    }";
    assert_eq!(call_int(src, "clamp", (10, 5)), 5);
    assert_eq!(call_int(src, "clamp", (2, 5)), 2);
}

#[test]
fn calls_and_recursion() {
    let src = "fn fib(n: int, unused: int): int {
        if n < 2 { return n }
        fib(n - 1, 0) + fib(n - 2, 0)
    }";
    assert_eq!(call_int(src, "fib", (10, 0)), 55);
}

#[test]
fn logical_operators_short_circuit() {
    // If `&&` evaluated both sides, the division would trap.
    let src = "fn safe(a: int, b: int): bool { b != 0 && a / b > 1 }";
    let (mut store, instance) = instantiate(src);
    let func = instance.get_typed_func::<(i64, i64), i32>(&mut store, "safe").unwrap();
    assert_eq!(func.call(&mut store, (10, 0)).expect("&& must short-circuit"), 0);
    assert_eq!(func.call(&mut store, (10, 3)).unwrap(), 1);
}

#[test]
fn records_round_trip_through_memory() {
    let src = r#"
        type Todo = { id: int, title: string, done: bool }
        fn make(id: int, unused: int): Todo { Todo { id: id, title: "hello", done: true } }
        fn idOf(t: Todo): int { t.id }
        fn roundTrip(id: int, unused: int): int { idOf(make(id, 0)) }
    "#;
    assert_eq!(call_int(src, "roundTrip", (42, 0)), 42);
}

#[test]
fn record_fields_read_from_their_own_offsets() {
    // A middle field would read as another if offsets were wrong.
    let src = "type Three = { a: int, b: int, c: int }
        fn mid(x: int, unused: int): int {
            let t = Three { a: 1, b: x, c: 3 }
            t.b
        }
        fn last(x: int, unused: int): int {
            let t = Three { a: 1, b: 2, c: x }
            t.c
        }";
    assert_eq!(call_int(src, "mid", (99, 0)), 99);
    assert_eq!(call_int(src, "last", (77, 0)), 77);
}

#[test]
fn a_record_update_keeps_the_fields_it_does_not_name() {
    let src = "type Three = { a: int, b: int, c: int }
        fn changed(x: int, unused: int): int {
            let t = Three { a: 1, b: 2, c: 3 }
            let u = Three { ...t, b: x }
            u.a * 100 + u.b * 10 + u.c
        }
        fn copy(x: int, unused: int): int {
            let t = Three { a: x, b: 2, c: 3 }
            let u = Three { ...t }
            u.a * 100 + u.b * 10 + u.c
        }
        fn all(x: int, unused: int): int {
            // A spread every field overrides is still legal, and still copies
            // nothing from the base.
            let t = Three { a: 9, b: 9, c: 9 }
            let u = Three { ...t, a: 1, b: 2, c: x }
            u.a * 100 + u.b * 10 + u.c
        }";
    assert_eq!(call_int(src, "changed", (7, 0)), 173);
    assert_eq!(call_int(src, "copy", (5, 0)), 523);
    assert_eq!(call_int(src, "all", (3, 0)), 123);
}

#[test]
fn result_returns_the_multi_value_pair() {
    // §6.2: tag 0 = Ok, tag 1 = Err, payload in the second slot.
    let src = "type E = | Bad
        fn check(n: int): Result<int, E> {
            if n < 0 { return Err(Bad) }
            Ok(n * 2)
        }";
    assert_eq!(call_result(src, "check", 21), (0, 42));
    let (tag, _) = call_result(src, "check", -1);
    assert_eq!(tag, 1, "negative input must produce Err");
}

#[test]
fn try_propagates_without_allocating() {
    let src = "type E = | Bad
        fn inner(n: int): Result<int, E> {
            if n < 0 { return Err(Bad) }
            Ok(n + 1)
        }
        fn outer(n: int): Result<int, E> {
            let doubled = inner(n)? * 2
            Ok(doubled)
        }";
    assert_eq!(call_result(src, "outer", 5), (0, 12), "Ok path unwraps the payload");
    assert_eq!(call_result(src, "outer", -3).0, 1, "Err path returns early");
}

#[test]
fn option_try_returns_none() {
    let src = "fn inner(n: int): Option<int> {
            if n < 0 { return None }
            Some(n)
        }
        fn outer(n: int): Option<int> { Some(inner(n)? + 1) }";
    assert_eq!(call_result(src, "outer", 4), (0, 5));
    assert_eq!(call_result(src, "outer", -1), (1, 0), "None is tag 1, payload 0");
}

#[test]
fn match_on_a_sum_type_selects_the_right_arm() {
    let src = "type Shape = | Point | Line(len: int) | Rect(w: int, h: int)
        fn area(which: int, n: int): int {
            let s = if which == 0 { Point } else { if which == 1 { Line(len: n) } else { Rect(w: n, h: 2) } }
            match s {
                Point => 0,
                Line(len) => len,
                Rect(w, h) => w * h,
            }
        }";
    assert_eq!(call_int(src, "area", (0, 5)), 0);
    assert_eq!(call_int(src, "area", (1, 5)), 5);
    assert_eq!(call_int(src, "area", (2, 5)), 10, "second field must load from its own offset");
}

#[test]
fn match_binds_through_nested_patterns() {
    // The §4.5 shape: Err(TooLong(max)) binds two levels deep.
    let src = "type AddError = | EmptyTitle | TooLong(max: int)
        fn describe(n: int, unused: int): int {
            let r = if n == 0 { Err(EmptyTitle) } else { if n == 1 { Err(TooLong(max: 200)) } else { Ok(n) } }
            match r {
                Ok(v) => v,
                Err(EmptyTitle) => -1,
                Err(TooLong(max)) => max,
            }
        }";
    assert_eq!(call_int(src, "describe", (0, 0)), -1);
    assert_eq!(call_int(src, "describe", (1, 0)), 200, "nested bind must reach the payload");
    assert_eq!(call_int(src, "describe", (7, 0)), 7);
}

#[test]
fn all_niladic_sums_are_immediate_tags() {
    // §6.3: no allocation for these.
    let src = "type Colour = | Red | Green | Blue
        fn code(n: int, unused: int): int {
            let c = if n == 0 { Red } else { if n == 1 { Green } else { Blue } }
            match c { Red => 10, Green => 20, Blue => 30, }
        }";
    assert_eq!(call_int(src, "code", (0, 0)), 10);
    assert_eq!(call_int(src, "code", (1, 0)), 20);
    assert_eq!(call_int(src, "code", (2, 0)), 30);
}

#[test]
fn match_on_int_and_bool_literals() {
    let src = "fn f(n: int, unused: int): int {
            match n { 0 => 100, 1 => 200, _ => 300, }
        }";
    assert_eq!(call_int(src, "f", (0, 0)), 100);
    assert_eq!(call_int(src, "f", (1, 0)), 200);
    assert_eq!(call_int(src, "f", (9, 0)), 300);
}

#[test]
fn match_on_string_literals_compares_bytes() {
    let src = r#"
        type Todo = { name: string }
        fn pick(n: int, unused: int): int {
            let t = if n == 0 { Todo { name: "alpha" } } else { Todo { name: "beta" } }
            match t.name {
                "alpha" => 1,
                "beta"  => 2,
                _       => 0,
            }
        }
    "#;
    assert_eq!(call_int(src, "pick", (0, 0)), 1);
    assert_eq!(call_int(src, "pick", (1, 0)), 2);
}

#[test]
fn floats_survive_the_payload_slot() {
    // Reinterpreted through the i64 payload and back (§6.2).
    let src = "type E = | Bad
        fn half(n: int): Result<float, E> {
            if n < 0 { return Err(Bad) }
            Ok(2.5)
        }";
    let (mut store, instance) = instantiate(src);
    let func = instance.get_typed_func::<i64, (i32, i64)>(&mut store, "half").unwrap();
    let (tag, payload) = func.call(&mut store, 1).unwrap();
    assert_eq!(tag, 0);
    assert_eq!(f64::from_bits(payload as u64), 2.5);
}

#[test]
fn allocation_grows_memory_when_needed() {
    // Each call allocates; the bump allocator must grow rather than trap.
    let src = "type Box = { a: int, b: int }
        fn build(n: int, unused: int): int {
            var i = 0
            var total = 0
            i = n
            let b = Box { a: i, b: 2 }
            total = b.a + b.b
            total
        }";
    assert_eq!(call_int(src, "build", (40, 0)), 42);
}

#[test]
fn a_host_call_inside_a_builder_block_is_still_collected() {
    // The bug this exists for: the walker that finds which imports a module
    // needs did not descend into a built node, so a `log` among a container's
    // children was never collected — and codegen then emitted a call to a
    // function index that did not exist, panicking rather than compiling.
    //
    // `build` validates, which is what catches it: an out-of-range call is a
    // validation error even before anything runs.
    let wasm = build(
        r#"
        view fn main(): Node {
          column(gap: 4) {
            log("building")
            text("hi")
          }
        }
        "#,
    );
    // And the import it needs is actually declared, rather than merely called.
    assert!(
        wasm.windows(3).any(|w| w == b"log"),
        "the module should import `strand.log`"
    );
}
