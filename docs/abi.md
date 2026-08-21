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

Imports are emitted **only when the program calls them**, so a module that
never touches the host stays standalone and can be instantiated with no imports
at all. Because imports occupy the lowest function indices, every defined
function shifts past them — that offset is applied once, in the emitter.

Callable from Strand today: `log(msg: string)`. It takes a Strand string and
unpacks §5's header into the `(ptr, len)` pair the host expects.

Imports:

    strand.log(ptr: i32, len: i32)
    strand.send(to: i32, ptr: i32, len: i32)
    strand.sleep_ms(ms: i64)            // async host call: suspends the fiber

Exports:

    memory
    strand_alloc(size: i32) -> i32      // bump allocator in the guest arena
    strand_main()                       // optional
    strand_on_message(ptr: i32, len: i32)   // optional

## 7. Channel messages: the wire format is the memory format

A message type is declared on the actor:

    actor Counter {
      state: Count
      message: Msg      // defaults to `string` when omitted
      ...
    }

**Message types must be flat.** A variant's fields may be `int`, `float` or
`bool` — nothing that holds a pointer. The checker enforces this and says why:
a message is copied into a *different* arena, and a pointer from the sender's
arena means nothing there.

That restriction buys the property `docs/inspiration-canon.md` takes from
Cap'n Proto: **the wire format is the memory format.** The bytes on the channel
are exactly §3's boxed-variant layout, so once the runtime has copied them into
the receiving arena with `strand_alloc`, the pointer it already has *is* a
valid value. `strand_on_message` hands it straight to `receive`. There is no
decode step, and adding one would be the bug.

Encoding, matching §3:

| message type | bytes on the wire |
|---|---|
| sum with any payload-carrying variant | `i32 tag` in the first 8-byte slot, then one 8-byte slot per field |
| all-niladic sum | bare `i32` tag |
| `int` / `float` | the 8-byte value |
| `bool` | `i32` 0 or 1 |
| `string` | raw UTF-8 bytes; codegen adds §5's length header on arrival |

`string` is the one relocated case, and it is safe only because codegen knows
that layout and rebuilds the header in the receiving arena.

**Not yet done.** Sending from Strand code needs a `send` builtin, which needs
host imports in emitted modules — the function-index space shifts once imports
exist, so it is a real change rather than an addition. Today the sending half
is exercised by a host encoder that reads layout from the same `Hir`, so both
ends still agree by construction.

## 8. The frame: how a view crosses into the host

A `view fn` (§6.2) does not return a tree. It **appends** to a fixed array in
its own arena and returns nothing — `Node` has no runtime representation at
all, and that absence is the design rather than an optimisation.

Layout, one record per node, 32 bytes:

| offset | field | notes |
|---|---|---|
| 0 | `i32 kind` | which widget (`strandc::ui::NodeKind`) |
| 4 | `i32 child_count` | how many of the preceding roots are this node's |
| 8 | `i32 id` | hit id; 0 means the node takes no input |
| 12 | `i32 flag` | `checked`, `focused` |
| 16 | `i32 text` | §5 string pointer, or 0 |
| 20 | `f32 number` | `gap`, or a scroll's `offset` |
| 24 | `f32 number2` | `padding` |
| 28 | `i32 text2` | a second string, for the one widget needing two |

**The array is post-order.** A view emits as it evaluates, so a container's
children are finished before it is, and it records how many of the unclaimed
roots belong to it. Rebuilding the tree is one left-to-right pass with a stack.
Nothing is moved, nothing is back-patched, and — as in §7 — there is no decode
step: the bytes the guest wrote are the bytes the host reads.

Codegen keeps one counter, `pending`, holding the number of finished roots not
yet claimed by a parent. A builder saves it before its children run and hands
the saved value to `node_push`, which computes `child_count = pending - marker`
and then sets `pending = marker + 1`. That single subtraction replaces the
child-tracking stack a tree builder would otherwise need, and it is what makes
conditional children free: an `if` that does not run appends nothing, so its
parent simply counts one fewer.

**Why `Node` is zero-width.** A node is emitted where it is written, so a value
that could be stored, passed, or used twice would be a node that appears
somewhere other than where it was built. Making the type carry nothing means
the checker rejects `let n = text("hi")` and `fn f(n: Node)` outright, and the
array is in tree order by construction rather than by discipline. The rule is
the same shape as §7's flat-message rule: a restriction that buys a property.

Exports:

    strand_nodes           // i32 global: where the array starts
    strand_node_count      // i32 global: how many records are in it
    strand_frame_reset()   // empty the array before building the next frame

The arena is fixed at 2048 nodes (`ui::NODE_CAPACITY`) and sits between the
static data and the bump heap. A view that exceeds it traps rather than
growing — following the arena discipline `docs/inspiration-canon.md` takes from
TigerBeetle, and because a trap arrives as a crash report naming the actor
(§8.4) while a silent truncation arrives as a view that stopped drawing halfway
down.

`crates/strandc/src/ui.rs` is the single table describing all of this. The
parser, the checker, codegen and the host's decoder all read it, so the two
ends of the boundary cannot disagree about a byte.
