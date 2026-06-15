//! Test-only conveniences shared across pass unit tests.
//!
//! Not part of the public API surface intended for production callers — these wrap
//! [`crate::runner::process`] with preset-based config so a pass test reads as
//! `process_with(src, Intensity::Medium, seed)`.

#![cfg(test)]

use mangler_config::{ConfigFlags, Intensity, ResolvedConfig};
use mangler_jsast::ParseOpts;

/// Resolve a config from a preset level + seed.
pub fn resolved(level: Intensity, seed: u64) -> ResolvedConfig {
    let flags = ConfigFlags {
        preset: Some(level),
        seed: Some(seed),
        ..Default::default()
    };
    ResolvedConfig::try_from(flags).expect("valid preset config")
}

/// Run the full pipeline at `level`/`seed` over `src` (default ES dialect),
/// returning the obfuscated output. Panics on a hard pipeline error.
pub fn process_with(src: &str, level: Intensity, seed: u64) -> String {
    crate::runner::process(src, &ParseOpts::default(), &resolved(level, seed))
        .expect("process failed")
        .0
}
