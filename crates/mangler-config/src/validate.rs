//! `TryFrom<ConfigFlags>` — turning the raw, every-combination-representable
//! flag surface into a validated [`ResolvedConfig`] (intent + assembled passes).
//!
//! This is the ONE place cross-flag constraints live. After this conversion the
//! illegal states are gone: the validated type has no way to express
//! "self-coupled key without the VM" or "two key sources at once", so passes
//! downstream never re-check. The entry point WP9's CLI calls is
//! `ResolvedConfig::try_from(flags)` (equivalently `flags.try_into()`).
//!
//! ## Validated cross-flag constraints
//! 1. `--strings-in-vm` requires string obfuscation (`strings != none`).
//! 2. `--self-coupled-key` requires `--strings-in-vm`.
//! 3. `--exec-trace-key` requires `--strings-in-vm`.
//! 4. The key-source forms are mutually exclusive: `--remote-key`,
//!    `--domain-lock`, and the explicit `--key-source`/`--key-expected` pair —
//!    at most one.
//! 5. `--key-source` and `--remote-key` each require `--key-expected`; and
//!    `--key-expected` requires a source.
//!
//! Determinism: `seed` is copied through unchanged; an absent seed becomes a
//! random one *here* (the single point a non-deterministic default is injected).

use crate::engine::{EngineConfig, ResolvedConfig};
use crate::enums::{Intensity, StringMode};
use crate::error::ConfigError;
use crate::flags::ConfigFlags;
use crate::pass::{DynamicKey, PassConfigs};

/// The conventional session-token slot `--remote-key` defaults to when given
/// bare. Mirrors the clap `default_missing_value`.
const DEFAULT_SESSION_SLOT: &str = "globalThis.__MANGLER_SESSION_KEY";

impl TryFrom<ConfigFlags> for ResolvedConfig {
    type Error = ConfigError;

    /// Validate raw flags into a [`ResolvedConfig`]. Precedence is already
    /// resolved upstream (WP9 merges CLI over file over preset into one
    /// `ConfigFlags`); this consumes the merged result. An absent `seed` is
    /// filled with `rand::random()` so the value plumbs through deterministically
    /// from here on.
    fn try_from(flags: ConfigFlags) -> Result<Self, Self::Error> {
        let fallback = rand::random::<u64>();
        Self::from_flags_with_seed(flags, fallback)
    }
}

impl ResolvedConfig {
    /// Validate, using `flags.seed` when present and `fallback_seed` otherwise.
    /// `TryFrom` passes a freshly-randomized fallback; tests pass a fixed one so
    /// they are deterministic without touching the global RNG. Either way the
    /// chosen seed plumbs through to [`EngineConfig::seed`] unchanged.
    pub fn from_flags_with_seed(
        flags: ConfigFlags,
        fallback_seed: u64,
    ) -> Result<Self, ConfigError> {
        let seed = flags.seed.unwrap_or(fallback_seed);
        let level = flags.preset.unwrap_or(Intensity::High);

        // Fold the preset, then layer the flag overrides onto the per-pass tuning.
        let mut passes = PassConfigs::for_preset(level);

        // -- mangle pass --
        if let Some(v) = flags.mangle {
            passes.mangle.enabled = v;
        }
        if let Some(n) = flags.identifier_naming {
            passes.mangle.naming = n;
        }
        let keep_names = flags.keep_names.map(|k| k.into_vec()).unwrap_or_default();
        passes.mangle.keep_names = keep_names.clone();

        // -- strings pass --
        if let Some(m) = flags.strings {
            passes.strings.mode = m;
        }

        // -- control-flow / dead-code pass --
        if let Some(v) = flags.control_flow {
            passes.cf_flatten.enabled = v;
        }
        if let Some(r) = flags.dead_code {
            passes.cf_flatten.dead_code_rate = r;
        }

        // -- anti-tamper pass --
        if let Some(v) = flags.self_defending {
            passes.anti_tamper.self_defending = v;
        }
        if let Some(v) = flags.debug_protection {
            passes.anti_tamper.debug_protection = v;
        }

        // -- global indirection --
        if let Some(g) = flags.global_indirect {
            passes.global_indirect.mode = g;
        }
        if flags.harden_global_anchor {
            passes.global_indirect.harden_anchor = true;
        }

        // -- virtualization --
        if let Some(g) = flags.virtualize {
            passes.virtualize.target = Some(g);
        }
        if let Some(g) = flags.require_virtualized {
            if passes.virtualize.target.is_none() {
                passes.virtualize.target = Some(g.clone());
            }
            passes.virtualize.required = Some(g);
        }
        if flags.virtualize_program {
            passes.virtualize.whole_program = true;
        }
        if let Some(e) = flags.virtualize_exclude {
            passes.virtualize.exclude = Some(e);
        }
        if flags.virtualize_desugar_class {
            passes.virtualize.desugar_class = true;
        }
        if flags.virtualize_desugar_regex {
            passes.virtualize.desugar_regex = true;
        }
        for (flag, pattern) in [
            ("--virtualize", &passes.virtualize.target),
            ("--require-virtualized", &passes.virtualize.required),
            ("--virtualize-exclude", &passes.virtualize.exclude),
        ] {
            if let Some(pattern) = pattern {
                glob::Pattern::new(pattern).map_err(|error| ConfigError::InvalidGlob {
                    flag,
                    value: pattern.clone(),
                    reason: error.to_string(),
                })?;
            }
        }
        // §1 matrix: whole-program wins; `target` is meaningless alongside it. We do
        // not reject the combo (it is harmless — the pass ignores `target` when
        // `whole_program` is set), we just clear it so the resolved config records the
        // effective behavior unambiguously.
        if passes.virtualize.whole_program {
            passes.virtualize.target = None;
        }

        // -- runtime-bound decode key (mutually-exclusive forms) --
        passes.strings.dynamic_key = resolve_dynamic_key(
            flags.domain_lock,
            flags.key_source,
            flags.key_expected,
            flags.remote_key,
        )?;

        // -- in-VM string hardening + its dependencies --
        if let Some(v) = flags.strings_in_vm {
            passes.strings.in_vm = v;
        }
        if passes.strings.in_vm && passes.strings.mode == StringMode::None {
            // (1) --strings-in-vm requires string obfuscation.
            return Err(ConfigError::MissingDependency {
                flag: "--strings-in-vm",
                requires: "string obfuscation (set --strings to encode/encrypt, not none)",
            });
        }
        if let Some(v) = flags.self_coupled_key {
            passes.strings.self_coupled_key = v;
        }
        if passes.strings.self_coupled_key && !passes.strings.in_vm {
            // (2) --self-coupled-key requires --strings-in-vm.
            return Err(ConfigError::MissingDependency {
                flag: "--self-coupled-key",
                requires: "--strings-in-vm (the self-hash binding lives in the VM decode path)",
            });
        }
        if let Some(v) = flags.exec_trace_key {
            passes.strings.exec_trace_key = v;
        }
        if passes.strings.exec_trace_key && !passes.strings.in_vm {
            // (3) --exec-trace-key requires --strings-in-vm.
            return Err(ConfigError::MissingDependency {
                flag: "--exec-trace-key",
                requires: "--strings-in-vm (the execution-trace accumulator is VM bytecode)",
            });
        }

        let engine = EngineConfig {
            level,
            seed,
            lang: flags.lang,
            verify: flags.verify,
            keep_names,
        };
        Ok(ResolvedConfig { engine, passes })
    }
}

/// Resolve the runtime-bound decode key from the three mutually-exclusive
/// sugar/explicit forms. Constraints (4) and (5) live here.
fn resolve_dynamic_key(
    domain_lock: Option<String>,
    key_source: Option<String>,
    key_expected: Option<String>,
    remote_key: Option<String>,
) -> Result<Option<DynamicKey>, ConfigError> {
    // (4) --remote-key is mutually exclusive with the other key forms.
    if remote_key.is_some() && (domain_lock.is_some() || key_source.is_some()) {
        return Err(ConfigError::MutuallyExclusive {
            a: "--remote-key",
            b: "--domain-lock / --key-source",
        });
    }
    if let Some(src_expr) = remote_key {
        // Bare --remote-key (clap default_missing_value) already supplies the slot.
        let source_expr = if src_expr.is_empty() {
            DEFAULT_SESSION_SLOT.to_string()
        } else {
            src_expr
        };
        return match key_expected {
            // (5)
            Some(tok) => Ok(Some(DynamicKey {
                source_expr,
                expected: tok,
            })),
            None => Err(ConfigError::MissingDependency {
                flag: "--remote-key",
                requires: "--key-expected (the per-session token baked into the ciphertext)",
            }),
        };
    }

    // (4) --domain-lock is mutually exclusive with the explicit pair.
    if domain_lock.is_some() && (key_source.is_some() || key_expected.is_some()) {
        return Err(ConfigError::MutuallyExclusive {
            a: "--domain-lock",
            b: "--key-source / --key-expected",
        });
    }
    if let Some(host) = domain_lock {
        return Ok(Some(DynamicKey {
            source_expr: "location.hostname".to_string(),
            expected: host,
        }));
    }

    // (5) the explicit pair must be complete.
    match (key_source, key_expected) {
        (Some(src), Some(exp)) => Ok(Some(DynamicKey {
            source_expr: src,
            expected: exp,
        })),
        (Some(_), None) => Err(ConfigError::MissingDependency {
            flag: "--key-source",
            requires: "--key-expected (the value to bake into the ciphertext)",
        }),
        (None, Some(_)) => Err(ConfigError::MissingDependency {
            flag: "--key-expected",
            requires: "--key-source (the runtime expression to evaluate)",
        }),
        (None, None) => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::enums::{GlobalIndirect, IdNaming};
    use crate::flags::{ConfigFlags, KeepNames};

    /// Build flags from a TOML snippet (the file surface) for terse tests.
    fn flags(toml_src: &str) -> ConfigFlags {
        toml::from_str(toml_src).unwrap()
    }

    fn resolve(toml_src: &str) -> Result<ResolvedConfig, ConfigError> {
        ResolvedConfig::from_flags_with_seed(flags(toml_src), 7)
    }

    #[test]
    fn defaults_to_high_preset() {
        let r = resolve("").unwrap();
        assert_eq!(r.engine.level, Intensity::High);
        assert_eq!(r.passes.strings.mode, StringMode::Encrypt);
        assert_eq!(r.passes.mangle.naming, IdNaming::Hex);
    }

    #[test]
    fn seed_plumbs_through_unchanged() {
        let r = ResolvedConfig::from_flags_with_seed(flags("seed = 99"), 12345).unwrap();
        // An explicit flag seed wins over the fallback seed argument.
        assert_eq!(r.engine.seed, 99);
        // Absent seed → the provided fallback is used unchanged.
        let r2 = ResolvedConfig::from_flags_with_seed(flags(""), 42).unwrap();
        assert_eq!(r2.engine.seed, 42);
    }

    #[test]
    fn flag_overrides_beat_preset() {
        let r =
            resolve("preset = \"high\"\nmangle = false\nglobal_indirect = \"aggressive\"").unwrap();
        assert_eq!(r.engine.level, Intensity::High);
        assert!(!r.passes.mangle.enabled);
        assert_eq!(r.passes.global_indirect.mode, GlobalIndirect::Aggressive);
    }

    #[test]
    fn keep_names_land_in_engine_and_mangle() {
        let f = ConfigFlags {
            keep_names: Some(KeepNames(vec!["myExport".into(), "init*".into()])),
            ..Default::default()
        };
        let r = ResolvedConfig::from_flags_with_seed(f, 0).unwrap();
        assert_eq!(r.engine.keep_names, vec!["myExport", "init*"]);
        assert_eq!(r.passes.mangle.keep_names, vec!["myExport", "init*"]);
    }

    #[test]
    fn whole_program_sets_flag_and_clears_target() {
        // `virtualize_program` true clears any `target` (whole-program wins, §1).
        let r = resolve("virtualize = \"hot*\"\nvirtualize_program = true").unwrap();
        assert!(r.passes.virtualize.whole_program);
        assert!(
            r.passes.virtualize.target.is_none(),
            "target ignored under whole_program"
        );
        // Plain `target` (no whole_program) is unchanged.
        let r2 = resolve("virtualize = \"hot*\"").unwrap();
        assert!(!r2.passes.virtualize.whole_program);
        assert_eq!(r2.passes.virtualize.target.as_deref(), Some("hot*"));
    }

    // ---- Constraint (1): --strings-in-vm requires string obfuscation ----

    #[test]
    fn strings_in_vm_requires_strings_enabled() {
        // minify preset => strings none => reject.
        assert!(resolve("preset = \"minify\"\nstrings_in_vm = true").is_err());
        // explicit none also conflicts.
        assert!(resolve("preset = \"high\"\nstrings = \"none\"\nstrings_in_vm = true").is_err());
    }

    #[test]
    fn strings_in_vm_accepts_when_strings_on() {
        let r = resolve("preset = \"high\"\nstrings_in_vm = true").unwrap();
        assert!(r.passes.strings.in_vm);
    }

    // ---- Constraint (2): --self-coupled-key requires --strings-in-vm ----

    #[test]
    fn self_coupled_key_requires_vm() {
        assert!(resolve("preset = \"high\"\nself_coupled_key = true").is_err());
        assert!(
            resolve("preset = \"high\"\nstrings_in_vm = false\nself_coupled_key = true").is_err()
        );
    }

    #[test]
    fn self_coupled_key_accepts_with_vm() {
        let r =
            resolve("preset = \"high\"\nstrings_in_vm = true\nself_coupled_key = true").unwrap();
        assert!(r.passes.strings.self_coupled_key);
        assert!(r.passes.strings.in_vm);
    }

    // ---- Constraint (3): --exec-trace-key requires --strings-in-vm ----

    #[test]
    fn exec_trace_key_requires_vm() {
        assert!(resolve("preset = \"high\"\nexec_trace_key = true").is_err());
    }

    #[test]
    fn exec_trace_key_accepts_with_vm() {
        let r = resolve("preset = \"high\"\nstrings_in_vm = true\nexec_trace_key = true").unwrap();
        assert!(r.passes.strings.exec_trace_key);
    }

    // ---- Constraint (4)/(5): key-source forms ----

    #[test]
    fn key_off_by_default() {
        assert!(
            resolve("preset = \"high\"")
                .unwrap()
                .passes
                .strings
                .dynamic_key
                .is_none()
        );
    }

    #[test]
    fn domain_lock_expands_to_hostname() {
        let dk = resolve("domain_lock = \"example.com\"")
            .unwrap()
            .passes
            .strings
            .dynamic_key
            .unwrap();
        assert_eq!(dk.source_expr, "location.hostname");
        assert_eq!(dk.expected, "example.com");
    }

    #[test]
    fn key_source_with_expected_resolves() {
        let dk = resolve("key_source = \"window.__S\"\nkey_expected = \"tok\"")
            .unwrap()
            .passes
            .strings
            .dynamic_key
            .unwrap();
        assert_eq!(dk.source_expr, "window.__S");
        assert_eq!(dk.expected, "tok");
    }

    #[test]
    fn key_source_without_expected_errors() {
        assert!(resolve("key_source = \"location.hostname\"").is_err());
    }

    #[test]
    fn key_expected_without_source_errors() {
        assert!(resolve("key_expected = \"tok\"").is_err());
    }

    #[test]
    fn domain_lock_conflicts_with_key_source() {
        let r = resolve("domain_lock = \"e.com\"\nkey_source = \"x\"\nkey_expected = \"y\"");
        assert!(matches!(r, Err(ConfigError::MutuallyExclusive { .. })));
    }

    #[test]
    fn remote_key_with_expected_resolves() {
        let dk = resolve("remote_key = \"window.__sess\"\nkey_expected = \"s3cr3t\"")
            .unwrap()
            .passes
            .strings
            .dynamic_key
            .unwrap();
        assert_eq!(dk.source_expr, "window.__sess");
        assert_eq!(dk.expected, "s3cr3t");
    }

    #[test]
    fn remote_key_bare_uses_default_slot() {
        // The empty string stands in for clap's bare `--remote-key`.
        let dk = resolve("remote_key = \"\"\nkey_expected = \"tok\"")
            .unwrap()
            .passes
            .strings
            .dynamic_key
            .unwrap();
        assert_eq!(dk.source_expr, DEFAULT_SESSION_SLOT);
        assert_eq!(dk.expected, "tok");
    }

    #[test]
    fn remote_key_without_expected_errors() {
        assert!(resolve("remote_key = \"window.__sess\"").is_err());
    }

    #[test]
    fn remote_key_conflicts_with_domain_lock() {
        let r = resolve("remote_key = \"w\"\nkey_expected = \"t\"\ndomain_lock = \"e.com\"");
        assert!(matches!(r, Err(ConfigError::MutuallyExclusive { .. })));
    }

    #[test]
    fn remote_key_conflicts_with_key_source() {
        let r = resolve("remote_key = \"w\"\nkey_source = \"x\"\nkey_expected = \"t\"");
        assert!(matches!(r, Err(ConfigError::MutuallyExclusive { .. })));
    }

    #[test]
    fn try_from_entry_point_works() {
        // The exact signature WP9 calls. Uses a fixed seed via the flag so it is
        // deterministic without touching the RNG default path.
        let f = flags("preset = \"max\"\nseed = 5");
        let r: ResolvedConfig = f.try_into().unwrap();
        assert_eq!(r.engine.level, Intensity::Max);
        assert_eq!(r.engine.seed, 5);
    }

    #[test]
    fn fragment_strips_unsafe_passes() {
        let r = resolve("preset = \"high\"\nstrings_in_vm = true").unwrap();
        let frag = r.for_fragment();
        assert!(!frag.passes.anti_tamper.self_defending);
        assert!(!frag.passes.strings.in_vm);
        assert_eq!(frag.engine.level, Intensity::High);
    }
    #[test]
    fn required_virtualization_selects_targets_and_rejects_invalid_globs() {
        let resolved = resolve("require_virtualized = \"pay*\"").unwrap();
        assert_eq!(resolved.passes.virtualize.required.as_deref(), Some("pay*"));
        assert_eq!(resolved.passes.virtualize.target.as_deref(), Some("pay*"));
        for key in ["virtualize", "require_virtualized", "virtualize_exclude"] {
            assert!(resolve(&format!("{key} = \"[\"")).is_err());
        }
    }
}
