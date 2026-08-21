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

`Model { ...state, draft: x }` is sugar and adds no representation. The checker
binds the spread to a local and turns every field the literal leaves out into
an ordinary field read of it, so the emitter sees the same `MakeRecord` it
always saw — there is no update instruction, and there is no in-place write to
be tempted by later. The binding is the part that matters: without it, a spread
whose base is a call would evaluate that call once per field it filled.

The spread has to come first. Anywhere else and a reader has to scan the whole
literal before knowing whether a field they can see is the one that wins.

## 5. Strings

Pointer to `{ i32 len, bytes... }`, UTF-8, immutable. No interning in the POC.

## 5a. Lists

Pointer to `{ i32 len, <pad>, elements... }`. The header is a whole word so the
elements after it stay 8-byte aligned, which is what lets an element be loaded
by exactly the code that loads a record field.

An element occupies `words(T)` slots, the same rule a record's fields follow.
That matters more than it looks: a `List<int>` strides by 8, a `List<Todo>`
strides by 8 (a pointer), and a `List<Result<int, E>>` strides by **16**,
because §2 gives `Result` two words. A stride that assumed one word would read
each element's tag out of the previous element's payload.

Immutable, like everything else (§4.2). `push` allocates a new list one longer
and copies — O(n), and honest at POC scale. The alternative is a growable
buffer with a capacity nobody can see, which is a different design rather than
a faster version of this one.

An empty literal takes its element type from context and is refused where there
is none: a `List<?>` has no representation, and guessing would surface as a
confusing failure much later.

`for x in list { ... }` walks it. The loop is an *expression* of type unit, so
it can stand among a container's children (§6.2) exactly the way `if` does —
and because a view appends as it evaluates, a `for` in a children block needs
no special handling at all. Each iteration simply appends, and the parent's
child count comes out right on its own.

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
    strand_view()          // reset, then draw the actor as it currently is

`strand_view` exists only on an actor that declares a `view fn`. It takes no
arguments because §6.5 makes a view a pure function of state, and the state
global is the only state there is — so it resets the arena, reads global 1, and
calls the view.

The runtime calls it after `strand_main` and after every message, then hands
the region to whoever asked for frames. It knows *where* a frame is and nothing
about what one means: layout, widgets and the compositor live on the far side
of a one-method trait, which is what keeps the actor runtime free of the
renderer.

## 8a. Generated string helpers

`+` on strings and the handful of functions in `crates/strandc/src/stdlib.rs`
compile to WASM the emitter generates into the module itself — not to host
imports. A program full of them still instantiates with nothing linked, and
`strand run` needs no runtime under it.

    str(value: int): string       // decimal
    char(code: int): string       // one character, from a scalar value
    len(s: string): int           // characters, not bytes
    isEmpty(s: string): bool      // len(s) == 0, built from pieces that exist
    trim(s: string): string
    dropLast(s: string): string   // what Backspace does

Only the helpers a module calls are emitted, the same rule imports follow, and
they are laid out last so their presence cannot move an index anything else
computed.

Three properties worth stating, because each is a bug someone would otherwise
write:

- **Strings stay immutable** (§5). A helper never edits its argument; it
  allocates. `a + b` leaves both `a` and `b` usable, which is what makes
  `first + (a + b)` mean what it reads as.
- **Characters, not bytes**, wherever a count is user-visible. A UTF-8
  continuation byte is `0b10xxxxxx`, so `dropLast` steps back over them and
  removes a whole character rather than leaving a broken tail.
- **`str` reads the magnitude unsigned.** Negating `int`'s most negative value
  wraps to itself; read without a sign, that bit pattern is exactly the
  magnitude wanted. Done the obvious way it prints one wrong digit or loops.

`+` on a string and a number is still rejected — §4.2's complaint about JS is
`"1" + 1`, not `"a" + "b"`, and mixed operands never reach the concatenation
path.

## 9. Input: the platform's own message type

A UI actor receives input as ordinary messages (§6.1, §6.5), so its mailbox
carries a type the platform declares:

    Click(id: int)
    Typed(ch: int)          // the Unicode scalar value
    Backspace
    Enter
    Escape
    Focus(id: int)          // 0 when nothing holds focus
    Scrolled(id: int, offset: float)

Every field is `int` or `float`, so §7's flatness rule is satisfied without an
exception: input crosses into the actor's arena carrying no pointers, and the
encoder that puts it on the wire is the same one the CLI uses for any message.

**Why the platform declares it rather than matching names.** The alternative
was to let an actor declare its own event type and have the host fill in
variants whose names it recognised. That is a protocol held together by
spelling — rename `Click` to `Pressed` and the actor silently stops receiving
clicks. Declaring it here means the checker knows the type, `match` is
exhaustive over it, and a typo is a compile error.

**Why it is opt-in.** Registering the type also registers `Click`, `Enter` and
the rest as constructors, and those are ordinary names a UI program might want.
So it appears only in a module that writes `message: Input`, and a module that
declares its own `type Input` keeps its own. A file that never mentions it
reserves nothing.

`crates/strandc/src/input.rs` is the single table, read by the checker and by
the host's translation from `InputEvent` — the same discipline `ui.rs` imposes
on the frame.

The arena is fixed at 2048 nodes (`ui::NODE_CAPACITY`) and sits between the
static data and the bump heap. A view that exceeds it traps rather than
growing — following the arena discipline `docs/inspiration-canon.md` takes from
TigerBeetle, and because a trap arrives as a crash report naming the actor
(§8.4) while a silent truncation arrives as a view that stopped drawing halfway
down.

`crates/strandc/src/ui.rs` is the single table describing all of this. The
parser, the checker, codegen and the host's decoder all read it, so the two
ends of the boundary cannot disagree about a byte.
