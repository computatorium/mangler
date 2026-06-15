//! Differential + determinism integration tests for the `mangler-js` runner.
//!
//! Validates that the pipeline — with the member-access + expr exemplar passes
//! enabled — PRESERVES BEHAVIOR over the committed corpus, is idempotent, and is
//! deterministic (same source + seed → byte-identical). All evaluation goes
//! through `mangler-testkit`'s in-process rquickjs harness; we never invoke a bare
//! `node` (a hard project rule).

use mangler_config::{ConfigFlags, Intensity, ResolvedConfig};
use mangler_jsast::ParseOpts;
use mangler_testkit::{corpus, eval, golden};

/// Resolve a config from a preset + seed.
fn cfg(level: Intensity, seed: u64) -> ResolvedConfig {
    let flags = ConfigFlags {
        preset: Some(level),
        seed: Some(seed),
        ..Default::default()
    };
    ResolvedConfig::try_from(flags).expect("valid config")
}

/// Run the pipeline over `src` at `level`/`seed`. Panics on a hard error (corpus
/// files are valid JS, so a hard error is a real bug worth surfacing loudly).
fn mangle(src: &str, level: Intensity, seed: u64) -> String {
    mangler_js::process(src, &ParseOpts::default(), &cfg(level, seed))
        .expect("pipeline must not hard-error on valid corpus input")
        .0
}

/// The whole behavioral corpus must round-trip unchanged in observable behavior at
/// Medium (member-access + expr obfuscation both active).
#[test]
fn corpus_behavior_preserved_at_medium() {
    let n = corpus::assert_all(|src| mangle(src, Intensity::Medium, 1));
    assert!(n >= 20, "expected the full corpus, checked only {n}");
}

/// Behavior must also be preserved at Minify (just mangle/minify — the baseline)
/// and across a couple of seeds, guarding the determinism-independent equivalence.
#[test]
fn corpus_behavior_preserved_across_levels_and_seeds() {
    for (level, seed) in [
        (Intensity::Minify, 1),
        (Intensity::Medium, 7),
        (Intensity::Medium, 99),
    ] {
        let n = corpus::assert_all(|src| mangle(src, level, seed));
        assert!(n >= 20, "checked only {n} at {level:?}/{seed}");
    }
}

/// Same source + same seed → byte-identical output (the determinism contract).
#[test]
fn determinism_byte_identical() {
    let samples = [
        "function f(a, b){ return a * 3 + b - 7; } f(2, 5);",
        "var o = { k: 1 }; globalThis.__out = String(o.k + 41);",
        "const xs = [1, 2, 3].map(x => x * 2 + 1); globalThis.__out = String(xs.length);",
    ];
    for src in samples {
        let a = mangle(src, Intensity::Medium, 42);
        let b = mangle(src, Intensity::Medium, 42);
        assert_eq!(a, b, "same source+seed must be byte-identical for: {src}");
    }
}

/// Idempotence: mangling the output again yields byte-identical bytes (the
/// transform reaches a fixpoint and does not churn its own output). The effective
/// seed is content-derived, so the second run uses a different effective seed than
/// the first — yet must still be a fixpoint, because the already-obfuscated output
/// has no further eligible literals/members to transform (they are now opaque exprs
/// / computed accesses), so the second pass is observably a near no-op that
/// stabilizes. We assert idempotence on the SECOND application onward.
#[test]
fn idempotent_at_minify() {
    // At Minify only mangle/minify runs (no literal/member rewriting), so the
    // transform is a clean fixpoint.
    let src = "function f(){ var localName = 1; return localName + 2; } globalThis.__out = String(f());";
    golden::assert_idempotent(|s| mangle(s, Intensity::Minify, 1), src);
}

/// A direct behavioral spot-check via `assert_behaviorally_equal` (the eval-sink
/// harness), independent of the corpus runner.
#[test]
fn spot_check_behavioral_equivalence() {
    let src = "globalThis.__out = String((2 + 40) * 3 - 6);";
    let out = mangle(src, Intensity::Medium, 5);
    eval::assert_behaviorally_equal(src, &out);
}
