# Project Strand — POC Design Document

*A reimagined browser runtime: typed language, multithreaded actor VM, declarative scene graph.*

**Status:** Draft v0.1 · **Date:** August 2026 · **Working name:** "Strand" (placeholder — evokes fibers/threads; rename freely)

---

## 1. Vision

The web platform's core architecture was fixed in the 1990s: a single-threaded scripting language driving a retained-mode document model, with types, concurrency, security, and app-style UI all retrofitted afterward. Strand explores what the platform looks like designed fresh in 2026, while keeping a credible path to backwards compatibility with the existing web.

The POC is a **full vertical slice**: a small typed language compiles to a multithreaded VM, which drives a declarative scene graph renderer, demonstrated by a working todo application. The goal is not completeness in any layer — it is proving that the layers compose and that the core claims hold.

## 2. Goals and Non-Goals

**The POC must demonstrate:**

1. A TS-flavored typed language with runtime-preserved types, `Result`-based error handling, and colorless concurrency.
2. An actor-based VM where components run in isolated memory arenas, scheduled M:N across OS threads, communicating over typed channels.
3. A declarative UI layer where the scene graph lives on a platform-owned render thread — application code physically cannot jank the compositor.
4. Crash isolation: a panicking actor dies and is reclaimed without affecting the rest of the app.
5. All of the above working together in a todo app.

**Explicit non-goals for the POC** (noted as future work, §9): backwards compatibility with HTML/CSS/JS, a self-hosted bytecode VM, content-addressed module distribution, the capability security model, networking, persistence, accessibility, and text input beyond the minimum the todo app needs.

## 3. Architecture Overview

Four layers, top to bottom:

```
┌─────────────────────────────────────────────┐
│  Strand language (.str files)               │  TS-like syntax, typed
│  compiler: .str → WASM component            │
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

**Implementation language: Rust.** Rationale from our earlier analysis: wasmtime and cranelift provide a production execution engine we do not have to write; tokio provides the M:N scheduler; wgpu provides the portable GPU renderer; and Rust's ownership semantics natively express the runtime's core invariant (transferable, non-shared buffers). Every comparable recent runtime (wasmtime, Deno core, wasmer) made the same choice, giving us extensive prior art.

**Build strategy: embed, don't build.** The POC embeds wasmtime rather than writing a bytecode engine. This converts a year-long VM project into a weeks-long integration project while still proving the interesting claims. A custom VM (potentially in Zig, whose comptime is well suited to interpreter dispatch) is future work, only justified after the model is validated.

## 4. The Strand Language

### 4.1 Design position

Strand should feel like "TypeScript with the lessons applied" — familiar curly-brace syntax so today's web developers read it on sight, but with the semantic holes closed. Types are not erased: the compiler emits typed WASM (leaning on WASM GC types and the Component Model's interface types), so the runtime never speculates about shapes.

### 4.2 Lessons from JS, applied

| JS scar tissue | Strand decision |
|---|---|
| `null` vs `undefined` | Neither. Single `Option<T>` type; `?` sugar on types (`string?` ≡ `Option<string>`) |
| Implicit coercion (`"1" + 1`) | None. No `==`, only `===`-semantics spelled `==` |
| `this` binding chaos | No `this`. Methods take explicit receiver; UI is functions, not classes |
| try/catch invisibility | `Result<T, E>` in signatures; `?` operator to propagate (§4.3) |
| Unhandled rejections | No promises. Colorless concurrency (§4.4) |
| async/await coloring | No `async` keyword. Blocking calls suspend the fiber |
| ESM/CJS split, side-effectful imports | One static module format; importing never executes code; explicit `main`/actor entry points |
| Mutable-by-default everything | `let` immutable, `var` mutable; data structures immutable by default |
| No stdlib | Small batteries-included stdlib from day one (collections, Option/Result combinators, formatting) |

### 4.3 Error handling — two tiers, no try/catch

**Tier 1: expected failures are values.** Fallible functions return `Result<T, E>`. The `?` operator propagates the error to the caller, making the happy path read straight-line. This is the post-2012 language consensus (Rust, Swift, Zig, Gleam), and `?` is specifically what makes it livable — Go's `if err != nil` demonstrates the cost of Results without ergonomic propagation.

**Tier 2: bugs are panics.** Out-of-bounds access, assertion failure, arithmetic overflow — these mean invariants are broken, so execution of the failing unit stops. A panic kills the current actor only. There is no catch mechanism; recovery is the supervisor's job (§5.4). This is Erlang's "let it crash," proven over decades, and it composes exactly with our arena model: a dead actor's memory is reclaimed in one deallocation.

### 4.4 Concurrency — colorless, structured, actor-isolated

Three levels:

**Within an actor: colorless blocking.** Any function may suspend; there is no `async` keyword and no function coloring. `sleep(1s)` or a (future) `fetch()` blocks the *fiber*, and the scheduler runs other fibers on that OS thread. This follows the direction of travel across the industry — Java Virtual Threads, Go, Erlang — and is nearly free for us because the runtime already mandates an M:N scheduler.

**Within an actor: structured spawn.** Concurrency inside an actor uses scoped nurseries: children spawned in a scope cannot outlive it, results join at the scope's end, and cancelling the scope cancels the children. This closes Go's goroutine-leak hole (Kotlin/Trio model).

```strand
fn loadDashboard(): Result<Dashboard, LoadError> {
  scope {
    let user  = spawn fetchUser()?
    let todos = spawn fetchTodos()?
    Ok(Dashboard { user: user.join()?, todos: todos.join()? })
  } // scope exit: all children joined or cancelled — leaks impossible
}
```

**Between actors: messages only.** Actors share no memory. Channels are typed, and sending a buffer *transfers ownership* (the sender loses access), which the Rust host enforces at zero cost. Data races are unrepresentable in the model rather than discouraged by convention.

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

### 4.6 POC compiler scope

Hand-written lexer + recursive-descent parser; bidirectional type checker (full inference is future work — annotations required on function signatures, inferred locals); direct emission of WASM via `wasm-encoder`. Language subset for the POC: primitives (`int`, `float`, `bool`, `string`), records, `List`/`Map`, sum types + `match`, `Option`/`Result`/`?`, functions/closures, `scope`/`spawn`/`join`, actor declarations, and the UI builtins (§6). Everything else is out.

## 5. The VM and Actor Runtime

### 5.1 Actors as the unit of everything

An actor is the unit of isolation (own wasmtime `Store` = own memory arena), scheduling (a tokio task), failure (panics die at the actor boundary), and reclamation (dropping the Store frees the arena in O(1), no tracing GC across the app). An application is a supervision tree of actors; the todo app uses roughly four (§7).

### 5.2 Scheduling

Tokio's multithreaded runtime provides M:N scheduling with work stealing. Strand's blocking calls compile to host functions that are async on the Rust side (via wasmtime's async support + epoch interruption), so a "blocked" actor costs no OS thread. Long-running compute is preempted at epoch boundaries so one hot actor cannot starve the rest — this, plus the platform-owned render thread, is the structural fix for "don't block the main thread."

### 5.3 Typed channels and ownership transfer

Channels are declared with a message type; the compiler checks both ends. Small values are copied; buffers are transferred — the host moves the underlying allocation between Stores and invalidates the sender's handle. This is the working version of what `postMessage` transferables gestured at, enforced rather than optional.

### 5.4 Supervision

Each actor has a parent. On panic, the runtime tears down the actor, reclaims its arena, and delivers a typed `ChildDown(reason)` message to the parent, which chooses: restart (fresh state), restart-with-snapshot (if the child exported one), or escalate. The UI system renders a built-in "component failed" boundary for dead UI actors — React error boundaries, but enforced by the platform. **POC target demo:** a deliberately-crashable actor in the todo app that the supervisor restarts without the app blinking.

## 6. UI: Declarative Scene Graph

### 6.1 Model

No DOM, no HTML, no diffing in userland. UI is a function of state, and the platform owns reconciliation:

- App actors build a lightweight **UI tree** (view functions returning nodes) and submit it to the render actor over a channel.
- The **render thread** (platform-owned: winit event loop + wgpu) diffs against the retained scene graph, computes layout (flexbox subset via `taffy`), and paints.
- Input events flow back as typed messages to whichever actor owns the hit node.

Because submission is a message, a slow app actor delays *its own* updates only; the compositor keeps running at frame rate. This is the POC's most visible claim, so the demo includes a "spin the CPU" button proving the UI stays responsive while an actor is pegged.

### 6.2 UI syntax: typed builder DSL — JSX only as future sugar

Two candidate syntaxes were considered:

**JSX-like** (`<Row gap={8}>…</Row>`): maximally familiar to today's web developers, but structurally it is a workaround — a way to embed declarative trees in a language that lacked syntax for them. Its recurring costs: control flow via nested ternaries and `&&` (with the classic `count && <Badge/>` rendering a literal `0`), manual `key` props for lists, a second syntax mode with `{}` escape hatches in both directions, and typing bolted on above the syntax rather than native to it.

**Typed builder DSL with trailing blocks** (`row(gap: 8) { … }`): the model every UI toolkit designed *together with its language* converged on — SwiftUI, Jetpack Compose, Flutter. Conditionals are ordinary `if`, lists are ordinary `for` (the runtime keys on stable IDs), children and props are type-checked like any function arguments, and there is no mode switch.

**Decision:** the builder DSL is the semantic core and the only syntax in the POC. A JSX-flavored surface can be added later as pure sugar that desugars to the DSL (exactly as JSX desugars to `createElement`) — familiarity as an on-ramp, without building the platform's foundation on a workaround.

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

### 6.3 Lessons from HTML/CSS, applied

| HTML/CSS scar tissue | Strand decision |
|---|---|
| "How do I center a div" — alignment fell out of 5 unrelated mechanisms (auto margins, flexbox, absolute+transform, table-cell, line-height) | Alignment is a first-class typed property of every container: `column(align: .center, justify: .center)`. One obvious way, same way everywhere |
| Floats → tables → flex → grid: decades of overlapping layout systems retrofitted onto a document model | Small set of purpose-built app-layout primitives: `row`/`column`/`stack` (flex semantics) in POC; `grid` as future work. No floats, no document flow |
| `justify-content` vs `align-items` confusion (meaning flips with flex direction) | Flutter-style axis naming: `mainAxis` / `crossAxis` — unambiguous regardless of orientation |
| The cascade: global scope + specificity ranking + `!important` escalation → stylesheets become append-only; nobody can safely delete a rule | No cascade, no selectors, no global styles. Styles are typed props colocated with the view. Unused style = dead code, provably removable by the compiler |
| Stringly-typed properties; typos (`witdh: 10px`) fail silently | Typed style props; typos and invalid values are compile errors |
| `content-box` vs `border-box` (everyone opts into border-box anyway) | Border-box semantics only |
| Unit zoo: px, em, rem, %, vh, vw, ch, ex… | Logical pixels + fractional weights (`flex: 1`-style) + percent-of-parent. That's it for POC |
| Media queries target the *viewport*; container queries (what components actually needed) arrived ~20 years late | Responsiveness is container-based by default: a view can branch on its own allotted size |
| Implicit stacking contexts and z-index wars | Paint order = tree order; overlays via an explicit `overlay { }` layer node. No z-index |
| Design tokens reinvented repeatedly (Sass vars → custom properties → Tailwind config) — Tailwind's success is a market signal that developers prefer constrained tokens over the cascade | Typed theme constants (`theme.spacing.md`, `theme.color.accent`) as a platform primitive |

### 6.4 POC widget set

The minimum for a todo app: `column`, `row`, `text`, `textInput`, `button`, `checkbox`, `scroll`. Styling is a small typed props struct (padding, gap, color, fontSize) — no CSS, no cascade. Rendering via wgpu with `glyphon` (or equivalent) for text.

```strand
view fn todoRow(t: Todo, onToggle: fn(Id)) -> Node {
  row(gap: 8) {
    checkbox(checked: t.done, onChange: fn() { onToggle(t.id) })
    text(t.title, strike: t.done)
  }
}
```

### 6.5 State model

Elm-style for the POC: each UI actor owns a state record; events are messages; handlers return updated state; the runtime re-invokes the view. No hooks, no effects system — the actor mailbox *is* the effect system.

## 7. The Demo: Todo App

Deliberately boring so the architecture is the story. Supervision tree:

```
root supervisor
├── AppState actor        — owns List<Todo>; the single writer
├── UI actor              — view fns + local input state; subscribes to AppState
├── Stats actor           — derived counts; independently crashable/restartable
└── (platform) Render     — scene graph, wgpu, input routing
```

**Demo script (what reviewers see):** add/complete/delete todos with validation errors surfaced via `Result` (empty title shows a notice, not a crash); a "crash stats" button panics the Stats actor — its panel shows a failure boundary for a beat, the supervisor restarts it, counts reappear, todos untouched; a "burn CPU" button pegs the Stats actor while typing in the input stays 60fps; a debug overlay shows live per-actor memory (arena sizes) and fiber counts, making isolation visible.

## 8. Milestones

Ordered so every milestone is independently demoable; estimates assume one focused developer.

1. **M0 — Walking skeleton (1–2 wks):** hand-written WASM module runs in wasmtime under tokio; two host actors exchange a typed message; wgpu window clears a color.
2. **M1 — Language core (2–3 wks):** lexer→parser→checker→WASM for functions, records, `match`, `Result`/`?`. CLI runs `.str` files. Golden-file test suite.
3. **M2 — Actor runtime (2 wks):** `actor` declarations, typed channels, buffer transfer, panic→ChildDown→restart. Demo: supervised pair of actors, one crashing on schedule.
4. **M3 — Scene graph (2–3 wks):** render thread with taffy layout + widget set + input routing; UI tree submitted from a host-side actor first, then from Strand code.
5. **M4 — Vertical slice (1–2 wks):** todo app in Strand, full demo script above.
6. **M5 — Measurement + writeup (1 wk):** input-to-frame latency under load, per-actor memory, actor spawn/kill cost, binary size; honest comparison notes vs an equivalent JS/React todo.

Total: ~9–13 weeks part-time-realistic; the M0 skeleton is the de-risking gate — if wasmtime-async + tokio + wgpu don't compose pleasantly, we learn it in week one.

## 9. Future Work (explicitly out of POC scope)

**Backwards compatibility** — the strategic linchpin, deliberately deferred. Two paths sketched earlier, in order of likely execution: (a) embed a JS engine as a legacy actor so existing code runs inside the sandbox (the x86-on-Apple-Silicon play); (b) compile a TS subset directly to the Strand VM for incremental file-by-file migration. Also: **capability-based security** (channels become the capability substrate — an actor can only touch what it was handed); **content-addressed modules** (hash-identified, signed, cached once across all apps); **custom bytecode VM** replacing wasmtime once semantics stabilize; **full type inference**; **`grid` layout primitive** and container-size-responsive helpers beyond the basic branch; **JSX-flavored surface syntax** desugaring to the builder DSL; text shaping/i18n/accessibility; networking + persistence host APIs; hot reload (arenas make actor-level reload natural).

## 10. Risks

| Risk | Read | Mitigation |
|---|---|---|
| wasmtime per-actor Store overhead too heavy for fine-grained actors | Medium | Measure at M0; actors are component-grained (dozens, not millions) in POC |
| WASM GC types too immature for the type mapping we want | Medium | Fallback: linear memory + our own layout for POC; revisit |
| Text rendering/input is a tarpit | High | Ruthless scope: one font, Latin-only, basic caret; glyphon does the rest |
| Compiler eats the schedule | Medium | Subset is fixed (§4.6); anything not needed by the todo app is cut |
| "Colorless" host-call plumbing (async wasmtime) is fiddly | Medium | This is exactly what M0 exists to prove |

## 11. Decision Log

| Decision | Choice | Why |
|---|---|---|
| POC shape | Full vertical slice | Proves the layers compose; each layer alone proves little |
| Impl language | Rust | wasmtime/tokio/wgpu ecosystem; ownership matches runtime semantics |
| Execution engine | Embed wasmtime | Weeks not years; custom VM only after validation |
| Concurrency | Colorless + structured scopes + actors | Industry direction (Loom/Go/Erlang); free given mandatory runtime |
| Errors | Result + `?`; panic kills actor; no try/catch | Post-2012 language consensus; composes with arenas/supervision |
| Null | Single `Option<T>` | Kills the null/undefined split |
| Syntax | TS-like curly braces | Meets today's developers where they are |
| UI | Declarative scene graph, platform-owned render thread | Removes the framework tax and the jank class of bugs |
| UI syntax | Typed builder DSL (Compose/SwiftUI-style); JSX later as sugar only | JSX was a workaround for JS's missing tree syntax; DSL gets native control flow + full typing |
| Styling | Typed, scoped props; no cascade/selectors; typed theme tokens | Cascade made CSS append-only; tokens > specificity wars (the Tailwind signal) |
| Layout | `row`/`column`/`stack` with `mainAxis`/`crossAxis` naming; first-class alignment | Ends the centering meme and the justify/align confusion; grid deferred |
| Demo | Todo app + crash/CPU-burn buttons | Boring app, visible architecture |
| Backwards compat | Deferred, documented | POC proves the new model; compat is a later, separable bet |