//! The enum-valued knobs (`Intensity`, `StringMode`, `GlobalIndirect`,
//! `IdNaming`, `Lang`) and their `FromStr` + `clap::ValueEnum` + serde impls.
//!
//! Each enum is declared exactly once here and given all three string-facing
//! representations from a single `to_str`/`from_str` pair, so the CLI surface,
//! the TOML file surface, and any programmatic use all agree by construction.
//! This is the source of truth that the old triplicated
//! `Cli`/`FileConfig`/`ObfuscationConfig` string-matching collapses into.

use crate::error::ConfigError;
use std::fmt;
use std::str::FromStr;

/// Generate `FromStr`, `clap::ValueEnum`, `serde::{Serialize, Deserialize}`,
/// `Display`, and an inherent `as_str` for a simple C-like enum whose variants
/// map 1:1 to lowercase string tokens.
macro_rules! string_enum {
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident ($knob:literal) {
            $( $variant:ident = $token:literal ),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        $vis enum $name {
            $( $variant ),+
        }

        impl $name {
            /// The canonical lowercase token for this variant (used by the CLI,
            /// TOML, `Display`, and serde alike).
            pub const fn as_str(self) -> &'static str {
                match self {
                    $( $name::$variant => $token ),+
                }
            }

            /// Every accepted token, in declaration order.
            pub const VALUES: &'static [&'static str] = &[ $( $token ),+ ];

            /// The knob name reported in error messages.
            pub const KNOB: &'static str = $knob;

            /// The accepted-values list rendered for error messages.
            pub const EXPECTED: &'static str = concat!( $( $token, "  " ),+ );
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl FromStr for $name {
            type Err = ConfigError;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                let lower = s.to_ascii_lowercase();
                match lower.as_str() {
                    $( $token => Ok($name::$variant), )+
                    _ => Err(ConfigError::UnknownEnumValue {
                        knob: $knob,
                        value: s.to_string(),
                        expected: $name::EXPECTED.trim_end(),
                    }),
                }
            }
        }

        impl clap::ValueEnum for $name {
            fn value_variants<'a>() -> &'a [Self] {
                &[ $( $name::$variant ),+ ]
            }
            fn to_possible_value(&self) -> Option<clap::builder::PossibleValue> {
                Some(clap::builder::PossibleValue::new(self.as_str()))
            }
        }

        impl serde::Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(self.as_str())
            }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let s = String::deserialize(d)?;
                s.parse().map_err(serde::de::Error::custom)
            }
        }
    };
}

string_enum! {
    /// Named obfuscation intensity. Each level is a single point in the knob
    /// space, materialized by every `PassConfig`'s `for_preset`. Ordered
    /// weakest → strongest: `Minify` is pure minification (no obfuscation),
    /// `Max` turns on every pass at its highest setting. Default is `High`.
    ///
    /// Replaces the old `Preset` enum; `Preset` is kept as an alias.
    pub enum Intensity ("preset") {
        Minify = "minify",
        Low = "low",
        Medium = "medium",
        High = "high",
        Max = "max",
    }
}

/// Back-compat alias: the old config called this `Preset`.
pub type Preset = Intensity;

string_enum! {
    /// How string literals are obfuscated.
    ///
    /// * `None`    — leave literals as-is (Minify default).
    /// * `Encode`  — base64/XOR-encode into a DAG-decoded array (Low/Medium).
    /// * `Encrypt` — full encryption with decoder shims + dead-arith wrappers
    ///   (High/Max).
    pub enum StringMode ("strings mode") {
        None = "none",
        Encode = "encode",
        Encrypt = "encrypt",
    }
}

string_enum! {
    /// Global-reference indirection mode.
    ///
    /// * `Off`        — no global indirection (Minify/Low default).
    /// * `Safe`       — indirect only allowlisted, never-declared free globals
    ///   (Medium/High/Max default).
    /// * `Aggressive` — drop the allowlist; indirect every never-declared free
    ///   name (opt-in only).
    pub enum GlobalIndirect ("global_indirect") {
        Off = "off",
        Safe = "safe",
        Aggressive = "aggressive",
    }
}

string_enum! {
    /// Local-identifier renaming scheme. Locals only.
    ///
    /// * `Short` — swc's built-in short-name mangle (`a`, `b`, …).
    /// * `Hex`   — `_0x4e2a`-style hexadecimal names; High/Max default.
    /// * `Soup`  — `lIl1I` homoglyph names; opt-in only.
    pub enum IdNaming ("identifier_naming") {
        Short = "short",
        Hex = "hex",
        Soup = "soup",
    }
}

string_enum! {
    /// A source language the driver knows how to process.
    pub enum Lang ("lang") {
        Js = "js",
        Css = "css",
        Html = "html",
    }
}

impl Lang {
    /// Detect language from a file extension (no leading dot). Looser than
    /// `FromStr`: accepts the extension aliases (`mjs`, `ts`, `htm`, …) that the
    /// `--lang` override does not.
    pub fn from_ext(ext: &str) -> Option<Lang> {
        match ext.to_ascii_lowercase().as_str() {
            "js" | "mjs" | "cjs" | "jsx" | "ts" | "tsx" => Some(Lang::Js),
            "css" => Some(Lang::Css),
            "html" | "htm" => Some(Lang::Html),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::ValueEnum;

    #[test]
    fn intensity_round_trips_through_str() {
        for v in Intensity::value_variants() {
            assert_eq!(v.as_str().parse::<Intensity>().unwrap(), *v);
        }
        assert_eq!(Intensity::VALUES.len(), 5);
    }

    #[test]
    fn from_str_is_case_insensitive() {
        assert_eq!("HIGH".parse::<Intensity>().unwrap(), Intensity::High);
        assert_eq!("Encode".parse::<StringMode>().unwrap(), StringMode::Encode);
    }

    #[test]
    fn from_str_rejects_unknown_with_structured_error() {
        let err = "nope".parse::<StringMode>().unwrap_err();
        match err {
            ConfigError::UnknownEnumValue { knob, value, .. } => {
                assert_eq!(knob, "strings mode");
                assert_eq!(value, "nope");
            }
            other => panic!("wrong error: {other:?}"),
        }
    }

    #[test]
    fn every_enum_from_str_covers_all_variants() {
        for v in StringMode::value_variants() {
            assert_eq!(v.as_str().parse::<StringMode>().unwrap(), *v);
        }
        for v in GlobalIndirect::value_variants() {
            assert_eq!(v.as_str().parse::<GlobalIndirect>().unwrap(), *v);
        }
        for v in IdNaming::value_variants() {
            assert_eq!(v.as_str().parse::<IdNaming>().unwrap(), *v);
        }
        for v in Lang::value_variants() {
            assert_eq!(v.as_str().parse::<Lang>().unwrap(), *v);
        }
    }

    #[test]
    fn value_enum_possible_values_match_tokens() {
        for v in IdNaming::value_variants() {
            let pv = v.to_possible_value().unwrap();
            assert_eq!(pv.get_name(), v.as_str());
        }
    }

    #[test]
    fn serde_round_trips_via_toml() {
        #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
        struct W {
            mode: StringMode,
            naming: IdNaming,
        }
        let w = W { mode: StringMode::Encrypt, naming: IdNaming::Soup };
        let s = toml::to_string(&w).unwrap();
        assert!(s.contains("\"encrypt\""));
        let back: W = toml::from_str(&s).unwrap();
        assert_eq!(back, w);
    }

    #[test]
    fn serde_rejects_unknown_token() {
        #[derive(serde::Deserialize)]
        struct W {
            #[allow(dead_code)]
            mode: StringMode,
        }
        assert!(toml::from_str::<W>("mode = \"bogus\"").is_err());
    }

    #[test]
    fn lang_from_ext_maps_aliases_but_from_str_is_strict() {
        assert_eq!(Lang::from_ext("mjs"), Some(Lang::Js));
        assert_eq!(Lang::from_ext("htm"), Some(Lang::Html));
        assert_eq!(Lang::from_ext("png"), None);
        // FromStr is strict: aliases are NOT accepted.
        assert!("mjs".parse::<Lang>().is_err());
        assert_eq!("css".parse::<Lang>().unwrap(), Lang::Css);
    }
}
