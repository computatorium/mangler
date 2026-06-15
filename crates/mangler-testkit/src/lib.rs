//! `mangler-testkit` — the correctness backbone for the workspace rewrite.
//!
//! A self-contained dev/test library that validates a JS transform by comparing the
//! *behavior* of the original source against the transformed source. It operates on
//! JS **source strings** only — it does NOT depend on the obfuscator crates, so any
//! pass crate (or the legacy `mangler` crate) can plug its `Fn(&str) -> String`
//! transform in to validate itself:
//!
//! ```no_run
//! # fn my_pass(s: &str) -> String { s.to_string() }
//! // Behavioral equivalence over the committed corpus:
//! mangler_testkit::corpus::assert_all(|src| my_pass(src));
//! // Deterministic VM/expression fuzz:
//! mangler_testkit::fuzz::assert_fuzz_transform(600, 0xF0F0_1234, |src| my_pass(src));
//! // Idempotence + size band:
//! mangler_testkit::golden::assert_idempotent(my_pass, "function f(){return 1;}");
//! ```
//!
//! # Modules / public API surface
//! * [`eval`] — `eval_same_value`, `assert_behaviorally_equal`, `CaptureMode`,
//!   `DiffResult`. rquickjs eval-and-compare with `Object.is`/SameValue semantics and
//!   symmetric error handling.
//! * [`corpus`] — `run_all`, `assert_all`, `run_dir`, `enumerate`,
//!   `default_corpus_dir`. Enumerate + run the behavioral corpus through a transform.
//! * [`fuzz`] — `Gen`, `build_program`, `check_program`, `fuzz_transform`,
//!   `assert_fuzz_transform`, and (feature `proptest`) `arith_expr`, `bitwise_expr`,
//!   `proptest_transform`. Deterministic + property-based program fuzzing.
//! * [`guards`] — `assert_expansion_band`, `assert_size_and_timing`,
//!   `time_transform`. Output-size band + soft timing guards.
//! * [`golden`] — `assert_idempotent`, `assert_golden`, `compare_golden`. Golden-file
//!   compare + idempotence.
//! * [`cross_engine`] — (feature `cross-engine`, OFF by default) the documented seam
//!   for a second JS engine differential.
//!
//! # Eval-and-compare semantics
//! Each program runs in its OWN fresh QuickJS runtime. A comparable value is captured
//! either from `globalThis.__out` ([`eval::CaptureMode::Sink`], the corpus
//! convention) or from the trailing-expression completion value
//! ([`eval::CaptureMode::Completion`]). Values are compared under `Object.is`
//! (SameValue): `-0` != `0`, `NaN` == `NaN`, BigInts compared by value, `undefined`/
//! `null`/numbers/strings kept distinct (the harness tags each so a string `"0"`
//! never collides with the number `-0`). Thrown errors are handled symmetrically:
//! both-throw is equal iff the engine-normalized messages match; value-vs-throw is
//! always unequal.
//!
//! # Test gotchas (READ THIS)
//!
//! ## (a) NEVER run bare `node` in tests
//! This project has a HARD rule: tests must never invoke a bare `node` binary. `node`
//! may be absent, the wrong version, or may HANG — turning a deterministic
//! correctness test into a flaky/blocking one. The PRIMARY engine is always the
//! in-process rquickjs ([`eval`]). The optional second engine ([`cross_engine`]) is
//! behind a non-default cargo feature AND requires an *explicit*
//! `MANGLER_TESTKIT_JS_ENGINE` env var — it never implicitly falls back to `node`,
//! and skips (never fails) when unconfigured.
//!
//! ## (b) git worktrees branch from a STALE base
//! When validating a change in a throwaway git worktree, the worktree is created from
//! whatever commit the base ref pointed at when the worktree was made — which may be
//! BEHIND the current branch. Before trusting a green/red result, run
//! `git reset --hard <current-branch>` (or rebase) in the worktree FIRST, or you will
//! be testing stale code and chasing already-fixed/not-yet-present behavior.

pub mod corpus;
pub mod eval;
pub mod fuzz;
pub mod golden;
pub mod guards;

#[cfg(feature = "cross-engine")]
pub mod cross_engine;

// Convenience re-exports for the most common entry points.
pub use eval::{assert_behaviorally_equal, eval_same_value, CaptureMode, DiffResult};
