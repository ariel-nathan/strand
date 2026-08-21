# Project Strand — Design Document

*A browser runtime, redesigned: a typed language, a multithreaded actor VM, a declarative scene graph.*

**Status:** Draft v0.2 · **Date:** August 2026 · **Name:** placeholder; rename freely.

Sections 1–9 are the proof of concept (POC). Sections 10–12 are post-POC design
that constrains POC primitives. Sections 13–18 are the plan, the risks, the
decisions and the open problems.

---

## 1. Vision

The web platform fixed its core architecture in the 1990s. A single-threaded
script language drives a retained-mode document model. Types, concurrency,
security and app-style UI all came later, as additions.

Strand asks what the platform looks like if you design it new in 2026, and keeps
a path back to the existing web.

The POC is a full vertical slice. A typed language compiles to a multithreaded
VM. The VM drives a declarative scene graph renderer. A todo application
demonstrates the result. The goal is not a complete layer. The goal is proof
that the layers compose.

## 2. Goals and Non-Goals

The POC must demonstrate five claims:

1. A typed language with a TypeScript flavor. Types survive to runtime. Errors
   are `Result` values. Concurrency is colorless.
2. An actor VM. Components run in isolated arenas, scheduled M:N across OS
   threads, and communicate over typed channels.
3. A declarative UI layer. The scene graph lives on a platform-owned render
   thread. Application code cannot jank the compositor.
4. Crash isolation. An actor that panics dies and is reclaimed. The application
   continues.
5. All of the above, together, in a todo application.

Non-goals, recorded as future work in section 15: backwards compatibility with
HTML, CSS and JS; a self-hosted bytecode VM; content-addressed module
distribution; the capability security model; networking; persistence;
accessibility; text input beyond the todo application's minimum.

## 3. Architecture

```
┌─────────────────────────────────────────────┐
│  Strand language (.str files)               │  TS-like syntax, typed
│  compiler: .str → WASM module               │
├─────────────────────────────────────────────┤
│  Actor runtime                              │  supervision, typed channels,
│  (host functions + scheduler)               │  structured spawn scopes
├─────────────────────────────────────────────┤
│  Execution engine: wasmtime                 │  one Store per actor = arena
│  Scheduler: tokio                           │  M:N fibers over OS threads
├─────────────────────────────────────────────┤
│  Scene graph + renderer: wgpu + winit       │  platform-owned render thread
└─────────────────────────────────────────────┘
```

**The implementation language is Rust.** wasmtime and cranelift give the
execution engine. tokio gives the M:N scheduler. wgpu gives the portable
renderer. Rust ownership expresses the core invariant directly: a buffer
transfers, and it never shares. wasmtime, Deno core and wasmer all chose Rust,
so prior art is available.

**Embed, do not build.** The POC embeds wasmtime instead of writing a bytecode
engine. That turns a year-long VM project into an integration project of weeks,
and it still proves the claims. A custom VM is future work, in Zig or another
language with comptime that suits interpreter dispatch.

## 4. The Strand Language

### 4.1 Position

Strand is TypeScript with the lessons applied. The curly-brace syntax reads on
sight. The semantic holes are closed. The checker knows every type; section 6.1
says what the emitted WASM keeps.

### 4.2 Lessons from JS

| JS scar tissue | Strand decision |
|---|---|
| `null` and `undefined` | One `Option<T>`. `string?` is `Option<string>` |
| Implicit coercion (`"1" + 1`) | None. `==` has `===` semantics |
| `this` binding | No `this`. A method takes an explicit receiver. UI is functions |
| try/catch is invisible in a signature | `Result<T, E>` in the signature. `?` propagates it (§4.3) |
| Unhandled rejections | No promises. Concurrency is colorless (§4.4) |
| async/await coloring | No `async` keyword. A blocking call suspends the fiber |
| The ESM and CJS split; imports with side effects | One static module format. An import never runs code |
| Mutable by default | `let` is immutable, `var` is mutable. Data structures are immutable |
| No standard library | Batteries included from day one (§11.7) |

### 4.3 Errors — two tiers, no try/catch

**Tier 1. An expected failure is a value.** A fallible function returns
`Result<T, E>`. The `?` operator sends the error to the caller, so the happy
path stays straight. Rust, Swift, Zig and Gleam agree on this. Go shows the cost
of results without `?`.

**Tier 2. A bug is a panic.** An out-of-bounds access, a failed assertion or an
overflow means a broken invariant, so the failing unit stops. A panic kills the
current actor only. There is no catch. Recovery is the supervisor's job (§5.4).
This composes with the arena model: one deallocation reclaims a dead actor.

### 4.4 Concurrency — colorless, structured, actor-isolated

**Inside an actor, a blocking call is colorless.** Any function can suspend.
There is no `async` keyword and no function coloring. `sleep(1s)` blocks the
*fiber*, and the scheduler runs other fibers on that OS thread. This costs
almost nothing, because the runtime needs an M:N scheduler anyway.

**Inside an actor, a spawn is structured.** A child cannot outlive its scope.
Results join at the end of the scope. Cancel the scope, and the children cancel.
This closes the goroutine-leak hole.

```strand
fn loadDashboard(): Result<Dashboard, LoadError> {
  scope {
    let user  = spawn fetchUser()?
    let todos = spawn fetchTodos()?
    Ok(Dashboard { user: user.join()?, todos: todos.join()? })
  } // scope exit: all children join or cancel — a leak is impossible
}
```

**Between actors, there are only messages.** Actors share no memory. A channel
is typed. A send *transfers ownership* of a buffer, and the sender loses access.
The Rust host enforces this at no cost, so a data race is not representable.

### 4.5 Syntax sample

```strand
type Todo = { id: Id, title: string, done: bool }

type AddError = | EmptyTitle | TooLong(max: int)

fn addTodo(list: List<Todo>, title: string): Result<List<Todo>, AddError> {
  if title.trim().isEmpty() { return Err(EmptyTitle) }
  if title.len() > 200      { return Err(TooLong(max: 200)) }
  Ok(list.push(Todo { id: Id.new(), title, done: false }))
}

match addTodo(todos, input) {
  Ok(next)          => state.todos = next,
  Err(EmptyTitle)   => state.notice = Some("Title can't be empty"),
  Err(TooLong(max)) => state.notice = Some(`Max ${max} characters`),
}
```

Data is immutable (§4.2), so a state transition is a whole new record. The
spread says which fields differ instead of restating the ones that do not:

```strand
fn withDraft(state: Model, draft: string): Model {
  Model { ...state, draft: draft, notice: "" }
}
```

### 4.6 POC compiler scope

The lexer is hand-written. The parser is recursive descent. The checker is
bidirectional: a signature needs annotations, and the checker infers a local.
Full inference is future work. The compiler emits WASM through `wasm-encoder`.

The subset is fixed: primitives (`int`, `float`, `bool`, `string`), records,
`List` and `Map`, sum types and `match`, `Option`, `Result` and `?`, functions
and closures, `scope`, `spawn` and `join`, actor declarations, and the UI
builtins (§7). Everything else is out.

### 4.7 The actor surface

An actor declares its state, its channels, and a handler per inbound channel.
The channels are named; nothing else about the other end is sayable.

```strand
actor Meter {
  state: Reading
  in  input:  Input       // the platform's events (§6.11)
  in  life:   Lifecycle   // news about its peers (§6.13)
  out totals: Total       // what it can say

  fn init(): Reading { Reading { total: 0, samples: 0 } }

  on input(state: Reading, msg: Input): Reading {
    let next = advance(state, msg)
    send(totals, Now(total: next.total, samples: next.samples))
    next
  }

  on life(state: Reading, msg: Lifecycle): Reading { state }
}

app Pipeline {
  meter    = Meter
  reporter = Reporter

  meter.totals -> reporter.totals
}
```

Three rules carry most of the weight, and §6.8 gives the reasons:

- A handler is named after the port it serves, so a channel and its code cannot
  drift apart.
- `send` names one of the actor's own out ports. **No expression in the language
  names another actor**, so an actor reaches exactly the peers the `app` block
  wired it to.
- `panic(msg)` ends the actor and nothing else (§4.3). Its type is `Never`, so
  it can stand as a match arm or as a tail where a value was owed.

`send` is unavailable inside a `view fn`. A view is a pure function of state
(§7.5) and the platform re-runs it whenever it likes, so a send from one would
send again each time — and Tier-1 hot reload (§9.3) rests on re-running a view
being free of consequence.

## 5. The VM and Actor Runtime

### 5.1 The actor is the unit of everything

- **Isolation.** It owns a wasmtime `Store`, and that Store is its arena.
- **Scheduling.** It is a tokio task.
- **Failure.** A panic stops at the actor boundary.
- **Reclamation.** A drop of the Store frees the arena in O(1). No tracing GC
  runs across the application.

An application is a supervision tree. The todo application uses four actors
(§8).

A bump allocator serves the arena, and it never frees. Every heap allocation
lives for the lifetime of the actor. Section 6.2 depends on this.

### 5.2 Scheduling

The tokio multithreaded runtime gives M:N scheduling with work stealing.

A blocking call compiles to a host function that is async on the Rust side,
through wasmtime async support and epoch interruption. A blocked actor therefore
costs no OS thread.

An epoch boundary preempts long compute, so one hot actor cannot starve the
others. This, plus the platform-owned render thread, is the structural fix for
"do not block the main thread".

### 5.3 Typed channels and ownership transfer

A channel declares a message type, and the compiler checks both ends.

A small value is copied. A buffer is transferred: the host moves the allocation
between Stores and invalidates the sender's handle. This is what `postMessage`
transferables gestured at, enforced instead of optional. Section 6.8 gives the
byte-level rule.

### 5.4 Supervision

Every actor has a parent. On a panic the runtime tears the actor down, reclaims
the arena, and delivers a typed `ChildDown(reason)` message to the parent.

The parent chooses one of three strategies: restart with fresh state, restart
from a snapshot that the child exported, or escalate.

The UI system draws a built-in "component failed" boundary for a dead UI actor.
This is a React error boundary that the platform enforces.

**POC demo:** the todo application holds an actor that crashes on demand. The
supervisor restarts it, and the application does not blink.

## 6. Value Representation and Host ABI

This section is scoped to the POC. It describes the compiler as built today.

### 6.1 No WASM GC, no Component Model

The compiler emits core WASM modules, with linear memory and our own layout.
Section 14 lists this as the approved fallback for immature GC types; we take it
from the start, so the compiler does not wait for the toolchain.

The emitted WASM erases types. The checker knows them. No claim in section 2
depends on GC types.

### 6.2 `Result<T,E>` and `Option<T>` are multi-value, never boxed

A function that returns `Result<T,E>` returns two WASM values:

    (i32 tag, i64 payload)     tag: 0 = Ok/Some, 1 = Err/None

The payload is one 64-bit word. Its content follows the *static* type at each
site: an `int`, the bits of an `f64`, a `bool`, or a 32-bit pointer into the
actor's arena. It carries no runtime tag, because the checker knows which arm
applies.

`?` compiles to three steps: call, test the tag, and on a non-zero tag return
the pair unchanged. There is no allocation, no copy and no branchy unwind.

**Why not a pointer to a tagged struct.** The bump allocator never frees before
the death of the actor (§5.1). Section 4.3 sends *every* fallible call through
`Result`. The demo calls `addTodo` on each keystroke, while a debug overlay
shows the live arena size of each actor (§8). A boxed `Result` would leak on
each fallible call, and the leak would appear in the overlay that must
demonstrate isolation.

### 6.3 A user sum type is boxed, and uniformly sized

`type AddError = | EmptyTitle | TooLong(max: int)` becomes a pointer to
`{ i32 tag, fields... }`. An enum where no variant carries a payload degrades to
a bare `i32` tag.

Every variant of a sum occupies the same room: one word for the tag, then one
per field of the **widest** variant. The narrow variants leave a few bytes
unused, and in exchange the size of a value becomes a property of its type
rather than of its tag. That is what lets `send` (§6.8) put a constant length on
the wire instead of computing one from the tag at run time — a whole small
subsystem that does not need to exist. It also removes a hazard: an exact-fit
block sitting at the end of the arena, read at the width of the widest variant,
would read past the end of linear memory.

Only an error path constructs these values, so an allocation there is acceptable
at POC scale. M5 measurement can change this (§13).

### 6.4 A record is a pointer to a flat struct

The struct lives in linear memory. Each field sits at a statically known offset.
A slot is 8 bytes.

A record is immutable (§4.2), so an update allocates a new record. The leak
caveat of section 6.2 applies, and M5 can change it.

`Model { ...state, draft: x }` is sugar and adds no representation. The checker
binds the spread to a local and turns every field the literal leaves out into an
ordinary field read of it, so the emitter sees the same record construction it
always saw. There is no update instruction, and therefore no in-place write to
be tempted by later.

The binding is the part that matters. Inlining the base instead would evaluate
it once per field it filled, so `Model { ...next(), b: 1 }` would call `next`
five times.

The spread comes first. Anywhere else and a reader has to scan the whole literal
to know whether a field they can see is the one that wins, and the freedom buys
nothing.

### 6.5 A string is a pointer to a header and bytes

The layout is `{ i32 len, bytes... }`, UTF-8, immutable. The POC does not intern
strings.

### 6.6 A list is a pointer to a header and elements

The layout is `{ i32 len, <pad>, elements... }`. The header takes a whole word,
so the elements stay aligned to 8 bytes. That alignment lets the emitter load an
element with exactly the code that loads a record field.

An element occupies `words(T)` slots, as a record field does:

- A `List<int>` strides by 8.
- A `List<Todo>` strides by 8, because an element is a pointer.
- A `List<Result<int, E>>` strides by **16**, because a `Result` takes two words
  (§6.2).

A stride that assumes one word reads each element's tag out of the previous
element's payload.

A list is immutable. `push` allocates a list one element longer and copies, at
O(n). The alternative is a growable buffer with a hidden capacity, which is a
different design.

An empty literal takes its element type from the context. The checker refuses an
empty literal with no context, because a `List<?>` has no representation and a
guess fails much later.

`for x in list { ... }` walks the list. The loop is an *expression* of type unit,
so it can stand among a container's children (§7.2), as `if` does. A view appends
as it evaluates, so each pass appends and the parent's child count comes out
right on its own.

### 6.7 The host ABI

A guest imports from module `strand` and exports a small set of entry points. M0
establishes the ABI, and each later milestone extends it.

The emitter writes an import **only when the program calls it**, so a module
that never touches the host instantiates with no imports. An import occupies a
low function index, so every defined function shifts past the imports. The
emitter applies that offset once.

Strand code can call `log(msg: string)`, `send(port, value)` and
`panic(msg: string)`. Each unpacks its argument into the `(ptr, len)` pair the
host expects — `log` and `panic` from the string header of section 6.5, `send`
per section 6.8.

Imports:

    strand.log(ptr: i32, len: i32)
    strand.send(port: i32, ptr: i32, len: i32)
    strand.panic(ptr: i32, len: i32)    // never returns
    strand.sleep_ms(ms: i64)            // async host call: suspends the fiber

Exports:

    memory
    strand_alloc(size: i32) -> i32      // bump allocator in the guest arena
    strand_main()                       // optional
    strand_on_message(port: i32, ptr: i32, len: i32)   // optional

`panic` is the second tier of §4.3. It raises out of the guest, the Store is
dropped, the arena goes with it, and the supervisor receives a crash report
whose reason is the message. That is why it is a host call rather than a bare
`unreachable`: the reason is the useful part. WASM cannot declare an import as
never returning, so the emitter follows the call with `unreachable` — without
it, a `panic` in tail position would fall off a function that owes its caller a
value.

An actor's own functions — `init`, its handlers, its view — are **not**
exported. They are reached through the entry points above, and two actors in one
file both declaring `init` is the ordinary case rather than a clash.

Import signatures are interned before the type section is written. Interning
them where the import section is built instead means an import whose type is new
at that point names a type index the module does not contain. The bug went
unnoticed while `log`'s `(i32, i32)` happened to match the old two-argument
`strand_on_message`.

### 6.8 Channels are ports, and the wire format is the memory format

An actor declares its channels by name and type, and nothing else:

    actor Meter {
      state: Reading
      in  samples: Sample     // what it can be told
      out totals:  Total      // what it can say
      ...
    }

Each `in` port has a handler named after it — `on samples(state, msg): State` —
and `send(totals, value)` puts a value on an out port. **The index is the
protocol.** The checker resolves a port name to its position in the actor's
inbox or outbox, and that number is what crosses the boundary. A name never
does.

**There is no actor address.** Nothing in the language can name another actor,
so an actor reaches exactly the peers it was wired to. The wiring lives in an
`app` block (§6.12), the registry holds it, and the guest holds none of it. Two
things fall out. Location transparency (§10.5) stops being a feature to add: a
port whose far end sits on another machine changes the `app` block and nothing
else, because there was never a local address to replace. And the capability
model of §10.2 gets its first instalment by having no addresses rather than by
checking them.

**A message type must be flat.** A field can be `int`, `float` or `bool`. It
must not hold a pointer. The checker enforces this and gives the reason: the
runtime copies a message into a *different* arena, where a pointer from the
sender's arena means nothing.

That restriction buys the Cap'n Proto property (§17): the wire format is the
memory format. The bytes on the channel are exactly the boxed-variant layout of
section 6.3. The runtime copies them into the receiving arena with
`strand_alloc`, and the pointer it holds is then a valid value.
`strand_on_message` gives that pointer straight to the port's handler. There is
no decode step, and a decode step would be the bug.

| message type | bytes on the wire |
|---|---|
| a sum with any variant that carries a payload | `i32 tag` in the first 8-byte slot, then one 8-byte slot per field of the widest variant (§6.3) |
| a sum where no variant carries a payload | a bare `i32` tag |
| `int` or `float` | the 8-byte value |
| `bool` | `i32` 0 or 1 |
| `string` | raw UTF-8 bytes. Codegen adds the header of §6.5 on arrival |

A `string` is the one case that relocates. It is safe because codegen rebuilds
the header in the receiving arena.

Sending is the same layout read the other way. A boxed variant is already laid
out as the wire wants it, so `send` hands over the pointer it holds and a length
that is a constant of the type (§6.3). An immediate — an `int`, a `bool`, a bare
tag — has no address, so codegen reserves one word of static scratch and writes
it there. The host copies the bytes before the call returns, so one slot serves
every send in the module.

The host encoder that the CLI uses reads layout from the same `Hir`, so both
sending paths agree with the receiving one by construction rather than by being
kept in step.

### 6.9 The frame: how a view crosses into the host

A `view fn` (§7.2) does not return a tree. It **appends** to a fixed array in
its own arena and returns nothing. `Node` has no runtime representation.

One record per node, 32 bytes:

| offset | field | notes |
|---|---|---|
| 0 | `i32 kind` | which widget (`strandc::ui::NodeKind`) |
| 4 | `i32 child_count` | how many roots before it belong to this node |
| 8 | `i32 id` | the hit id. 0 means the node takes no input |
| 12 | `i32 flag` | `checked`, `focused` |
| 16 | `i32 text` | a string pointer per §6.5, or 0 |
| 20 | `f32 number` | `gap`, or a scroll's `offset` |
| 24 | `f32 number2` | `padding` |
| 28 | `i32 text2` | a second string, for the one widget that needs two |

**The array is post-order.** A view emits as it evaluates, so a container's
children finish first, and the container records how many unclaimed roots belong
to it. One left-to-right pass with a stack rebuilds the tree. Nothing moves,
nothing is back-patched, and there is no decode step.

Codegen keeps one counter, `pending`: the number of finished roots that no
parent has claimed. A builder saves it before its children run, then gives the
saved value to `node_push`. `node_push` computes `child_count = pending -
marker`, then sets `pending = marker + 1`.

That subtraction replaces the child-tracking stack that a tree builder needs. It
also makes a conditional child free: an `if` that does not run appends nothing,
so its parent counts one child fewer.

**Why `Node` is zero-width.** A view emits a node where the programmer writes
it. A value that code could store, pass or reuse would be a node that appears
elsewhere. A type that carries nothing makes the checker reject
`let n = text("hi")` and `fn f(n: Node)`, so the array is in tree order by
construction.

Exports:

    strand_nodes           // i32 global: where the array starts
    strand_node_count      // i32 global: how many records are in it
    strand_frame_reset()   // empty the array before the next frame
    strand_view()          // reset, then draw the actor as it is now

`strand_view` exists only on an actor that declares a `view fn`. It takes no
argument, because a view is a pure function of state (§7.5) and the state global
is the only state. It resets the arena, reads global 1, and calls the view.

The runtime calls `strand_view` after `strand_main` and after every message,
then gives the region to whoever asked for frames. It knows *where* a frame is,
and nothing about what a frame means. Layout, widgets and the compositor live
behind a trait with one method, which keeps the actor runtime free of the
renderer.

The array holds 2048 nodes (`ui::NODE_CAPACITY`), between the static data and
the bump heap. A view that exceeds it traps; it does not grow. A trap arrives as
a crash report that names the actor (§9.4). A silent truncation arrives as a
view that stops halfway down.

`crates/strandc/src/ui.rs` is the single table for all of this. The parser, the
checker, codegen and the host decoder read it, so the two ends cannot disagree
about a byte.

### 6.10 Generated string helpers

`+` on strings, and the functions in `crates/strandc/src/stdlib.rs`, compile to
WASM that the emitter generates into the module. They are not host imports, so a
program full of them instantiates with nothing linked and `strand run` needs no
runtime under it.

    str(value: int): string       // decimal
    char(code: int): string       // one character, from a scalar value
    len(s: string): int           // characters, not bytes
    isEmpty(s: string): bool      // len(s) == 0
    trim(s: string): string
    dropLast(s: string): string   // what Backspace does

The emitter writes only the helpers that a module calls, as with imports. They
are laid out last, so they cannot move an index that something else computed.

Three properties, because each is a bug that somebody otherwise writes:

- **A string stays immutable.** A helper allocates; it never edits its argument.
  `a + b` leaves both usable, which is what makes `first + (a + b)` mean what it
  reads as.
- **A user-visible count is in characters.** A UTF-8 continuation byte is
  `0b10xxxxxx`, so `dropLast` steps back over them and removes a whole
  character.
- **`str` reads the magnitude unsigned.** A negation of the most negative `int`
  wraps to itself. Read without a sign, that bit pattern is the magnitude you
  want. The obvious method prints one wrong digit, or it loops.

`+` between a string and a number stays rejected. The complaint in section 4.2
is about `"1" + 1`, not `"a" + "b"`.

### 6.11 Input is the platform's message type

A UI actor receives input as an ordinary message (§7.1), so its mailbox carries
a type that the platform declares:

    Click(id: int)
    Typed(ch: int)          // the Unicode scalar value
    Backspace
    Enter
    Escape
    Focus(id: int)          // 0 when nothing holds focus
    Scrolled(id: int, offset: float)

Every field is an `int` or a `float`, so the flatness rule holds with no
exception (§6.8). The encoder that puts input on the wire is the encoder that
the CLI uses for any message.

**Why the platform declares the type.** The alternative was an actor-declared
event type, with the host filling in the variants whose names it recognised.
Spelling would then hold the protocol together: rename `Click` to `Pressed`, and
clicks stop arriving in silence. A platform declaration means the checker knows
the type, a `match` is exhaustive, and a typo is a compile error.

**Why it is opt-in.** Registering the type also registers `Click`, `Enter` and
the rest as constructors, and those are names a UI program might want. The type
therefore appears only in a module that names `Input` as the type of a port. A
module with its own `type Input` keeps its own.

The platform finds the port by **type, not by name**: whichever `in` port
carries `Input` is where clicks are delivered. Calling it `input` is a
convention; carrying `Input` is the fact. Matching on the name would be the
protocol-held-together-by-spelling that this subsection exists to avoid.

`crates/strandc/src/input.rs` is the single table, read by the checker and by
the host translation from `InputEvent`.

### 6.12 The app block: the supervision tree, written down

    app Pipeline {
      meter    = Meter
      reporter = Reporter

      meter.totals -> reporter.totals
    }

An instance is an actor that runs, under the name the wires call it by. A wire
joins one actor's out port to another's in port. The checker requires that both
ends exist and that their types match exactly.

Every out port must be wired. An unwired one is a `send` that vanishes, and
"your messages went nowhere" discovered at run time is the opposite of what §9.2
asks a diagnostic to be.

Nothing says the two ends of a wire must be different actors. A port wired back
to its own actor is a self-scheduled loop, which is how `todo_demo.str` sustains
a CPU burn as many short handler calls instead of one long one.

The block lives in the source rather than in a config file because the wiring is
typed and the compiler is the only thing that can check it. §9.1 asks for zero
config files, and a wiring the compiler cannot see is a wiring that fails later,
somewhere else.

**One module per actor.** A module carries one actor's ABI: the state global,
`strand_main` and `strand_on_message` are singular by construction. A file
holding several actors compiles once per actor, which is also what gives each
its own arena — a file is a unit of source, and §5.1's unit of isolation is the
instance. The emitted modules are near-identical apart from which functions the
entry points call, so the cost is paid in bytes rather than in isolation.

A file with one actor and no `app` block is an app of one actor with no wires.
There is one path rather than a general one and a special case.

### 6.13 Lifecycle: what a guest hears about its peers

§5.4 delivers a typed `ChildDown` to the supervisor. In the POC the supervisor
is the host, and that is where it stopped: a guest could not learn that a peer
had died. §8's demo needs it to, so the platform declares a second type, opted
into exactly like `Input` by naming it as the type of a port:

    Down(port: int)     the peer feeding this port of mine has died
    Up(port: int)       a fresh one has taken its place

The peer is named by a port because no other name exists. An actor holds no
addresses (§6.8), so "who died" can only be said in terms the receiver already
has: `port` is the index of the receiver's own `in` port that the departed peer
was wired to.

`Up` fires for an actor's **first** life as well as for a replacement. Coming up
for the first time is the same news, and treating it as such saves every peer
from sending a speculative hello. In `todo_demo.str` the first `Up` is what asks
for the first count.

Two orderings turn this from a race into a sequence:

- **Every mailbox is reserved before any actor runs.** The registry creates the
  channel up front, so an actor that sends from `init` does not depend on which
  task the scheduler started first. Without it, "can I send to you yet" is a
  coin flip.
- **`Up` is announced by the new life, once it has taken its mailbox** — not by
  the supervisor beforehand. A peer answers `Up` immediately, which is what `Up`
  is for, so the answer must have somewhere to go. On a restart the supervisor
  reserves a fresh mailbox first, and anything sent during the gap waits there
  instead of being refused.

The runtime delivers bytes that were encoded by whoever set the watch. Encoding
a value means knowing a type's layout, and a second implementation of §6.8
living in the runtime is exactly what the host encoder exists to avoid.

**Honest gap.** Comparing against the port index means writing the number: there
is no way yet to spell "the index of my `tally` port". With one peer it does not
come up, and inventing the syntax before something needs it would be guessing at
the shape.

`crates/strandc/src/lifecycle.rs` is the single table, on the same terms as
`input.rs`.

## 7. UI: The Declarative Scene Graph

### 7.1 Model

There is no DOM, no HTML and no diff in user code. UI is a function of state,
and the platform owns reconciliation.

- An application actor builds a UI tree. A view function appends nodes (§6.9).
  The actor submits the tree to the render actor over a channel.
- Layout resolves the tree into a flat typed **render command array**: rect,
  text, image, clip-start and clip-end. clay proves this architecture (§17). The
  array is renderer-agnostic, diffable, cheap to serialize over a channel, and
  it compiles to HTML — which is the compatibility story run in reverse (§12.3).
- Layout allocates from a per-frame arena and resets it each frame, so UI adds
  no GC pressure. clay uses about 3.5 MB for 8000 elements.
- The **render thread** belongs to the platform: a winit event loop and wgpu. It
  diffs a command array against the retained scene graph and paints. Layout uses
  `taffy` first; the clay algorithm is the reference if we replace it.
- An input event flows back as a typed message, to the actor that owns the hit
  node.

Submission is a message, so a slow actor delays *its own* updates only. The
compositor keeps its frame rate. This is the most visible claim, so the demo
includes a "spin the CPU" button.

### 7.2 UI syntax: a typed builder DSL

A JSX-like syntax (`<Row gap={8}>…</Row>`) is familiar, but structurally it
works around a language that lacked tree syntax. Its costs recur: control flow
through a nested ternary and `&&`, where `count && <Badge/>` renders a literal
`0`; a manual `key` prop for a list; two syntax modes with a `{}` escape hatch
in both directions; and types bolted above the syntax.

A typed builder DSL with a trailing block (`row(gap: 8) { … }`) is what every
toolkit converged on when the toolkit and the language were designed together —
SwiftUI, Compose, Flutter. A conditional is an ordinary `if`. A list is an
ordinary `for`, keyed on a stable ID. The checker checks children and props like
any argument. There is no mode switch.

**Decision.** The DSL is the semantic core and the only POC syntax. A
JSX-flavored surface can arrive later as sugar that desugars to the DSL, exactly
as JSX desugars to `createElement`.

```strand
view fn todoList(todos: List<Todo>, onToggle: fn(Id)) -> Node {
  column(gap: 4) {
    if todos.isEmpty() {
      text("Nothing yet — add your first todo", color: theme.muted)
    }
    for t in todos {            // keyed by t.id automatically (stable-ID rule)
      todoRow(t, onToggle)
    }
  }
}
```

### 7.3 Lessons from HTML and CSS

| HTML/CSS scar tissue | Strand decision |
|---|---|
| "How do I center a div": alignment fell out of five unrelated mechanisms | Alignment is a typed property of every container: `column(align: .center, justify: .center)` |
| Floats, tables, flex, then grid on a document model | `row`, `column` and `stack` with flex semantics. `grid` is future work. No floats, no document flow |
| `justify-content` against `align-items`: the meaning flips with direction | Flutter-style `mainAxis` and `crossAxis`. Unambiguous at any orientation |
| The cascade: global scope, specificity rank, `!important`. A stylesheet becomes append-only | No cascade, no selector, no global style. A style is a typed prop beside the view. An unused style is provably dead code |
| Stringly-typed properties. `witdh: 10px` fails in silence | Typed style props. A typo is a compile error |
| `content-box` against `border-box` | Border-box only |
| The unit zoo: px, em, rem, %, vh, vw, ch, ex | Logical pixels, fractional weights, percent of parent |
| A media query targets the viewport. A container query arrived 20 years late | Responsiveness is container-based. A view branches on the size it receives |
| Implicit stacking contexts and z-index wars | Paint order is tree order. A float declares an attach point: `attach(element: .bottomCenter, anchor: .topCenter)`. No z-index; a float sorts by a small layer number, only against other floats |
| width, height, min, max, flex-basis, flex-grow, flex-shrink | The clay vocabulary: `fit(min, max)`, `grow(min, max)`, `fixed(n)`, `percent(p)` |
| Design tokens reinvented: Sass variables, custom properties, a Tailwind config | Typed theme constants as a primitive: `theme.spacing.md`, `theme.color.accent` |

### 7.4 The POC widget set

Seven widgets: `column`, `row`, `text`, `textInput`, `button`, `checkbox`,
`scroll`.

A style is a small typed props struct: padding, gap, color, font size. wgpu
renders. `glyphon` draws the text.

```strand
view fn todoRow(t: Todo, onToggle: fn(Id)) -> Node {
  row(gap: 8) {
    checkbox(checked: t.done, onChange: fn() { onToggle(t.id) })
    text(t.title, strike: t.done)
  }
}
```

### 7.5 State model

The POC uses the Elm model. Each UI actor owns a state record. An event is a
message. A handler returns updated state. The runtime then calls the view again.

There are no hooks and no effects system. The actor's mailbox *is* the effects
system.

## 8. The Demo: A Todo Application

The application is boring, so that the architecture is the story.
`examples/strand/todo_demo.str` is the whole of it, in Strand:

```
app TodoDemo
├── TodoUi actor      — owns List<Todo>, draws the window, takes input
│                       in input / in tally / in life / out stats
└── Stats actor       — derived counts, independently crashable
                        in commands / in spins / out tally / out again
   (platform) Render  — scene graph, wgpu, input routing
```

What a reviewer sees, in order:

1. Add, complete and delete a todo. A `Result` surfaces a validation error: an
   empty title shows a notice, not a crash.
2. Press "crash stats". The Stats actor panics — a real `panic()` in Strand, not
   a division dressed up as one. Its panel shows a failure boundary for a
   moment, because the platform told the UI actor its peer was gone (§6.13). The
   supervisor restarts it, the `Up` that follows asks for a fresh count, and the
   todos are untouched, because they were never in that arena.
3. Press "burn CPU". The Stats actor pegs a core with guest code — the burning
   is Strand, not a host call obliging it. Typing stays at frame rate. Stats
   hands itself work through a port wired back to itself, so the load is
   sustained and the actor is still between messages at every turn.
4. Open the debug overlay. Two rows, two arenas, and the Stats generation number
   ticking up each time it is crashed.

**Where this differs from the sketch above it.** §5 describes an AppState actor
as the single writer, with the UI subscribing to it. The demo folds those two
together: the UI actor owns the list. Splitting them would demonstrate the same
machinery a second time — another pair of ports, another wire — rather than
anything new, and §14's ruthless-scope entry applies. The beats that argue for
the architecture are 2 and 3, and both need only that Stats be a separate actor.

## 9. Developer Experience

DX is architecture here. The demos that argue for the *platform*, rather than
for the todo application, live in this section.

### 9.1 One binary, zero configuration

`strand` is one binary: `strand run`, `strand fmt`, and later `strand test` and
`strand doc`. The POC has no configuration file.

Formatting follows gofmt: one true style, no options. The achievement of gofmt
was the end of formatting arguments.

### 9.2 Diagnostics are a product surface

The core pitch is "types that survive to runtime", so the compiler is the first
impression. Diagnostics get first-class treatment from M1: a source span, a
labeled underline, and a suggested fix where one exists. The `miette` crate
gives Rust-quality rendering for almost nothing. Good errors are cheap at the
start and miserable to retrofit.

### 9.3 Hot reload — three tiers on supervision

Hot reload is a supervisor restart where the replacement actor runs newer code.
Section 5.4 built the machinery.

**Tier 1 — view reload. In the POC.** A view function is a pure `state → Node`.
On a file change the tool recompiles the module, ships it to the UI actor over a
channel, and calls the views again against existing state. No state migration
exists, so the loop is sub-second.

**Tier 2 — actor logic reload. A stretch goal.** Behavior changes, and the state
shape does not. Snapshot the state, restart on the new code, restore the
snapshot. A state record is typed, so the runtime verifies statically that the
shapes match before the swap. Erlang hot code loading cannot make that check.

**Tier 3 — schema migration. Future work.** The state shape changes. Run an
optional `migrate(old) -> new`, or restart fresh.

### 9.4 Debugging — replay, not breakpoints

A stepping debugger needs DWARF through wasmtime, then lldb or DAP. That is a
large lift, and we defer it. Actors interact *only* through typed messages, so a
record of an actor's inbound messages is a complete record of its inputs.

In POC scope:

- **Message tracing.** A structured causal log of who sent what to whom, with
  typed payloads, and a toggle for each actor.
- **A structured crash report.** A panic yields the actor, the message in
  flight, a state snapshot and a WASM backtrace, delivered to the supervisor.
  The supervisor is a crash reporter by construction.
- **A debug overlay.** Arena size, fiber count and mailbox depth for each actor,
  drawn by the platform as injected render commands.

Future work: deterministic single-actor replay, which feeds a recorded message
log into a fresh instance and gives time-travel debugging of one component
without whole-program record and replay; DAP integration; an LSP.

## 10. Platform Services

Post-POC, recorded because each lesson constrains a POC primitive, and because
the biggest wins reuse machinery we already build.

### 10.1 Storage — one typed API

The web shipped five: cookies (4 KB, stringly typed), localStorage
(synchronous, blocks the main thread), sessionStorage, IndexedDB (so hostile
that everybody wraps it) and AppCache.

Strand ships one API. It is async only, so a call suspends the fiber and
colorless concurrency hides it. It is typed, so a record persists with its
schema. It is transactional. It has explicit tiers — session-scoped and durable
— and an expiry. It is a **capability** with a quota, so an origin gets no
ambient access.

### 10.2 Identity — no ambient credentials

A cookie attaches itself to every request. That is ambient authority, and it is
the root cause of the CSRF class.

Strand has no cookie equivalent. A session is a capability token that an actor
holds and presents explicitly. CSRF becomes unrepresentable, as ownership
transfer made a data race unrepresentable (§4.4).

### 10.3 Navigation — the URL stays sacred

A **typed route** is a platform primitive. A URL is typed data, not a string to
parse. An actor declares the mapping between its state and its URL, so every
meaningful state is addressable, linkable and safe for the back button.

### 10.4 Rendering strategy — resume, do not hydrate

The pendulum between SSR and CSR compensates for two platform gaps: a slow cold
start, and no way to move a running application's state between machines.

Strand closes both. A content-addressed AOT module makes the cold start
near-native (§11.3). The typed state snapshot already exists for hot reload
(§9.3) and crash reports (§9.4); it is also the resumability primitive.

The flow: a server runs the actor, streams the first render command array, which
is already serializable (§7.1), and transfers the snapshot. The client resumes
from it. First paint comes from the server. The client re-executes nothing.
There is no hydration step, and no mismatch bug class.

POC constraint: nothing in the snapshot format may assume same-machine
resumption.

### 10.5 Server functions and distribution

`"use server"`, TanStack Start and Remix loaders converge on typed RPC beside
the UI code. The RSC client/server split then reintroduced coloring at the
component level: `"use client"` fractures the tree.

The actor model gives the principled version through location transparency. A
server function is an actor that runs on a server. The typed channel *is* the
RPC contract. There is no directive and no colored component.

POC constraint: channel semantics never assume shared memory. That is already
true, because a send transfers ownership (§4.4). Section 12.4 gives the
transport.

An optimistic update stops being a library pattern under Elm-style state and
`Result` effects: apply the predicted state, reconcile on `Ok`, revert on `Err`.
Linear, Replicache, Zero and Electric point at a **synced state record** as a
future primitive.

### 10.6 Framework lessons

| Framework scar tissue | Strand decision |
|---|---|
| Hooks tie state to call order, so the Rules of Hooks leak compiler work onto the user. A `useEffect` dependency array is manual cache invalidation. `useMemo` is a memoization tax | State lives in a typed actor record, not a call position. An effect is a message, so there is no dependency array. The re-render unit is bounded, so there is no manual memoization |
| What React got right | A component is a pure function, data flows one way, composition works. All three stay |
| Signals won fine-grained reactivity, because React re-renders an unbounded subtree | **The actor is the re-render unit.** One actor's state change re-runs one actor's views into one command array. The platform enforces the blast radius. Signals *inside* an actor are the planned optimization: a reactive read compiles to a targeted patch of the command array. Strand is compiled, so this needs no API break |
| Svelte: the framework can disappear into the compiler | Reactivity is a compilation target, not a runtime library |
| Next.js: a magic file convention becomes the API | No filename encodes semantics. A route is declared in code, with types. The specification stays vendor-neutral |
| React Query: server state is not UI state | Typed routes are a primitive (§10.3). A `resource` abstraction with staleness semantics is future work |

## 11. Ecosystem and Packages

Post-POC. The module format (§11.3) and the capability model (§11.6) shape POC
primitives.

### 11.1 The stakes

JavaScript is irreplaceable because of two million packages, not because of the
language. A platform that ignores this loses before it starts.

npm won before supply-chain attacks were common, before content addressing was
mainstream, and before capability security was practical. Its problems are
patches on a foundation that cannot change. The opportunity is an ecosystem
where whole classes of npm failures are not representable, plus an answer to the
cold-start problem (§11.9).

### 11.2 Scar tissue

| Ecosystem lesson | Source | Strand decision |
|---|---|---|
| left-pad: one unpublish broke the internet, because registry state is mutable | npm, 2016 | Content-addressed immutable modules (§11.3) |
| A `postinstall` script runs arbitrary code at install time | npm: event-stream, ua-parser-js | **No install script, ever.** An install is a data transfer. A build is a sandboxed pure function (§11.6) |
| An account takeover swaps code under an unchanged name, in silence | npm, repeatedly | A transparency log (§11.4) |
| Transitive trust is unauditable at scale | npm | A capability manifest, summed across the tree (§11.6) |
| node_modules duplicates. Hoisting creates a phantom dependency | npm; pnpm fixed part | One content-addressed cache per machine. An import resolves only through the manifest |
| `is-even` exists because there is no standard library | npm culture | Batteries included, plus a blessed tier (§11.7) |
| Semver is a social promise | npm | Enforced semver: an API diff at publish forces the bump (§11.5) |
| SAT-solver resolution: a build changes overnight | npm, yarn | Minimum Version Selection: deterministic, solver-free (§11.5) |
| URL imports: broken links, mutable targets, no discovery | Deno 1.x, retreated | A registry where a name is metadata over a hash (§11.4) |
| Publish source, generate docs, attest provenance, score | JSR | Adopted whole (§11.8) |
| Speed changes adoption economics; compatibility is the on-ramp | Bun | An install is a cache hit. The Component Model is the bridge (§11.9) |
| Hosted docs for every package raised whole-ecosystem quality | docs.rs | `strand publish` generates and hosts typed docs (§11.8) |
| Registry ownership sets the trust ceiling | npm Inc.; Flash | Open-spec protocol and name system, under foundation governance |

### 11.3 Content-addressed modules

The unit of distribution is a compiled typed WASM component, identified by the
hash of its content. A name is metadata *about* a hash, never the identity of
code. Four structural consequences:

- **Immutability is physics.** A hash cannot change meaning, so a left-pad event
  is not representable.
- **One cache per machine.** Ten thousand applications that depend on the same
  HTTP library fetch and compile it once, ever. At this layer an application and
  a package are the same thing.
- **Reproducibility by default.** A lockfile is a list of hashes. Two machines
  with the same manifest resolve identically, forever.
- **Typosquatting weakens.** The dangerous moment is the name-to-hash
  resolution, which happens once, through the registry's verified index.

Prior art: the Git object model is the precedent; Nix proves the
reproducibility; Unison is the maximal version, where a hash of the AST
addresses each definition. Strand adopts the direction at module granularity,
trading elegance for comprehensibility (§18).

### 11.4 Names, the registry, the transparency log

- **A name is scoped** (`@author/pkg`) and maps a version to a hash. The
  registry is a lookup service and an index. Authority lives in the hash.
- **A transparency log** records every publish as an append-only event in a
  Merkle tree, on the Go checksum database and Certificate Transparency model,
  with Sigstore for identity. A client verifies an inclusion proof. A
  compromised registry cannot serve different bytes for an existing version
  without producing evidence that an auditor sees.
- **Provenance is on by default.** A publish is attested to a source revision
  and a reproducible build. The registry builds from source, or verifies a
  reproducible-build proof.
- **Governance is neutral.** The protocol, the name system and the log format
  are open specifications. Anybody can run a mirror or an auditor. A foundation
  operates the default registry. The ecosystem's root of trust must never be a
  company's asset.

### 11.5 Enforced semver and Minimum Version Selection

**Enforced semver.** Types survive compilation, so `strand publish` diffs the
public API against the previous version. A removed or changed signature forces a
major bump. An addition forces at least a minor bump. The tool refuses a
mislabeled publish. A behavioral break inside an unchanged type stays possible;
that is the residual risk. A capability change also forces a major bump (§11.6).

**Minimum Version Selection.** Resolution picks the *minimum* version that
satisfies all constraints. It is deterministic and solver-free, so a build never
changes because somebody published last night. An upgrade is an explicit act.
With content addressing, the output is a stable set of hashes.

**Two schemes.** The rules above apply to a package, where a version is a
machine-checked contract. The platform and toolchain use CalVer: `Strand 27.1`
is the 2027 train, first update. For a product the date is the meaningful
signal, and CalVer commits to release trains. The registry shows the publish
date beside every semver, so a human gets the age signal and the machine channel
stays clean.

### 11.6 The capability manifest

A package is a WASM component, so it can touch only what it receives.

- Every package declares its capabilities in its manifest: `net(hosts?)`,
  `storage`, `clock`, `random`, `spawn`. The component's imports enforce the
  declaration, verified statically at publish and at load. A markdown parser
  that declares nothing provably cannot exfiltrate data.
- Tooling shows the **capability sum of the dependency tree**. A review audits a
  short verified list instead of transitive source. A dependency that adds a
  capability is a major-version event and a loud diff.
- **No lifecycle script runs anywhere.** A build runs as a sandboxed pure
  function on registry infrastructure: source in, component out, no network, no
  ambient filesystem.
- Registry scoring weights capability minimalism. "Requires: nothing" becomes
  the status symbol.

### 11.7 Standard library strategy

1. **The standard library** ships and versions with the platform: collections,
   `Option` and `Result` combinators, strings and formatting, time, math,
   encoding, testing. It is broad enough that an `is-even` package never forms.
2. **`strand-x/`** is the blessed tier, on the `golang.org/x` model: official
   quality, versioned separately, capability-audited. HTTP client, crypto,
   compression, image codecs. A community package can graduate into it.
3. **The community tier** is everything else, ranked by the scoring system.

The doctrine for an early ecosystem is curation over volume. A small coherent
core that feels complete beats a large bazaar that feels random.

### 11.8 Publishing

`strand publish` is one command: the semver check (§11.5), capability
verification (§11.6), the reproducible build and its attestation (§11.4), doc
generation from types with runnable examples, and log inclusion (§11.4). There
is no configuration beyond the package manifest.

Near-zero publish friction is what created npm's ecosystem at all. Keep the lack
of friction; delete the failure modes.

### 11.9 The cold-start problem

Better tooling has never bootstrapped a community by itself. Three levers stack:

1. **The Component Model is an ecosystem loan.** Rust, Go and C libraries
   compile to WASM components today. Wrap one with a typed interface and a
   capability manifest, and it is a first-class package. This is Bun's
   compatibility play, moved to the boundary where the sandbox holds.
2. **Curate early** (§11.7). The first hundred packages set the culture.
3. **Make a publish a joy** (§11.8), and make the capability badge a status
   economy. The flex becomes *how little* your package needs.

Later, the legacy-web layer (§15) extends the bridge to JS libraries. Behind the
sandbox, a JS dependency also arrives with a capability manifest.

## 12. The Strand Web: Naming, Transport, Access

Post-POC. It constrains primitives the POC already has: channels, manifests,
render commands, snapshots.

### 12.1 Four layers

| Layer | Question | Answer | Novelty spent |
|---|---|---|---|
| **Naming** | How do you refer to a place? | URLs, DNS, TLS, unchanged | None |
| **Bootstrap** | How does an application load? | A signed manifest over HTTPS. Hashes from anywhere | Low |
| **Session** | How does a running application talk? | Typed channels over QUIC streams | **High** |
| **Discovery** | How does anybody find anything? | Typed-route content indexes | Open problem |

The protocol innovation already happened when we chose content addressing and
typed channels. The wire is only the truck. A custom transport forfeits the
world's CDNs, firewalls, proxies and ops tooling on day one. gRPC rode HTTP/2,
WebSockets rode an HTTP handshake, OCI registries rode HTTP conventions, and
people reach IPFS through HTTP gateways.

### 12.2 Naming

`https://example.com/recipes/42` holds the open web's social contract: anybody
with a domain can publish, there is no gatekeeper, and every state is linkable.
DNS and certificates are the only deployed naming-plus-trust system that reaches
everybody. A `strand://` scheme would spend adoption capital on the one layer
that was never the problem.

- A domain names an application. A typed-route URL names a state (§10.3).
- The TLS certificate on the manifest origin authenticates *who publishes*. The
  content hash authenticates *what the code is*. That separation is the
  structural upgrade over HTTPS, where the channel vouches for both.

### 12.3 Bootstrap

1. **Fetch the manifest** — a small signed document at
   `/.well-known/strand/`, or content-negotiated at the page URL. It holds the
   entry actor, the module hash set, the capability requests, the route table
   and the fallback information.
2. **Resolve the hashes against the machine-wide cache.** All applications share
   modules (§11.3), so a first visit usually pulls only the application's own
   code.
3. **Fetch each missing module by hash.** `GET /modules/{hash}` goes to the
   origin, a CDN, a mirror or a LAN peer. Any dumb HTTP server qualifies.
   Integrity comes from verification after the fetch, not from trust in the
   channel:
   - A mirror needs zero trust. Fetch from whatever is fastest.
   - **Cache invalidation is deleted.** No Cache-Control heuristic, no ETag, no
     revalidation, no stale-content bug class.
   - "Installed" means "the hashes are local", so offline use and instant
     rollback fall out.
4. **Verify the capabilities, then start or resume the root actor** (§10.4).

**Transport: HTTP/3 over QUIC.** TLS stays on for privacy, which hides what you
fetch. It is not there for integrity.

**Compatibility is the adoption strategy.** The same URL serves both worlds
through content negotiation: a Strand client receives the manifest, and a legacy
browser receives HTML — a fallback page, or eventually the render-command
projection (§7.1). One URL, two webs, and no link splits. HTTP/3 deployed this
way: advertise the upgrade, fall back in silence.

### 12.4 Session: the typed channel is the protocol

HTTP's request-response shape was built for documents. The application web is
twenty years of workarounds on top — XHR, long polling, SSE, WebSockets, REST
conventions, JSON at every hop — because an application needs a conversation.

Strand's native primitive already is the conversation. A live session is client
actors and server actors that exchange typed messages. A dynamic website is an
application whose supervision tree spans two machines.

- **Carrier: WebTransport over HTTP/3.** One stream per channel, so a jammed
  channel never blocks its siblings. Stream lifecycle maps to channel lifecycle.
  Datagrams carry loss-tolerant traffic, such as a cursor position.
- **Wire format: the channel's message type, zero-copy.** The wire layout is the
  memory layout (§6.8), so a network crossing deserializes nothing. There is no
  JSON tax, no REST layer and no API-version drift. The channel type is the
  contract, and enforced semver versions it (§11.5).
- **Supervision spans the network.** A server actor's crash arrives as
  `ChildDown` at the client-side supervisor, so reconnect and retry are ordinary
  supervision strategy (§5.4). A partition is a typed, expected failure.
- **A network channel is a capability** (§11.6). An application opens a session
  only to a host its manifest declares, so a dependency cannot phone home.

The static case collapses: a site with no server actor is only the manifest and
the modules, CDN-served and cached forever. The SSR-against-CSR pendulum comes
to rest. Nothing between the two poles needs to exist.

### 12.5 Peer-to-peer

Content addressing makes P2P possible, because integrity never depended on the
source. The IPFS lesson is that mandatory P2P inherits P2P's availability and
latency floors.

Doctrine: HTTP from an origin or a CDN is the guaranteed path. A peer is a
transparent cache tier, discovered opportunistically, trusted zero.

### 12.6 Discovery — the open problem

SPAs broke crawlability, and a decade of SSR hacks clawed it back. A web of
manifests and WASM is worse, unless we design for the crawler from the start.

An application's routes are declared typed data, so the application can export a
**content index**: a machine-readable projection of its addressable state space,
mapping a route to a content summary or a snapshot. A crawler ingests it and
executes nothing.

Open: index freshness for dynamic content, abuse resistance without a central
gatekeeper, and whether the render-command-to-HTML projection doubles as the
crawlable form.

### 12.7 Privacy caveats

- A fetch by hash leaks *what you run* to the infrastructure operator, even
  under TLS, because a hash is identifiable. Mitigations are future work: an
  oblivious HTTP relay, request padding, and PIR for a popular module.
- The machine-wide cache is a cross-application fingerprint surface if timing is
  observable. Browsers partitioned their HTTP caches for this reason. Direction:
  a cache hit stays free, but no code may probe for an entry across a capability
  boundary, and load timing gets normalized for an unknown module.

## 13. Milestones

Every milestone is independently demoable. Estimates assume one focused
developer.

1. **M0 — Walking skeleton (1–2 weeks).** A hand-written WASM module runs in
   wasmtime under tokio. Two host actors exchange a typed message. A wgpu window
   clears to a color.
2. **M1 — Language core (2–3 weeks).** Lexer, parser, checker and WASM emission
   for functions, records, `match`, `Result` and `?`. The CLI runs a `.str`
   file. A golden-file test suite. Diagnostics through miette from the start.
3. **M2 — Actor runtime (2 weeks).** `actor` declarations, typed channels,
   buffer transfer, panic → `ChildDown` → restart, structured crash reports.
   Demo: a supervised pair, where one crashes on schedule.
4. **M3 — Scene graph (2–3 weeks).** Render thread with taffy layout, the widget
   set, input routing. A host-side actor submits the UI tree first; Strand code
   submits it after.
5. **M4 — Vertical slice (1–2 weeks).** The todo application in Strand, and the
   demo script of section 8.
6. **M5 — DX slice (1–2 weeks).** Tier-1 view hot reload; message tracing with
   typed payloads; a debug overlay on real runtime statistics. Stretch: Tier-2
   actor reload with a verified snapshot. This milestone demos the platform.
7. **M6 — Measurement (1 week).** Input-to-frame latency under load, memory per
   actor, actor spawn and kill cost, hot-reload round-trip time, binary size.
   Comparison notes against an equivalent JS and React todo application.

Total: about 10 to 15 weeks part-time. M0 is the de-risking gate. If wasmtime
async, tokio and wgpu do not compose pleasantly, we learn it in week one.

**Where things stand.** M0 through M4 are done, including the demo script of
§8. M5 is partial: message tracing and the debug overlay are wired to real
runtime statistics, and Tier-1 hot reload has not been started. M6 has measured
compositor frame rate and nothing else.

Named gaps worth keeping in view: there is no `xs[0]` (the syntax parses as two
expressions and fails later with a confusing type error, which is worse than
either adding indexing or rejecting it); `str` takes an `int` only; method-call
syntax is unimplemented and the stdlib is free functions; `scope` and `spawn`
are lexed and nothing more, which is why the overlay's fiber count is 0 or 1;
`push` is O(n); match exhaustiveness is one level deep; and there is no
`strand fmt` despite §9.1 asking for one binary that does it.

## 14. Risks

| Risk | Read | Mitigation |
|---|---|---|
| The per-actor wasmtime Store is too heavy | Medium | Measure at M0. An actor is component-grained: dozens, not millions |
| WASM GC types are too immature for the type mapping | Medium | Linear memory and our own layout. Section 6.1 takes this fallback from the start |
| Text rendering and input are a tarpit | High | One font, Latin only, a basic caret. glyphon does the rest |
| The compiler eats the schedule | Medium | The subset is fixed (§4.6). Cut anything the todo application does not need |
| Colorless host-call plumbing is fiddly | Medium | This is what M0 exists to prove |
| Hot reload creeps in scope | Medium | Tier 1 is the M5 bar. Tier 2 is a stretch. Tier 3 is banned |

## 15. Future Work

**Backwards compatibility** is the strategic linchpin, deferred deliberately.
Two paths, in likely order: embed a JS engine as a legacy actor, so existing
code runs inside the sandbox; then compile a TypeScript subset directly to the
VM, for migration one file at a time.

- **Security and distribution:** capability-based security, where a channel is
  the capability substrate (§10.2, §11.6); content-addressed modules (§11.3);
  distributed actors and location-transparent channels (§10.5, §12.4).
- **Language and compiler:** a custom bytecode VM to replace wasmtime once the
  semantics settle; full type inference; a JSX-flavored surface syntax (§7.2).
- **UI:** the `grid` primitive; container-size helpers beyond the basic branch;
  text shaping, i18n and accessibility; in-actor signals (§10.6).
- **Platform services:** networking and persistence host APIs; typed capability
  storage (§10.1); typed routes (§10.3); server-side actors with snapshot
  resumption (§10.4); synced state records (§10.5); a `resource` abstraction
  (§10.6).
- **Tooling:** Tier-3 hot reload (§9.3); deterministic single-actor replay
  (§9.4); a DAP debugger; an LSP; `strand test` and `strand doc`.
- **The web:** the content-index specification (§12.6); privacy hardening
  (§12.7).

## 16. Decision Log

The load-bearing choices, where an implementer could plausibly choose otherwise.

| Decision | Choice | Why |
|---|---|---|
| POC shape | A full vertical slice | One layer alone proves little |
| Implementation language | Rust | wasmtime, tokio and wgpu; ownership matches the runtime semantics |
| Execution engine | Embed wasmtime | Weeks, not years |
| Errors | `Result` and `?`; a panic kills the actor | Composes with arenas and supervision |
| Concurrency | Colorless, structured scopes, actors | Free, because the M:N runtime is mandatory |
| Value layout | Core WASM and our own layout. No GC types | The compiler does not wait for toolchain maturity (§6.1) |
| `Result` layout | Two WASM values, never boxed | A bump arena never frees, so a box leaks on every fallible call (§6.2) |
| Message layout | The wire format is the memory format; a message must be flat | A copy into another arena needs no decode step (§6.8) |
| `Node` type | Zero-width; a view appends to a post-order array | The array is in tree order by construction, not by discipline (§6.9) |
| UI | A scene graph on a platform-owned render thread | Removes the framework tax and the jank bug class |
| UI syntax | A typed builder DSL; JSX later as sugar | JSX worked around missing tree syntax in JS |
| Styling | Typed scoped props; no cascade; typed theme tokens | The cascade made CSS append-only |
| UI pipeline | A flat render command array from a per-frame arena | Renderer-agnostic, diffable, serializable; zero GC pressure |
| Reactivity | The actor is the re-render unit; signals later | A bounded blast radius by construction beats manual memoization |
| Toolchain | One binary, zero configuration, one true format | gofmt ended arguments, not just formatting |
| Hot reload | Tier 1 in the POC; Tier 2 stretch; Tier 3 deferred | It is a supervisor restart with newer code (§5.4) |
| Debugging | Message tracing and crash reports first; DAP later | A typed message log is a complete input record |
| Storage | One typed, async, transactional, capability-scoped API | The web never shipped one good one |
| Credentials | No ambient authority; a session is a capability token | Ambient authority is the root cause of CSRF |
| Rendering strategy | Resume, do not hydrate | The snapshot already exists for hot reload and crash reports |
| Distribution unit | A content-addressed typed WASM component | Immutability as physics; one cache per machine |
| Registry | Names as metadata over hashes; open protocol; foundation governance | The ecosystem's root of trust must not be a company's asset |
| Integrity | Transparency log, provenance, reproducible builds | A compromise leaves cryptographic evidence |
| Versioning | Enforced semver plus Minimum Version Selection | A build never changes in silence |
| Version schemes | CalVer for the platform, semver for a package | Two audiences: a machine needs a contract, a human needs an age signal |
| Install-time code | Banned; builds are sandboxed and pure | postinstall is npm's top attack vector |
| Trust model | Capability manifests, summed across the tree | Turns unauditable transitive trust into a short verified list |
| Transport | HTTP/3 and QUIC; no custom wire protocol | Every winner rode existing rails |
| Naming | URLs, DNS and TLS, unchanged | This layer was never the problem |
| Caching | Immutable hashes; cache forever; any source | Deletes cache invalidation; mirrors and offline use come free |
| Compatibility | Content negotiation: one URL serves a manifest or HTML | Adoption never splits a link |
| Session | Typed channels over WebTransport, zero-copy | Deletes REST, WebSockets and JSON; supervision handles network failure |
| P2P | An optional cache tier, never load-bearing | Mandatory P2P inherits P2P's floors |
| Backwards compatibility | Deferred and documented | The POC proves the new model; compatibility is a separable bet |

## 17. Prior Art

Almost every entry below wins because it **deletes a subsystem**. LMDB deletes
the write-ahead log. WireGuard deletes cipher negotiation. esbuild deletes
compiler passes. TigerBeetle deletes malloc. Immediate-mode UI deletes retained
state.

Strand's deletions so far: no hydration, no cascade, no try/catch, no z-index,
no `async` keyword, no cookies, no decode step (§6.8, §6.9), no cache
invalidation (§12.3).

### 17.1 The two that shape the POC directly

**clay** (nicbarker/clay) — a single-header C layout library. Adopted directly:
the render command array (§7.1), the static per-frame arena, the
`fit`/`grow`/`fixed`/`percent` vocabulary (§7.3), attach-point floating elements
(§7.3), and the debug inspector as injected render commands (§9.4). clay is not
thread-safe, so it informs the layout algorithm and the API shape, not the
concurrency architecture. Its algorithm is the reference if we outgrow `taffy`.

**raylib** (raysan5/raylib) — the DX north star. Zero dependencies, a ten-line
hello world, 140+ examples instead of a specification, and a stable C API with
bindings in 70+ languages. Adopted: hello world fits on a slide, the platform is
batteries-included, docs are examples-first, and the host ABI (§6.7) stays
boring so another language can target the VM.

### 17.2 The canon

| Project | Lesson |
|---|---|
| SQLite | Reliability is the feature. A small, boring, self-contained interface wins for decades |
| LMDB | A memory-mapped copy-on-write B-tree deletes the WAL, the cache layer and the background threads. Peak elegance is what you did not build |
| TigerBeetle | Static allocation at startup — the arena philosophy at database scale. Deterministic simulation testing makes the actor VM testable. Section 6.9 already follows the arena rule |
| Redis, early era | A single-threaded event loop plus the right data structures beats complicated concurrency |
| LuaJIT / Lua | Constraint as design: a complete language on one data structure |
| BEAM and OTP | Section 5 as running software. Study the implementation choices; you will face each one |
| Chez Scheme | The nanopass compiler: dozens of tiny, verifiable passes. This shape stays debuggable as it grows |
| Turbo Pascal | Compiled, edited and ran in 64K. The ancestor of the one-binary doctrine (§9.1) |
| TeX | Software can be finished |
| esbuild | 100x through a fast language, parallelism, one parse, and refused features |
| WireGuard | ~4000 lines replace hundreds of thousands. Fewer knobs *are* the security model |
| seL4 | Capability security costs nothing at runtime when you design it in (§10.2, §11.6) |
| nginx | Event-driven master and worker beat thread-per-connection. The ancestor of §5.2 |
| ripgrep | Finite automata done properly, plus respect for the memory hierarchy. The Rust codebase to read first |
| qmail / daemontools | Mutually untrusting components, minimal privilege, narrow interfaces. Actor isolation, shipped in 1998 |
| id Tech | Precompute the structure so the hot loop does almost nothing. Do the thinking at layout time, not paint time (§7.1) |
| RollerCoaster Tycoon | One person, in assembly. Not a practice to copy; a ceiling |
| Dear ImGui | Debug tooling, input and layout stay simple when the API refuses retained state |
| Zed / GPUI | M3 shipped as a product. Read it when the renderer gets hard |
| Git's object model | An immutable content-addressed DAG with four object types. The precedent for §11.3 — and a reminder that an elegant core does not excuse an inelegant CLI |
| Cap'n Proto | The wire format is the memory format, so a parse costs nothing. Applied in §6.8, §6.9 and §12.4 |

### 17.3 The landscape

| Project | What it proves | Why it is not Strand |
|---|---|---|
| Flutter | The full stack works: own language, no DOM, GPU scene graph, adoption | Single-threaded isolates, no supervision, exceptions kept. An app framework, not a sandboxed platform |
| Lunatic | Supervised WASM actors in Rust are buildable — section 5, nearly verbatim | Server-side only, no language, no UI. Stalled: a runtime without a platform lacks market pull |
| wasmCloud | Actors plus capability security work at production scale | Cloud infrastructure. No UI, no language |
| Makepad, GPUI, Slint, Iced | Rust "own renderer, no DOM" UIs ship at 120 fps | App frameworks for trusted code. No sandbox, no language, no supervision |
| Blazor, Uno, Yew | There is demand for a non-JS language in the browser | They render through the DOM, so they inherit the tax we design out |
| Dioxus Blitz | HTML and CSS rendering without a browser engine is feasible | It aims at today's content; relevant to compatibility, not the new model |
| Flash, Silverlight, applets | Own VM plus renderer can reach mass adoption | Died of proprietary ownership, plugin security and vendor politics. Open spec, capability sandbox, no plugin model |

The defensible position is the intersection of four properties: a typed language
whose types survive to runtime, supervised actor isolation, a platform-owned
scene graph, and an open sandboxed platform. Every row above holds one or two.
None holds all four.

### 17.4 Reading order

1. **Lunatic source** — section 5 is built there. Learn from the code, and from
   why it stalled.
2. **ripgrep and esbuild** — how to structure the Rust codebase, and how to
   treat performance as architecture.
3. **TigerBeetle simulation testing** — before the scheduler, so determinism is
   designed in.
4. **clay.h and Dear ImGui** — before M3.
5. **BEAM internals** — before Tier-2 hot reload and supervision edge cases.
6. **Chez nanopass papers** — before the compiler grows past the POC subset.

## 18. Open Questions

- **Definition-level against module-level addressing.** Per-definition hashing
  dissolves more conflict classes, and complicates tooling. Module level is the
  pragmatic start. Revisit if version-conflict pain appears (§11.3).
- **Private registries.** The protocol must support mirrors and private scopes
  over the same log format. The trust model for merging a private tree and a
  public tree needs design (§11.4).
- **Capability granularity.** `net(host)` is right. Open: whether storage needs
  sub-scoping for a quota or a namespace, and how `spawn` interacts with
  capability inheritance for a child actor (§11.6).
- **Funding.** Whether the registry builds in sponsorship rails is a governance
  question. Answer it before scale (§11.4).
- **The content-index specification** (§12.6): freshness, abuse resistance, and
  its relationship to the HTML projection.
- **Privacy hardening** (§12.7): which mitigation fits the latency budget, and
  how to defend against cache probing.
- **Session resumption across a network move.** QUIC connection migration helps.
  Channel semantics across an IP change and a long mobile suspension need a
  definition — likely the same snapshot machinery (§10.4).
- **Manifest signing lifecycle:** key rotation, domain transfer, and whether an
  application manifest joins a transparency log as a package does. Leaning yes
  (§11.4).

## Appendix A — Screenshots

M3 color-space and root-fill fixes:

- `screenshots/m3-before-srgb-and-root-fill.png`
- `screenshots/m3-after-fixes.png`
