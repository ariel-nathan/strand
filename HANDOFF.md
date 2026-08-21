# Handoff — delete when the work below is started

```
rm HANDOFF.md && git add HANDOFF.md && git commit -m "chore: drop the handoff note"
```

Everything durable is in `docs/strand-design.md` and the git log. Commit
messages carry the reasoning; `git log -S` the line before changing it.

## State

453 tests, 28 binaries, `cargo test --workspace` green. M0–M4 done including
§8's demo sequence. §13 has the status and the known gaps; read it rather than
trusting this file.

Two examples matter: `examples/strand/todo_demo.str` is §8's demo (two actors,
crash and burn buttons) and `examples/strand/pipeline.str` is the smallest
thing that shows ports.

## Next: M5 Tier-1 hot reload

§9.3 says Tier 1 is cheap because "No state moves". That is true of the state
*shape* — it does not change, so there is no `migrate` to write. It is not true
of the state itself. New code means a new module, a new `Store`, a new linear
memory; the record the running actor holds is a pointer into memory that is
about to be dropped. So Tier 1 needs the typed snapshot §9.3 files under Tier 2.

Do it anyway: §10.4 says that snapshot is the resumability primitive, and §9.4
already claims a crash report carries one. It does not. One mechanism, three
uses.

1. **Typed snapshot.** Host-side walker that reads a value out of guest memory
   given its `Ty` — records, strings, lists, sums, `Result`/`Option`, scalars —
   and a writer that rebuilds it in another instance through `strand_alloc`.
   Both read layout from the same `Hir`, the way `strand-cli/src/encode.rs` and
   `frame.rs` already do. `strand_state` (global 1) is the entry pointer.
   Test headlessly: snapshot out of instance A, restore into B, assert the view
   draws an identical tree.
2. **Swap.** A supervisor path that starts the replacement on new bytes with the
   snapshot restored instead of running `init`.
3. **Watch.** `strand view --watch <file>`: recompile on change, swap, redraw.
   Needs a file-watch dependency; there is none today.
4. **Crash reports carry the snapshot** — nearly free once (1) exists, and it
   closes §9.4.

Three things to get right, and to say plainly in the commit:

- A shape mismatch (the record was edited too) must be detected and fall back to
  a fresh `init` with a clear message. That check is what §9.3 says Erlang
  cannot make, so it is the interesting part rather than a safety net.
- Sharing is not preserved: two fields pointing at one list become two lists.
  Honest at POC scale. Say so.
- Cycles cannot occur — data is immutable with no back-references — so the walk
  terminates.

## If the priority changes

- **M6 measurement.** Input-to-frame latency under load, memory per actor. The
  overlay already observes most of it; it needs recording and a writeup.
- **`xs[0]`.** Parses as two expressions and fails later as a confusing type
  error. §13 calls that worse than either fixing or refusing it.
- **LSP completion.** `ui::BUILDERS` and `stdlib::FUNCTIONS` already describe
  everything needed.

## Environment

- **Run `cargo` through PowerShell, never the Bash tool.** Git Bash puts its own
  `link.exe` ahead of MSVC's; the failure reads `link: extra operand`.
- **`strand.exe` is held open** by any running window and by VS Code's language
  server, which respawns it. Kill it in a loop while cargo builds, or the link
  step fails with "Access is denied".
- **Tests hang under `sim` if an actor never idles.** Virtual time only advances
  when every task is idle, so a self-scheduled loop (the CPU burn) stops the
  clock. `tests/tree.rs::drive_realtime` is the real-clock driver for those.
- **Mixed CRLF/LF.** A patch matching `\n` silently matches nothing in a CRLF
  file. Normalise, patch, restore.
- **Backticks in `git commit -m`** get shell-substituted. Use `-F`.
- **The user works in this repo while you do.** Stage explicit paths; check
  `git status --short` first.

## Habits worth keeping

- **Verify a regression test catches the bug.** Break the fix, watch it fail,
  restore. Every fix this session was checked that way and one test needed
  rewriting because of it.
- **Measure before naming a cause.** The overlay bug looked like command
  ordering and was a two-pass renderer; the difference was one grep.
- **`strand view <file> <w> <h>`** prints the laid-out tree with real font
  metrics. Use it constantly. Ask for a screenshot only for colour, blending and
  alignment — and state your prediction first, so the answer is diagnostic.
