# Project Strand — Design Document

*A new browser runtime: a typed language, a multithreaded actor VM, a declarative scene graph.*

**Status:** Draft v0.3 · **Date:** August 2026 · **Name:** a placeholder. You can change it.

**Language note.** This document obeys the writing rules of ASD-STE100 Simplified Technical English. Words such as "actor", "arena", "fiber", "channel", "supervisor", and tool names are technical names. Code blocks are source text. The rules of STE do not apply to source text.

Sections 1 to 9 specify the proof of concept (POC). Sections 10 to 12 specify post-POC design that constrains the POC primitives. Sections 13 to 18 give the plan, the risks, the decisions, and the open problems.

---

## 1. Vision

The core architecture of the web platform is from the 1990s. A script language with one thread controls a retained document model. Types, concurrency, security, and app-style UI came later as additions.

Strand is a design for a platform that starts new in 2026. Strand also keeps a path to the current web.

The POC is a full vertical slice. A typed language compiles to a multithreaded VM. The VM controls a declarative scene graph renderer. A todo application shows the result. The goal is not a complete layer. The goal is proof that the layers connect correctly.

## 2. Goals and Non-Goals

The POC must show five properties:

1. A typed language with a TypeScript flavor. Types stay available at runtime. An error is a `Result` value. Concurrency is colorless.
2. An actor VM. Each component operates in an isolated arena. The scheduler assigns actors M:N across OS threads. Actors communicate only through typed channels.
3. A declarative UI layer. The scene graph stays on a render thread that the platform owns. Application code cannot decrease the frame rate of the compositor.
4. Crash isolation. An actor that has a panic stops, and the runtime releases its memory. The application continues.
5. All properties above, together, in a todo application.

These items are not goals of the POC. Section 15 keeps them as future work: compatibility with HTML, CSS, and JS; a custom bytecode VM; content-addressed module distribution; the capability security model; a network stack; persistent storage; accessibility; text input that is more than the minimum for the todo application.

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

**The implementation language is Rust.** wasmtime and cranelift supply the execution engine. tokio supplies the M:N scheduler. wgpu supplies the portable renderer. Rust ownership shows the core invariant directly: a buffer transfers, and it never shares. wasmtime, Deno core, and wasmer use Rust, so prior art is available.

**Embed an engine. Do not build one.** The POC embeds wasmtime. The POC does not include a new bytecode engine. This decision changes a VM project of one year into an integration project of weeks. The result gives proof of the same properties. A custom VM is future work, possibly in Zig, because comptime is a good tool for interpreter dispatch.

## 4. The Strand Language

### 4.1 Position

Strand is TypeScript with the lessons applied. The curly-brace syntax is easy to read for a web developer. The known semantic holes are closed. The checker knows each type. Section 6.1 tells which types the emitted WASM keeps.

### 4.2 Lessons from JS

| JS problem | Strand decision |
|---|---|
| `null` and `undefined` | One `Option<T>`. `string?` is `Option<string>` |
| Implicit coercion (`"1" + 1`) | None. `==` has `===` semantics |
| `this` binding | No `this`. A method receives an explicit receiver. UI is functions |
| try/catch is not visible in a signature | `Result<T, E>` in the signature. `?` moves the error to the caller (§4.3) |
| A rejection with no handler | No promises. Concurrency is colorless (§4.4) |
| async/await colors each function | No `async` keyword. A blocked call suspends the fiber |
| The ESM and CJS split; an import with side effects | One static module format. An import never runs code |
| Data is mutable by default | `let` is immutable. `var` is mutable. Data structures are immutable |
| No standard library | The standard library is included from day one (§11.7) |

### 4.3 Errors — two tiers, no try/catch

**Tier 1. An expected failure is a value.** A fallible function returns `Result<T, E>`. The `?` operator moves the error to the caller, so the good path stays straight. Rust, Swift, Zig, and Gleam agree on this design. Go shows the cost of results without `?`.

**Tier 2. A bug is a panic.** An access out of bounds, a failed assertion, or an overflow shows a broken invariant. Thus the unit with the failure stops. A panic stops the current actor only. There is no catch. Recovery is the task of the supervisor (§5.4). This design agrees with the arena model: one deallocation releases a dead actor.

### 4.4 Concurrency — colorless, structured, actor-isolated

**In an actor, a blocked call is colorless.** Each function can suspend. There is no `async` keyword. No function has a color. `sleep(1s)` blocks the *fiber*. The scheduler then runs other fibers on that OS thread. The cost is almost zero, because the runtime contains an M:N scheduler in all cases.

**In an actor, a spawn is structured.** A child cannot live longer than its scope. Results join at the end of the scope. When you cancel the scope, the children cancel. This closes the goroutine-leak hole.

```strand
fn loadDashboard(): Result<Dashboard, LoadError> {
  scope {
    let user  = spawn fetchUser()?
    let todos = spawn fetchTodos()?
    Ok(Dashboard { user: user.join()?, todos: todos.join()? })
  } // scope exit: all children join or cancel — a leak is impossible
}
```

**Between actors, there are only messages.** Actors share no memory. A channel is typed. A send *transfers ownership* of a buffer. The sender then has no access. The Rust host makes sure of this at zero cost. Thus a data race is not possible in the model.

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

Data is immutable (§4.2). Thus a state transition makes a new record. The spread names the fields that change. The spread does not give the fields again that stay the same:

```strand
fn withDraft(state: Model, draft: string): Model {
  Model { ...state, draft: draft, notice: "" }
}
```

### 4.6 POC compiler scope

The lexer is written by hand. The parser uses recursive descent. The checker is bidirectional: a signature must have annotations, and the checker infers a local. Full inference is future work. The compiler emits WASM through `wasm-encoder`.

The subset is fixed. It contains: the primitives (`int`, `float`, `bool`, `string`), records, `List` and `Map`, sum types and `match`, `Option`, `Result` and `?`, functions and closures, `scope`, `spawn` and `join`, actor declarations, and the UI builtins (§7). All other features are out.

### 4.7 The actor surface

An actor declares its state, its channels, and one handler for each inbound channel. The channels have names. The actor can say nothing else about the other end.

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

Three rules carry most of the weight. Section 6.8 gives the reasons:

- A handler has the name of the port that it serves. Thus a channel and its code cannot move apart.
- `send` names one of the out ports of the same actor. **No expression in the language names a different actor.** Thus an actor communicates only with the peers that the `app` block connects to it.
- `panic(msg)` stops the actor and nothing else (§4.3). Its type is `Never`. Thus it is a permitted match arm, or a permitted tail where a value is necessary.

`send` is not available in a `view fn`. A view is a pure function of state (§7.5). The platform runs a view again at a time of its choice. A send from a view would send again on each run. Also, Tier-1 hot reload (§9.3) is safe only because a second run of a view has no effects.

## 5. The VM and Actor Runtime

### 5.1 The actor is the unit of each property

- **Isolation.** The actor owns one wasmtime `Store`. That Store is its arena.
- **The schedule.** The actor is one tokio task.
- **Failure.** A panic stops at the actor boundary.
- **Release of memory.** A drop of the Store releases the arena in O(1). No tracing GC operates across the application.

An application is a supervision tree. The todo application uses two guest actors (§8).

A bump allocator serves the arena. The allocator never releases memory before the end of the actor. Each heap allocation stays for the life of the actor. Section 6.2 depends on this rule.

### 5.2 The schedule

The tokio multithreaded runtime gives an M:N schedule with work stealing.

A blocked call compiles to a host function. The host function is async on the Rust side, through wasmtime async support and epoch interruption. Thus a blocked actor uses no OS thread.

An epoch boundary interrupts long computation. Thus one hot actor cannot stop the other actors. This rule, plus the render thread that the platform owns, is the structural correction for "do not block the main thread".

### 5.3 Typed channels and ownership transfer

A channel declares a message type. The compiler checks the two ends.

The runtime copies a small value. The runtime transfers a buffer: the host moves the allocation between Stores, and the handle of the sender becomes invalid. `postMessage` transferables pointed at this design. Strand makes the design mandatory. Section 6.8 gives the byte-level rule.

### 5.4 Supervision

Each actor has a parent. After a panic, the runtime stops the actor, releases the arena, and sends a typed `ChildDown(reason)` message to the parent.

The parent selects one of three strategies: a restart with fresh state, a restart from a snapshot that the child made, or escalation.

The UI system shows a "component failed" boundary for a dead UI actor. This is a React error boundary that the platform makes mandatory.

**POC demo:** the todo application contains an actor that stops on command. The supervisor restarts the actor. The application shows no interruption.

## 6. Value Representation and Host ABI

This section applies to the POC only. It specifies the compiler as built today.

### 6.1 No WASM GC, no Component Model

The compiler emits core WASM modules, with linear memory and our own layout. Section 14 lists this layout as the approved alternative for immature GC types. We use the alternative from the start. Thus the compiler does not wait for the toolchain.

The emitted WASM does not keep the types. The checker knows the types. No property in section 2 depends on GC types.

### 6.2 `Result<T,E>` and `Option<T>` are multi-value, never boxed

A function that returns `Result<T,E>` returns two WASM values:

    (i32 tag, i64 payload)     tag: 0 = Ok/Some, 1 = Err/None

The payload is one 64-bit word. Its content obeys the *static* type at each site: an `int`, the bits of an `f64`, a `bool`, or a 32-bit pointer into the arena of the actor. The payload has no runtime tag, because the checker knows the applicable arm.

`?` compiles to three steps: the call, a test of the tag, and, for a tag that is not zero, a return of the pair without change. There is no allocation, no copy, and no unwind.

**Why the value is not a pointer to a tagged struct.** The bump allocator releases no memory before the end of the actor (§5.1). Section 4.3 sends *each* fallible call through `Result`. The demo calls `addTodo` at each keystroke. At the same time, a debug overlay shows the live arena size of each actor (§8). A boxed `Result` would cause a leak at each fallible call. The leak would then show in the overlay that must show isolation.

### 6.3 A user sum type is boxed, with one size for all variants

`type AddError = | EmptyTitle | TooLong(max: int)` becomes a pointer to `{ i32 tag, fields... }`. An enum where no variant has a payload becomes a bare `i32` tag.

Each variant of a sum uses the same space: one word for the tag, then one word for each field of the **widest** variant. A narrow variant keeps some bytes unused. In exchange, the size of a value is a property of its type, not of its tag. This rule lets `send` (§6.8) put a constant length on the wire. Without the rule, the runtime computes a length from the tag, which is a small unnecessary subsystem. The rule also removes a hazard: a block with an exact fit at the end of the arena, read at the width of the widest variant, would read past the end of linear memory.

Only an error path makes these values. Thus an allocation there is acceptable at POC scale. The M5 measurement can change this decision (§13).

### 6.4 A record is a pointer to a flat struct

The struct is in linear memory. Each field is at an offset that is known statically. A slot is 8 bytes.

A record is immutable (§4.2). Thus an update allocates a new record. The leak note of section 6.2 applies. M5 can change the decision.

`Model { ...state, draft: x }` is sugar. It adds no representation. The checker binds the spread to a local. The checker changes each field that the literal does not give into a read of that local. Thus the emitter sees the usual record construction. There is no update instruction. Thus no later change can write in place.

The binding is the important part. An inline base would run once for each field that it fills. Then `Model { ...next(), b: 1 }` would call `next` five times.

The spread comes first. In a different position, a reader must scan the full literal to find the field that wins. That freedom gives no value.

### 6.5 A string is a pointer to a header and bytes

The layout is `{ i32 len, bytes... }`, UTF-8, immutable. The POC does not intern strings.

### 6.6 A list is a pointer to a header and elements

The layout is `{ i32 len, <pad>, elements... }`. The header uses a full word. Thus the elements stay aligned to 8 bytes. With this alignment, the emitter loads an element with the same code that loads a record field.

An element uses `words(T)` slots, the same as a record field:

- A `List<int>` has a stride of 8.
- A `List<Todo>` has a stride of 8, because an element is a pointer.
- A `List<Result<int, E>>` has a stride of **16**, because a `Result` uses two words (§6.2).

A stride of one word would read the tag of each element out of the payload of the element before it.

A list is immutable. `push` allocates a list with one more element and copies, at O(n). The alternative is a buffer that can grow, with a hidden capacity. That is a different design.

An empty literal gets its element type from the context. The checker refuses an empty literal with no context, because a `List<?>` has no representation, and a guess fails much later.

`for x in list { ... }` walks the list. The loop is an *expression* of type unit. Thus it can stand with the children of a container (§7.2), as `if` does. A view appends while it runs. Thus each pass appends, and the child count of the parent is correct with no more work.

### 6.7 The host ABI

A guest imports from module `strand` and exports a small set of entry points. M0 makes the ABI. Each subsequent milestone extends it.

The emitter writes an import **only when the program calls it**. Thus a module that does not touch the host starts with no imports. An import uses a low function index. Thus each defined function moves past the imports. The emitter applies that offset one time.

Strand code can call `log(msg: string)`, `send(port, value)`, and `panic(msg: string)`. Each call unpacks its argument into the `(ptr, len)` pair that the host expects. `log` and `panic` unpack from the string header of section 6.5. `send` obeys section 6.8.

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

`panic` is the second tier of §4.3. It goes up and out of the guest. The runtime drops the Store, the arena goes with it, and the supervisor receives a crash report. The reason in the report is the message. The reason is the useful part. Thus `panic` is a host call and not a bare `unreachable`. WASM cannot declare an import that never returns. Thus the emitter puts `unreachable` after the call. Without it, a `panic` in tail position would exit a function that must give its caller a value.

The functions of an actor — `init`, its handlers, its view — are **not** exports. The entry points above reach them. Two actors in one file can both declare `init`. That is the normal case, not a clash.

The emitter interns the import signatures before it writes the type section. Interned at the import section, an import with a new type names a type index that the module does not contain. The bug was not visible while the `(i32, i32)` of `log` matched the old two-argument `strand_on_message`.

### 6.8 Channels are ports, and the wire format is the memory format

An actor declares its channels by name and type, and nothing else:

    actor Meter {
      state: Reading
      in  samples: Sample     // what it can be told
      out totals:  Total      // what it can say
      ...
    }

Each `in` port has a handler with the name of the port — `on samples(state, msg): State`. `send(totals, value)` puts a value on an out port. **The index is the protocol.** The checker changes a port name into its position in the inbox or the outbox of the actor. That number crosses the boundary. A name never crosses.

**There is no actor address.** No language construct can name a different actor. Thus an actor communicates only with the peers that the wiring gives it. The wiring is in an `app` block (§6.12). The registry holds the wiring. The guest holds none of it. Two results follow. Location transparency (§10.5) is not a feature to add: a port with its far end on a different machine changes the `app` block and nothing else, because there was never a local address to replace. Also, the capability model of §10.2 gets its first part free: there are no addresses to check.

**A message type must be flat.** A field can be `int`, `float`, or `bool`. It must not hold a pointer. The checker makes sure of this and gives the reason: the runtime copies a message into a *different* arena, where a pointer from the arena of the sender points at nothing.

That restriction gives the Cap'n Proto property (§17): the wire format is the memory format. The bytes on the channel are exactly the boxed-variant layout of section 6.3. The runtime copies the bytes into the receiving arena with `strand_alloc`. The pointer that it holds is then a valid value. `strand_on_message` gives that pointer directly to the handler of the port. There is no decode step. A decode step would be the bug.

| Message type | Bytes on the wire |
|---|---|
| A sum with a variant that has a payload | `i32 tag` in the first 8-byte slot, then one 8-byte slot for each field of the widest variant (§6.3) |
| A sum where no variant has a payload | A bare `i32` tag |
| `int` or `float` | The 8-byte value |
| `bool` | `i32` 0 or 1 |
| `string` | The raw UTF-8 bytes. Codegen adds the header of §6.5 at arrival |

A `string` is the one case that moves. It is safe because codegen makes the header again in the receiving arena.

A send is the same layout, read in the other direction. A boxed variant already has the wire layout. Thus `send` gives the pointer that it holds, and a length that is a constant of the type (§6.3). An immediate — an `int`, a `bool`, a bare tag — has no address. Thus codegen keeps one word of static scratch and writes the value there. The host copies the bytes before the call returns. Thus one slot serves each send in the module.

The host encoder that the CLI uses reads the layout from the same `Hir`. Thus the two send paths agree with the receive path by construction. No procedure keeps them in step.

### 6.9 The frame: how a view crosses into the host

A `view fn` (§7.2) does not return a tree. It **appends** to a fixed array in its own arena and returns nothing. `Node` has no runtime representation.

One record for each node, 32 bytes:

| Offset | Field | Notes |
|---|---|---|
| 0 | `i32 kind` | The widget (`strandc::ui::NodeKind`) |
| 4 | `i32 child_count` | The number of roots before it that belong to this node |
| 8 | `i32 id` | The hit id. 0 means the node gets no input |
| 12 | `i32 flag` | `checked`, `focused` |
| 16 | `i32 text` | A string pointer (§6.5), or 0 |
| 20 | `f32 number` | `gap`, or the `offset` of a scroll |
| 24 | `f32 number2` | `padding` |
| 28 | `i32 text2` | A second string, for the one widget that uses two |

**The array is in post-order.** A view emits while it runs. Thus the children of a container end first, and the container records the number of roots that belong to it. One pass from left to right, with a stack, makes the tree again. Nothing moves. Nothing gets a patch after the fact. There is no decode step.

Codegen keeps one counter, `pending`: the number of complete roots with no parent. A builder saves the counter before its children run. The builder then gives the saved value to `node_push`. `node_push` computes `child_count = pending - marker`, then sets `pending = marker + 1`.

That subtraction replaces the child stack that a tree builder uses. It also makes a conditional child free: an `if` that does not run appends nothing, so its parent counts one child less.

**Why `Node` has zero width.** A view emits a node where the programmer writes it. A value that code can keep, pass, or use again would be a node that shows in a different place. A type with no content makes the checker refuse `let n = text("hi")` and `fn f(n: Node)`. Thus the array is in tree order by construction.

Exports:

    strand_nodes           // i32 global: where the array starts
    strand_node_count      // i32 global: how many records are in it
    strand_frame_reset()   // empty the array before the next frame
    strand_view()          // reset, then draw the actor as it is now

`strand_view` exists only on an actor that declares a `view fn`. It has no argument, because a view is a pure function of state (§7.5), and the state global is the only state. It resets the arena, reads global 1, and calls the view.

The runtime calls `strand_view` after `strand_main` and after each message. The runtime then gives the region to the component that asked for frames. The runtime knows *where* a frame is. It knows nothing about the meaning of a frame. Layout, the widgets, and the compositor are behind a trait with one method. Thus the actor runtime stays free of the renderer.

The array holds 2048 nodes (`ui::NODE_CAPACITY`), between the static data and the bump heap. A view that goes past the limit causes a trap. The array does not grow. The trap arrives as a crash report with the name of the actor (§9.4). A silent cut would arrive as a view that stops halfway down.

`crates/strandc/src/ui.rs` is the single table for all of this. The parser, the checker, codegen, and the host decoder read it. Thus the two ends cannot disagree about a byte.

### 6.10 Generated string helpers

`+` on strings, and the functions in `crates/strandc/src/stdlib.rs`, compile to WASM that the emitter puts into the module. They are not host imports. Thus a program full of them starts with no linked runtime, and `strand run` operates alone.

    str(value: int): string       // decimal
    char(code: int): string       // one character, from a scalar value
    len(s: string): int           // characters, not bytes
    isEmpty(s: string): bool      // len(s) == 0
    trim(s: string): string
    dropLast(s: string): string   // what Backspace does

The emitter writes only the helpers that a module calls, as with the imports. The helpers come last in the layout. Thus they cannot move an index that a different part computed.

Three properties follow. Each property prevents a bug that a person writes without it:

- **A string stays immutable.** A helper allocates. It never changes its argument. `a + b` keeps the two arguments usable. Thus `first + (a + b)` has the meaning that it shows.
- **A count that a user sees is in characters.** A UTF-8 continuation byte is `0b10xxxxxx`. Thus `dropLast` steps back across them and removes a full character.
- **`str` reads the magnitude as unsigned.** A negation of the most negative `int` gives the same value again. Read with no sign, that bit pattern is the correct magnitude. The obvious method prints one incorrect digit, or it loops.

`+` between a string and a number stays refused. The complaint in section 4.2 is about `"1" + 1`, not `"a" + "b"`.

### 6.11 Input is the message type of the platform

A UI actor receives input as a normal message (§7.1). Thus its mailbox carries a type that the platform declares:

    Click(id: int)
    Typed(ch: int)          // the Unicode scalar value
    Backspace
    Enter
    Escape
    Focus(id: int)          // 0 when nothing holds focus
    Scrolled(id: int, offset: float)

Each field is an `int` or a `float`. Thus the flat-message rule holds with no exception (§6.8). The encoder that puts input on the wire is the encoder that the CLI uses for each message.

**Why the platform declares the type.** The alternative was an event type that the actor declares, where the host fills the variants with known names. Spelling would then hold the protocol together: change `Click` to `Pressed`, and clicks stop with no error. A platform declaration means that the checker knows the type, a `match` is exhaustive, and an incorrect name is a compile error.

**Why the type is opt-in.** The type also registers `Click`, `Enter`, and the other constructors. A UI program possibly wants those names for itself. Thus the type appears only in a module that names `Input` as the type of a port. A module with its own `type Input` keeps its own.

The platform finds the port by **type, not by name**. The `in` port that carries `Input` is where clicks arrive. The name `input` is a convention. The type `Input` is the fact. A match on the name would be the protocol that spelling holds together. This subsection exists to prevent that protocol.

`crates/strandc/src/input.rs` is the single table. The checker reads it. The host translation from `InputEvent` reads it.

### 6.12 The app block: the supervision tree, written down

    app Pipeline {
      meter    = Meter
      reporter = Reporter

      meter.totals -> reporter.totals
    }

An instance is an actor that runs, under the name that the wires use. A wire connects the out port of one actor to the in port of a second actor. The checker makes sure that the two ends exist, and that their types match exactly.

Each out port must have a wire. A port with no wire is a `send` that disappears. "Your messages went to no place", found at runtime, is the opposite of the diagnostics goal of §9.2.

The two ends of a wire can be the same actor. A port with a wire back to its own actor is a loop with its own schedule. `todo_demo.str` uses this loop to keep a CPU burn going as many short handler calls, not as one long call.

The block is in the source, not in a configuration file. The wiring is typed, and only the compiler can check it. §9.1 asks for zero configuration files. Wiring that the compiler cannot see is wiring that fails later, in a different place.

**One module for each actor.** A module carries the ABI of one actor: the state global, `strand_main`, and `strand_on_message` are singular by construction. A file with several actors compiles one time for each actor. This also gives each actor its own arena. A file is a unit of source. The unit of isolation of §5.1 is the instance. The emitted modules are almost identical. Only the functions that the entry points call differ. Thus the cost is in bytes, not in isolation.

A file with one actor and no `app` block is an app of one actor with no wires. There is one path, not a general path plus a special case.

### 6.13 Lifecycle: the news that a guest gets about its peers

§5.4 sends a typed `ChildDown` to the supervisor. In the POC the supervisor is the host, and there the news stopped: a guest could not learn that a peer was dead. The demo of §8 makes this necessary. Thus the platform declares a second type. An actor opts in exactly as with `Input`: it names the type as the type of a port.

    Down(port: int)     the peer that feeds this port of mine is dead
    Up(port: int)       a new peer is in its place

A port identifies the peer, because no other name exists. An actor holds no addresses (§6.8). Thus "who is dead" can only use terms that the receiver has: `port` is the index of the receiver's own `in` port that the dead peer had a wire to.

`Up` occurs for the **first** life of an actor and for each replacement. A first start is the same news. With one rule, no peer must send a speculative first message. In `todo_demo.str`, the first `Up` asks for the first count.

Two order rules change this from a race into a sequence:

- **The registry makes each mailbox before an actor runs.** Thus an actor that sends from `init` does not depend on the start order of the tasks. Without this rule, "can I send to you yet" is random.
- **The new life announces `Up`, after it takes its mailbox.** The supervisor does not announce it before. A peer answers `Up` immediately. That answer must have a destination. At a restart, the supervisor makes a fresh mailbox first. A message sent in the gap waits there. The runtime does not refuse it.

The runtime sends bytes that the watch setter encoded. To encode a value is to know the layout of a type. A second implementation of §6.8 in the runtime is exactly what the host encoder exists to prevent.

**Known gap.** A comparison against the port index uses a written number. There is no syntax for "the index of my `tally` port". With one peer, the gap has no effect. Syntax invented before a use would be a guess at the shape.

`crates/strandc/src/lifecycle.rs` is the single table, on the same terms as `input.rs`.

### 6.14 The state snapshot: an image, and a list of the pointers in it

The runtime must move the state of an actor into a different arena (§9.3). A Strand value is a block of bytes with pointers into the arena that holds it. Thus the host copies each object that the state can reach into one image. A pointer in the image is an offset from the start of the image. A list of the positions of those pointers goes with it.

    Snapshot { shape, bytes, relocations, root }

To put the image into a new arena is three steps: one `strand_alloc` for the full length, one write, and an addition of the new address to each position in the list. The state global then gets the root.

A relocation is 4 bytes wide. This width is sufficient for the payload slot of a `Result`, because codegen extends a pointer into 64 bits with zeros (§6.2). Thus the low 4 bytes are the full pointer.

The walk reads the layout from the same `Hir` as §6.8 and §6.9. Three properties come from immutable data (§4.2):

- The walk stops, because data has no back-reference. Thus there is no cycle.
- Sharing stays. The walk copies each address one time. Thus two fields that point at one list point at one list after the move.
- A state that a stopped actor leaves is readable. A handler that stops did not write its result. Thus the arena holds the last good state.

`shape` is a structural description of the state type. The compiler makes it. The runtime compares two of them for equality, and it does nothing else with the bytes. The rule of §6.8 holds here: a second implementation of the layout in the runtime is the mistake that the host encoder prevents.

`crates/strand-cli/src/snapshot.rs` walks the state. `crates/strand-runtime/src/snapshot.rs` moves the image and compares the two shapes.

## 7. UI: The Declarative Scene Graph

### 7.1 Model

There is no DOM, no HTML, and no diff in user code. UI is a function of state. The platform owns reconciliation.

- An application actor builds a UI tree. A view function appends nodes (§6.9). The actor submits the tree to the render actor over a channel.
- Layout changes the tree into a flat typed **render command array**: rect, text, image, clip-start, and clip-end. clay gives proof of this architecture (§17). The array is independent of the renderer, easy to diff, and cheap to serialize over a channel. It also compiles to HTML, which is the compatibility story in the other direction (§12.3).
- Layout allocates from a per-frame arena and resets the arena at each frame. Thus UI adds no GC pressure. clay uses approximately 3.5 MB for 8000 elements.
- The **render thread** belongs to the platform: a winit event loop and wgpu. It diffs a command array against the retained scene graph, and it paints. Layout uses `taffy` first. The clay algorithm is the reference for a replacement.
- An input event flows back as a typed message, to the actor that owns the hit node.

A submission is a message. Thus a slow actor delays *its own* updates only. The compositor keeps its frame rate. This is the most visible property, so the demo includes a "spin the CPU" button.

### 7.2 UI syntax: a typed builder DSL

A JSX-like syntax (`<Row gap={8}>…</Row>`) is familiar. But structurally, it is a workaround for a language with no tree syntax. Its costs occur again and again: control flow through a nested ternary and `&&`, where `count && <Badge/>` shows a literal `0`; a manual `key` prop for a list; two syntax modes with a `{}` escape hatch in the two directions; and types added above the syntax.

A typed builder DSL with a block (`row(gap: 8) { … }`) is the design that each toolkit selected when the toolkit and the language were designed together: SwiftUI, Compose, Flutter. A conditional is a normal `if`. A list is a normal `for`, with a stable ID as the key. The checker checks children and props as arguments. There is no mode switch.

**Decision.** The DSL is the semantic core and the only POC syntax. A JSX-flavored surface can come later as sugar. The sugar changes into the DSL, exactly as JSX changes into `createElement`.

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

| HTML/CSS problem | Strand decision |
|---|---|
| "How do I center a div": alignment came out of five unrelated mechanisms | Alignment is a typed property of each container: `column(align: .center, justify: .center)` |
| Floats, tables, flex, then grid on a document model | `row`, `column`, and `stack` with flex semantics. `grid` is future work. No floats. No document flow |
| `justify-content` against `align-items`: the meaning changes with the direction | Flutter-style `mainAxis` and `crossAxis`. Clear at each orientation |
| The cascade: global scope, specificity rank, `!important`. A stylesheet becomes append-only | No cascade, no selector, no global style. A style is a typed prop adjacent to the view. An unused style is dead code, and the compiler shows it |
| A property in a string. `witdh: 10px` fails with no message | Typed style props. An incorrect name is a compile error |
| `content-box` against `border-box` | Border-box only |
| The unit zoo: px, em, rem, %, vh, vw, ch, ex | Logical pixels, fractional weights, percent of the parent |
| A media query examines the viewport. The container query came 20 years late | Response to size is container-based. A view branches on the size that it receives |
| Implicit stack contexts and z-index wars | Paint order is tree order. A float declares an attach point: `attach(element: .bottomCenter, anchor: .topCenter)`. There is no z-index. A float sorts by a small layer number, only against other floats |
| width, height, min, max, flex-basis, flex-grow, flex-shrink | The clay vocabulary: `fit(min, max)`, `grow(min, max)`, `fixed(n)`, `percent(p)` |
| Design tokens made again and again: Sass variables, custom properties, a Tailwind configuration | Typed theme constants as a primitive: `theme.spacing.md`, `theme.color.accent` |
| CSS-in-JS: styled-components, then emotion, then zero-runtime libraries | The ten-year direction is one rule: a style must compile, not execute. Typed props adjacent to the view are zero-runtime from the start |

### 7.4 The POC widget set

Seven widgets: `column`, `row`, `text`, `textInput`, `button`, `checkbox`, `scroll`.

A style is a small typed props struct: padding, gap, color, font size. wgpu paints. `glyphon` draws the text.

```strand
view fn todoRow(t: Todo, onToggle: fn(Id)) -> Node {
  row(gap: 8) {
    checkbox(checked: t.done, onChange: fn() { onToggle(t.id) })
    text(t.title, strike: t.done)
  }
}
```

### 7.5 State model

The POC uses the Elm model. Each UI actor owns a state record. An event is a message. A handler returns new state. The runtime then calls the view again.

There are no hooks and no effects system. The mailbox of the actor *is* the effects system.

### 7.6 Scale tokens and the behavior layer

**The Tailwind lessons.** The known lesson is the token set, and §7.3 has it. Two larger lessons stand above it.

First: **the worst work in CSS was the invention of names.** A "semantic" class name was a polite fiction, and BEM was ceremony for a global namespace that was not necessary. Utility styles adjacent to the markup won because *the absence of names* won. Typed props adjacent to the view keep this property.

Second: **constraint wins against freedom.** The true Tailwind product is a finite scale: spacing steps, a color set, a type ramp. Each selection is consistent by default, and the design system is the path with the least resistance. The current §7.4 props accept a raw float (`gap: 8`). That is the infinite-value problem of CSS, with types. The planned correction: a prop accepts a token from a typed finite scale by default (`gap: .sm`). A raw value stays possible, and it is visible as an escape. Tailwind `[13px]` values give proof that the escape must exist. Their overuse gives proof that the escape must look like one. A typed token is also easy to analyze. Thus dead-style removal is normal dead-code removal, with no purge step.

**The shadcn and Radix lesson.** A widget is two separable parts. The **behavior** is focus movement, keyboard navigation, dismissal, portal position, and the semantic roles. That is the hard, invisible 80 percent. The **appearance** is the easy 20 percent that each team wants to own. The ecosystem agreed: platform-grade behavior, user-owned appearance. The Strand widget set obeys this split. The platform includes the behavior in each widget kind. The appearance is the typed props that user code controls now. No person builds the keyboard navigation of a combobox a second time. No person is locked to its border radius.

### 7.7 Accessibility — a commitment, not a line item

A departure from the DOM is a departure from the free accessibility bridge. A screen reader understands HTML. It does not understand a wgpu canvas. Flutter fought this exact battle for years, with mixed results. This design must not undersell the problem.

The direction comes from §7.6. The behavior layer of each widget kind carries a role and a state. Thus a **semantic tree** derives from the widget tree. The platform exports that tree to the OS accessibility APIs. AccessKit is the probable bridge in the Rust ecosystem.

The POC does not include the export (§2). The commitment is in the design now: each widget kind carries its semantics from the start, so the semantic tree stays possible, and no widget must change later.

## 8. The Demo: A Todo Application

The application is simple by intention. Thus the architecture is the story. `examples/strand/todo_demo.str` is the full application, in Strand:

```
app TodoDemo
├── TodoUi actor      — owns List<Todo>, draws the window, takes input
│                       in input / in tally / in life / out stats
└── Stats actor       — derived counts, independently crashable
                        in commands / in spins / out tally / out again
   (platform) Render  — scene graph, wgpu, input routing
```

A reviewer sees this sequence:

1. Add, complete, and delete a todo. A `Result` shows a validation error: an empty title shows a notice, not a crash.
2. Push "crash stats". The Stats actor has a panic. This is a true `panic()` in Strand, not a division made to look like one. Its panel shows a failure boundary for a moment, because the platform told the UI actor that its peer was dead (§6.13). The supervisor restarts the actor. The `Up` that follows asks for a fresh count. The todos have no damage, because they were never in that arena.
3. Push "burn CPU". The Stats actor holds one core at full load with guest code. The load is Strand code, not a host call that does the work. Text input stays at frame rate. Stats gives itself work through a port with a wire back to itself. Thus the load continues, and the actor is between messages at each moment.
4. Open the debug overlay. Two rows, two arenas, and the generation number of Stats. The number increases at each crash.

**A difference from the design above.** §5 gives an AppState actor as the single writer, with the UI as a subscriber. The demo puts the two together: the UI actor owns the list. A split would show the same machine parts a second time — one more pair of ports, one more wire — and no new property. The ruthless-scope entry of §14 applies. Points 2 and 3 make the argument for the architecture. The two points only need Stats as a separate actor.

## 9. Developer Experience

DX is architecture here. The demos that make the argument for the *platform*, not for the todo application, are in this section.

### 9.1 One binary, zero configuration

`strand` is one binary: `strand run`, `strand fmt`, and later `strand test` and `strand doc`. The POC has no configuration file.

The format tool follows gofmt: one true style, no options. The achievement of gofmt was the end of arguments about format.

### 9.2 Diagnostics are a product surface

The core message is "types that stay to runtime". Thus the compiler is the first impression. Diagnostics get first-class treatment from M1: a source span, an underline with a label, and a suggested correction where one exists. The `miette` crate gives Rust-quality output at almost no cost. Good errors are cheap at the start. A retrofit is painful.

### 9.3 Hot reload — three tiers on supervision

Hot reload is a supervisor restart where the replacement actor runs newer code. Section 5.4 built the machine parts.

**The state moves in all cases.** New code is a new module, a new `Store`, and a new arena. Thus the record that the running actor holds points into memory that the runtime is about to release. The tiers below differ in what the *shape* of the state does, not in whether the state travels. The snapshot of §6.14 is the one machine part under all of them.

**Tier 1 — view reload. In the POC.** A view function is a pure `state → Node`. At a file change, the tool compiles the module again and gives the new module to the supervisor. The supervisor puts the state of the old life into the new one and the view draws again. The loop completes in less than one second.

**Tier 2 — actor logic reload. In the POC.** The behavior changes. The state shape does not. Tier 1 and Tier 2 use one code path, because a change to a view and a change to a handler are the same event: a new module. A state record is typed. Thus the runtime compares the shape that the snapshot carries against the shape that the new module declares, and it restores the state only if the two agree. Erlang hot code load cannot make that check.

**Tier 3 — schema migration. Future work.** The state shape changes. Today the replacement runs `init`, and the tool names the field that changed. An optional `migrate(old) -> new` is future work.

`strand view <file> --watch` is the loop. It examines the modified time of one file two times each second. A file that does not compile leaves the application in operation, and the diagnostic goes to the terminal (§9.2). A file that changes the wiring gets a refusal with a reason: the registry made each mailbox, route, and watch one time, before an actor ran (§6.13), and a swap of a module cannot do that again.

**A reload replaces the code and keeps the data.** Thus a literal in a view changes at the moment of the save, and a literal that a handler put into the state keeps its old text until that handler operates again. The two behaviors are the same rule. `F5` is the other half: it restarts each actor from `init` and lets the state go, on the code that is in operation. Save and `F5` keep the meanings that the browser gave them.

### 9.4 Debug strategy — replay, not breakpoints

A step debugger needs DWARF through wasmtime, then lldb or DAP. That is a large task, and we keep it for later. Actors communicate *only* through typed messages. Thus a record of the inbound messages of an actor is a complete record of its inputs.

In POC scope:

- **Message trace.** A structured causal log: the sender, the receiver, and the typed payload. A switch turns the trace on or off for each actor.
- **A structured crash report.** A panic gives the actor, the message in flight, a state snapshot, and a WASM backtrace, sent to the supervisor. The supervisor is a crash reporter by construction. The snapshot is the one of §6.14. A handler that stops did not write its result. Thus the report carries the state that the actor had when the fatal message arrived.
- **A debug overlay.** The arena size, the fiber count, and the mailbox depth for each actor. The platform draws the overlay as injected render commands.

Future work: deterministic single-actor replay, which sends a recorded message log into a fresh instance. That gives time-travel debug for one component, with no whole-program record. Also: DAP integration, and an LSP.

### 9.5 Almost no lint rules

Examine the large ESLint rules: `eqeqeq` guards a coercion that §4.2 deletes. `no-implicit-coercion` is the same. `rules-of-hooks` and `exhaustive-deps` guard hooks and dependency arrays that §10.6 deletes. `no-floating-promises` guards promises that §4.4 deletes. `no-undef` and `no-unused-vars` are checker work in a language with a checker.

The thesis: **the lint industry is, for the most part, a patch kit for language flaws, paid for by each project.** The configuration pain shows the cost: plugin systems, the flat-config migration, and a package with one job — turn off the rules of a second package. The Rust rewrites — Biome, oxc, Ruff — corrected the speed. They could not correct the category error.

The Strand position: `strand fmt` has no options (§9.1). A correctness rule is a compiler diagnostic, with no exception. The remainder is a small fixed set of convention checks in the one binary. There are no plugins and no configuration file. The test for each proposed rule: "why is this not a type error?"

### 9.6 Tests and the workbench

The Jest, jsdom, and snapshot stack is layers of imitation: jsdom imitates a browser, a snapshot test decays, and an end-to-end test fails at random on time limits. The architecture dissolves most of it:

- **A unit test has no mocks.** A handler is pure: state in, message in, state out. Compare the result.
- **An integration test repeats exactly.** The scheduler has a deterministic test mode, on the TigerBeetle model (§17). A test failure repeats from a seed. There are no random failures.
- **A UI assertion is exact.** A view emits a render command array (§6.9). A golden-command test compares arrays. There is no screenshot diff and no DOM query.

`strand test` (§9.1) gets this philosophy. **The workbench** applies the Storybook lesson at low cost: an actor is an isolated unit now, so `strand workbench` renders one actor alone and walks its states. Storybook needed heavy tools for this in React. Here it is a thin surface above §5.1.

## 10. Platform Services

This section is post-POC. It is here because each lesson constrains a POC primitive, and because the largest wins use machine parts that the POC builds.

### 10.1 Storage — one typed API

The web sent out five storage APIs: cookies (4 KB, strings only), localStorage (synchronous, blocks the main thread), sessionStorage, IndexedDB (so hostile that all users wrap it), and AppCache.

Strand sends out one API. It is async only: a call suspends the fiber, and colorless concurrency hides it. It is typed: a record persists with its schema. It is transactional. It has explicit tiers — session and durable — and an expiry. It is a **capability** with a quota. Thus an origin gets no ambient access.

### 10.2 Identity — no ambient credentials

A cookie attaches itself to each request. That is ambient authority. Ambient authority is the root cause of the CSRF class.

Strand has no cookie equivalent. A session is a capability token that an actor holds and shows explicitly. CSRF becomes impossible in the model, as ownership transfer made a data race impossible (§4.4).

### 10.3 Navigation and routes — the URL stays untouchable

A link to a deep state is the top feature of the web. SPAs broke it for ten years: dead back buttons, states with no link. A platform that replaces the browser must keep the feature. The framework wars show the correct design:

**Bad example — Next.js file routes.** A full route language in file names: `[id]`, `[...slug]`, `(groups)`, `@parallel`, and special names such as `page`, `layout`, and `error`. The language has no types and is not visible to tools. A rename breaks it with no message. Conflicts obey precedence rules that the user cannot see. A user cannot compose routes or make them with code.

**Good example — TanStack Router.** Routes can have types from end to end: a bad link is a compile error, and params arrive parsed. **Search params are typed state with a schema.** That is where a SPA keeps its true UI state: filters, sort, tabs. Before this, search params were loose strings. Also, its file convention is only generated code above a typed route tree. The typed tree is the true layer.

**Good example — Remix.** Nested route segments, with a data loader for each segment and an error boundary for each segment. That design is an approximation of supervised actors, built inside React.

**The Strand design:**

1. **Routes are typed values in code.** A declared route tree, where each segment carries typed params. A URL parse makes a typed route value at the boundary, or a typed not-found. No code after the boundary touches a string.
2. **A link is a constructor** (`routes.post(id).url()`). A malformed link is a compile error. A route rename corrects each link.
3. **Search params are typed state with a schema**, with defaults and serialization. The URL becomes the honest form of UI state that a user can share.
4. **A route segment is an actor.** The actor of a layout segment stays alive across child navigation. Thus no state is lost at navigation. A navigation swaps the child actor under the supervision of the layout. An error boundary for a segment *is* §5.4 supervision. Parallel segment loads are actors that start at the same time.
5. **The compiler checks the match.** The route tree is typed data. Thus a route with a conflict, or a route that code cannot reach, is a compile diagnostic.
6. **A file convention, if one comes, is generated code** that makes the typed tree. It is never the semantic layer.

### 10.4 The render strategy — resume, do not hydrate

The pendulum between SSR and CSR compensates for two platform gaps: a slow cold start, and no method to move the state of a live application between machines.

Strand closes the two gaps. A content-addressed AOT module makes the cold start almost native (§11.3). The typed state snapshot exists for hot reload (§9.3) and crash reports (§9.4). The same snapshot is the resume primitive.

The flow: a server runs the actor, streams the first render command array (§7.1), and transfers the snapshot. The client resumes from the snapshot. The first paint comes from the server. The client runs nothing a second time. There is no hydration step, and no mismatch bug class.

POC constraint: no part of the snapshot format can assume a resume on the same machine.

### 10.5 Server functions and distribution

`"use server"`, TanStack Start, and Remix loaders agree on one point: typed RPC adjacent to the UI code. The RSC client/server split then added color at the component level: `"use client"` divides the tree.

The actor model gives the correct version through location transparency. A server function is an actor that runs on a server. The typed channel *is* the RPC contract. There is no directive and no colored component.

POC constraint: channel semantics never assume shared memory. That is true now, because a send transfers ownership (§4.4). Section 12.4 gives the transport.

An optimistic update stops as a library pattern under Elm-style state and `Result` effects: apply the predicted state, confirm at `Ok`, go back at `Err`. Linear, Replicache, Zero, and Electric point at a **synced state record** as a future primitive.

### 10.6 Framework lessons

| Framework problem | Strand decision |
|---|---|
| Hooks tie state to call order. Thus the Rules of Hooks move compiler work onto the user. A `useEffect` dependency array is manual cache invalidation. `useMemo` is a memoization tax | State is in a typed actor record, not at a call position. An effect is a message. Thus there is no dependency array. The re-render unit has a limit. Thus there is no manual memoization |
| The correct parts of React | A component is a pure function. Data flows in one direction. Composition operates. The three parts stay |
| Signals won fine-grained reactivity, because React draws an unbounded subtree again | **The actor is the re-render unit.** A state change in one actor runs the views of one actor into one command array. The platform sets the limit of the effect. Signals *in* an actor are the planned optimization: a reactive read compiles to a small patch of the command array. Strand is compiled. Thus this needs no API break |
| Svelte: the framework can disappear into the compiler | Reactivity is a compilation target, not a runtime library |
| Next.js: a magic file convention becomes the API | No file name encodes semantics. A route is declared in code, with types. The specification stays neutral about vendors |
| React Query: server state is not UI state | Typed routes are a primitive (§10.3). A `resource` abstraction with staleness semantics is future work |

### 10.7 State management — the actor is the store

React sent out no state model above the component. The result was ten years of libraries: Flux, Redux, thunks and sagas, MobX, Context, Zustand, Jotai, Recoil, XState. No library won, because no library could correct the absent platform primitives: a unit of ownership, a limit on the re-render effect, and an effects model.

**Redux was an actor, built without the platform.** A store holds state. An action is a message that can be serialized. A pure reducer is a handler. New state is immutable. The action log gives time travel. Each part maps to this design: the action log is the message trace (§9.4), time travel is deterministic replay (§9.4), and "an action must be serializable" is the flat-message rule (§6.8), held by convention, not by a checker.

The Redux failures are also lessons:

- **All state was global by default.** One store gives an unlimited blast radius. The selector and memoization industry was a manual tax for an absent limit. In Strand, the actor is the re-render unit (§10.6). The limit is structural.
- **The ceremony was heavy.** Action constants, creators, and connectors were typed message infrastructure, built by hand in a language without types. The actor surface of §4.7 is the same model with almost no ceremony. The Zustand adoption numbers show that developers accepted the Redux model and refused its ritual.
- **Effects had no model.** A reducer must be pure, but the world is not. Async arrived through middleware, and the thunk, saga, and observable wars followed. In Strand, a handler is a pure state transition, a `send` is the effect, and the mailbox is the effects system (§7.5). Colorless concurrency (§4.4) makes an async effect normal code.

**MobX and Valtio** show the cost of implicit reactivity: a proxy tracks dependencies with no visible record, and the user cannot tell what causes what. Strand keeps effects explicit as messages.

**XState** gives the one lesson that is new here: state has a shape. Most application state is a set of finite modes with legal transitions, not a bag of fields. The bug class is the impossible state: `isLoading && isError`. In Strand, this protection is not a library. Model the state record as a sum type, and the exhaustive `match` (§4.5) makes the checker refuse a handler that ignores a mode. XState v5 gave its units the name "actors". That is one more framework that arrived at this primitive.

**Results of "the actor is the store":**

- Derived state is a view computation, or a downstream actor. The Stats actor of §8 *is* a Redux selector, run as a supervised process.
- Undo and redo are a pattern, not a library: immutable state records plus the message log.
- Persistence middleware is not necessary: typed storage (§10.1) plus the snapshot (§10.4).

**Open gap.** Shared state with many readers — a theme, a locale, the current user — needs explicit fan-out wires in a model with no addresses (§6.8). At scale, that is verbose. Candidates: a broadcast port kind in the `app` block, or read-only snapshots that the platform serves. Section 18 records this as an open question.

### 10.8 Forms and validation — the zod lesson

TypeScript deletes its types at compile time. Thus the ecosystem built a second type system — zod and its peers — to check data at the boundary at runtime. The form-library churn (formik, react-hook-form) is the same signal.

Strand types stay to runtime. Thus the boundary check derives from the type itself. There is no second type language. "Parse, do not validate" becomes a platform function: bytes in, a typed value or a typed error out.

A form is then a pattern, not a library: a typed draft record, plus a validator that returns a `Result`. The dirty state and the submit flow are the normal Elm loop (§7.5). A field error is a normal `Err` arm in a `match`.

## 11. Ecosystem and Packages

This section is post-POC. The module format (§11.3) and the capability model (§11.6) shape POC primitives.

### 11.1 The stakes

JavaScript cannot be replaced because of two million packages, not because of the language. A platform that ignores this fact loses before it starts.

npm won before supply-chain attacks were common, before content addressing was usual, and before capability security was practical. Its problems are patches on a foundation that cannot change. The opportunity: an ecosystem where full classes of npm failures are not possible, plus an answer to the cold-start problem (§11.9).

### 11.2 Ecosystem lessons

| Lesson | Source | Strand decision |
|---|---|---|
| left-pad: one removed package broke the internet, because registry state is mutable | npm, 2016 | Content-addressed immutable modules (§11.3) |
| A `postinstall` script runs unknown code at install time | npm: event-stream, ua-parser-js | **No install script.** An install is a data transfer. A build is a pure function in a sandbox (§11.6) |
| An account takeover changes code under an unchanged name, with no signal | npm, many times | A transparency log (§11.4) |
| Trust in transitive dependencies is not auditable at scale | npm | A capability manifest, summed across the tree (§11.6) |
| node_modules makes copies. Hoists make a phantom dependency | npm; pnpm corrected a part | One content-addressed cache for each machine. An import resolves only through the manifest |
| `is-even` exists because there is no standard library | npm culture | The standard library is included, plus an approved tier (§11.7) |
| Semver is a social promise | npm | Enforced semver: an API diff at publish forces the correct bump (§11.5) |
| SAT-solver resolution: a build changes at night | npm, yarn | Minimum Version Selection: deterministic, with no solver (§11.5) |
| URL imports: dead links, mutable targets, no discovery | Deno 1.x, then a retreat | A registry where a name is metadata above a hash (§11.4) |
| Publish the source, make the docs, attest provenance, score the package | JSR | All adopted (§11.8) |
| Speed changes adoption economics. Compatibility is the entrance | Bun | An install is a cache hit. The Component Model is the bridge (§11.9) |
| Docs for each package increased the quality of the full ecosystem | docs.rs | `strand publish` makes and serves typed docs (§11.8) |
| The registry owner sets the trust ceiling | npm Inc.; Flash | An open protocol and name system, with foundation governance |

### 11.3 Content-addressed modules

The unit of distribution is a compiled typed WASM component. The hash of its content identifies it. A name is metadata *about* a hash. A name is never the identity of code. Four structural results:

- **Immutability is physics.** A hash cannot change its meaning. Thus a left-pad event is not possible.
- **One cache for each machine.** Ten thousand applications with the same HTTP library get and compile it one time, ever. At this layer, an application and a package are the same thing.
- **Reproducible resolution by default.** A lockfile is a list of hashes. Two machines with the same manifest resolve to the same set, always.
- **Typosquat attacks become weak.** The dangerous moment is the resolution from a name to a hash. That occurs one time, through the checked index of the registry.

Prior art: the Git object model is the precedent. Nix gives proof of reproducible builds. Unison is the maximum version: a hash of the AST identifies each definition. Strand adopts the direction at module granularity, and trades some elegance for clarity (§18).

### 11.4 Names, the registry, the transparency log

- **A name has a scope** (`@author/pkg`) and maps a version to a hash. The registry is a lookup service and an index. Authority is in the hash.
- **A transparency log** records each publish as an append-only event in a Merkle tree, on the model of the Go checksum database and Certificate Transparency, with Sigstore for identity. A client checks an inclusion proof. A registry under attack cannot serve different bytes for a known version without evidence that an auditor sees.
- **Provenance is on by default.** A publish is attested to a source revision and a reproducible build. The registry builds from the source, or it checks a reproducible-build proof.
- **Governance is neutral.** The protocol, the name system, and the log format are open specifications. Any person can operate a mirror or an auditor. A foundation operates the default registry. The root of trust of the ecosystem must never be the asset of a company.

### 11.5 Enforced semver and Minimum Version Selection

**Enforced semver.** Types stay through compilation. Thus `strand publish` diffs the public API against the version before. A removed or changed signature forces a major bump. An addition forces at least a minor bump. The tool refuses a publish with an incorrect label. A behavior break in an unchanged type stays possible. That is the risk that remains. A capability change also forces a major bump (§11.6).

**Minimum Version Selection.** Resolution selects the *minimum* version that satisfies all constraints. It is deterministic, with no solver. Thus a build never changes because a person made a publish at night. An upgrade is an explicit act. With content addresses, the output is a stable set of hashes.

**Two schemes.** The rules above apply to a package, where a version is a contract that a machine checks. The platform and the toolchain use CalVer: `Strand 27.1` is the 2027 train, first update. For a product, the date is the useful signal, and CalVer commits to release trains. The registry shows the publish date adjacent to each semver. Thus a person gets the age signal, and the machine channel stays clean.

### 11.6 The capability manifest

A package is a WASM component. Thus it can touch only what it receives.

- Each package declares its capabilities in its manifest: `net(hosts?)`, `storage`, `clock`, `random`, `spawn`. The imports of the component enforce the declaration. The check is static, at publish and at load. A markdown parser that declares nothing cannot move data out, and the proof is available.
- Tools show the **capability sum of the dependency tree**. A review examines a short checked list, not transitive source. A dependency that adds a capability is a major-version event and a loud diff.
- **No lifecycle script runs at any point.** A build runs as a pure function in a sandbox on registry infrastructure: source in, component out, no network, no ambient filesystem.
- Registry scores give weight to capability minimalism. "Requires: nothing" becomes the status symbol.

### 11.7 Standard library strategy

1. **The standard library** comes with the platform and has the same version: collections, `Option` and `Result` combinators, strings and format functions, time, math, encoders, test tools. It is wide enough that an `is-even` package never forms.
2. **`strand-x/`** is the approved tier, on the `golang.org/x` model: official quality, a separate version, an audit of capabilities. An HTTP client, crypto, compression, image codecs. A community package can move up into it.
3. **The community tier** is all other packages, with a rank from the score system.

The rule for a young ecosystem is curation above volume. A small clear core that feels complete wins against a large bazaar that feels random.

### 11.8 Publication

`strand publish` is one command: the semver check (§11.5), the capability check (§11.6), the reproducible build and its attestation (§11.4), doc generation from types with examples that run, and inclusion in the log (§11.4). There is no configuration other than the package manifest.

Publish friction near zero is what created the npm ecosystem at all. Keep the low friction. Delete the failure modes.

### 11.9 The cold-start problem

Better tools alone have never started a community. Three levers apply together:

1. **The Component Model is an ecosystem loan.** Rust, Go, and C libraries compile to WASM components today. Wrap one with a typed interface and a capability manifest, and it is a first-class package. This is the compatibility play of Bun, moved to the boundary where the sandbox holds.
2. **Curate at the start** (§11.7). The first hundred packages set the culture.
3. **Make a publish a joy** (§11.8), and make the capability badge a status economy. The boast becomes *how little* your package needs.

Later, the legacy-web layer (§15) extends the bridge to JS libraries. Behind the sandbox, a JS dependency also arrives with a capability manifest.

### 11.10 The vendored-source tier — the shadcn lesson

The npm component libraries — MUI, Chakra, Ant — hit the same wall: the user fights the abstraction ceiling, fights the style overrides, and waits on the roadmap of the library. shadcn made a new distribution mode: the user does not install the component. **The user copies it.** The component becomes source that the user owns.

Strand adopts this as a tier adjacent to the hash packages (§11.3): a registry of source that a user vendors into the project tree. The registry keeps the provenance. Thus a diff against the upstream source stays possible. The tier fits the case where the owner must edit the appearance: UI components above the platform behavior layer (§7.6).

## 12. The Strand Web: Naming, Transport, Access

This section is post-POC. It constrains primitives that the POC has: channels, manifests, render commands, snapshots.

### 12.1 Four layers

| Layer | Question | Answer | New design |
|---|---|---|---|
| **Names** | How do you point at a place? | URLs, DNS, TLS, unchanged | None |
| **Bootstrap** | How does an application load? | A signed manifest over HTTPS. Hashes from any source | Low |
| **Session** | How does a live application talk? | Typed channels over QUIC streams | **High** |
| **Discovery** | How does a person find a thing? | Content indexes from typed routes | Open problem |

The protocol invention occurred at the selection of content addresses and typed channels. The wire is only the truck. A custom transport loses the CDNs, the firewalls, the proxies, and the operation tools of the world on day one. gRPC rode HTTP/2. WebSockets rode an HTTP handshake. OCI registries rode HTTP conventions. Users reach IPFS through HTTP gateways.

### 12.2 Names

`https://example.com/recipes/42` holds the social contract of the open web: a person with a domain can publish, there is no gatekeeper, and each state has a link. DNS and certificates are the only deployed name-plus-trust system that reaches all persons. A `strand://` scheme would spend adoption capital on the one layer that was never the problem.

- A domain names an application. A typed-route URL names a state (§10.3).
- The TLS certificate on the manifest origin authenticates *the publisher*. The content hash authenticates *the code*. That separation is the structural upgrade above HTTPS, where the channel gives assurance for the two at the same time.

### 12.3 Bootstrap

1. **Get the manifest.** It is a small signed document at `/.well-known/strand/`, or content-negotiated at the page URL. It holds the entry actor, the module hash set, the capability requests, the route table, and the fallback data.
2. **Resolve the hashes against the machine-wide cache.** All applications share modules (§11.3). Thus a first visit usually gets only the code of the application itself.
3. **Get each absent module by hash.** `GET /modules/{hash}` goes to the origin, a CDN, a mirror, or a LAN peer. A simple HTTP server is sufficient. Integrity comes from a check after the transfer, not from trust in the channel:
   - A mirror needs zero trust. Get from the fastest source.
   - **Cache invalidation is deleted.** No Cache-Control heuristic, no ETag, no revalidation, no stale-content bug class.
   - "Installed" means "the hashes are local". Thus offline use and immediate rollback come free.
4. **Check the capabilities. Then start the root actor, or resume it** (§10.4).

**Transport: HTTP/3 over QUIC.** TLS stays on for privacy: it hides what you get. It is not there for integrity.

**Compatibility is the adoption strategy.** The same URL serves the two worlds through content negotiation. A Strand client receives the manifest. A legacy browser receives HTML: a fallback page, or later the render-command projection (§7.1). One URL, two webs, and no link divides. HTTP/3 used this method at deployment: announce the upgrade, and fall back with no signal.

### 12.4 Session: the typed channel is the protocol

The request-response shape of HTTP was built for documents. The application web is twenty years of workarounds on top — XHR, long polls, SSE, WebSockets, REST conventions, JSON at each hop — because an application needs a conversation.

The native Strand primitive is the conversation. A live session is client actors and server actors that exchange typed messages. A dynamic website is an application with a supervision tree across two machines.

- **Carrier: WebTransport over HTTP/3.** One stream for each channel. Thus a blocked channel never blocks its siblings. The stream lifecycle maps to the channel lifecycle. Datagrams carry traffic that permits loss, such as a cursor position.
- **Wire format: the message type of the channel, zero-copy.** The wire layout is the memory layout (§6.8). Thus a network crossing decodes nothing. There is no JSON tax, no REST layer, and no drift between API versions. The channel type is the contract, and enforced semver versions it (§11.5).
- **Supervision spans the network.** A crash of a server actor arrives as `ChildDown` at the client-side supervisor. Thus a reconnection and a retry are normal supervision strategy (§5.4). A partition is a typed, expected failure.
- **A network channel is a capability** (§11.6). An application opens a session only to a host that its manifest declares. Thus a dependency cannot call home.

The static case becomes simple: a site with no server actor is only the manifest and the modules, served from a CDN and cached forever. The pendulum between SSR and CSR stops. No point between the two poles must exist.

### 12.5 Peer-to-peer

Content addresses make P2P possible, because integrity never depended on the source. The IPFS lesson: mandatory P2P gets the availability floor and the latency floor of P2P.

The rule: HTTP from an origin or a CDN is the path with a guarantee. A peer is a transparent cache tier, found by opportunity, trusted zero.

### 12.6 Discovery — the open problem

SPAs broke the crawl of the web, and ten years of SSR workarounds got it back. A web of manifests and WASM is worse, unless the design includes the crawler from the start.

The routes of an application are declared typed data. Thus the application can export a **content index**: a machine-readable projection of its addressable state space. The index maps a route to a content summary or a snapshot. A crawler reads the index and runs nothing.

Open items: index freshness for dynamic content, resistance to abuse without a central gatekeeper, and the question whether the render-command-to-HTML projection is also the crawl format.

### 12.7 Privacy caveats

- A transfer by hash shows *what you run* to the infrastructure operator, also under TLS, because a hash is identifiable. Mitigations are future work: an oblivious HTTP relay, request padding, and PIR for a popular module.
- The machine-wide cache is a fingerprint surface across applications, if timing is observable. Browsers divided their HTTP caches for this reason. Direction: a cache hit stays free, but no code can examine the cache for an entry across a capability boundary, and the load timing becomes normalized for an unknown module.

## 13. Milestones

Each milestone has its own demo. The estimates assume one developer with focus.

1. **M0 — A skeleton that walks (1 to 2 weeks).** A hand-written WASM module runs in wasmtime under tokio. Two host actors exchange a typed message. A wgpu window clears to a color.
2. **M1 — The language core (2 to 3 weeks).** The lexer, the parser, the checker, and WASM emission for functions, records, `match`, `Result`, and `?`. The CLI runs a `.str` file. A golden-file test suite. Diagnostics through miette from the start.
3. **M2 — The actor runtime (2 weeks).** `actor` declarations, typed channels, buffer transfer, panic → `ChildDown` → restart, structured crash reports. Demo: a supervised pair, where one actor crashes on a schedule.
4. **M3 — The scene graph (2 to 3 weeks).** The render thread with taffy layout, the widget set, and input routes. A host-side actor submits the UI tree first. Strand code submits it after.
5. **M4 — The vertical slice (1 to 2 weeks).** The todo application in Strand, with the demo sequence of section 8.
6. **M5 — The DX slice (1 to 2 weeks).** Tier-1 view hot reload. Message traces with typed payloads. A debug overlay on true runtime data. Stretch: Tier-2 actor reload with a checked snapshot. This milestone shows the platform.
7. **M6 — Measurement (1 week).** The latency from input to frame under load, the memory for each actor, the cost of an actor start and stop, the round-trip time of hot reload, and the binary size. Notes against an equal JS and React todo application.

Total: approximately 10 to 15 weeks at part time. M0 is the risk gate. If wasmtime async, tokio, and wgpu do not connect well, week one shows it.

**Current status.** M0 through M4 are complete, with the demo sequence of §8. M5 is complete with one exception: §9.4 asks for a switch that turns the trace on or off for each actor, and the trace is always on. Tier-1 and Tier-2 hot reload operate through `strand view --watch`, on the typed snapshot of §6.14. M6 has one measurement: the compositor frame rate.

**Known gaps to keep in view:**

- There is no `xs[0]`. The parser reads the syntax as two expressions, and the failure comes later as a confusing type error. That is worse than an index feature and worse than a clear refusal.
- `str` accepts an `int` only.
- Method-call syntax is not implemented. The standard library is free functions.
- `scope` and `spawn` pass the lexer and nothing more. Thus the fiber count in the overlay is 0 or 1.
- `push` is O(n).
- The match exhaustiveness check goes one level deep.
- There is no `strand fmt`, although §9.1 asks for one binary that includes it.

## 14. Risks

| Risk | Level | Mitigation |
|---|---|---|
| The wasmtime Store for each actor is too heavy | Medium | Measure at M0. An actor is component-grained: tens, not millions |
| WASM GC types are not mature for the type map | Medium | Linear memory and our own layout. Section 6.1 uses this alternative from the start |
| Text render and text input use too much time | High | One font, Latin only, a basic caret. glyphon does the rest |
| The compiler uses the full schedule | Medium | The subset is fixed (§4.6). Cut each feature that the todo application does not need |
| The colorless host-call connection is complex | Medium | M0 exists to show this |
| Hot reload grows in scope | Medium | Tier 1 is the M5 bar. Tier 2 is a stretch. Tier 3 is not permitted |

## 15. Future Work

**Compatibility with the current web** is the strategic key, kept for later by intention. Two paths, in the probable order: first, embed a JS engine as a legacy actor, so current code runs in the sandbox; then, compile a TypeScript subset directly to the VM, for migration one file at a time.

- **Security and distribution:** capability security, where a channel is the capability substrate (§10.2, §11.6); content-addressed modules (§11.3); distributed actors and location-transparent channels (§10.5, §12.4).
- **Language and compiler:** a custom bytecode VM in place of wasmtime, after the semantics become stable; full type inference; a JSX-flavored surface syntax (§7.2).
- **UI:** the `grid` primitive; container-size helpers; the scale-token vocabulary for props (§7.6); text shape functions and i18n; the semantic-tree export through AccessKit (§7.7); in-actor signals (§10.6).
- **Platform services:** network and persistence host APIs; typed capability storage (§10.1); the typed route tree and segment-actor navigation (§10.3); server-side actors with snapshot resume (§10.4); synced state records (§10.5); a `resource` abstraction (§10.6); boundary validation from types, and the form pattern (§10.8).
- **Tools:** Tier-3 hot reload (§9.3); deterministic single-actor replay (§9.4); a DAP debugger; an LSP; `strand test` with the deterministic mode and golden-command assertions (§9.6); `strand workbench` (§9.6); `strand doc`.
- **The web:** the content-index specification (§12.6); privacy work (§12.7).

## 16. Decision Log

The load-bearing selections, where an implementer could make a different selection.

| Decision | Selection | Reason |
|---|---|---|
| POC shape | A full vertical slice | One layer alone gives little proof |
| Implementation language | Rust | wasmtime, tokio, and wgpu; ownership matches the runtime semantics |
| Execution engine | Embed wasmtime | Weeks, not years |
| Errors | `Result` and `?`; a panic stops the actor | Agrees with arenas and supervision |
| Concurrency | Colorless, structured scopes, actors | Almost free, because the M:N runtime is mandatory |
| Value layout | Core WASM and our own layout. No GC types | The compiler does not wait for the toolchain (§6.1) |
| `Result` layout | Two WASM values, never boxed | A bump arena keeps all memory, so a box leaks at each fallible call (§6.2) |
| Message layout | The wire format is the memory format. A message must be flat | A copy into a different arena needs no decode step (§6.8) |
| `Node` type | Zero width. A view appends to a post-order array | The array is in tree order by construction, not by discipline (§6.9) |
| UI | A scene graph on a render thread that the platform owns | Removes the framework tax and the frame-rate bug class |
| UI syntax | A typed builder DSL. JSX later as sugar | JSX was a workaround for absent tree syntax in JS |
| Styles | Typed scoped props. No cascade. Typed theme tokens | The cascade made CSS append-only |
| UI pipeline | A flat render command array from a per-frame arena | Independent of the renderer, easy to diff, easy to serialize. Zero GC pressure |
| Reactivity | The actor is the re-render unit. Signals later | A limit by construction wins against manual memoization |
| Routes | A typed route tree in code. A link is a constructor. Search params have a schema. A segment is an actor. A file convention only as generated code | Next.js shows the file DSL as the bad pattern. TanStack shows route types. Remix segment boundaries approximate supervised actors |
| State management | The actor is the store. State is a sum type where modes exist. No state library | Redux was an actor with no platform: keep its virtues structurally, delete the global default, the memoization tax, and the effects wars. XState's protection comes free from `match` |
| Widget architecture | The behavior is in the platform. The appearance is typed props | Radix and shadcn: the hard 80 percent is behavior. No person builds it two times, and no person is locked to a look |
| Style values | Tokens from a typed finite scale by default. A raw value is a visible escape | Tailwind: constraint wins, and the absence of names wins |
| Lint | A correctness rule is a compiler diagnostic. A small fixed set. No plugins | Most ESLint rules patch language flaws that this design deletes |
| Tests | Pure handlers, a deterministic schedule, golden command arrays | No mocks, no random failures, no screenshot diffs |
| Forms | The boundary validator derives from the type | zod exists because TypeScript deletes types. Strand keeps them |
| Component distribution | A vendored-source tier with provenance, adjacent to hash packages | shadcn: a copy that you own wins against an abstraction that you fight |
| Accessibility | A semantic tree derives from widget behavior. The platform exports it | The free DOM bridge is lost. The duty is not |
| Toolchain | One binary, zero configuration, one true format | gofmt ended arguments, not only formats |
| Hot reload | Tier 1 and Tier 2 in the POC. Tier 3 later | It is a supervisor restart with newer code (§5.4) |
| State snapshot | One relocatable image, and a shape check at the swap | A value is bytes with pointers, so a copy plus a list of the pointers moves it. The check is what makes the swap safe (§6.14, §9.3) |
| Debug strategy | Message traces and crash reports first. DAP later | A typed message log is a complete input record |
| Storage | One typed, async, transactional, capability API | The web never sent out one good API |
| Credentials | No ambient authority. A session is a capability token | Ambient authority is the root cause of CSRF |
| Render strategy | Resume. Do not hydrate | The snapshot exists for hot reload and crash reports |
| Distribution unit | A content-addressed typed WASM component | Immutability as physics. One cache for each machine |
| Registry | Names as metadata above hashes. An open protocol. Foundation governance | The root of trust of the ecosystem must not be the asset of a company |
| Integrity | A transparency log, provenance, reproducible builds | An attack leaves cryptographic evidence |
| Versions | Enforced semver plus Minimum Version Selection | A build never changes with no signal |
| Version schemes | CalVer for the platform. Semver for a package | Two audiences: a machine needs a contract, a person needs an age signal |
| Install-time code | Not permitted. Builds are pure and in a sandbox | postinstall is the top npm attack vector |
| Trust model | Capability manifests, summed across the tree | Changes transitive trust that no person can audit into a short checked list |
| Transport | HTTP/3 and QUIC. No custom wire protocol | Each winner rode rails that existed |
| Names | URLs, DNS, and TLS, unchanged | This layer was never the problem |
| Caches | Immutable hashes. Cache forever. Any source | Deletes cache invalidation. Mirrors and offline use come free |
| Compatibility | Content negotiation: one URL serves a manifest or HTML | Adoption never divides a link |
| Session | Typed channels over WebTransport, zero-copy | Deletes REST, WebSockets, and JSON. Supervision manages network failure |
| P2P | An optional cache tier, never load-bearing | Mandatory P2P gets the floors of P2P |
| Compatibility with the current web | Later, and documented | The POC shows the new model. Compatibility is a separate bet |

## 17. Prior Art

Almost each entry below wins because it **deletes a subsystem**. LMDB deletes the write-ahead log. WireGuard deletes cipher negotiation. esbuild deletes compiler passes. TigerBeetle deletes malloc. Immediate-mode UI deletes retained state.

The Strand deletions so far: no hydration, no cascade, no try/catch, no z-index, no `async` keyword, no cookies, no decode step (§6.8, §6.9), no cache invalidation (§12.3).

### 17.1 The two that shape the POC directly

**clay** (nicbarker/clay) — a single-header C layout library. Adopted directly: the render command array (§7.1), the static per-frame arena, the `fit`/`grow`/`fixed`/`percent` vocabulary (§7.3), attach-point float elements (§7.3), and the debug inspector as injected render commands (§9.4). clay is not thread-safe. Thus it shapes the layout algorithm and the API, not the concurrency architecture. Its algorithm is the reference if `taffy` becomes too small.

**raylib** (raysan5/raylib) — the DX example. Zero dependencies, a hello world of ten lines, more than 140 examples in place of a specification, and a stable C API with bindings in more than 70 languages. Adopted: hello world goes on one slide, the platform includes the batteries, the docs put examples first, and the host ABI (§6.7) stays simple so a different language can target the VM.

### 17.2 The canon

| Project | Lesson |
|---|---|
| SQLite | Reliability is the feature. A small, simple, self-contained interface wins for tens of years |
| LMDB | A memory-mapped copy-on-write B-tree deletes the WAL, the cache layer, and the background threads. The top elegance is the part that you did not build |
| TigerBeetle | Static allocation at start — the arena philosophy at database scale. Deterministic simulation tests make the actor VM testable. Section 6.9 obeys the arena rule now |
| Redis, early era | An event loop on one thread, plus the correct data structures, wins against complex concurrency |
| LuaJIT / Lua | Constraint as design: a complete language on one data structure |
| BEAM and OTP | Section 5 as software that runs. Study the implementation selections. You will meet each one |
| Chez Scheme | The nanopass compiler: tens of small passes, each easy to check. This shape stays clear as it grows |
| Turbo Pascal | Compiled, edited, and ran in 64K. The ancestor of the one-binary rule (§9.1) |
| TeX | Software can be complete |
| esbuild | 100x through a fast language, parallel work, one parse, and refused features |
| WireGuard | Approximately 4000 lines replace hundreds of thousands. Fewer controls *are* the security model |
| seL4 | Capability security costs nothing at runtime when the design includes it (§10.2, §11.6) |
| nginx | Event-driven master and worker won against a thread for each connection. The ancestor of §5.2 |
| ripgrep | Finite automata done correctly, plus respect for the memory hierarchy. The Rust codebase to read first |
| qmail / daemontools | Components with no mutual trust, minimum privilege, narrow interfaces. Actor isolation, sent out in 1998 |
| id Tech | Compute the structure before, so the hot loop does almost nothing. Do the thought at layout time, not paint time (§7.1) |
| RollerCoaster Tycoon | One person, in assembly. Not a practice to copy. A ceiling |
| Dear ImGui | Debug tools, input, and layout stay simple when the API refuses retained state |
| Zed / GPUI | M3 as a product on the market. Read it when the renderer becomes hard |
| The Git object model | An immutable content-addressed DAG with four object types. The precedent for §11.3 — and a warning that an elegant core does not excuse a bad CLI |
| Cap'n Proto | The wire format is the memory format, so a parse costs nothing. Applied in §6.8, §6.9, and §12.4 |

### 17.3 The landscape

| Project | What it shows | Why it is not Strand |
|---|---|---|
| Flutter | The full stack operates: an own language, no DOM, a GPU scene graph, adoption | Isolates with one thread, no supervision, exceptions kept. An app framework, not a platform with a sandbox |
| Lunatic | Supervised WASM actors in Rust are possible — section 5, almost word for word | Server-side only, no language, no UI. Stopped: a runtime without a platform has no market pull |
| wasmCloud | Actors plus capability security operate at production scale | Cloud infrastructure. No UI, no language |
| Makepad, GPUI, Slint, Iced | Rust UIs with an own renderer and no DOM ship at 120 fps | App frameworks for code with trust. No sandbox, no language, no supervision |
| Blazor, Uno, Yew | There is demand for a language other than JS in the browser | They paint through the DOM, so they get the tax that we design out |
| Dioxus Blitz | HTML and CSS paint without a browser engine is possible | It points at current content. Applicable to compatibility, not to the new model |
| Flash, Silverlight, applets | An own VM plus renderer can reach mass adoption | Dead from closed ownership, plugin security, and vendor politics. Open specification, capability sandbox, no plugin model |

The position with a defense is the intersection of four properties: a typed language with types that stay to runtime, supervised actor isolation, a scene graph that the platform owns, and an open platform with a sandbox. Each row above holds one or two. No row holds all four.

### 17.4 Read order

1. **The Lunatic source** — section 5 is built there. Learn from the code, and from the reason that it stopped.
2. **ripgrep and esbuild** — the structure of the Rust codebase, and performance as architecture.
3. **TigerBeetle simulation tests** — before the scheduler, so determinism is in the design.
4. **clay.h and Dear ImGui** — before M3.
5. **BEAM internals** — before Tier-2 hot reload and the edge cases of supervision.
6. **The Chez nanopass papers** — before the compiler grows past the POC subset.

## 18. Open Questions

- **Definition-level against module-level addresses.** A hash for each definition dissolves more conflict classes, and makes the tools complex. Module level is the practical start. Examine again if version-conflict pain appears (§11.3).
- **Private registries.** The protocol must support mirrors and private scopes over the same log format. The trust model for a merge of a private tree and a public tree needs design (§11.4).
- **Capability granularity.** `net(host)` is correct. Open: whether storage needs sub-scopes for a quota or a namespace, and how `spawn` interacts with capability inheritance for a child actor (§11.6).
- **Money.** Whether the registry includes sponsor rails is a governance question. Answer it before scale (§11.4).
- **The content-index specification** (§12.6): freshness, resistance to abuse, and its relation to the HTML projection.
- **Privacy work** (§12.7): which mitigation fits the latency budget, and the defense against cache probes.
- **Session resume across a network move.** QUIC connection migration helps. Channel semantics across an IP change and a long mobile suspension need a definition — probably the same snapshot machine parts (§10.4).
- **Shared state with many readers** (§10.7). A theme or the current user has many readers and one writer. Explicit fan-out wires become verbose at scale. Candidates: a broadcast port kind in the `app` block, or read-only snapshots that the platform serves. The selection needs a design pass, with the no-address rule (§6.8) kept intact.
- **The manifest signature lifecycle:** key rotation, domain transfer, and whether an application manifest joins a transparency log as a package does. The direction is yes (§11.4).

## Appendix A — Screenshots

M3 color-space and root-fill corrections:

- `screenshots/m3-before-srgb-and-root-fill.png`
- `screenshots/m3-after-fixes.png`
