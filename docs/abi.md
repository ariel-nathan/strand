# Strand POC — value representation and host ABI

Working notes for the compiler. Companion to `poc-design-doc.md`; this file
records decisions the design doc leaves open. Scoped to the POC.

## 1. No WASM GC, no Component Model

Core WASM modules, linear memory, our own object layout. Design doc §10 lists
this as the pre-approved fallback for "WASM GC types too immature"; we take it
from the start so the compiler is not blocked on toolchain maturity.

Consequence: types are erased in the emitted WASM but fully known to the
checker. Nothing in POC claims 1-5 (§2) depends on GC types.

## 2. `Result<T,E>` and `Option<T>`: multi-value, never boxed

A function returning `Result<T,E>` returns two WASM values:

    (i32 tag, i64 payload)     tag: 0 = Ok/Some, 1 = Err/None

The payload slot is one 64-bit word holding, per the *static* type at each
site: an `int`, an `f64`'s bits, a `bool`, or a 32-bit pointer into the
actor's linear memory. There is no runtime tag on the payload itself — the
checker knows which arm is which.

`?` compiles to: call, test tag, and on non-zero re-return the pair unchanged.
No allocation, no copy, no branchy unwinding.

**Why not a pointer to a tagged struct.** The POC has no GC. Per §5.1 an
actor's arena is reclaimed in a single deallocation at actor death, and until
then the bump allocator never frees — so every heap allocation is a leak for
the actor's lifetime. §4.3 routes *every* fallible call through `Result`, and
§7's demo calls `addTodo` per keystroke while a debug overlay displays live
per-actor arena sizes. Boxing every `Result` would leak on each fallible call
and make the leak visible in the very overlay meant to demonstrate isolation.

## 3. User sum types: boxed

`type AddError = | EmptyTitle | TooLong(max: int)` becomes a pointer to
`{ i32 tag, fields... }`. An all-niladic enum degrades to a bare `i32` tag.

These are constructed on error paths, so allocation there is acceptable at POC
scale. Revisit if measurement (M5) says otherwise.

## 4. Records

Pointer to a flat struct in linear memory; fields at statically-known offsets,
8-byte slots for uniformity. Immutable by default (§4.2), so updates allocate
a new record — the same leak caveat applies, and the same M5 revisit.

## 5. Strings

Pointer to `{ i32 len, bytes... }`, UTF-8, immutable. No interning in the POC.

## 6. Host ABI

Guests import from module `strand` and export a small set of entry points.
Established in M0, extended per milestone.

Imports:

    strand.log(ptr: i32, len: i32)
    strand.send(to: i32, ptr: i32, len: i32)
    strand.sleep_ms(ms: i64)            // async host call: suspends the fiber

Exports:

    memory
    strand_alloc(size: i32) -> i32      // bump allocator in the guest arena
    strand_main()                       // optional
    strand_on_message(ptr: i32, len: i32)   // optional
