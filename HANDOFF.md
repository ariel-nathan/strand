# Handoff — read once, then delete

**This file is ephemeral.** When you have read it and started work, delete it and
commit the deletion:

```
rm HANDOFF.md && git add -A && git commit -m "chore: drop the handoff note"
```

It exists because the previous session ran out of context, not because the
project needs a second design document. Everything durable lives in `docs/` and
the git log.

---

## Read these first, in this order

1. `docs/poc-design-doc.md` — the spec. The user maintains it and updates it
   mid-flight; treat a change to it as binding on work in progress.
2. `docs/abi.md` — value representation and host ABI. Written by us, records
   decisions the design doc leaves open.
3. `docs/inspiration-canon.md` — prior art, with lessons that are *directives*,
   not colour. Two have already changed the code.
4. `git log --oneline` — 26 commits, each message explains a decision.

## Where things stand

M0, M1 and M2 are complete. M3 is most of the way there.

| milestone | state |
|---|---|
| M0 walking skeleton | done |
| M1 language core | done — lexer, parser, checker, WASM, CLI, golden files |
| M2 actor runtime | done — actors, typed channels, transfer, supervision |
| M3 scene graph | layout, painting, input, widgets, inspector done; **views in Strand not started** |
| M4 todo app in Strand | not started (blocked on M3's last piece) |
| M5 DX slice | partial — message tracing done, hot reload not started |
| M6 measurement | partial — compositor fps measured, nothing else |

145 tests, 17 test binaries, clean tree. `cargo test --workspace` should be green
before and after anything you do.

## Commands

```
strand run <file.str>       compile and run `main`
strand build <file.str>     compile to .wasm
strand todo                 the todo UI (opens a window)
strand inspect [w h]        print the todo UI tree — no window, read this yourself
strand ui [--burn]          compositor demo; --burn proves the app cannot jank it
strand demo [--trace]       M0 actors; --trace prints the causal message log
strand crash [--trace]      supervised crash and restart
```

## Layout of the code

- `crates/strandc` — the compiler. `lexer` → `parser` → `check` (emits typed
  `hir`) → `codegen` (wasm-encoder). `diag` is the shared diagnostic type.
- `crates/strand-runtime` — actors on wasmtime + tokio. `sim` runs scenarios
  deterministically. Supervision, crash reports, buffer transfer live here.
- `crates/strand-render` — `scene` (tree, layout, command array), `paint` (wgpu
  rects), `text` (glyphon), `widgets`, `compositor` (channels), `inspect`.
- `crates/strand-cli` — the `strand` binary, plus `todo` (host-side UI actor)
  and `encode` (typed message encoder).

## Decisions you should not re-litigate

Each of these was argued through and is load-bearing. The reasoning matters more
than the choice, so if you want to change one, engage the reasoning.

- **No WASM GC, no Component Model.** Core modules and linear memory. §10 of the
  design doc pre-approves this fallback; taking it kept the compiler unblocked.
- **`Result` returns as `(i32 tag, i64 payload)`, never boxed.** The POC has no
  GC, so the per-actor bump allocator never frees. Boxing every `Result` would
  leak on every fallible call — and §7's debug overlay displays live arena
  sizes, so the leak would appear in the demo meant to prove isolation.
- **Message types must be flat.** No interior pointers. This is not a limitation
  to apologise for: it is exactly what makes the wire format *be* the memory
  format, so a message needs no decoding on arrival.
- **The render thread is the main thread.** winit requires it, so the compositor
  owns `main()` and the actor runtime is its guest. This is a stronger guarantee
  than §6.1 states — app code has no handle to that thread at all.
- **Determinism before scheduling.** `sim` runs on one thread with virtual time
  and a seeded RNG. The canon says design this in before the scheduler hardens;
  it paid off immediately, since every crash and restart test is replayable.
- **One `Trace`, two uses.** The causal message log is both §8.4's debugger and
  the determinism oracle. Do not build a second one.

## Environment traps

- **Run `cargo` through PowerShell, never the Bash tool.** Git Bash puts its own
  `link.exe` ahead of MSVC's, and builds fail with a `link: extra operand` error
  that looks like a code problem. File reads/writes/greps through Bash are fine.
- **Bash heredocs choke on large Rust literals** and write nothing, silently.
  Use the Write tool for source files, or write a `.py` patch file and run it.
- **Backticks in `git commit -m` get shell-substituted** and eat words. Write the
  message to a file and use `-F`.
- **Kill a running `strand.exe` before rebuilding** or the linker cannot replace
  the binary.

## How to see what you built

You cannot see the screen. This cost four rendering bugs that 145 tests missed,
because each was correct as *data* and wrong on *screen*.

- **`strand inspect`** prints the laid-out tree with font-accurate geometry. Use
  it to check layout yourself. It is truthful: layout and rendering share one
  `FontSystem`.
- **`crates/strand-render/tests/pixels.rs`** renders headless and asserts named
  pixels. Add a case here for any new visual behaviour.
- **Ask the user for a screenshot only for what genuinely needs eyes**: colour,
  glyph rendering, blending, overlay alignment. Not geometry.
- When you do ask, **state your predictions first** so the answer is diagnostic
  rather than decorative. This worked well.

## What to do next, in order

1. **Actor stats in the inspector overlay** (§8.4). Arena sizes, mailbox depths,
   fiber counts. The runtime already has `Trace`; wiring it to `inspect` makes
   §7's "isolation visible" claim real. Small, and closes a doc-specified gap.
2. **`textInput` and `scroll`** (§6.4). Scroll needs clip commands — §6.1 lists
   `clip-start`/`clip-end` and they do not exist yet. textInput needs a caret and
   key routing. Both are absent rather than stubbed, deliberately.
3. **§6.2's builder DSL** — view functions in Strand. The largest remaining
   piece, and what M4 depends on. `crates/strand-cli/src/todo.rs` was written
   against the same channels a Strand UI actor will use, so replacing it should
   change only who builds the tree.

## Known gaps, stated honestly

Do not let the test count imply coverage these do not have.

- **Buffer transfer copies.** Ownership is enforced — using a transferred handle
  traps — but bytes still go guest → host → guest. Zero-copy needs shared memory
  or a mappable layout, which is a real decision, not an optimisation.
- **No `send` from Strand.** The plumbing exists; the blocker is that a sender
  must be checked against the *receiver's* message type, and an actor address is
  currently a bare `int`. Needs channel handles carrying their type.
- **`ChildDown` is a variant guests ignore.** Supervision is host-side. A Strand
  actor cannot yet pattern-match on a child's death.
- **Method calls are unimplemented.** `title.trim()`, `list.push()` reject with
  "not supported yet" — they need the stdlib §4.6 defers past M1.
- **`let r = Ok(1)` with nothing to pin the error type** fails at codegen with a
  clear message. The branch-join case is handled; full inference is §11.
- **Match exhaustiveness is one level deep.** Covers `Result`/`Option`/sum
  scrutinees; nested coverage is not tracked.
- **One actor per module**, and actor state must be a single-word type.
- **No `strand fmt`** despite §8.1 asking for one binary that does it.

## One recurring mistake worth naming

Twice I fixed wasmtime error extraction and got it wrong the first time, because
I patched the symptom I could see rather than the shape of the problem. wasmtime
leads every error with `"error while executing at wasm backtrace:"` and puts the
real cause under `Caused by:` — traps and host-function errors both. If crash
reports ever go vague again, that is where to look.
