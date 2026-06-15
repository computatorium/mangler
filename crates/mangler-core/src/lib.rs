//! `mangler-core` — the dependency-free foundation crate.
//!
//! This crate owns the pieces every downstream crate (pass graph, language
//! front-ends, the VM, config) builds on, and nothing else. It pulls in no
//! front-end (no swc, no CSS/HTML parser): it is pure foundation.
//!
//! # The determinism contract
//!
//! `mangler`'s defining guarantee is **same seed ⇒ byte-identical output**.
//! Every source of randomness in the pipeline is seeded; nothing ever consults
//! the OS, `thread_rng`, `Date.now()`, or `Math.random()` on a deterministic
//! path. This crate is where that contract is defined and enforced:
//!
//! * [`rng`] — per-pass derived RNG. A pass's randomness is a pure function of
//!   `(seed, pass_id)`, independent of any other pass's draw order. This is the
//!   fix for the original shared-RNG order coupling.
//! * [`names`] — collision-free, deterministic, non-sequential identifier
//!   allocation.
//! * [`hash`] — the single canonical home for the deterministic hash primitives
//!   (FNV-1a 64, golden-ratio mixer, DJB2), some of which are re-emitted as JS
//!   and so have load-bearing integer semantics.
//!
//! # The rest of the seam
//!
//! * [`error`] — the one [`Error`](enum@Error)/[`Result`] propagation path, plus
//!   the separate non-error [`Note`]/[`Notes`] channel.
//! * [`language`] — the generic [`Language`] trait the pass graph is generic
//!   over (swc-free; the JS impl lands later).
//! * [`config`] — a [`PassConfig`] *stub* so the pass graph can reference its
//!   shape ahead of the full config model.

#![warn(missing_docs)]

pub mod config;
pub mod error;
pub mod hash;
pub mod language;
pub mod names;
pub mod rng;

pub use config::PassConfig;
pub use error::{Error, Note, Notes, Result, Span};
pub use hash::{Djb2, Fnv64};
pub use language::Language;
pub use names::NameAllocator;
pub use rng::Rng;
