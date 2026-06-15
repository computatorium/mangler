//! Structured configuration error.
//!
//! Kept local (a `thiserror` enum) rather than coupled to `mangler-core`'s
//! `Error`: WP1 may not have its error API ready, and config validation has its
//! own small, closed set of failure modes. When `mangler-core::Error` lands we
//! can add a `From<ConfigError>` there; nothing here needs to change.

/// Everything that can go wrong turning raw flags into a validated config.
///
/// Every variant is a *user* error (bad flag value or an illegal combination),
/// never an internal bug — so the CLI (WP9) can print the `Display` text
/// straight to stderr and exit non-zero.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// An enum-valued flag (`--strings`, `--global-indirect`, …) got a value
    /// outside its variant set.
    #[error("unknown {knob} value: {value:?} (expected one of: {expected})")]
    UnknownEnumValue {
        /// The flag/knob name, e.g. `strings mode`.
        knob: &'static str,
        /// The offending value the user supplied.
        value: String,
        /// Comma-separated list of accepted values.
        expected: &'static str,
    },

    /// A cross-flag dependency was violated: `flag` needs `requires` to also be
    /// set/enabled.
    #[error("{flag} requires {requires}")]
    MissingDependency {
        /// The flag that was given.
        flag: &'static str,
        /// What it depends on (human-readable).
        requires: &'static str,
    },

    /// Two (or more) flags that cannot be combined were given together.
    #[error("{a} is mutually exclusive with {b}")]
    MutuallyExclusive {
        /// One flag in the conflicting pair.
        a: &'static str,
        /// The other flag in the conflicting pair.
        b: &'static str,
    },

    /// Parsing the `--config` TOML failed (I/O or syntax/unknown-key).
    #[error("config file: {0}")]
    File(String),
}
