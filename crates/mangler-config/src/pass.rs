//! Per-pass tuning types.
//!
//! Each pass owns its tuning struct and knows how to derive itself from an
//! [`Intensity`] preset via the [`PassPreset`] trait. The old top-level
//! `ObfuscationConfig` god-struct mixed *intent* (what to run) with *tuning*
//! (how each pass behaves) and materialized presets from a positional tuple
//! (`config.rs:237-243`); here every knob lives on exactly one pass and every
//! preset is a fold over the per-pass `for_preset` impls. Adding a pass means
//! adding a struct + a `PassPreset` impl and folding it in `PassConfigs::new` —
//! no central tuple to edit.
//!
//! The runtime-bound decode key ([`DynamicKey`]) is the one piece of opt-in
//! *intent* that lands on a pass (strings): it is never a preset default, so it
//! is left `None` here and injected by validation.

use crate::enums::{GlobalIndirect, IdNaming, Intensity, StringMode};

/// Common interface: build a pass's tuning from an intensity preset.
///
/// A whole-config preset is just `PassPreset::for_preset` folded over every
/// registered pass (see [`PassConfigs::for_preset`]).
pub trait PassPreset {
    /// The pass tuning that corresponds to `level`, with all opt-in fields off.
    fn for_preset(level: Intensity) -> Self;
}

/// Runtime-bound string-decode key (opt-in; never a preset default).
///
/// When set, the string decoder's per-byte key is XOR-folded with a keystream
/// derived at RUNTIME from `source_expr` (a JS expression evaluated in the host,
/// e.g. `location.hostname`); at build time the keystream from `expected` is
/// baked into the ciphertext. Decoding is byte-exact only when the live value
/// equals `expected`, otherwise every string decodes to garbage. This is the
/// only way to defeat a purely static "port the decoder" attack — the live key
/// byte is never present in the emitted file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DynamicKey {
    /// JS expression evaluated in the host to produce the live key material.
    pub source_expr: String,
    /// The value `source_expr` is expected to yield; baked into the ciphertext.
    pub expected: String,
}

/// Identifier-mangling pass (locals only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MangleConfig {
    /// Whether to rename local identifiers at all.
    pub enabled: bool,
    /// The renaming scheme (`short` / `hex` / `soup`).
    pub naming: IdNaming,
    /// Identifier-name globs preserved from renaming (`--keep-names`). Not a
    /// preset knob — populated by validation from CLI/file.
    pub keep_names: Vec<String>,
}

impl PassPreset for MangleConfig {
    fn for_preset(level: Intensity) -> Self {
        let naming = match level {
            Intensity::High | Intensity::Max => IdNaming::Hex,
            _ => IdNaming::Short,
        };
        // Every preset mangles; `--mangle false` is an override, not a default.
        MangleConfig {
            enabled: true,
            naming,
            keep_names: Vec::new(),
        }
    }
}

/// String-obfuscation pass and its in-VM / runtime-key hardening.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StringsConfig {
    /// Obfuscation mode (`none` disables the pass).
    pub mode: StringMode,
    /// Array partitions the encrypted string array is split across.
    pub partitions: u8,
    /// Number of decoder shims emitted (each call site picks one).
    pub decoders: u8,
    /// Junk-byte rate per encoded entry (0 disables junk padding).
    pub junk_rate: u8,
    /// Route the decoder through the bytecode VM (opt-in; never a preset
    /// default). Requires `mode != None` — enforced by validation.
    pub in_vm: bool,
    /// Stage 4 self-coupled decode key (opt-in; requires `in_vm`).
    pub self_coupled_key: bool,
    /// Stage 5 oblivious execution-trace key (opt-in; requires `in_vm`).
    pub exec_trace_key: bool,
    /// Runtime-bound decode key (opt-in; never a preset default).
    pub dynamic_key: Option<DynamicKey>,
}

impl PassPreset for StringsConfig {
    fn for_preset(level: Intensity) -> Self {
        let (mode, partitions, decoders, junk_rate) = match level {
            Intensity::Minify => (StringMode::None, 0, 0, 0),
            Intensity::Low => (StringMode::Encode, 1, 1, 0),
            Intensity::Medium => (StringMode::Encode, 3, 3, 32),
            Intensity::High => (StringMode::Encrypt, 4, 3, 64),
            Intensity::Max => (StringMode::Encrypt, 6, 3, 128),
        };
        StringsConfig {
            mode,
            partitions,
            decoders,
            junk_rate,
            in_vm: false,
            self_coupled_key: false,
            exec_trace_key: false,
            dynamic_key: None,
        }
    }
}

/// Control-flow flattening pass + opaque-predicate dead-code injection.
#[derive(Debug, Clone, PartialEq)]
pub struct CfFlattenConfig {
    /// Whether to flatten eligible function bodies into a switch state machine.
    pub enabled: bool,
    /// State variables used in the switch dispatch.
    pub state_vars: u8,
    /// Fraction of switch cases that are dead (unreachable) states.
    pub dead_state_rate: f32,
    /// Per-body probability of opaque-predicate dead-branch injection. Kept
    /// deliberately low (dead code is cheap for LLM analysis to filter; its ROI
    /// is sub-linear while output-size cost is not).
    pub dead_code_rate: f64,
}

impl PassPreset for CfFlattenConfig {
    fn for_preset(level: Intensity) -> Self {
        let (enabled, state_vars, dead_state_rate, dead_code_rate) = match level {
            Intensity::Minify => (false, 1, 0.0, 0.0),
            Intensity::Low => (false, 1, 0.0, 0.0),
            Intensity::Medium => (true, 1, 0.1, 0.05),
            Intensity::High => (true, 2, 0.25, 0.12),
            Intensity::Max => (true, 2, 0.5, 0.2),
        };
        CfFlattenConfig {
            enabled,
            state_vars,
            dead_state_rate,
            dead_code_rate,
        }
    }
}

/// Expression obfuscation: integer/boolean literal rewriting + member-access
/// conversion (`obj.prop` → `obj["prop"]`). Split gates so member-access can run
/// at Low+ while the heavier number rewriter stays Medium+.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExprConfig {
    /// Integer/boolean literal rewriter gate (Medium+).
    pub expr_obfuscation: bool,
    /// Member-access conversion gate (Low+).
    pub member_access: bool,
}

impl PassPreset for ExprConfig {
    fn for_preset(level: Intensity) -> Self {
        let member_access = !matches!(level, Intensity::Minify);
        let expr_obfuscation =
            matches!(level, Intensity::Medium | Intensity::High | Intensity::Max);
        ExprConfig {
            expr_obfuscation,
            member_access,
        }
    }
}

/// Global-reference indirection + the optional `globalThis`-anchor hardening.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlobalIndirectConfig {
    /// Indirection mode (`off` disables the pass).
    pub mode: GlobalIndirect,
    /// Hide the literal `globalThis` anchor behind a seeded derivation (opt-in;
    /// off in every preset).
    pub harden_anchor: bool,
}

impl PassPreset for GlobalIndirectConfig {
    fn for_preset(level: Intensity) -> Self {
        let mode = match level {
            Intensity::Minify | Intensity::Low => GlobalIndirect::Off,
            _ => GlobalIndirect::Safe,
        };
        GlobalIndirectConfig {
            mode,
            harden_anchor: false,
        }
    }
}

/// Anti-tamper wraps: the self-defending beautification guard and the
/// debug-protection `debugger` trap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AntiTamperConfig {
    /// Self-defending anti-beautification guard.
    pub self_defending: bool,
    /// Debug-protection (`debugger`) trap.
    pub debug_protection: bool,
}

impl PassPreset for AntiTamperConfig {
    fn for_preset(level: Intensity) -> Self {
        let on = matches!(level, Intensity::High | Intensity::Max);
        AntiTamperConfig {
            self_defending: on,
            debug_protection: on,
        }
    }
}

/// Function virtualization (opt-in; never a preset default).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VirtualizeConfig {
    /// Glob matching function names to virtualize. `None` = pass disabled (unless
    /// `whole_program` is set). Ignored when `whole_program` is true (§1 matrix:
    /// whole-program wins).
    pub target: Option<String>,
    /// Protection gate: every matching source function must be virtualized.
    pub required: Option<String>,
    /// NEW (Phase 1): wrap the ENTIRE top-level program body as one synthetic VM
    /// chunk (all-or-nothing — if it compiles the whole top level is virtualized,
    /// else the program is left native). Opt-in; never a preset default. When set,
    /// `target` is ignored. Bail-to-safe: a program that cannot compile (or that
    /// contains `import`/`export`) is left entirely native.
    pub whole_program: bool,
    /// Glob matching function names to KEEP NATIVE (exclude from virtualization).
    /// `None` = no exclusions. Evaluated after `target`: a function must match
    /// `target` AND NOT match `exclude` to be virtualized. Bail-to-safe: exclude
    /// only ever keeps a function native; it never miscompiles.
    pub exclude: Option<String>,
    /// Protect eligible ordinary class methods while retaining native class
    /// constructors, fields, heritage, and lexical bindings. Only has effect with
    /// `whole_program`; named targets can select methods directly. Default false.
    pub desugar_class: bool,
    /// Legacy compatibility flag. Regex literals remain native because replacing
    /// them with `new RegExp` observes shadowed or modified constructor bindings.
    pub desugar_regex: bool,
}

impl PassPreset for VirtualizeConfig {
    fn for_preset(_level: Intensity) -> Self {
        VirtualizeConfig {
            target: None,
            required: None,
            whole_program: false,
            exclude: None,
            desugar_class: false,
            desugar_regex: false,
        }
    }
}

/// The assembled set of every pass's tuning — the output of preset folding plus
/// validation. This is what the pipeline (WP9+) hands to each pass.
#[derive(Debug, Clone, PartialEq)]
pub struct PassConfigs {
    /// Identifier-mangling tuning.
    pub mangle: MangleConfig,
    /// String-obfuscation tuning.
    pub strings: StringsConfig,
    /// Control-flow-flattening tuning.
    pub cf_flatten: CfFlattenConfig,
    /// Expression-obfuscation tuning.
    pub expr: ExprConfig,
    /// Global-indirection tuning.
    pub global_indirect: GlobalIndirectConfig,
    /// Anti-tamper tuning.
    pub anti_tamper: AntiTamperConfig,
    /// Virtualization tuning.
    pub virtualize: VirtualizeConfig,
}

impl PassConfigs {
    /// A whole-config preset: `for_preset` folded over every registered pass.
    /// This replaces the positional preset tuple — adding a pass adds a field
    /// and a line here, not an edit to a shared tuple.
    pub fn for_preset(level: Intensity) -> Self {
        PassConfigs {
            mangle: MangleConfig::for_preset(level),
            strings: StringsConfig::for_preset(level),
            cf_flatten: CfFlattenConfig::for_preset(level),
            expr: ExprConfig::for_preset(level),
            global_indirect: GlobalIndirectConfig::for_preset(level),
            anti_tamper: AntiTamperConfig::for_preset(level),
            virtualize: VirtualizeConfig::for_preset(level),
        }
    }

    /// Strip the passes that must never run on an embedded fragment (inline
    /// `on*=` handler or `style=`): anti-tamper wraps, virtualization, and the
    /// in-VM string hardening. We never want a `setInterval(debugger)` injected
    /// into every inline event handler, nor a VM/self-hash binding that a
    /// fragment cannot satisfy. All other tuning is preserved.
    pub fn for_fragment(&self) -> Self {
        let mut f = self.clone();
        f.anti_tamper = AntiTamperConfig {
            self_defending: false,
            debug_protection: false,
        };
        f.virtualize = VirtualizeConfig {
            target: None,
            required: None,
            whole_program: false,
            exclude: None,
            desugar_class: false,
            desugar_regex: false,
        };
        f.strings.in_vm = false;
        f.strings.self_coupled_key = false;
        f.strings.exec_trace_key = false;
        f
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minify_folds_to_conservative_passes() {
        let p = PassConfigs::for_preset(Intensity::Minify);
        assert_eq!(p.strings.mode, StringMode::None);
        assert_eq!(p.strings.partitions, 0);
        assert!(!p.cf_flatten.enabled);
        assert_eq!(p.cf_flatten.dead_code_rate, 0.0);
        assert_eq!(p.cf_flatten.state_vars, 1);
        assert!(!p.expr.member_access);
        assert_eq!(p.global_indirect.mode, GlobalIndirect::Off);
        assert_eq!(p.mangle.naming, IdNaming::Short);
        assert!(!p.anti_tamper.self_defending);
    }

    #[test]
    fn high_folds_to_strong_passes() {
        let p = PassConfigs::for_preset(Intensity::High);
        assert_eq!(p.strings.mode, StringMode::Encrypt);
        assert_eq!(p.strings.partitions, 4);
        assert_eq!(p.strings.decoders, 3);
        assert_eq!(p.strings.junk_rate, 64);
        assert!(p.cf_flatten.enabled);
        assert_eq!(p.cf_flatten.state_vars, 2);
        assert!((p.cf_flatten.dead_state_rate - 0.25).abs() < f32::EPSILON);
        assert_eq!(p.global_indirect.mode, GlobalIndirect::Safe);
        assert_eq!(p.mangle.naming, IdNaming::Hex);
        assert!(p.anti_tamper.self_defending);
        assert!(p.anti_tamper.debug_protection);
    }

    #[test]
    fn max_is_the_strongest() {
        let p = PassConfigs::for_preset(Intensity::Max);
        assert_eq!(p.strings.partitions, 6);
        assert_eq!(p.strings.junk_rate, 128);
        assert!((p.cf_flatten.dead_state_rate - 0.5).abs() < f32::EPSILON);
        assert!((p.cf_flatten.dead_code_rate - 0.2).abs() < f64::EPSILON);
    }

    #[test]
    fn expr_member_access_gated_at_low_plus() {
        assert!(!ExprConfig::for_preset(Intensity::Minify).member_access);
        for l in [
            Intensity::Low,
            Intensity::Medium,
            Intensity::High,
            Intensity::Max,
        ] {
            assert!(
                ExprConfig::for_preset(l).member_access,
                "{l} must enable member_access"
            );
        }
    }

    #[test]
    fn expr_obfuscation_gated_at_medium_plus() {
        for l in [Intensity::Minify, Intensity::Low] {
            assert!(!ExprConfig::for_preset(l).expr_obfuscation);
        }
        for l in [Intensity::Medium, Intensity::High, Intensity::Max] {
            assert!(ExprConfig::for_preset(l).expr_obfuscation);
        }
    }

    #[test]
    fn opt_in_fields_off_in_every_preset() {
        for l in [
            Intensity::Minify,
            Intensity::Low,
            Intensity::Medium,
            Intensity::High,
            Intensity::Max,
        ] {
            let p = PassConfigs::for_preset(l);
            assert!(!p.strings.in_vm, "{l}: strings.in_vm must be opt-in");
            assert!(
                !p.strings.self_coupled_key,
                "{l}: self_coupled_key must be opt-in"
            );
            assert!(
                !p.strings.exec_trace_key,
                "{l}: exec_trace_key must be opt-in"
            );
            assert!(
                p.strings.dynamic_key.is_none(),
                "{l}: dynamic_key must be opt-in"
            );
            assert!(
                !p.global_indirect.harden_anchor,
                "{l}: harden_anchor must be opt-in"
            );
            assert!(
                p.virtualize.target.is_none(),
                "{l}: virtualize.target must be opt-in"
            );
            assert!(
                !p.virtualize.whole_program,
                "{l}: virtualize.whole_program must be opt-in"
            );
            assert!(
                p.virtualize.exclude.is_none(),
                "{l}: virtualize.exclude must be opt-in"
            );
            assert!(
                !p.virtualize.desugar_class,
                "{l}: virtualize.desugar_class must be opt-in"
            );
            assert!(
                !p.virtualize.desugar_regex,
                "{l}: virtualize.desugar_regex must be opt-in"
            );
            // Aggressive / Soup never preset defaults.
            assert_ne!(p.global_indirect.mode, GlobalIndirect::Aggressive);
            assert_ne!(p.mangle.naming, IdNaming::Soup);
        }
    }

    #[test]
    fn for_fragment_strips_anti_tamper_and_vm_but_keeps_rest() {
        let mut p = PassConfigs::for_preset(Intensity::High);
        p.virtualize.target = Some("*".to_string());
        p.virtualize.whole_program = true;
        p.virtualize.exclude = Some("render*".to_string());
        p.virtualize.desugar_class = true;
        p.virtualize.desugar_regex = true;
        p.strings.in_vm = true;
        p.strings.self_coupled_key = true;
        p.strings.exec_trace_key = true;
        let f = p.for_fragment();
        assert!(!f.anti_tamper.self_defending);
        assert!(!f.anti_tamper.debug_protection);
        assert!(f.virtualize.target.is_none());
        assert!(
            !f.virtualize.whole_program,
            "fragment strips whole_program too"
        );
        assert!(
            f.virtualize.exclude.is_none(),
            "fragment strips exclude too"
        );
        assert!(
            !f.virtualize.desugar_class,
            "fragment strips desugar_class too"
        );
        assert!(
            !f.virtualize.desugar_regex,
            "fragment strips desugar_regex too"
        );
        assert!(!f.strings.in_vm);
        assert!(!f.strings.self_coupled_key);
        assert!(!f.strings.exec_trace_key);
        // Preserved.
        assert!(f.expr.expr_obfuscation);
        assert_eq!(f.mangle.naming, IdNaming::Hex);
        assert_eq!(f.strings.mode, StringMode::Encrypt);
    }
}
