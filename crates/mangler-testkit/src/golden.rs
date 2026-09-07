//! Golden-file compare + idempotence harness.
//!
//! Ported from `tests/golden.rs` / `tests/idempotence.rs` and generalized to a
//! caller-provided transform closure.
//!
//! * [`assert_idempotent`] — `transform(transform(src)) == transform(src)`,
//!   byte-identical. A transform that keeps churning its own output (non-fixpoint)
//!   trips this.
//! * [`assert_golden`] / [`compare_golden`] — compare a transform's output against a
//!   committed golden file. Set `MANGLER_TESTKIT_BLESS=1` (or call with `bless`) to
//!   (re)write the golden instead of asserting — the standard "review the diff, then
//!   bless" workflow.

use std::path::Path;

/// Assert `transform` is idempotent on `src`: applying it twice yields the same
/// bytes as applying it once. Returns the once-transformed output. Panics on a
/// mismatch with a unified-ish diff hint.
pub fn assert_idempotent<F>(mut transform: F, src: &str)
where
    F: FnMut(&str) -> String,
{
    let once = transform(src);
    let twice = transform(&once);
    assert_eq!(
        once,
        twice,
        "transform is not idempotent (transform∘transform != transform)\n\
         --- once ({} bytes) ---\n{}\n--- twice ({} bytes) ---\n{}",
        once.len(),
        once,
        twice.len(),
        twice
    );
}

/// Whether the bless/update-golden mode is active via the
/// `MANGLER_TESTKIT_BLESS` env var (any non-empty value).
pub fn bless_from_env() -> bool {
    std::env::var_os("MANGLER_TESTKIT_BLESS")
        .map(|v| !v.is_empty())
        .unwrap_or(false)
}

/// The result of a golden comparison.
#[derive(Debug, PartialEq, Eq)]
pub enum GoldenResult {
    /// Output matched the committed golden exactly.
    Matched,
    /// `bless` was set; the golden was written.
    Wrote,
    /// Output differed from the golden (with the two byte counts).
    Mismatch {
        golden_len: usize,
        actual_len: usize,
    },
}

/// Compare `actual` against the golden file at `golden_path`. If `bless` is true, write `actual` to the path and return [`GoldenResult::Wrote`].
/// Otherwise return [`GoldenResult::Matched`] or [`GoldenResult::Mismatch`]. Never
/// panics — the caller decides (use [`assert_golden`] for the panicking variant).
pub fn compare_golden(
    golden_path: &Path,
    actual: &str,
    bless: bool,
) -> std::io::Result<GoldenResult> {
    let existing = match std::fs::read_to_string(golden_path) {
        Ok(value) => Some(value),
        Err(error) if bless && error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    match existing {
        Some(golden) if !bless => {
            if golden == actual {
                Ok(GoldenResult::Matched)
            } else {
                Ok(GoldenResult::Mismatch {
                    golden_len: golden.len(),
                    actual_len: actual.len(),
                })
            }
        }
        _ => {
            if let Some(parent) = golden_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(golden_path, actual)?;
            Ok(GoldenResult::Wrote)
        }
    }
}

/// Assert `actual` matches the golden at `golden_path`, honoring the
/// `MANGLER_TESTKIT_BLESS` env var to (re)write it. Panics on mismatch or I/O error.
pub fn assert_golden(golden_path: &Path, actual: &str) {
    let bless = bless_from_env();
    match compare_golden(golden_path, actual, bless) {
        Ok(GoldenResult::Matched) => {}
        Ok(GoldenResult::Wrote) => {
            eprintln!(
                "[golden] wrote {} ({} bytes) — re-run without MANGLER_TESTKIT_BLESS to assert",
                golden_path.display(),
                actual.len()
            );
        }
        Ok(GoldenResult::Mismatch {
            golden_len,
            actual_len,
        }) => {
            panic!(
                "golden mismatch for {}: golden {golden_len} bytes, actual {actual_len} bytes — \
                 review and re-bless with MANGLER_TESTKIT_BLESS=1 if intended\n--- actual ---\n{actual}",
                golden_path.display()
            );
        }
        Err(e) => panic!("golden I/O error for {}: {e}", golden_path.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A genuinely idempotent transform: collapse runs of spaces to one, trim. Twice
    // == once.
    fn collapse_spaces(s: &str) -> String {
        let mut out = String::new();
        let mut prev_space = false;
        for c in s.trim().chars() {
            if c == ' ' {
                if !prev_space {
                    out.push(' ');
                }
                prev_space = true;
            } else {
                out.push(c);
                prev_space = false;
            }
        }
        out
    }

    #[test]
    fn idempotent_transform_passes() {
        assert_idempotent(collapse_spaces, "a   b   c  ");
    }

    #[test]
    #[should_panic(expected = "not idempotent")]
    fn non_idempotent_transform_is_detected() {
        // Appending a marker each run never reaches a fixpoint.
        assert_idempotent(|s| format!("{s}!"), "x");
    }

    #[test]
    fn golden_write_then_match_then_mismatch() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("mangler_testkit_golden_{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&path);

        // A missing baseline fails unless blessing was explicit.
        assert_eq!(
            compare_golden(&path, "hello", false).unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
        assert_eq!(
            compare_golden(&path, "hello", true).unwrap(),
            GoldenResult::Wrote
        );
        // Now matches.
        assert_eq!(
            compare_golden(&path, "hello", false).unwrap(),
            GoldenResult::Matched
        );
        // Different -> Mismatch.
        assert!(matches!(
            compare_golden(&path, "world!", false).unwrap(),
            GoldenResult::Mismatch { .. }
        ));
        // Bless rewrites.
        assert_eq!(
            compare_golden(&path, "world!", true).unwrap(),
            GoldenResult::Wrote
        );
        assert_eq!(
            compare_golden(&path, "world!", false).unwrap(),
            GoldenResult::Matched
        );

        let _ = std::fs::remove_file(&path);
    }
}
