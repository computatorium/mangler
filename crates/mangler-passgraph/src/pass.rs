//! The [`Pass`] trait — a single AST transform that *declares* its data
//! dependencies instead of being placed by hand.
//!
//! A [`Pass`] is generic over [`Language`], so it operates on `&mut L::Ast` (the
//! concrete swc `Program` for JS, a CSS stylesheet for CSS, …) with no erasure.
//! Crucially, a pass no longer states *where* it runs — it states *what it reads*
//! and *what it writes*, and the scheduler ([`crate::schedule`]) derives the
//! order. This is the central generalization over the original trait, which
//! carried an explicit `phase()` (PreResolver/PostResolver) and a closed
//! `provides`/`requires` capability pair.
//!
//! # What replaced what
//!
//! * `phase()` is **gone**. The name resolver is modeled as a pass that *writes*
//!   [`Resource::resolved_scopes`](crate::resource::Resource::resolved_scopes);
//!   passes that needed `PostResolver` simply *read* it, and the Pre/Post split
//!   falls out of the topological sort.
//! * `provides`/`requires` (two capabilities) become [`Pass::writes`] /
//!   [`Pass::reads`] over the open [`Resource`] vocabulary.
//! * The central `for_fragment` allow-list becomes per-pass
//!   [`Pass::fragment_safe`].
//! * `name()` becomes [`Pass::id`] — still stable and unique, and now explicitly
//!   load-bearing: it seeds the pass's RNG via
//!   [`Rng::for_pass`](mangler_core::Rng::for_pass).

use crate::bus::ArtifactBus;
use crate::resource::Resource;
use mangler_core::{Language, Notes, PassConfig, Result, Rng};

/// A single AST-mutating step in the pipeline, generic over its [`Language`].
///
/// Implementations declare their data dependencies ([`reads`](Pass::reads) /
/// [`writes`](Pass::writes)) and mutate the AST in [`run`](Pass::run). The
/// scheduler orders passes so every reader runs after every writer of the same
/// resource; the runner constructs each pass's RNG from `(cfg.seed(), id())` and
/// scopes the bus to the declared reads/writes before calling `run`.
///
/// `C: PassConfig` is left generic so WP5's concrete config can plug in without
/// this crate depending on `mangler-config` (avoiding a cycle). A pass that needs
/// richer config than [`PassConfig`] exposes today can bound `C` on its own
/// extension trait once WP5 widens the contract.
pub trait Pass<L: Language, C: PassConfig> {
    /// A stable, unique identifier for this pass.
    ///
    /// **Load-bearing**: it is both the scheduler's deterministic tie-break key
    /// *and* the `pass_id` the runner feeds to
    /// [`Rng::for_pass`](mangler_core::Rng::for_pass), so the pass's randomness is
    /// a pure function of `(seed, id)`. Two passes in one schedule must never
    /// share an id.
    fn id(&self) -> &'static str;

    /// Resources this pass consumes. The scheduler orders this pass *after* every
    /// enabled pass that [`writes`](Pass::writes) any of these. The bus permits a
    /// `get::<T>()` only for `T` whose resource is listed here. Default: none.
    fn reads(&self) -> &[Resource] {
        &[]
    }

    /// Resources this pass produces. The scheduler orders every enabled pass that
    /// [`reads`](Pass::reads) any of these *after* this pass. The bus permits a
    /// `put::<T>()` only for `T` whose resource is listed here. Default: none.
    fn writes(&self) -> &[Resource] {
        &[]
    }

    /// Whether this pass should run under `cfg`. The scheduler considers only
    /// enabled passes when building the order, so a disabled writer simply makes
    /// its resource absent (readers soft-degrade). Default: always enabled.
    fn enabled(&self, _cfg: &C) -> bool {
        true
    }

    /// Whether this pass is safe to run on a *fragment* (a partial AST, e.g. an
    /// HTML inline `<script>` or a snippet), as opposed to a whole module. This
    /// replaces the original central `for_fragment` list with a per-pass
    /// declaration. Default: `false` (conservative — opt in explicitly).
    fn fragment_safe(&self) -> bool {
        false
    }

    /// Apply the transform.
    ///
    /// Mutates `ast` in place, drawing all randomness from `rng` (constructed by
    /// the runner as `Rng::for_pass(cfg.seed(), self.id())` — never `thread_rng`,
    /// so output stays reproducible), reading/writing cross-pass artifacts via
    /// `bus` (scoped to this pass's declared reads/writes), and pushing non-fatal
    /// [`Note`](mangler_core::Note)s onto `notes`. Returns `Err` only on a hard
    /// failure; a declined transform is a note, not an error. Called only when
    /// [`enabled`](Pass::enabled) is true.
    fn run(
        &self,
        ast: &mut L::Ast,
        cfg: &C,
        rng: &mut Rng,
        bus: &mut ArtifactBus,
        notes: &mut Notes,
    ) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use mangler_core::Note;

    // A trivial language whose AST is a string we append to.
    struct Toy;
    impl Language for Toy {
        type Ast = String;
        type ParseOpts = ();
        const ID: &'static str = "toy";
        fn parse(&self, src: &str, _: &()) -> Result<String> {
            Ok(src.to_string())
        }
        fn print(&self, ast: &String) -> String {
            ast.clone()
        }
    }

    struct Cfg {
        seed: u64,
    }
    impl PassConfig for Cfg {
        fn seed(&self) -> u64 {
            self.seed
        }
    }

    struct AppendPass;
    impl Pass<Toy, Cfg> for AppendPass {
        fn id(&self) -> &'static str {
            "append"
        }
        fn run(
            &self,
            ast: &mut String,
            _cfg: &Cfg,
            rng: &mut Rng,
            _bus: &mut ArtifactBus,
            notes: &mut Notes,
        ) -> Result<()> {
            // Draw from the per-pass rng to prove it threads through.
            ast.push_str(&format!("+{}", rng.pick(100)));
            notes.push(Note::from("append", "ran"));
            Ok(())
        }
    }

    #[test]
    fn pass_runs_with_per_pass_rng_and_notes() {
        let p = AppendPass;
        let cfg = Cfg { seed: 7 };
        let mut ast = String::from("base");
        let mut rng = Rng::for_pass(cfg.seed(), p.id());
        let mut bus = ArtifactBus::new();
        let mut notes = Notes::new();
        p.run(&mut ast, &cfg, &mut rng, &mut bus, &mut notes).unwrap();

        // Deterministic: same seed+id reproduces the same draw.
        let mut rng2 = Rng::for_pass(7, "append");
        assert_eq!(ast, format!("base+{}", rng2.pick(100)));
        assert_eq!(notes.len(), 1);
    }

    #[test]
    fn defaults_are_conservative() {
        let p = AppendPass;
        assert!(p.reads().is_empty());
        assert!(p.writes().is_empty());
        assert!(!p.fragment_safe());
        assert!(p.enabled(&Cfg { seed: 0 }));
    }
}
