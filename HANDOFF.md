# Handoff — delete when the work below is started

```
rm HANDOFF.md && git add HANDOFF.md && git commit -m "chore: drop the handoff note"
```

Everything durable is in `docs/strand-design.md` and the git log. Commit
messages carry the reasoning; `git log -S` the line before changing it.

## State

490 tests, green on Windows and Linux — CI runs both on every push, and it
earned that on its first run by finding two tests that were only true on
this machine. M0–M4 done including §8's demo sequence. M5 is done bar one
item — read §13 rather than trusting this file.

Hot reload landed this session: `strand view <file> --watch` recompiles on
save, swaps every actor onto the new module, and carries its state across.
The mechanism is §6.14's typed snapshot, and it has three users — the swap,
§9.4's crash report, and (later) §10.4's resume.

A reload replaces code and keeps data, so a literal a handler stored in the
state keeps its old text until that handler runs again. That is correct and
it reads as a bug the first time; F5 restarts every actor from `init`, and
`--watch` says so on startup.

Three examples matter: `todo_demo.str` is §8's demo, `pipeline.str` is the
smallest thing that shows ports, and `--watch` is best seen on the first.

## Next: pick one

- **M6 measurement.** Input-to-frame latency under load, memory per actor,
  and now the round-trip time of a reload — §13 asks for that one by name,
  and the loop that would report it is in `watch.rs` already. The overlay
  observes most of the rest; it needs recording and a writeup.

- **`Policy::Resume`.** §5.4 names three supervisor strategies and the
  runtime has two. The third — restart from a snapshot the child made — is
  now a few lines: `report.state` already holds one, and `run_life` already
  takes a `restore`. The interesting part is the policy question, not the
  code: a state that caused a trap will probably cause it again, so Resume
  needs a bound (n times, then fresh) or it is a crash loop with extra steps.

- **The trace switch.** §9.4 asks for a per-actor switch on the message
  trace and `Trace` records everything unconditionally. This is the last M5
  item.

- **`xs[0]`.** Parses as two expressions and fails later as a confusing type
  error. §13 calls that worse than either fixing or refusing it.

- **LSP completion.** `ui::BUILDERS` and `stdlib::FUNCTIONS` already
  describe everything needed.

## What the snapshot does not do yet

- **The image is not written anywhere.** §10.4 wants it over a wire; nothing
  serialises `Snapshot` today. The format is already address-free, so this
  is a codec, not a redesign.
- **A reload cannot change the wiring.** `watch.rs` refuses and says so.
  Re-running §6.13's ordering — mailboxes and routes before any actor — for
  a live tree is the real work behind lifting that.
- **Tier 3 is a fresh `init`.** A shape that changed gets a message naming
  the field and nothing else. `migrate(old) -> new` is still future work.

## Environment

- **Run `cargo` through PowerShell, never the Bash tool.** Git Bash puts its
  own `link.exe` ahead of MSVC's; the failure reads `link: extra operand`.
- **PowerShell has no heredoc.** `git commit -F <file>` — which is also what
  keeps backticks in a message from being substituted.
- **`strand.exe` is held open** by any running window and by VS Code's
  language server, which respawns it, and the link step then fails with
  "Access is denied". Do not kill the window — it may be the user's live
  session. Windows lets a running executable be *renamed*:
  `Move-Item target\debug\strand.exe target\debug\strand-locked.old -Force`
  and cargo writes a fresh one while the old process keeps its own handle.
- **Tests hang under `sim` if an actor never idles.** Virtual time only
  advances when every task is idle, so a self-scheduled loop (the CPU burn)
  stops the clock. `tests/tree.rs::drive_realtime` is the real-clock driver.
- **Mixed CRLF/LF.** A patch matching `\n` silently matches nothing in a
  CRLF file. Normalise, patch, restore. `.gitattributes` now pins `eol=lf`
  on checkout, which is what stops the same hazard reaching a test fixture —
  it already had, and the test passed while proving nothing.
- **No `jq` on this machine.** Two watchers on a CI run sat silent until
  they timed out because a script assumed it. `gh run view <id>` reads fine
  on its own.
- **The user works in this repo while you do.** Stage explicit paths; check
  `git status --short` first. `docs/strand-design.md` in particular has been
  carrying an uncommitted rewrite.

## Habits worth keeping

- **Verify a regression test catches the bug.** Break the fix, watch it
  fail, restore. Four mutations were run against the snapshot walker this
  session and two of them passed — the tests that were supposed to catch
  them were testing something narrower than they claimed. Both gaps became
  new fixtures.
- **Run the thing, not only the tests.** `--watch` was green in the test
  suite while every real save from PowerShell failed to compile: a UTF-8
  BOM, which the lexer refused and no test had ever fed it.
- **Measure before naming a cause.** The overlay bug looked like command
  ordering and was a two-pass renderer; the difference was one grep.
- **`strand view <file> <w> <h>`** prints the laid-out tree with real font
  metrics. Use it constantly. Ask for a screenshot only for colour, blending
  and alignment — and state your prediction first, so the answer is
  diagnostic.
