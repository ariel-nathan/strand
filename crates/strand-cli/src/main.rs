//! The `strand` binary (§8.1): one tool, no config files.
//!
//!     strand run <file.str>      compile and run `main`
//!     strand build <file.str>    write a .wasm module
//!     strand demo [--window]     the M0 actor skeleton

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};

const USAGE: &str = "\
strand — the Strand toolchain

compiling and running Strand:
  strand run <file.str>            compile and run `main`
  strand build <file.str> [-o out] compile to a .wasm module
  strand view <file.str>           draw a Strand view in a window (§6.2);
                                   an actor with a `view fn` is interactive
  strand view <file.str> <w> <h>   print its laid-out tree instead

windows to look at:
  strand todo                      the todo app (§7) — type, scroll, delete
  strand crash --window            watch an actor die and restart (§5.4)
  strand demo --window             the M0 actor skeleton
  strand ui [--burn]               a busy actor cannot jank the compositor

editor support:
  strand lsp                       speak LSP on stdin/stdout (§8.4)

in the terminal:
  strand inspect [w h]             print the todo UI tree (§8.4)
  strand demo [--trace]            actors; --trace prints the causal log
  strand crash [--trace]           supervision, as a transcript
  strand help                      show this message

Press F12 in any window for the inspector overlay (§8.4).
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();

    let result = match refs.as_slice() {
        [] | ["help"] | ["--help"] | ["-h"] => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        ["run", file] => run(Path::new(file)),
        ["build", file] => build(Path::new(file), None),
        ["build", file, "-o", out] => build(Path::new(file), Some(Path::new(out))),
        // `--stdio` is what LSP clients conventionally pass to say which
        // transport they want; stdio is the only one offered, so it is accepted
        // and ignored rather than being an error.
        ["lsp"] | ["lsp", "--stdio"] => lsp(),
        ["demo"] => strand_cli::demo::run(false),
        ["demo", "--window"] => strand_cli::demo::run(true),
        ["demo", "--trace"] => strand_cli::demo::run_with(false, true),
        ["todo"] => strand_cli::todo::run(),
        ["view", file] => view(Path::new(file), None),
        ["view", file, w, h] => match (w.parse::<f32>(), h.parse::<f32>()) {
            (Ok(w), Ok(h)) => view(Path::new(file), Some((w, h))),
            _ => Err(anyhow::anyhow!("view takes a width and height in pixels")),
        },
        ["inspect"] => {
            print!("{}", strand_cli::todo::inspect((800.0, 628.0)));
            return ExitCode::SUCCESS;
        }
        ["inspect", w, h] => match (w.parse::<f32>(), h.parse::<f32>()) {
            (Ok(w), Ok(h)) => {
                print!("{}", strand_cli::todo::inspect((w, h)));
                return ExitCode::SUCCESS;
            }
            _ => Err(anyhow::anyhow!("inspect takes a width and height in pixels")),
        },
        ["ui"] => strand_cli::demo::ui(false),
        ["ui", "--burn"] => strand_cli::demo::ui(true),
        ["crash"] => strand_cli::demo::crash(false),
        ["crash", "--trace"] => strand_cli::demo::crash(true),
        ["crash", "--window"] => strand_cli::demo::crash_windowed(),
        _ => {
            eprint!("{USAGE}");
            return ExitCode::FAILURE;
        }
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// Compiles to the typed IR, rendering diagnostics (§8.2) if it fails.
fn front_end(path: &Path) -> Result<(strandc::hir::Hir, String)> {
    let source = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;
    let name = path.display().to_string();

    match strandc::compile(&name, &source) {
        Ok(hir) => Ok((hir, source)),
        Err(report) => {
            // Printed rather than returned: miette renders the full report,
            // and anyhow would flatten it to one line.
            eprintln!("{:?}", miette::Report::new(report));
            Err(anyhow::anyhow!("could not compile {}", path.display()))
        }
    }
}

/// Speaks the Language Server Protocol on stdin/stdout until the editor
/// disconnects (§8.4). Editors launch a server this way, and keeping it a
/// subcommand keeps §8.1's one binary.
fn lsp() -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;
    rt.block_on(strand_lsp::serve());
    Ok(())
}

fn run(path: &Path) -> Result<()> {
    let (hir, _) = front_end(path)?;
    let wasm = strandc::codegen::emit(&hir).map_err(|e| anyhow::anyhow!("{e}"))?;
    let value = strand_cli::run::run_main(&hir, &wasm)?;
    println!("{value}");
    Ok(())
}

/// Runs a `view fn` written in Strand (§6.2). With a viewport it prints the
/// laid-out tree instead of opening a window, which is how a view is checked
/// without a screen.
fn view(path: &Path, viewport: Option<(f32, f32)>) -> Result<()> {
    let (hir, _) = front_end(path)?;
    match viewport {
        Some(viewport) => {
            print!("{}", strand_cli::view::inspect(&hir, viewport)?);
            Ok(())
        }
        None => strand_cli::view::run(&hir),
    }
}

fn build(path: &Path, out: Option<&Path>) -> Result<()> {
    let (hir, _) = front_end(path)?;
    let wasm = strandc::codegen::emit(&hir).map_err(|e| anyhow::anyhow!("{e}"))?;

    let destination: PathBuf = match out {
        Some(out) => out.to_path_buf(),
        None => path.with_extension("wasm"),
    };
    std::fs::write(&destination, &wasm)
        .with_context(|| format!("writing {}", destination.display()))?;
    println!("wrote {} ({} bytes)", destination.display(), wasm.len());
    Ok(())
}
