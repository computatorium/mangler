//! Behavioral and release gates for JavaScript transforms.
//!
//! [`eval`] runs independent QuickJS script realms with explicit strictness,
//! time/stack/memory/job limits, typed primitive results, thrown values, console
//! traces and unhandled promise rejections. It settles microtasks before reading
//! `globalThis.__out`. Unsupported captures and exhausted limits never prove
//! equivalence. Serialize object results explicitly, or record structured effects
//! in `globalThis.__trace`.
//!
//! [`corpus`] and [`fuzz`] apply that observation contract to committed fixtures
//! and reproducible generated programs. [`guards`] measures real transform output
//! size and release throughput; [`golden`] checks explicitly blessed snapshots.
//!
//! [`cross_engine`] executes exact artifacts in explicitly configured Node and
//! Chrome binaries. Local tests can omit those environment variables; CI sets
//! `MANGLER_TESTKIT_REQUIRE_ENGINES=1` so missing engines fail instead of silently
//! skipping coverage. Subprocess deadlines include output draining and cleanup.

mod allocation;
pub mod corpus;
pub mod cross_engine;
pub mod eval;
pub mod fuzz;
pub mod golden;
pub mod guards;

pub use eval::{CaptureMode, DiffResult, assert_behaviorally_equal, eval_same_value};
