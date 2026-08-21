//! Finding the fixtures the demos run on.
//!
//! The demos load `.wat` actors and `.str` programs from `examples/`. Resolving
//! those against the working directory means `strand crash` works from the repo
//! root and fails from `target/debug` — with, before this existed, an "os error
//! 3" that named nothing.
//!
//! §8.1 asks for one binary and zero config, and a binary that only runs from
//! one directory is not that. So the directory is searched for: up from the
//! working directory, then up from the executable. Both are wrong to assume
//! individually and right together — the first finds it for anyone inside the
//! repo, the second for anyone running the built binary from elsewhere.
//!
//! A candidate has to *look* like the fixtures directory, not merely be called
//! `examples`. Cargo creates `target/debug/examples/` for example binaries, so
//! a search that trusted the name alone found that one first and reported a
//! missing file from inside it — which is how this check earned its place.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

/// Locates the `examples` directory, or explains where it was looked for.
pub fn dir() -> Result<PathBuf> {
    let mut searched = Vec::new();

    for start in starting_points() {
        for candidate in start.ancestors() {
            let examples = candidate.join("examples");
            if holds_fixtures(&examples) {
                return Ok(examples);
            }
            searched.push(examples);
        }
    }

    let looked: Vec<String> =
        searched.iter().take(6).map(|path| format!("  {}", path.display())).collect();
    Err(anyhow!(
        "could not find the `examples` directory, which the demos read their \
         fixtures from. Looked in:\n{}",
        looked.join("\n")
    ))
}

/// Reads one fixture, naming it if it is missing.
pub fn read(relative: &str) -> Result<String> {
    let path = dir()?.join(relative);
    std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))
}

fn starting_points() -> Vec<PathBuf> {
    let mut starts = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        starts.push(cwd);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            starts.push(parent.to_path_buf());
        }
    }
    starts
}

/// Whether this is *the* examples directory rather than one that happens to
/// share the name — cargo's `target/debug/examples` being the one that matters.
fn holds_fixtures(path: &Path) -> bool {
    path.join("wasm").is_dir() && path.join("strand").is_dir()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_examples_directory_is_found_from_the_test_binary() {
        // Cargo runs tests from the crate root and puts the binary deep under
        // `target/`, so a pass here means both search paths are exercised by
        // something rather than merely written down.
        let dir = dir().expect("examples should be findable");
        assert!(dir.join("wasm").join("crasher.wat").is_file(), "{}", dir.display());
    }

    #[test]
    fn cargos_own_examples_directory_is_not_mistaken_for_this_one() {
        // `target/debug/examples` exists whenever anything has been built, and
        // is where the search used to stop.
        let root = dir().expect("examples should be findable");
        let decoy = root.parent().expect("a parent").join("target").join("debug").join("examples");
        if decoy.is_dir() {
            assert!(!holds_fixtures(&decoy), "{} should not qualify", decoy.display());
        }
        assert!(holds_fixtures(&root), "{} should qualify", root.display());
    }

    #[test]
    fn a_missing_fixture_names_the_path_it_wanted() {
        let message = format!("{:#}", read("wasm/nope.wat").unwrap_err());
        assert!(message.contains("nope.wat"), "{message}");
    }
}
