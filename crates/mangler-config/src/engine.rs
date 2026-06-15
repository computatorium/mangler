//! [`EngineConfig`] — the *intent* layer, and [`ResolvedConfig`] — intent plus
//! the assembled per-pass tuning.
//!
//! `EngineConfig` answers "what do we obfuscate, and at what level" — preset,
//! seed, language, opt-in feature toggles, verify, keep-names. It deliberately
//! holds NO per-pass tuning knob (those live on [`crate::pass`] types). The
//! intensity it carries is what every pass folds against.
//!
//! Validation ([`crate::validate`]) produces a [`ResolvedConfig`] = an
//! `EngineConfig` next to a fully-assembled [`PassConfigs`]; that pair is the
//! single thing the pipeline consumes.

use crate::enums::{Intensity, Lang};
use crate::pass::PassConfigs;

/// The validated *intent* of an obfuscation run. No per-pass tuning knobs leak
/// in here — only what to run and the run-wide invariants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineConfig {
    /// Named intensity level the passes fold against.
    pub level: Intensity,
    /// Fixed RNG seed — plumbed through unchanged for determinism.
    pub seed: u64,
    /// Forced input language (`--lang`); `None` = auto-detect by extension.
    pub lang: Option<Lang>,
    /// Re-parse the output and assert validity after mangling (`--verify`).
    pub verify: bool,
    /// Identifier-name globs preserved from renaming across the whole run.
    /// (The mangle pass also receives these; this is the run-wide reserved set.)
    pub keep_names: Vec<String>,
}

/// Validated configuration the pipeline consumes: intent + assembled per-pass
/// tuning. Producing one is the only way to obtain assembled `PassConfigs`, so
/// every cross-flag invariant has necessarily been checked (see
/// [`crate::validate`]).
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedConfig {
    /// What to run and the run-wide invariants.
    pub engine: EngineConfig,
    /// How each pass behaves.
    pub passes: PassConfigs,
}

impl ResolvedConfig {
    /// Derive the configuration for an embedded fragment (inline `on*=` handler
    /// or `style=`): the same intent, but with fragment-unsafe passes stripped
    /// from the tuning (see [`PassConfigs::for_fragment`]).
    pub fn for_fragment(&self) -> Self {
        ResolvedConfig {
            engine: self.engine.clone(),
            passes: self.passes.for_fragment(),
        }
    }
}
