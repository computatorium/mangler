//! `mangler-js` — the JS obfuscation passes, wired into the pass-graph model.
//!
//! This crate is the connective tissue every JS pass plugs into: it owns the
//! per-file pipeline runner, the per-file config type, the bus artifact vocabulary,
//! the decoupled opaque-predicate library, the AST-fingerprint seed derivation, and
//! the post-codegen anti-tamper finalizer. The individual obfuscation passes
//! (member-access + expr are implemented here as exemplars; the rest are documented
//! stubs in [`passes`]) are [`mangler_passgraph::Pass`]es over the [`mangler_jsast::Js`]
//! language, run against a [`config::FileConfig`].
//!
//! # The flow ([`runner::process`])
//!
//! ```text
//! parse → fingerprint→eff_seed → build FileConfig (reserve idents)
//!       → collect enabled passes (+ resolver & minify pseudo-passes)
//!       → schedule_nodes  (topological sort over declared reads/writes)
//!       → run loop  (Rng::for_pass(eff_seed, id); bus.enter_pass; pass.run)
//!       → codegen (Js::print_optimized, reading MangleControl)
//!       → finalizers (self-coupled key patch; anti-tamper wrap; exact --verify reparse)
//! ```
//!
//! The legacy PreResolver/PostResolver phase split is **not** hardcoded: the
//! resolver is a pseudo-pass that writes
//! [`Resource::resolved_scopes`](mangler_passgraph::Resource::resolved_scopes), so a
//! "post-resolver" pass simply declares it as a read and the scheduler places it
//! after the resolver. See [`runner`] for why the whole flow shares one swc
//! `GLOBALS` scope.
//!
//! # The determinism contract
//!
//! Same source + same `--seed` → byte-identical output. Every pass's randomness is
//! `Rng::for_pass(eff_seed, pass.id())` (independent of pass order); every injected
//! name comes from a single shared [`mangler_core::NameAllocator`] behind a
//! `RefCell` on [`config::FileConfig`] (collision-free file-wide — see its docs for
//! the soundness rationale). The effective seed is the user seed mixed with an
//! AST fingerprint ([`seed`]) so cross-file output is decorrelated without losing
//! reproducibility.
//!
//! # Soundness
//!
//! Top-level / global names are never renamed (swc mangle runs with
//! `top_level = false`) because arbitrary input may reference them externally
//! (eval, inline handlers, sibling files). Behavior is preserved by construction
//! except the opt-in anti-tamper wrap ([`selfdefend`]), which intentionally
//! diverges under debugging / beautification.
//!
//! # Module map
//!
//! * [`runner`] — the per-file pipeline ([`runner::process`], [`runner::register_passes`]).
//! * [`config`] — [`config::FileConfig`]: the per-file `PassConfig` (eff_seed +
//!   shared name allocator + resolved config).
//! * [`artifacts`] — the bus artifact types ([`artifacts::ResolvedScopesArtifact`],
//!   [`artifacts::DecoderAnchorArtifact`], [`artifacts::VmTableArtifact`],
//!   [`artifacts::MangleControlArtifact`], …).
//! * [`opaque`] — the decoupled opaque-predicate library (its own anchor fallback).
//! * [`passes`] — one submodule per pass (member-access + expr; rest documented stubs).
//! * [`seed`] — AST fingerprint → effective seed + source-ident collection.
//! * [`selfdefend`] — the anti-tamper post-codegen finalizer.

pub mod artifacts;
pub mod config;
pub mod opaque;
pub mod passes;
pub mod runner;
pub mod seed;
pub mod selfdefend;

#[cfg(test)]
mod test_support;

// The most common entry points, re-exported for convenience.
pub use config::FileConfig;
pub use runner::{process, register_passes};
