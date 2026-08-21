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

usage:
  strand run <file.str>            compile and run `main`
  strand build <file.str> [-o out] compile to a .wasm module
  strand todo                      the todo UI (§7)
  strand view <file.str> [w h]     draw a Strand view; with w h, print its tree
  strand inspect [w h]             print the todo UI tree (§8.4)
  strand ui [--burn]               compositor demo (§6.1)
  strand demo [--window|--trace]   run the M0 actor skeleton
  strand crash [--trace|--window]  supervised crash and restart (§5.4)
  strand help                      show this message
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
    let wasm = strandc::codegen::emit(&hir).map_err(|e| anyhow::anyhow!("{e}"))?;
    match viewport {
        Some(viewport) => {
            print!("{}", strand_cli::view::inspect(&hir, &wasm, viewport)?);
            Ok(())
        }
        None => strand_cli::view::run(&hir, &wasm),
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
