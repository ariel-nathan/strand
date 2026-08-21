# Handoff — read once, then delete

**This file is ephemeral.** When you have read it and started work, delete it
and commit the deletion:

```
rm HANDOFF.md && git add HANDOFF.md && git commit -m "chore: drop the handoff note"
```

It exists because a session ended, not because the project needs a third design
document. Everything durable lives in `docs/` and the git log — and the commit
messages carry the *reasoning*, not just the change. When something looks
arbitrary, `git log -S` the line before changing it.

---

## Read these first, in this order

1. `docs/poc-design-doc.md` — the spec. The user maintains it and updates it
   mid-flight; treat a change to it as binding on work in progress.
2. `docs/abi.md` — value representation and host ABI. Written by us. §8 (the
   frame), §9 (input) and §5a (lists) are the newest and the least obvious.
3. `docs/inspiration-canon.md` — prior art, with lessons that are *directives*,
   not colour. Several have changed the code.
4. `docs/strand-web-transport-design.md` — the user's, post-POC. Nothing to act
   on, but it constrains primitives the POC already has.
5. `git log --oneline` — 44 commits, each message explains a decision.

## Where things stand

M0–M4 are done. 427 tests across 22 binaries; `cargo test --workspace` is green
and should stay that way.

| milestone | state |
|---|---|
| M0 walking skeleton | done |
| M1 language core | done |
| M2 actor runtime | done |
| M3 scene graph | done — layout, paint, input, widgets, scroll, text input, inspector |
| M4 vertical slice | **app done, demo script not** — see "what to do next" |
| M5 DX slice | partial — tracing and the overlay are done; **Tier-1 hot reload is not started** |
| M6 measurement | barely started — compositor fps only |

The language now has: records, sums, `match`, `Result`/`?`, actors, typed
channels, `view fn` with §6.2's builder DSL, `List<T>`, `for`, string
concatenation and a six-function string stdlib.

**`examples/strand/todo_app.str` is the headline.** §7's todo app, written
entirely in Strand: typing, validation through `Result`, toggling, deleting,
scrolling, and `for todo in state.todos { todoRow(todo) }` straight out of §6.2.

## Commands

```
strand run <file.str>            compile and run `main`
strand build <file.str> [-o out] compile to a .wasm module
strand view <file.str>           draw a Strand view; an actor with a `view fn`
                                 is interactive
strand view <file.str> <w> <h>   print its laid-out tree instead — READ THIS
strand todo                      the host-side todo UI (§7, the Rust one)
strand crash --window            watch an actor die and restart (§5.4)
strand demo --window             the M0 actor skeleton
strand ui [--burn]               a busy actor cannot jank the compositor
strand lsp                       the language server (VS Code starts this)
strand inspect [w h]             print the host-side todo UI tree
```

## Layout of the code

- `crates/strandc` — the compiler. `lexer` → `parser` → `check` (emits typed
  `hir`) → `codegen` (wasm-encoder). Plus three tables that are each the *only*
  description of their subject, read from several places:
  - `ui.rs` — the builder vocabulary and the frame's memory layout. Read by the
    parser, the checker, codegen **and** the host's decoder.
  - `input.rs` — the platform's `Input` message type. Read by the checker and
    by the host's translation from `InputEvent`.
  - `stdlib.rs` — the string functions. Read by the checker and by hover.
  - `analysis.rs` / `line_index.rs` — position-indexed facts for the editor.
- `crates/strand-runtime` — actors on wasmtime + tokio. `sim` runs scenarios
  deterministically. Supervision, crash reports, per-actor stats, and `Frames`,
  the one-method trait a UI actor's output leaves through.
- `crates/strand-render` — `scene` (tree, layout, clip commands), `paint` (wgpu
  rects with scissor batching), `text` (glyphon), `widgets`, `compositor`,
  `inspect` (outlines + the §8.4 actor panel).
- `crates/strand-lsp` — diagnostics, hover, go-to-definition, references,
  symbols. `features` is plain functions of the source; `server` is a shell.
- `crates/strand-cli` — the `strand` binary. `app` runs a Strand UI actor,
  `frame` decodes what one drew, `view` handles the static case, `todo` is the
  host-side UI, `encode` the typed message encoder.
- `editors/vscode` — TextMate grammar plus a thin client.

## Decisions you should not re-litigate

Each was argued through and is load-bearing. The reasoning matters more than
the choice, so if you want to change one, engage the reasoning.

- **No WASM GC, no Component Model.** §10 pre-approves this fallback.
- **`Result` returns as `(i32 tag, i64 payload)`, never boxed.** No GC, so the
  bump allocator never frees; boxing every `Result` would leak per fallible
  call and show up in the very overlay meant to prove isolation.
- **Message types must be flat.** That restriction is what makes the wire
  format *be* the memory format.
- **The render thread is the main thread.** winit requires it, so the
  compositor owns `main()` and the actor runtime is its guest.
- **`Ty::Node` is zero-width.** A view does not return a tree; it *appends* to
  a per-frame array. Consequences: the array comes out post-order for free, and
  a node cannot be stored, passed or used twice — so it can never appear
  anywhere other than where it was written. `let n = text("hi")` is a compile
  error on purpose.
- **One counter does all child tracking.** `child_count = pending - marker`.
  This is why `if` and `for` among children needed no special handling at all.
- **`Input` is declared by the platform and opted into** with `message: Input`.
  Name-matching against a user's own type would be a protocol held together by
  spelling. Opt-in because it also takes `Click`, `Enter`, `Escape` out of the
  namespace.
- **`Frames` lives on the `Registry`,** not in `spawn_supervised`'s arguments —
  "where does this actor's output go" is the address book's question.
- **Determinism before scheduling.** `sim` is single-threaded with virtual time
  and a seeded RNG; every crash and restart test is replayable.
- **One `Trace`, two uses** — §8.4's debugger and the determinism oracle. The
  live gauges (`ActorStats`) are deliberately *separate*: history is a log,
  liveness is a gauge.

## Environment traps

- **Run `cargo` through PowerShell, never the Bash tool.** Git Bash puts its own
  `link.exe` ahead of MSVC's; builds fail with `link: extra operand`, which
  looks like a code problem.
- **Bash heredocs eat backslashes.** A `\\b` in a patch script arrives as `\b`.
  Write the script with the Write tool and run it, or you will silently patch
  nothing and get a confusing assertion.
- **The user's checked-in files are CRLF.** A Python patch matching `\n` against
  `editors/…` will not match. Read as bytes, normalise, restore on write.
- **`strand.exe` gets held open** by any running window *and* by VS Code's
  language server. `cargo build` then fails with "failed to remove file …
  Access is denied". Kill both before rebuilding; the LSP respawns.
- **Backticks in `git commit -m` get shell-substituted.** Write the message to a
  file and use `-F`.
- **The user works in this repo while you do.** They have added a whole crate
  mid-session. `git add -A` will sweep their work into your commit — stage
  explicit paths, and check `git status --short` before committing.

## How to see what you built

You cannot see the screen. This has cost real bugs — several were correct as
*data* and wrong on *screen*.

- **`strand view <file> <w> <h>`** prints the laid-out tree with font-accurate
  geometry. This is the single most useful tool in the repo. Use it constantly.
- **`crates/strand-render/tests/pixels.rs`** renders headless and asserts named
  pixels. Add a case for any new visual behaviour.
- **Ask the user for a screenshot only for what genuinely needs eyes** —
  colour, glyph rendering, blending, overlay alignment. Not geometry.
- **State your predictions before asking.** It makes the answer diagnostic
  rather than decorative, and it has worked every time.

## What to do next, in order

1. **`send` from Strand, and §7's demo script.** This is the biggest remaining
   POC gap. `todo_app.str` is one actor, but §7 specifies a supervision tree —
   AppState, UI, and a **Stats** actor that a "crash stats" button kills and the
   supervisor restarts while the todos survive, plus a "burn CPU" button that
   pegs Stats while typing stays at 60fps. Those two beats are the demo's whole
   argument for the architecture, and they need a second actor, which needs
   `send`.

   The blocker is typing, not plumbing: a sender must be checked against the
   *receiver's* message type, and an actor address is a bare `int` today. The
   options considered so far: channel handles that carry their payload type, or
   an `out:` declaration on the actor that the host wires at spawn (which
   deletes the address entirely — worth a hard look, it is the "delete a
   subsystem" move).

2. **Record-update syntax.** `Model { ...state, draft: x }`. Small, and
   `todo_app.str` is the evidence: with six fields, every transition spells all
   six out. §4.2 makes data immutable, which makes the sugar close to required
   rather than nice.

3. **M5's Tier-1 hot reload.** §8.3 says view functions are pure `state → Node`,
   so this is a supervisor restart against preserved state — the machinery from
   §5.4 already exists. The file-watch → recompile → swap loop reuses the whole
   pipeline. This is the milestone that demos the *platform* rather than the app.

4. **M6 measurement.** Nothing but compositor fps is measured. Input-to-frame
   latency under load and per-actor memory are both already observable through
   the overlay; they just need recording.

## Known gaps, stated honestly

Do not let the test count imply coverage these do not have.

**Language**
- **No record-update syntax.** See above.
- **No indexing.** `xs[0]` does not error — it parses as *two* expressions,
  `xs` and `[0]`, and fails later with a confusing type error. Either add
  indexing or reject the syntax; the current silence is the worst option.
- **`str` takes `int` only.** Float formatting is a real project (§12's
  "ruthless scope"), and nothing needs it yet.
- **Method calls are unimplemented.** `title.trim()` says so. The stdlib is
  free functions; method syntax can land later as sugar resolving to them.
- **No closures, no `scope`/`spawn`.** §4.4's structured concurrency is lexed
  and nothing more. This is why `fibers` in the overlay is 0-or-1: an actor
  *is* one fiber today, and the column is honest about it.
- **No `send` from Strand**, so no Strand-to-Strand messaging.
- **`ChildDown` is a variant guests ignore.** Supervision is host-side.
- **One actor per module**, and actor state must be a single-word type.
- **Match exhaustiveness is one level deep.**
- **`let r = Ok(1)` with nothing to pin the error type** fails at codegen with
  a clear message. Full inference is §11.

**Runtime and rendering**
- **Buffer transfer copies.** Ownership is enforced — using a transferred
  handle traps — but bytes still go guest → host → guest.
- **`push` is O(n).** Every append allocates a new list. Honest at POC scale;
  a growable buffer is a different design, not a faster version.
- **No `strand fmt`,** despite §8.1 asking for one binary that does it.
- **`Build` is the largest AST variant at 152 bytes** and sets the parser's
  depth ceiling. Boxing its `Option<Block>` buys real headroom if `MAX_DEPTH`
  ever gets in the way.

**Editor**
- Hover, go-to-definition, references, symbols and diagnostics work. **No
  completion**, which for a fixed builder vocabulary is the obvious next win —
  `ui::BUILDERS` and `stdlib::FUNCTIONS` already describe everything needed.
- The grammar mirrors two closed tables by hand. A test asserts the fixture
  exercises every name in them, so adding a widget without colouring it fails.

## Recurring mistakes worth naming

- **Fix the shape of the problem, not the symptom you can see.** Twice now a
  first attempt patched the visible instance and missed the class: wasmtime
  error extraction, and the text field's caret (moving the caret would have
  relocated the jump rather than removed it — the fix was to delete the state
  where both could coexist).
- **Verify a regression test actually catches the bug.** Disable the fix, watch
  it fail, restore. Two tests written this session would have passed against the
  broken code without that step.
- **Do not assert a cause you have not measured.** The depth-guard failure
  looked like `Expr` growing; `Expr` had not changed. It was `primary`'s debug
  stack frame growing from two new match arms. A five-minute measurement
  replaced a plausible wrong answer.
- **When a diagnostic sends someone the wrong way, that is a bug.** "This file
  has no view" was true and useless; naming the functions the module *does*
  have turned a dead end into a next step. §8.2 treats diagnostics as a product
  surface — hold them to it.
