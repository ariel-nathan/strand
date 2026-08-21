//! Golden-file suite (§10, M1).
//!
//! Each `tests/golden/*.str` is compiled, run, and its rendered result compared
//! against the matching `.expected`. Fixtures are discovered from the
//! directory, so adding a case is adding a file.
//!
//! Regenerate after an intentional change:
//!
//!     STRAND_UPDATE_GOLDEN=1 cargo test -p strand-cli --test golden
//!
//! Regenerating rewrites the expectations wholesale, so read the diff before
//! committing it — a blessed wrong answer is worse than no test.

use std::fs;
use std::path::{Path, PathBuf};

fn golden_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("golden")
}

fn compile_and_run(path: &Path, source: &str) -> Result<String, String> {
    let name = path.file_name().unwrap().to_string_lossy().into_owned();
    let hir = strandc::compile(&name, source)
        .map_err(|report| format!("{:?}", miette::Report::new(report)))?;
    let wasm = strandc::codegen::emit(&hir).map_err(|e| format!("emit failed: {e}"))?;
    if let Err(e) = wasmparser::validate(&wasm) {
        return Err(format!("emitted invalid WASM: {e}"));
    }
    strand_cli::run::run_main(&hir, &wasm).map_err(|e| format!("run failed: {e:#}"))
}

#[test]
fn golden_files_match() {
    let dir = golden_dir();
    let updating = std::env::var_os("STRAND_UPDATE_GOLDEN").is_some();

    let mut sources: Vec<PathBuf> = fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()))
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.extension().is_some_and(|e| e == "str"))
        .collect();
    sources.sort();
    assert!(!sources.is_empty(), "no fixtures in {}", dir.display());

    let mut failures = Vec::new();
    for source_path in &sources {
        let source = fs::read_to_string(source_path).expect("reading fixture");
        let actual = match compile_and_run(source_path, &source) {
            Ok(value) => value,
            Err(message) => message,
        };
        let expected_path = source_path.with_extension("expected");

        if updating {
            fs::write(&expected_path, format!("{actual}\n")).expect("writing golden file");
            continue;
        }

        let Ok(expected) = fs::read_to_string(&expected_path) else {
            failures.push(format!(
                "{}: no .expected file (run with STRAND_UPDATE_GOLDEN=1)\n  actual: {actual}",
                source_path.display()
            ));
            continue;
        };
        if expected.trim_end() != actual.trim_end() {
            failures.push(format!(
                "{}:\n  expected: {}\n  actual:   {}",
                source_path.display(),
                expected.trim_end(),
                actual.trim_end()
            ));
        }
    }

    if updating {
        // Fail loudly so an update run is never mistaken for a passing run.
        panic!("golden files regenerated ({} fixtures); re-run without STRAND_UPDATE_GOLDEN", sources.len());
    }
    assert!(failures.is_empty(), "golden mismatches:\n\n{}", failures.join("\n\n"));
}
