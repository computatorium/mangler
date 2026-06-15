//! Configuration seam — a **stub** for now.
//!
//! The full configuration model (flag parsing, per-pass option structs, the
//! profile/level system) is WP5. This module exists only so WP2's pass graph
//! can name the *shape* of the thing it is configured by, without depending on
//! the eventual concrete config crate.
//!
//! [`PassConfig`] is therefore a minimal marker trait. When WP5 lands, the real
//! config types will implement it (or it will be widened with the accessor
//! methods the pass graph actually needs). Treat anything here as provisional.

/// Marker trait for a type that configures the pass pipeline.
///
/// **Stub (WP1).** Currently carries only a single global knob — the seed — that
/// every pass needs to derive its [`crate::rng::Rng`] via
/// [`Rng::for_pass`](crate::rng::Rng::for_pass). WP5 will flesh this out with
/// per-pass options and enable/disable flags; downstream code should depend on
/// it as narrowly as possible so that growth here stays backward compatible.
pub trait PassConfig {
    /// The global deterministic seed. Combined with each pass's `pass_id` to
    /// derive that pass's independent RNG stream.
    fn seed(&self) -> u64;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rng::Rng;

    struct Cfg {
        seed: u64,
    }
    impl PassConfig for Cfg {
        fn seed(&self) -> u64 {
            self.seed
        }
    }

    #[test]
    fn stub_config_drives_per_pass_rng() {
        let cfg = Cfg { seed: 99 };
        let a = Rng::for_pass(cfg.seed(), "p").random_bytes(8);
        let b = Rng::for_pass(99, "p").random_bytes(8);
        assert_eq!(a, b);
    }
}
