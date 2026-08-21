# Strand for VS Code

Language support for Strand (`.str`): live diagnostics, hover types,
go-to-definition, find-references, an outline, and syntax highlighting.

Two halves. Highlighting and editing behaviour are declarative — a TextMate
grammar plus a language configuration, no code involved. Everything else comes
from `strand lsp`, the language server built into the toolchain binary; this
extension only launches it.

## Build

The client is bundled with [bun](https://bun.sh); the server is part of the
normal Cargo build.

```bash
cargo build -p strand-cli      # builds the `strand` binary, which hosts the server
cd editors/vscode
bun install
bun run build                  # bundles src/extension.ts -> out/extension.js
```

## Install

Copy this directory into your VS Code extensions folder and reload the window.
`node_modules/` is not needed at runtime — `bun run build` inlines everything
into `out/extension.js`.

```powershell
# Windows
Copy-Item -Recurse -Force editors\vscode "$env:USERPROFILE\.vscode\extensions\strand-lang"
```

```bash
# macOS / Linux
cp -r editors/vscode ~/.vscode/extensions/strand-lang
```

Then run **Developer: Reload Window** from the command palette. Any `.str` file
will pick up the `strand` language mode automatically.

To develop against it instead, open this folder in VS Code and press <kbd>F5</kbd>
to launch an Extension Development Host.

## Finding the server

The extension runs `strand lsp`. By default it looks for `strand` on your `PATH`;
set `strand.server.path` to point somewhere specific. This repository ships a
workspace setting (`.vscode/settings.json`) pointing at its own `target/debug`
build, so opening the repo works after `cargo build -p strand-cli` with nothing
installed globally. `${workspaceFolder}` is expanded in that setting.

If the server cannot be started you get a notification, and the **Strand Language
Server** output channel holds the details.

## What it does

From the language server:

- **Live diagnostics** as you type, carrying the compiler's own message and its
  suggested fix where §8.2 provides one. Because both the parser and the checker
  recover, a half-typed declaration reports itself without blanking out the rest
  of the file.
- **Hover** showing the inferred type, rendered the way it is written in source
  (`Result<int, AddError>`, not an internal id).
- **Go to definition** (<kbd>F12</kbd>) and **find references**
  (<kbd>Shift</kbd>+<kbd>F12</kbd>) for locals, parameters, functions, types,
  constructors and match bindings.
- **Outline and breadcrumbs** (<kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>O</kbd>),
  with record fields, sum variants and actor members nested under their
  declarations.

From the grammar and language configuration:

- **Highlighting** for the full implemented language: the 16 keywords, `//`
  comments, strings with their five legal escapes, decimal numbers, built-in
  types, the `log` intrinsic, the UI builder vocabulary, constructors, sum-type
  variants, and all operators.
- **Comment toggling** with <kbd>Ctrl</kbd>+<kbd>/</kbd>.
- **Bracket matching**, auto-closing and auto-surrounding for `{}`, `()`, `[]`
  and `"`.
- **Auto-indent** after `{`, outdent on `}`, and `//` continuation on Enter.
- **Error scopes** for text the Strand lexer rejects outright — a lone `&`, an
  unknown string escape, and sigils like `$`, `@`, `#`, `` ` `` and `'`. These
  show up in your theme's "invalid" colour as you type, rather than at build time.

## What it does not do

No completion, no rename, no formatting, and no semantic-token highlighting —
colouring still comes entirely from the TextMate grammar. There is also no
workspace-wide search, because Strand has no module system: one `.str` file is
the whole compilation unit, so every answer is scoped to the open document.

One case is deliberately left alone: the grammar does **not** try to distinguish
a record literal (`Count { total: 1 }`) from a block (`if a > b { a }`). The
parser needs a lookahead flag to do it (`parser.rs:19-22`, `parser.rs:715-721`)
and a regex grammar cannot replicate that, so braces get one neutral scope
rather than a guess that would be wrong half the time.

## Keeping it in sync with the compiler

The grammar is derived from the compiler, not from guesswork. Two places matter:

| Grammar rule | Source of truth |
|---|---|
| Keywords, comments, strings, numbers, operators | `crates/strandc/src/lexer.rs` |
| Built-in types, the `log` builtin | `crates/strandc/src/check.rs` |
| The 11 widget names | `crates/strandc/src/ui.rs:181` (the `BUILDERS` table) |

The lexer has been stable since the `M1: lexer` commit, so the token rules should
rarely need touching. The widget list is the moving part — `ui.rs` says adding a
widget is "adding a row here", so when that table grows, add the new name to the
`widget` rule in `syntaxes/strand.tmLanguage.json`.

## Testing

`test/highlight-fixture.str` is an annotated corpus of the cases the real
examples do not reach — the `1.max(2)` dot rule, `view fn` declarations,
`string??`, shadowed contextual names, and every lexer-rejected form. Each case
carries a comment stating what it should colour as.

**It is intentionally not compilable**; the last section exists purely to exercise
the `invalid.illegal` scopes. Now that the language server diagnoses every open
`.str` file, opening it lights up the Problems panel — that is expected, and the
file's own header says so.

To check a specific token, put the cursor on it and run **Developer: Inspect
Editor Tokens and Scopes**.

`examples/strand/lsp_demo.str` is the counterpart for the server: four
deliberate mistakes, one per diagnostic worth seeing, with correct functions
around them so you can watch recovery keep those working.

The server's own behaviour is tested in Rust, without a client or a connection:

```bash
cargo test -p strand-lsp      # source -> diagnostics/hover/definition/symbols
cargo test -p strandc         # recovery, the line index, and the analysis tables
```
