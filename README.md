# Strand

*A new browser runtime: a typed language, a multithreaded actor VM, a declarative scene graph.*

The core architecture of the web platform is from the 1990s. A script language
with one thread controls a retained document model. Types, concurrency,
security and app-style UI all came later, as additions.

Strand asks what that platform looks like if you start it new in 2026.

This repository is the proof of concept: a full vertical slice, not one layer.
A typed language compiles to WASM. A multithreaded actor runtime hosts it. The
actors drive a GPU scene graph on a render thread the platform owns. A todo
application ties the three together. The goal is not a complete layer — it is
proof that the layers connect.

`docs/strand-design.md` is the design document. Every claim below is a section
of it.

## Run it

Needs a Rust toolchain and a GPU. Nothing else — no configuration file, no
package manager, no code generation step.

```
cargo build          # then target/debug/strand, or `cargo run -p strand-cli --`
```

```
strand todo                                the todo app: type, scroll, delete
strand view examples/strand/todo_demo.str          the same app, in Strand
strand view examples/strand/todo_demo.str --watch  ... reloaded as you edit it
strand view examples/strand/toggles.str 800 600    the laid-out tree, no window
strand crash --window                      an actor dying and coming back
strand help                                everything else
```

In any window: **F12** for the debug overlay — a row per actor, with its arena
size, mailbox depth and generation. **F5** restarts the app from `init`.

Three things in the demo make the argument for the architecture:

1. **Crash an actor.** "crash stats" panics the Stats actor with a real
   `panic()` in Strand. Its panel shows a failure boundary, the supervisor puts
   a fresh one in its place, and not one todo is disturbed — they were never in
   that arena.
2. **Peg a core.** "burn CPU" holds a core at full load with guest code. Typing
   stays at frame rate, because the compositor is a different thread and the
   busy actor is a different arena.
3. **Edit the file.** With `--watch`, a save recompiles, swaps every actor onto
   the new module, and carries its state across. Change the state record
   instead and it says which field moved, then starts fresh.

## The language

TypeScript's shape, with the known holes closed. No `null`, no implicit
coercion, no `this`, no promises, no `async` keyword, no try/catch. An expected
failure is a `Result` value; a bug is a panic that ends one actor.

```strand
type Todo = { id: int, title: string, done: bool }

type AddError = | EmptyTitle | TooLong(max: int)

fn accept(title: string): Result<string, AddError> {
  let clean = trim(title)
  if isEmpty(clean) { return Err(EmptyTitle) }
  if len(clean) > 40 { return Err(TooLong(max: 40)) }
  Ok(clean)
}
```

An actor declares its state, its channels and one handler per inbound channel.
**No expression in the language can name another actor.** An `app` block is the
supervision tree, written down and checked by the compiler. From
`examples/strand/pipeline.str`, which runs:

```strand
type Total =
  | Now(total: int, samples: int)
  | Cleared

actor Meter {
  state: Reading
  in  input:  Input      // the platform's events
  out totals: Total      // what it can say

  fn init(): Reading { Reading { total: 0, samples: 0 } }

  on input(state: Reading, msg: Input): Reading {
    match msg {
      Click(id) => {
        let next = clicked(state, id)
        // The port is named, not addressed. Where it leads is not this
        // actor's business.
        send(totals, Now(total: next.total, samples: next.samples))
        next
      },
      // `Input` is a platform type, so `match` has to account for every
      // event it can deliver.
      // Everything else leaves the meter as it was.
      Typed(ch) => state,
      Backspace => state,
      Enter => state,
      Escape => state,
      Focus(id) => state,
      Scrolled(id, offset) => state,
    }
  }

  view fn draw(state: Reading): Node {
    screen(gap: 12, padding: 16) {
      text("meter — " + str(state.total))
      row(gap: 8) {
        button(id: ADD(), label: "add 7")
        button(id: RESET(), label: "reset")
      }
    }
  }
}

app Pipeline {
  meter    = Meter
  reporter = Reporter

  meter.totals -> reporter.totals
}
```

UI is a typed builder DSL, not JSX: a conditional is an `if`, a list is a
`for`, and props are checked arguments. A view is a pure function of state, and
the platform re-runs it after every message. There is no DOM, no cascade, no
z-index and no diff in user code.

## How it works

| Layer | What it is |
|---|---|
| Language | Hand-written lexer, recursive-descent parser, bidirectional checker, WASM emitted with `wasm-encoder`. Diagnostics through `miette`. |
| Runtime | One wasmtime `Store` per actor — that is its arena. One tokio task per actor, M:N over threads. A panic ends one actor; the supervisor restarts it. |
| UI | Views append to a flat post-order array in the guest's own memory. The host reads it, lays it out with `taffy`, and paints with `wgpu` and `glyphon`. |

Two rules do most of the work.

**No GC types, no Component Model.** Core WASM and our own layout, so the
compiler waits on no toolchain. `Result` and `Option` cross a return boundary as
two values, `(i32 tag, i64 payload)` — never boxed, because a bump arena keeps
everything until the actor dies and a boxed `Result` would leak at every
fallible call.

**The wire format is the memory format.** A message type must be flat, so the
bytes on a channel already are a valid value in the receiving arena. There is
no decode step for a message, none for a frame, and none for a state snapshot.
Every one of those reads its layout from the same table the emitter wrote it
with (`crates/strandc/src/layout.rs`), so the two ends cannot disagree about a
byte.

## What is real, and what is not

Milestones M0 to M5 are done bar one item; §13 of the design document keeps
the list, and it is the one to trust.

Real: the compiler and its diagnostics; actors, typed channels, supervision and
crash reports; the scene graph, layout, text and input; the todo application
written in Strand; hot reload with a typed state snapshot; a debug overlay on
live data; a language server with hover, go-to-definition, references and
symbols. 489 tests, `cargo test --workspace` green.

Not real yet, and known: `scope` and `spawn` parse and do nothing, so the fiber
count is 0 or 1. There is no `xs[0]`, no method-call syntax, and no `strand
fmt`. `push` is O(n). Match exhaustiveness goes one level deep. Everything in
sections 10 to 12 of the design document — storage, routes, capabilities, the
package model, the network — is design, not code.

Nothing here is a product. It is a proof that the layers connect, and a
document arguing they should.

## Layout

```
crates/strandc         lexer, parser, checker, WASM emitter, layout table
crates/strand-runtime  actors, scheduling, supervision, snapshots
crates/strand-render   scene graph, layout, wgpu renderer, debug overlay
crates/strand-cli      the `strand` binary
crates/strand-lsp      the language server
editors/vscode         syntax highlighting and LSP client
examples/strand        programs, each one a commentary on a design section
docs/strand-design.md  the design document
```

Windows note: build through PowerShell. Git Bash puts its own `link.exe` ahead
of the MSVC one, and the link step fails with `link: extra operand`.

## License

MIT.
