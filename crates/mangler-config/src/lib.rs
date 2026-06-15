//! `mangler-config` — configuration: intent vs. per-pass tuning.
//!
//! This crate separates **what** to obfuscate from **how** each pass is tuned,
//! collapses the old three-way flag duplication into one declaration, and makes
//! invalid flag combinations unrepresentable in the validated type.
//!
//! ## Layers
//!
//! * [`ConfigFlags`] (`flags`) — the *single* declaration of every config flag,
//!   deriving BOTH `clap::Args` and `serde::Deserialize`. Raw and permissive:
//!   every illegal combination is still representable here.
//! * [`EngineConfig`] (`engine`) — validated *intent*: level/preset, seed,
//!   language, verify, keep-names. No per-pass tuning leaks in.
//! * The `pass` types ([`MangleConfig`], [`StringsConfig`], [`CfFlattenConfig`],
//!   [`ExprConfig`], [`GlobalIndirectConfig`], [`AntiTamperConfig`],
//!   [`VirtualizeConfig`]) — each pass owns its tuning and a `for_preset` fold.
//! * [`PassConfigs`] (`pass`) — all the above assembled; a whole-config preset
//!   is just `for_preset` folded over each registered pass (no central tuple).
//! * [`ResolvedConfig`] (`engine`) — `EngineConfig` + `PassConfigs`, the single
//!   validated thing the pipeline consumes.
//!
//! ## Entry point
//!
//! WP9's CLI merges CLI flags over the optional TOML file over the preset into
//! one [`ConfigFlags`], then validates:
//!
//! ```ignore
//! let resolved: ResolvedConfig = flags.try_into()?;   // ResolvedConfig::try_from(flags)
//! ```
//!
//! All cross-flag constraints (key-source mutual exclusion; `--strings-in-vm` /
//! `--self-coupled-key` / `--exec-trace-key` dependencies) are checked there and
//! only there. See [`crate::validate`] for the full list.

mod engine;
mod enums;
mod error;
mod flags;
mod pass;
mod validate;

pub use engine::{EngineConfig, ResolvedConfig};
pub use enums::{GlobalIndirect, IdNaming, Intensity, Lang, Preset, StringMode};
pub use error::ConfigError;
pub use flags::{ConfigFlags, KeepNames};
pub use pass::{
    AntiTamperConfig, CfFlattenConfig, DynamicKey, ExprConfig, GlobalIndirectConfig, MangleConfig,
    PassConfigs, PassPreset, StringsConfig, VirtualizeConfig,
};
