//! Behavioral corpus runner.
//!
//! Enumerates a directory of `.js` corpus files and runs a caller-provided
//! transform closure over each, asserting behavioral equivalence (via the
//! [`crate::eval`] sink-capture path — the corpus convention is an IIFE that
//! assigns its JSON result to `globalThis.__out`).
//!
//! Any pass crate validates itself with one call:
//! ```no_run
//! # fn my_transform(s: &str) -> String { s.to_string() }
//! mangler_testkit::corpus::run_all(|src| my_transform(src)).unwrap();
//! ```
//!
//! The corpus path is configurable. [`default_corpus_dir`] resolves the repo's
//! `tests/corpus/` relative to this crate's manifest, so the runner works from any
//! crate's test without hard-coding an absolute path.

use crate::eval::{eval_same_value_with, CaptureMode, DiffResult};
use std::path::{Path, PathBuf};

/// The repo's default behavioral corpus directory: `<repo>/tests/corpus/`.
///
/// Resolved from this crate's `CARGO_MANIFEST_DIR` (`crates/mangler-testkit`) by
/// walking up two levels to the workspace root. Falls back to a relative path if
/// the manifest dir is unexpectedly shaped.
pub fn default_corpus_dir() -> PathBuf {
    let manifest = env!("CARGO_MANIFEST_DIR"); // .../crates/mangler-testkit
    let mut p = PathBuf::from(manifest);
    // crates/mangler-testkit -> crates -> <repo root>
    p.pop();
    p.pop();
    p.push("tests");
    p.push("corpus");
    p
}

/// One corpus file's differential outcome.
#[derive(Debug)]
pub struct CorpusOutcome {
    /// The corpus file name (e.g. `numeric_edge_cases.js`).
    pub name: String,
    /// The differential result of original-vs-transformed.
    pub diff: DiffResult,
}

/// Enumerate the `.js` files in `dir`, sorted by name for determinism.
pub fn enumerate(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().map(|e| e == "js").unwrap_or(false))
        .collect();
    files.sort();
    Ok(files)
}

/// Run `transform` over every corpus file in `dir`, returning a per-file outcome
/// list. Does NOT panic on divergence — the caller decides (use [`run_all`] /
/// [`assert_all`] for the panicking variants). An I/O error reading the directory
/// is returned as `Err`.
pub fn run_dir<F>(dir: &Path, mut transform: F) -> std::io::Result<Vec<CorpusOutcome>>
where
    F: FnMut(&str) -> String,
{
    let files = enumerate(dir)?;
    let mut outcomes = Vec::with_capacity(files.len());
    let mode = CaptureMode::sink();
    for path in files {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let original = std::fs::read_to_string(&path)?;
        let transformed = transform(&original);
        let diff = eval_same_value_with(&original, &transformed, &mode);
        outcomes.push(CorpusOutcome { name, diff });
    }
    Ok(outcomes)
}

/// Run `transform` over the default corpus dir, asserting behavioral equivalence
/// for every file. Returns the number of files checked. Panics on the first
/// divergence (with the file name, reason, and both outcomes) or on an I/O error —
/// the test-ergonomic entry point.
pub fn assert_all<F>(transform: F) -> usize
where
    F: FnMut(&str) -> String,
{
    assert_all_in(&default_corpus_dir(), transform)
}

/// As [`assert_all`] but over an explicit directory.
pub fn assert_all_in<F>(dir: &Path, transform: F) -> usize
where
    F: FnMut(&str) -> String,
{
    let outcomes = run_dir(dir, transform)
        .unwrap_or_else(|e| panic!("corpus runner I/O error reading {}: {e}", dir.display()));
    assert!(
        !outcomes.is_empty(),
        "corpus dir {} contained no .js files",
        dir.display()
    );
    for o in &outcomes {
        assert!(
            o.diff.equal,
            "corpus file {} diverged: {}\n  original:    {}\n  transformed: {}",
            o.name, o.diff.reason, o.diff.original, o.diff.transformed
        );
    }
    outcomes.len()
}

/// Non-panicking entry point: run `transform` over the default corpus and return
/// `Ok(count)` if all files are equivalent, or `Err(list_of_failures)` describing
/// each divergence. Lets a caller aggregate failures rather than stopping at the
/// first.
#[allow(clippy::result_large_err)]
pub fn run_all<F>(transform: F) -> Result<usize, Vec<CorpusOutcome>>
where
    F: FnMut(&str) -> String,
{
    let outcomes = run_dir(&default_corpus_dir(), transform)
        .unwrap_or_else(|e| panic!("corpus runner I/O error: {e}"));
    let total = outcomes.len();
    let failures: Vec<CorpusOutcome> = outcomes.into_iter().filter(|o| o.diff.is_divergent()).collect();
    if failures.is_empty() {
        Ok(total)
    } else {
        Err(failures)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_corpus_dir_exists_and_is_populated() {
        let dir = default_corpus_dir();
        assert!(dir.is_dir(), "default corpus dir not found: {}", dir.display());
        let files = enumerate(&dir).unwrap();
        assert!(
            files.len() >= 20,
            "expected the ~31-file corpus, found {}",
            files.len()
        );
    }

    #[test]
    fn identity_transform_passes_whole_corpus() {
        // The identity transform must be behaviorally equal to every corpus file —
        // this proves the sink-capture harness evaluates every corpus file cleanly
        // in QuickJS (no DOM/engine gaps tripping it).
        let n = assert_all(|src| src.to_string());
        assert!(n >= 20, "checked only {n} corpus files");
    }

    #[test]
    fn run_all_reports_failures() {
        // A transform that corrupts the sink must be reported as failing for every
        // file (each file's __out changes), via the non-panicking aggregator.
        let res = run_all(|src| format!("{src}\nglobalThis.__out = 'CORRUPTED';"));
        match res {
            Ok(_) => panic!("corrupting transform must not pass the corpus"),
            Err(failures) => assert!(!failures.is_empty()),
        }
    }
}
