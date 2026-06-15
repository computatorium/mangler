//! Config plumbing for the binary: merging the optional `--config` TOML file
//! under the CLI flags, then validating into a [`ResolvedConfig`].
//!
//! All transform-relevant validation (cross-flag invariants) lives in
//! `mangler-config`; this module only does the *merge* (file is the default
//! layer, an explicit CLI flag wins) and the `try_into` call. It also offers a
//! small [`builder`] for library/test use that never touches the file system.

use mangler_config::{ConfigFlags, ResolvedConfig};

/// Read and merge a `--config` TOML file under the CLI `flags` (an explicit flag
/// beats the file), then validate into a [`ResolvedConfig`].
///
/// Precedence: explicit CLI flag → `--config` file value → preset baseline
/// (applied inside `mangler-config`). Unknown TOML keys are a hard error
/// (`ConfigFlags` derives `deny_unknown_fields`).
pub fn resolve(
    flags: ConfigFlags,
    config_file: Option<&std::path::Path>,
) -> anyhow::Result<ResolvedConfig> {
    let merged = match config_file {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .map_err(|e| anyhow::anyhow!("read config {}: {e}", path.display()))?;
            let file: ConfigFlags = toml::from_str(&text)
                .map_err(|e| anyhow::anyhow!("parse config {}: {e}", path.display()))?;
            merge(flags, file)
        }
        None => flags,
    };
    merged
        .try_into()
        .map_err(|e: mangler_config::ConfigError| anyhow::anyhow!("{e}"))
}

/// Overlay `cli` over `file`: each field present (`Some`/`true`) on the CLI wins,
/// otherwise the file's value is the default. Mirrors the legacy
/// "explicit flag > config file" precedence, field by field.
fn merge(cli: ConfigFlags, file: ConfigFlags) -> ConfigFlags {
    ConfigFlags {
        preset: cli.preset.or(file.preset),
        lang: cli.lang.or(file.lang),
        seed: cli.seed.or(file.seed),
        verify: cli.verify || file.verify,
        mangle: cli.mangle.or(file.mangle),
        strings: cli.strings.or(file.strings),
        control_flow: cli.control_flow.or(file.control_flow),
        dead_code: cli.dead_code.or(file.dead_code),
        self_defending: cli.self_defending.or(file.self_defending),
        debug_protection: cli.debug_protection.or(file.debug_protection),
        virtualize: cli.virtualize.or(file.virtualize),
        global_indirect: cli.global_indirect.or(file.global_indirect),
        identifier_naming: cli.identifier_naming.or(file.identifier_naming),
        harden_global_anchor: cli.harden_global_anchor || file.harden_global_anchor,
        keep_names: cli.keep_names.or(file.keep_names),
        key_source: cli.key_source.or(file.key_source),
        key_expected: cli.key_expected.or(file.key_expected),
        domain_lock: cli.domain_lock.or(file.domain_lock),
        remote_key: cli.remote_key.or(file.remote_key),
        strings_in_vm: cli.strings_in_vm.or(file.strings_in_vm),
        self_coupled_key: cli.self_coupled_key.or(file.self_coupled_key),
        exec_trace_key: cli.exec_trace_key.or(file.exec_trace_key),
    }
}

/// A small fluent builder over [`ConfigFlags`] for embedders/tests, so library
/// callers construct a [`ResolvedConfig`] without hand-mutating structs or
/// touching the file system.
///
/// ```
/// use mangler_cli::config::builder;
/// use mangler_config::Intensity;
/// let cfg = builder().preset(Intensity::High).seed(42).build().unwrap();
/// assert_eq!(cfg.engine.seed, 42);
/// ```
pub fn builder() -> Builder {
    Builder::default()
}

/// Fluent [`ConfigFlags`] builder. Call [`Builder::build`] to validate into a
/// [`ResolvedConfig`].
#[derive(Debug, Default, Clone)]
pub struct Builder {
    flags: ConfigFlags,
}

impl Builder {
    /// Start from a fully-formed [`ConfigFlags`] (e.g. parsed argv).
    pub fn from_flags(flags: ConfigFlags) -> Self {
        Builder { flags }
    }

    /// Set the intensity preset.
    pub fn preset(mut self, level: mangler_config::Intensity) -> Self {
        self.flags.preset = Some(level);
        self
    }

    /// Set the fixed RNG seed (for deterministic output).
    pub fn seed(mut self, seed: u64) -> Self {
        self.flags.seed = Some(seed);
        self
    }

    /// Force the input language.
    pub fn lang(mut self, lang: mangler_config::Lang) -> Self {
        self.flags.lang = Some(lang);
        self
    }

    /// Set the string-obfuscation mode.
    pub fn strings(mut self, mode: mangler_config::StringMode) -> Self {
        self.flags.strings = Some(mode);
        self
    }

    /// Enable post-mangle re-parse verification.
    pub fn verify(mut self, on: bool) -> Self {
        self.flags.verify = on;
        self
    }

    /// The mutable flags, for any knob not covered by a named setter.
    pub fn flags_mut(&mut self) -> &mut ConfigFlags {
        &mut self.flags
    }

    /// Validate the accumulated flags into a [`ResolvedConfig`].
    pub fn build(self) -> anyhow::Result<ResolvedConfig> {
        self.flags
            .try_into()
            .map_err(|e: mangler_config::ConfigError| anyhow::anyhow!("{e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mangler_config::{Intensity, StringMode};

    #[test]
    fn builder_round_trips_seed_and_preset() {
        let cfg = builder().preset(Intensity::Max).seed(7).build().unwrap();
        assert_eq!(cfg.engine.seed, 7);
        assert_eq!(cfg.engine.level, Intensity::Max);
    }

    #[test]
    fn merge_cli_beats_file() {
        let cli = ConfigFlags { seed: Some(42), ..Default::default() };
        let file = ConfigFlags {
            seed: Some(7),
            preset: Some(Intensity::Max),
            strings: Some(StringMode::Encrypt),
            ..Default::default()
        };
        let merged = merge(cli, file);
        assert_eq!(merged.seed, Some(42), "CLI seed wins");
        assert_eq!(merged.preset, Some(Intensity::Max), "file preset used");
        assert_eq!(merged.strings, Some(StringMode::Encrypt));
    }

    #[test]
    fn resolve_with_toml_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("m.toml");
        std::fs::write(&p, "preset = \"max\"\nseed = 9\n").unwrap();
        let cfg = resolve(ConfigFlags::default(), Some(&p)).unwrap();
        assert_eq!(cfg.engine.level, Intensity::Max);
        assert_eq!(cfg.engine.seed, 9);
    }

    #[test]
    fn resolve_rejects_unknown_toml_key() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("bad.toml");
        std::fs::write(&p, "predet = \"max\"\n").unwrap();
        assert!(resolve(ConfigFlags::default(), Some(&p)).is_err());
    }

    #[test]
    fn invalid_combination_is_rejected() {
        // --strings-in-vm requires string obfuscation; minify has none.
        let flags = ConfigFlags {
            preset: Some(Intensity::Minify),
            strings_in_vm: Some(true),
            ..Default::default()
        };
        assert!(resolve(flags, None).is_err());
    }
}
