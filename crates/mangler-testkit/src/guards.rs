//! Expansion-band and throughput/timing guards.
//!
//! Ported from `tests/common/mod.rs` (`expansion_band`, `assert_expansion_band`,
//! `assert_mangle_perf`, `assert_many_strings_decode_throughput`) and generalized
//! to operate on a caller-provided transform closure instead of calling
//! `mangler::process` directly — keeping this crate self-contained.
//!
//! Two guard families:
//! * **Expansion band** — output size must stay within a configurable multiplier of
//!   input size. Catches order-of-magnitude blow-ups (a pass re-wrapping another
//!   pass's opaque constants) deterministically, without flaking on speed.
//! * **Throughput / timing** — a SOFT wall-clock signal. Following the repo's
//!   convention, timing is only hard-enforced in release builds (debug codegen/eval
//!   timing is too noisy to gate); in debug it is logged via `eprintln!`. The HARD
//!   guard is always the deterministic size band and (where applicable) a
//!   correctness checksum.

use std::time::{Duration, Instant};

/// Assert the transform's output stays within `band`× the input size.
///
/// `band` is the maximum allowed `output.len() / input.len()` ratio (input length
/// is floored at 1 to avoid divide-by-zero on empty input). Panics with the tag and
/// the measured ratio on a blow-up. Returns the transformed output so the caller can
/// reuse it.
pub fn assert_expansion_band<F>(src: &str, tag: &str, band: f64, mut transform: F) -> String
where
    F: FnMut(&str) -> String,
{
    let out = transform(src);
    assert!(
        !out.is_empty(),
        "{tag}: transformed output must be non-empty"
    );
    let ratio = out.len() as f64 / src.len().max(1) as f64;
    assert!(
        ratio < band,
        "{tag}: output is {ratio:.1}x input (band {band:.1}x; {} -> {} bytes) — expansion blow-up?",
        src.len(),
        out.len()
    );
    out
}

/// Run `transform` once, returning `(output, elapsed)`. The building block for the
/// throughput guards; lets a caller apply its own timing policy.
pub fn time_transform<F>(src: &str, mut transform: F) -> (String, Duration)
where
    F: FnMut(&str) -> String,
{
    let start = Instant::now();
    let out = transform(src);
    (out, start.elapsed())
}

/// Combined size-band + SOFT timing guard.
///
/// Enforces the expansion `band` (HARD, build-independent) and a wall-clock budget
/// (`max` ) that is HARD only in release builds — in debug the elapsed time is logged
/// and never fails the test (mirroring the repo's `assert_mangle_perf`). Returns the
/// transformed output.
pub fn assert_size_and_timing<F>(
    src: &str,
    tag: &str,
    band: f64,
    max: Duration,
    transform: F,
) -> String
where
    F: FnMut(&str) -> String,
{
    let (out, elapsed) = time_transform(src, transform);
    assert!(
        !out.is_empty(),
        "{tag}: transformed output must be non-empty"
    );
    let ratio = out.len() as f64 / src.len().max(1) as f64;
    assert!(
        ratio < band,
        "{tag}: output is {ratio:.1}x input (band {band:.1}x; {} -> {} bytes) — expansion blow-up?",
        src.len(),
        out.len()
    );
    if cfg!(not(debug_assertions)) {
        assert!(
            elapsed < max,
            "{tag}: transform took {elapsed:?} (release budget {max:?}) — pathological slowdown?"
        );
    } else {
        eprintln!("[perf-info, debug] {tag}: transform took {elapsed:?}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_within_band() {
        let src = "abcdefghij";
        let out = assert_expansion_band(src, "identity", 2.0, |s| s.to_string());
        assert_eq!(out, src);
    }

    #[test]
    #[should_panic(expected = "expansion blow-up")]
    fn blowup_trips_band() {
        let src = "x";
        // 100x expansion must trip a 10x band.
        assert_expansion_band(src, "blowup", 10.0, |s| s.repeat(100));
    }

    #[test]
    fn timing_guard_passes_for_fast_identity() {
        let out = assert_size_and_timing("hello world", "fast", 2.0, Duration::from_secs(5), |s| {
            s.to_string()
        });
        assert_eq!(out, "hello world");
    }

    #[test]
    fn time_transform_measures() {
        let (out, _elapsed) = time_transform("abc", |s| s.to_uppercase());
        assert_eq!(out, "ABC");
    }
}
