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

**Explicit non-goals for the POC** (noted as future work, §11): backwards compatibility with HTML/CSS/JS, a self-hosted bytecode VM, content-addressed module distribution, the capability security model, networking, persistence, accessibility, and text input beyond the minimum the todo app needs.

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
- Layout resolves the tree into a flat, typed **render command array** (rect, text, image, clip-start/end) — an architecture proven by clay (see §13), which is renderer-agnostic, trivially diffable, cheaply serializable over actor channels, and even compilable to HTML (a useful proof-of-concept for the future backwards-compat story, run in reverse). Layout allocates from a per-frame arena that is reset each frame — no GC pressure from UI, following clay's static-arena model (~3.5 MB for 8k elements).
- The **render thread** (platform-owned: winit event loop + wgpu) diffs command arrays against the retained scene graph and paints. Layout uses `taffy` initially; clay's algorithm is the reference if we replace it.
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
| Implicit stacking contexts and z-index wars | Paint order = tree order. Tooltips/modals use clay-style **floating elements with attach points** — declare which point of the floating node pins to which point of its anchor (`attach(element: .bottomCenter, anchor: .topCenter)`). No z-index; floats sort by an explicit small layer number only among themselves |
| Sizing via width/height/min/max/flex-basis/flex-grow/flex-shrink interplay | Clay's four-word sizing vocabulary: `fit(min, max)`, `grow(min, max)`, `fixed(n)`, `percent(p)` — covers the same ground, reads unambiguously |
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

## 8. Developer Experience & Tooling

DX is treated as architecture, not polish — the POC's audience is developers, and the demos that argue for the *platform* (rather than the todo app) live here.

### 8.1 One binary, zero config

The deepest JS-ecosystem lesson is toolchain sprawl: webpack + babel + eslint + prettier + jest + tsconfig meant five config files before hello world. Cargo, Deno, and Bun all won developer affection by collapsing the toolchain. `strand` is a single binary: `strand run`, `strand fmt`, `strand test` (post-POC), `strand doc` (post-POC). No config files in the POC at all. Formatting follows the gofmt doctrine — one true style, zero options — because gofmt's real achievement was ending formatting arguments, not formatting.

### 8.2 Compiler diagnostics as a product surface

Elm proved error messages are a feature; Rust institutionalized it. Since Strand's core pitch is "types that survive to runtime," the compiler is the first impression and its diagnostics get first-class treatment from M1: source spans, labeled underlines, and a suggested fix where one exists (via the `miette` crate, which provides Rust-quality rendering nearly free). Retrofitting good errors later is miserable; starting with them is cheap.

### 8.3 Hot reload — three tiers on top of supervision

Hot reload is not a separate subsystem: it is a supervisor restart where the replacement actor runs newer code, so §5.4 already built the machinery. Three tiers, in order of difficulty:

**Tier 1 — view reload (in POC).** View functions are pure `state → Node`. On file change: recompile the module, ship it to the UI actor over a channel, re-invoke views against existing state. No state-migration problem exists, so this is sub-second and cheap to build — the file-watch → recompile → swap loop reuses the whole pipeline.

**Tier 2 — actor logic reload (POC stretch goal).** Behavior changed, state shape unchanged: snapshot state, restart the actor on new code, restore the snapshot. Because state records are typed, the runtime *statically verifies* old and new shapes match before attempting the swap — a safety check Erlang's hot code loading cannot make.

**Tier 3 — schema migration (future work).** State shape changed: run an optional `migrate(old) -> new`, else restart fresh. This is where hot reload gets genuinely hard; deferred deliberately.

### 8.4 Debugging — the actor superpower is replay, not breakpoints

A traditional stepping debugger (DWARF emission through wasmtime → lldb / DAP) is a large lift and deferred. The actor model offers something stronger first: since actors interact *only* through typed messages, recording an actor's inbound messages is a complete record of its inputs. Consequences, in POC scope:

- **Message tracing** — a structured, causal log of who sent what to whom, with typed payloads, toggleable per actor. This is Erlang's observer, but typed.
- **Structured crash reports** — a panic yields {actor, message being processed, state snapshot, wasm backtrace} delivered to the supervisor. The supervisor is a crash reporter by construction; no stack-trace soup.
- **Debug overlay** — per-actor arena sizes, fiber counts, mailbox depths, rendered by the platform (clay demonstrated inspectors can be injected render commands).

Future work: **deterministic single-actor replay** — feed a recorded message log into a fresh instance for time-travel debugging of one component without whole-program record/replay (no platform has shipped the typed version of this); DAP integration; LSP for editor support (the TS lesson: the language server *is* the daily product).

## 9. Platform Services — Lessons from the App-Platform Web

Mostly future work, recorded now because several lessons impose design constraints on the POC's primitives, and because the biggest wins are places where machinery we already built does double duty.

### 9.1 Storage — one typed API, not five broken ones

The web shipped cookies (4KB, stringly, retrofitted security), synchronous main-thread-blocking localStorage, sessionStorage, IndexedDB (so hostile everyone wraps it — an ecosystem verdict), and AppCache (the canonical API-design disaster). Strand ships **one** storage API: async-only (calls suspend the fiber; colorless concurrency makes this invisible), **typed** (records persist with their language schema — no stringify tax), transactional, with explicit tiers (session-scoped vs durable) and expiry. Storage is a **capability** granted to an actor with a quota — no ambient per-origin access.

### 9.2 Identity — no ambient credentials, ever

Cookies' auto-attachment to every request is ambient authority, and ambient authority is the root cause of the entire CSRF class — patched decades later with SameSite/HttpOnly duct tape. Strand has no cookie equivalent: a session is a capability token an actor explicitly holds and explicitly presents. CSRF becomes unrepresentable, the same way ownership-transfer channels made data races unrepresentable.

### 9.3 Navigation — the URL stays sacred

Deep-linking is the web's superpower, and SPAs spent a decade breaking it (dead back buttons, unlinkable states). A platform that replaces the browser must keep it: **typed routes** as a platform primitive (URLs as typed data — the TanStack Router lesson, not strings to parse), with a declared mapping between an actor's state and its URL so every meaningful state is addressable, linkable, and back-button-safe.

### 9.4 Rendering strategy — resume, don't hydrate

The SSR↔CSR pendulum (server pages → SPAs → SSR + hydration → streaming/islands/RSC/resumability) is twenty years of compensating for two platform gaps: slow cold start, and no way to transfer a running app's state between machines. Strand closes both structurally: content-addressed AOT modules make cold start near-native (removing most of the need to "ship HTML first"), and **the typed state snapshot built for hot reload (§8.3) and crash reports is also the resumability primitive**. A server runs the actor, streams the first render command array (already serializable — §6.1), transfers the snapshot; the client resumes the actor from it. First paint from the server, zero client re-execution, no hydration step, no mismatch bug class. Qwik's resumability, derived from existing primitives instead of heroic closure serialization. POC constraint honored: nothing in the snapshot format may assume same-machine resumption.

### 9.5 Server functions, distribution, and optimistic updates

Next's `"use server"`, TanStack Start, and Remix loaders converge on typed RPC colocated with UI code — but RSC's client/server component split reintroduced **coloring at the component level** (`"use client"` fracturing the tree: the async/await mistake, relearned). The actor model gives the principled version via Erlang's **location transparency**: a "server function" is just an actor running on a server; the typed channel *is* the RPC contract. No directive, no colored components. POC constraint honored: channel semantics never assume shared memory (already true — ownership transfer), so distribution is additive later.

Optimistic updates stop being a library pattern in Elm-style state + `Result` effects: apply predicted state, reconcile on `Ok`, revert on `Err`. The industry's local-first sync engines (Linear, Replicache/Zero, Electric) point at an eventual **synced state record** platform primitive so apps stop hand-rolling reconciliation — future work, named here.

### 9.6 Framework lessons distilled

| Framework scar tissue / insight | Strand decision |
|---|---|
| React hooks: state tied to call order (Rules of Hooks = compiler work leaking onto users); `useEffect` dependency arrays = manual cache invalidation, the #1 bug source; `useMemo`/`useCallback` = manual memoization tax | State lives in typed actor records, not call positions. Effects are messages through the mailbox — no dependency arrays. No manual memoization: the re-render unit is bounded (below) |
| React's keeps | Components as pure functions, unidirectional data flow, composition — all retained |
| Solid/signals: fine-grained reactivity won (Vue, Preact, Angular, Svelte runes, TC39 proposal) — because React re-renders unbounded subtrees | **The actor is the re-render unit**: one actor's state change re-runs one actor's views into one command array; the blast radius is enforced by the platform, not managed by the developer. Signals *within* an actor (compiling reactive reads to targeted command-array patches, Svelte-style) is the planned future optimization — possible without API break because Strand is compiled |
| Svelte: the framework can disappear into the compiler | Adopted as direction: reactivity is a compilation target, not a runtime library |
| Next.js: magic file conventions as API; churn from vendor coupling | No filename-encoded semantics in the platform; routing is declared in code with types. Platform spec stays vendor-neutral (the Flash anti-lesson, again) |
| React Query / TanStack: server state ≠ UI state (caching, staleness, retry are first-class); typed routing | Typed routes as platform primitive (§9.3); a `resource` abstraction for remote data with staleness semantics is future work, named so apps don't reinvent it |

## 10. Milestones

Ordered so every milestone is independently demoable; estimates assume one focused developer.

1. **M0 — Walking skeleton (1–2 wks):** hand-written WASM module runs in wasmtime under tokio; two host actors exchange a typed message; wgpu window clears a color.
2. **M1 — Language core (2–3 wks):** lexer→parser→checker→WASM for functions, records, `match`, `Result`/`?`. CLI runs `.str` files. Golden-file test suite. Diagnostics are first-class from the start: spans, labels, suggestions (miette).
3. **M2 — Actor runtime (2 wks):** `actor` declarations, typed channels, buffer transfer, panic→ChildDown→restart, structured crash reports. Demo: supervised pair of actors, one crashing on schedule.
4. **M3 — Scene graph (2–3 wks):** render thread with taffy layout + widget set + input routing; UI tree submitted from a host-side actor first, then from Strand code.
5. **M4 — Vertical slice (1–2 wks):** todo app in Strand, full demo script above.
6. **M5 — DX slice (1–2 wks):** Tier-1 view hot reload (file watch → recompile → live swap while the todo app runs); message tracing with typed payloads; debug overlay wired to real runtime stats. Stretch: Tier-2 actor reload with verified state snapshot. This is the milestone that demos the *platform*, not the app.
7. **M6 — Measurement + writeup (1 wk):** input-to-frame latency under load, per-actor memory, actor spawn/kill cost, hot-reload round-trip time, binary size; honest comparison notes vs an equivalent JS/React todo.

Total: ~10–15 weeks part-time-realistic; the M0 skeleton is the de-risking gate — if wasmtime-async + tokio + wgpu don't compose pleasantly, we learn it in week one.

## 11. Future Work (explicitly out of POC scope)

**Backwards compatibility** — the strategic linchpin, deliberately deferred. Two paths sketched earlier, in order of likely execution: (a) embed a JS engine as a legacy actor so existing code runs inside the sandbox (the x86-on-Apple-Silicon play); (b) compile a TS subset directly to the Strand VM for incremental file-by-file migration. Also: **capability-based security** (channels become the capability substrate — an actor can only touch what it was handed); **content-addressed modules** (hash-identified, signed, cached once across all apps); **custom bytecode VM** replacing wasmtime once semantics stabilize; **full type inference**; **`grid` layout primitive** and container-size-responsive helpers beyond the basic branch; **JSX-flavored surface syntax** desugaring to the builder DSL; text shaping/i18n/accessibility; networking + persistence host APIs; **Tier-3 hot reload** (schema migration; Tiers 1–2 moved into POC, §8.3); **deterministic single-actor message replay** (time-travel debugging); **DAP debugger** via DWARF through wasmtime; **LSP**; `strand test` and `strand doc`; **typed capability storage API** (§9.1); **typed routes + URL/state mapping** (§9.3); **server-side actor execution + snapshot resumption** (§9.4); **distributed actors / location-transparent channels** (§9.5); **synced state records** (local-first sync primitive); **`resource` abstraction** for remote data with staleness semantics; **in-actor signals** compiling to targeted command-array patches (§9.6).

## 12. Risks

| Risk | Read | Mitigation |
|---|---|---|
| wasmtime per-actor Store overhead too heavy for fine-grained actors | Medium | Measure at M0; actors are component-grained (dozens, not millions) in POC |
| WASM GC types too immature for the type mapping we want | Medium | Fallback: linear memory + our own layout for POC; revisit |
| Text rendering/input is a tarpit | High | Ruthless scope: one font, Latin-only, basic caret; glyphon does the rest |
| Compiler eats the schedule | Medium | Subset is fixed (§4.6); anything not needed by the todo app is cut |
| "Colorless" host-call plumbing (async wasmtime) is fiddly | Medium | This is exactly what M0 exists to prove |
| Hot reload scope creep (Tier 2/3 pulls in migration machinery) | Medium | Tier 1 only is the M5 bar; Tier 2 is stretch, Tier 3 is banned from POC |

## 13. Decision Log

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
| UI pipeline | Layout emits flat render command array (clay model); per-frame arena allocation | Renderer-agnostic, diffable, serializable across channels; zero UI GC pressure |
| Overlays | Attach-point floating elements (clay model) | Solves "tooltip escapes container" declaratively; strictly better than a bare overlay node |
| DX philosophy | Tiny hello-world, batteries included, examples-first docs (raylib model) | Adoption follows joy; raylib's 70+ language bindings prove simple stable APIs travel |
| Toolchain | One `strand` binary: run/fmt/test; zero config; one true format | The anti-webpack/babel/eslint/prettier lesson; gofmt ended arguments, not just formatted code |
| Diagnostics | First-class from M1 (spans, labels, fixes via miette) | Elm/Rust proved errors are a product surface; cheap early, miserable to retrofit |
| Hot reload | Tier 1 (views) in POC, Tier 2 stretch, Tier 3 deferred | It's a supervisor restart with newer code — §5.4 already built the machinery; Flutter proved the retention value |
| Debugging | Message tracing + structured crash reports + overlay in POC; replay/DAP/LSP later | Typed message logs beat breakpoints as the first tool: complete input record per actor |
| Storage | One typed, async, transactional, capability-scoped API | Cookies/localStorage/IndexedDB/AppCache: five broken APIs because the web never shipped one good one |
| Credentials | No ambient auth; sessions are explicit capability tokens | Cookie auto-attachment (ambient authority) is the root cause of CSRF |
| Navigation | Typed routes + actor-state↔URL mapping (future, named) | Deep links are the web's superpower; SPAs breaking the back button is the anti-lesson; TanStack proved routes can be typed |
| Rendering strategy | Resume, don't hydrate: snapshot transfer replaces SSR+hydration | The state snapshot (hot reload, crash reports) doubles as the resumability primitive; kills the double-render and mismatch bug class |
| Distribution | Channels designed location-transparent; server actors later | Erlang's answer to "server functions" without RSC's client/server component coloring |
| Reactivity | Actor = re-render unit now; in-actor signals later | Bounded blast radius by construction beats hooks' manual memoization; signals won the fine-grained argument and compile in cleanly later |

## 14. Prior Art & Inspiration

**clay** (nicbarker/clay) — single-header C layout library with microsecond performance. Directly adopted: the render-command-array output architecture (§6.1), static per-frame arena allocation for layout, the `fit/grow/fixed/percent` sizing vocabulary, attach-point floating elements for overlays, and the idea that a debug inspector can be implemented as injected render commands (our M4 debug overlay). Caveat: clay is explicitly not multi-thread-safe — it informs the layout algorithm and API shape, not the concurrency architecture. Its layout algorithm is the reference implementation if we outgrow `taffy`.

**raylib** (raysan5/raylib) — the DX north star. Zero external dependencies, ~10-line hello world, learned through 140+ examples rather than a spec, and a simple stable C API that earned bindings in 70+ languages. Strand adopts the philosophy: hello world must fit on a slide, the platform is batteries-included, documentation is examples-first plus a cheatsheet, and the host ABI stays boring so other languages can target the VM later. Raylib's `rlgl` (separable GPU abstraction) mirrors our layering.

**The landscape — what already exists, and the gap:**

| Project | What it proves | Why it isn't Strand |
|---|---|---|
| Flutter | Full stack works: own language, no DOM, GPU scene graph, huge adoption | Single-threaded isolates without supervision; Dart kept exceptions; an app framework, not a sandboxed platform for third-party code |
| Lunatic | Erlang-style supervised WASM actors in Rust are buildable (our §5, nearly verbatim) | Server-side only, no language, no UI; development stalled — evidence that runtime-without-platform lacks a market pull |
| wasmCloud | Actor model + capability security on WASM at production scale | Cloud infrastructure orientation; no UI, no language |
| Makepad / GPUI (Zed) / Slint / Iced | Rust "own renderer, no DOM" UIs ship real products at 120fps | App frameworks for trusted code; no sandbox, no new language, no supervision |
| Blazor / Uno / Yew | Non-JS languages in browsers have demand | Still render through the DOM — inherit the exact tax we design out |
| Dioxus Blitz | HTML/CSS rendering without a full browser engine is feasible | Aims at today's web content; relevant to our backwards-compat future work, not the new model |
| Flash / Silverlight / applets | "Own VM + renderer inside the browser" can reach mass adoption | Died of proprietary ownership, plugin security, and vendor politics — the anti-lessons: open spec, capability sandbox from day one, no plugin model |

The defensible position is the intersection: **typed language with runtime types + supervised actor isolation + platform-owned scene graph + open sandboxed-platform ambition**. Every row above occupies one or two of those; none occupies all four.