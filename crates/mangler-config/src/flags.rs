//! [`ConfigFlags`] — the single declaration of every configuration flag.
//!
//! This ONE struct derives BOTH `clap::Args` and `serde::Deserialize`, so each
//! knob is declared exactly once and is reachable identically from the CLI
//! (`--strings encode`) and from a `--config` TOML file (`strings = "encode"`).
//! It collapses the old three-way duplication between `Cli`, `FileConfig`, and
//! `ObfuscationConfig`'s string-matching resolver.
//!
//! It is the *raw* surface: every knob is an `Option`/`bool` where absence means
//! "use the preset (or file) default". It carries no invariants — illegal
//! combinations are still representable HERE; they are rejected by
//! [`crate::validate`] when converting into the validated [`ResolvedConfig`].
//! Purely CLI-shaped args (inputs, `-o/--output`, `--in-place`, `--jobs`,
//! `--verbose`, `--keep-going`, `--config`) are NOT here — they belong to the
//! binary driver (WP9), not the obfuscation config.

use crate::enums::{GlobalIndirect, IdNaming, Intensity, Lang, StringMode};

/// Every configuration flag, declared once for clap and serde.
///
/// `#[serde(default)]` lets a TOML file omit any key (→ `None`/`false`), and
/// `deny_unknown_fields` makes a typo'd key a hard error instead of a silent
/// no-op. clap derives the matching `--flag` surface from the same fields.
#[derive(clap::Args, serde::Deserialize, serde::Serialize, Debug, Default, Clone, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct ConfigFlags {
    /// Obfuscation preset (`minify|low|medium|high|max`). Default: `high`
    /// (overridable by a `--config` file, which an explicit flag beats).
    #[arg(short, long, value_enum)]
    pub preset: Option<Intensity>,

    /// Forced input language (`js|css|html`), bypassing extension detection.
    /// Required when reading from stdin.
    #[arg(short, long, value_enum)]
    pub lang: Option<Lang>,

    /// Fixed RNG seed for deterministic output. Omitted → a random seed.
    #[arg(long)]
    pub seed: Option<u64>,

    /// Re-parse the output and assert it is valid after mangling.
    #[arg(long)]
    pub verify: bool,

    // ---- Per-pass overrides (None = preset default) ----
    /// Enable/disable local-identifier mangling (`true|false`).
    #[arg(long, action = clap::ArgAction::Set)]
    pub mangle: Option<bool>,

    /// String obfuscation mode (`none|encode|encrypt`).
    #[arg(long, value_enum)]
    pub strings: Option<StringMode>,

    /// Enable/disable control-flow flattening (`true|false`).
    #[arg(long, action = clap::ArgAction::Set)]
    pub control_flow: Option<bool>,

    /// Dead-code injection ratio (0.0 – 1.0). 0.0 disables.
    #[arg(long)]
    pub dead_code: Option<f64>,

    /// Enable/disable the self-defending anti-beautification guard (`true|false`).
    #[arg(long, action = clap::ArgAction::Set)]
    pub self_defending: Option<bool>,

    /// Enable/disable the debug-protection trap (`true|false`).
    #[arg(long, action = clap::ArgAction::Set)]
    pub debug_protection: Option<bool>,

    /// Virtualize functions whose names match this glob (e.g. `hot*`). Opt-in.
    #[arg(long)]
    pub virtualize: Option<String>,

    /// Require every function matching GLOB to be virtualized; fail on native or unmatched targets.
    #[arg(long, value_name = "GLOB")]
    pub require_virtualized: Option<String>,

    /// Virtualize the ENTIRE top-level program as one synthetic VM chunk
    /// (all-or-nothing: if it compiles the whole top level is virtualized, else the
    /// program is left native). Opt-in; bail-to-safe. When set, `--virtualize`
    /// (`target`) is ignored. Phase-1 limitation: a program containing `import`/
    /// `export` is left entirely native.
    #[arg(long, action = clap::ArgAction::SetTrue)]
    pub virtualize_program: bool,

    /// Keep functions whose names match this glob NATIVE (never virtualized), even
    /// when they would otherwise be selected by `--virtualize`. Opt-in; bail-to-safe.
    #[arg(long, value_name = "GLOB")]
    pub virtualize_exclude: Option<String>,

    /// Protect eligible class method bodies while retaining native constructors,
    /// fields, and inheritance. Use with --virtualize-program. Default OFF.
    #[arg(long, action = clap::ArgAction::SetTrue)]
    pub virtualize_desugar_class: bool,

    /// Legacy compatibility flag; regex literals remain native to preserve intrinsic
    /// constructor semantics, including shadowed or modified RegExp bindings.
    #[arg(long, action = clap::ArgAction::SetTrue)]
    pub virtualize_desugar_regex: bool,

    /// Global-reference indirection (`off|safe|aggressive`).
    #[arg(long, value_enum)]
    pub global_indirect: Option<GlobalIndirect>,

    /// Local-identifier naming scheme (`short|hex|soup`).
    #[arg(long, value_enum)]
    pub identifier_naming: Option<IdNaming>,

    /// Hide the literal `globalThis` anchor behind a seeded derivation. Opt-in.
    #[arg(long, action = clap::ArgAction::SetTrue)]
    pub harden_global_anchor: bool,

    /// Comma-separated identifier-name globs to PRESERVE from renaming.
    /// (From a TOML file this may instead be a list; see [`KeepNames`].)
    #[arg(long, value_name = "GLOBS")]
    pub keep_names: Option<KeepNames>,

    /// Bind string decoding to a RUNTIME JS expression (e.g. `location.hostname`).
    /// Requires `--key-expected`. Opt-in.
    #[arg(long)]
    pub key_source: Option<String>,

    /// The value `--key-source` is expected to yield; baked into the ciphertext.
    #[arg(long)]
    pub key_expected: Option<String>,

    /// Sugar for `--key-source 'location.hostname' --key-expected <HOST>`.
    /// Mutually exclusive with `--key-source`/`--key-expected`.
    #[arg(long)]
    pub domain_lock: Option<String>,

    /// Stage 6 remote/session key. Sugar over `--key-source <EXPR> --key-expected
    /// <TOKEN>`; bare value defaults the expression to the conventional session
    /// slot. Requires `--key-expected`. Mutually exclusive with the other key
    /// forms.
    #[arg(long, num_args = 0..=1, default_missing_value = "globalThis.__MANGLER_SESSION_KEY")]
    pub remote_key: Option<String>,

    /// Route the string decoder through the bytecode VM (`true|false`). Requires
    /// string obfuscation enabled. Opt-in.
    #[arg(long, action = clap::ArgAction::Set)]
    pub strings_in_vm: Option<bool>,

    /// Stage 4 self-coupled decode key (`true|false`). Requires `--strings-in-vm`.
    /// Opt-in; fragile (re-minification breaks decoding).
    #[arg(long, action = clap::ArgAction::Set)]
    pub self_coupled_key: Option<bool>,

    /// Stage 5 oblivious execution-trace key (`true|false`). Requires
    /// `--strings-in-vm`. Opt-in; robust to re-minification.
    #[arg(long, action = clap::ArgAction::Set)]
    pub exec_trace_key: Option<bool>,
}

/// Identifier-name globs as accepted from EITHER surface: a comma-separated
/// string on the CLI (`--keep-names "a,b*"`) or a TOML list (`["a", "b*"]`).
///
/// A custom newtype is what lets one field serve both clap (which only sees a
/// `String`) and serde (which prefers a list) without a second declaration.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct KeepNames(pub Vec<String>);

impl KeepNames {
    /// The collected globs (trimmed, non-empty).
    pub fn into_vec(self) -> Vec<String> {
        self.0
    }

    /// Parse a comma-separated list, dropping empty entries.
    fn from_csv(s: &str) -> Self {
        KeepNames(
            s.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect(),
        )
    }
}

impl std::str::FromStr for KeepNames {
    type Err = std::convert::Infallible;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(KeepNames::from_csv(s))
    }
}

impl<'de> serde::Deserialize<'de> for KeepNames {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        /// Accept a string ("a,b") or a sequence (["a","b"]).
        #[derive(serde::Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Csv(String),
            List(Vec<String>),
        }
        Ok(match Raw::deserialize(d)? {
            Raw::Csv(s) => KeepNames::from_csv(&s),
            Raw::List(v) => KeepNames(v.into_iter().filter(|s| !s.trim().is_empty()).collect()),
        })
    }
}

impl serde::Serialize for KeepNames {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// Minimal harness so `ConfigFlags` (an `Args` group) can be parsed from argv
    /// in tests, the same way WP9 will `#[command(flatten)]` it into the `Cli`.
    #[derive(Parser, Debug)]
    struct Harness {
        #[command(flatten)]
        flags: ConfigFlags,
    }

    fn parse(argv: &[&str]) -> ConfigFlags {
        Harness::parse_from(std::iter::once("mangler").chain(argv.iter().copied())).flags
    }

    #[test]
    fn clap_parses_enum_flags() {
        let f = parse(&[
            "--preset",
            "max",
            "--strings",
            "encrypt",
            "--identifier-naming",
            "soup",
        ]);
        assert_eq!(f.preset, Some(Intensity::Max));
        assert_eq!(f.strings, Some(StringMode::Encrypt));
        assert_eq!(f.identifier_naming, Some(IdNaming::Soup));
    }

    #[test]
    fn clap_rejects_bad_enum_value() {
        let r = Harness::try_parse_from(["mangler", "--preset", "nope"]);
        assert!(r.is_err());
    }

    #[test]
    fn clap_parses_tristate_bool() {
        assert_eq!(parse(&["--mangle", "false"]).mangle, Some(false));
        assert_eq!(parse(&[]).mangle, None);
    }

    #[test]
    fn clap_keep_names_csv() {
        let f = parse(&["--keep-names", "myExport, init*"]);
        assert_eq!(f.keep_names.unwrap().into_vec(), vec!["myExport", "init*"]);
    }

    #[test]
    fn remote_key_bare_defaults_slot() {
        let f = parse(&["--remote-key"]);
        assert_eq!(
            f.remote_key.as_deref(),
            Some("globalThis.__MANGLER_SESSION_KEY")
        );
    }

    #[test]
    fn serde_round_trips_full_toml() {
        let toml_src = r#"
            preset = "max"
            seed = 7
            self_defending = false
            identifier_naming = "soup"
            strings = "encrypt"
            keep_names = ["a", "b*"]
            harden_global_anchor = true
        "#;
        let f: ConfigFlags = toml::from_str(toml_src).unwrap();
        assert_eq!(f.preset, Some(Intensity::Max));
        assert_eq!(f.seed, Some(7));
        assert_eq!(f.self_defending, Some(false));
        assert_eq!(f.identifier_naming, Some(IdNaming::Soup));
        assert_eq!(f.strings, Some(StringMode::Encrypt));
        assert_eq!(f.keep_names.clone().unwrap().into_vec(), vec!["a", "b*"]);
        assert!(f.harden_global_anchor);

        // Re-serialize and read back: structural round-trip.
        let s = toml::to_string(&f).unwrap();
        let g: ConfigFlags = toml::from_str(&s).unwrap();
        assert_eq!(f, g);
    }

    #[test]
    fn serde_keep_names_accepts_csv_string_too() {
        let f: ConfigFlags = toml::from_str(r#"keep_names = "a, b*, ""#).unwrap();
        assert_eq!(f.keep_names.unwrap().into_vec(), vec!["a", "b*"]);
    }

    #[test]
    fn serde_rejects_unknown_key() {
        assert!(toml::from_str::<ConfigFlags>("predet = \"max\"").is_err());
    }

    #[test]
    fn empty_toml_is_all_defaults() {
        let f: ConfigFlags = toml::from_str("").unwrap();
        assert_eq!(f, ConfigFlags::default());
    }
}
