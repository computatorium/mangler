//! `mangler-passgraph` — dependency-driven pass scheduling.
//!
//! This crate replaces the original hand-ordered pass-registration block (and the
//! prose comments that justified it) with an order **derived** from each pass's
//! declared resource dependencies. Four pieces compose:
//!
//! * [`resource`] — the open [`Resource`] vocabulary (the nouns passes read and
//!   write). Richer than the original two-value `Capability` enum, and extensible
//!   per language via [`Resource::custom`](resource::Resource::custom).
//! * [`pass`] — the [`Pass`] trait, generic over [`Language`](mangler_core::Language).
//!   A pass declares [`id`](Pass::id) / [`reads`](Pass::reads) /
//!   [`writes`](Pass::writes) / [`enabled`](Pass::enabled) /
//!   [`fragment_safe`](Pass::fragment_safe) and mutates `&mut L::Ast` in
//!   [`run`](Pass::run).
//! * [`schedule`] — a topological sort over reads/writes producing a valid,
//!   deterministic linear order; cycles are rejected with a clear error. The name
//!   resolver, minify, anti-tamper and emit-patch are modeled as ordinary passes,
//!   so the old Pre/PostResolver phase split falls out of the sort.
//! * [`bus`] — the type-keyed [`ArtifactBus`] (a generalization of the original
//!   `PipelineArtifacts` struct), validated against the same declared
//!   reads/writes.
//!
//! # How a runner (WP6) uses this
//!
//! 1. Collect the enabled passes (`pass.enabled(cfg)`), build a [`PassNode`] per
//!    pass via [`node_for`], and call [`schedule_nodes`] to get the order (handle
//!    a [`ScheduleError`] as a hard config error).
//! 2. For each pass in order: construct its RNG as
//!    `Rng::for_pass(cfg.seed(), pass.id())`, scope the bus with
//!    `bus.enter_pass(pass.id(), pass.reads(), pass.writes())`, then call
//!    `pass.run(ast, cfg, &mut rng, &mut bus, &mut notes)`.
//!
//! Because the RNG is keyed on `(seed, id)` only, the schedule and the per-pass
//! randomness are both independent of registration/draw order — same seed and
//! pass set ⇒ byte-identical output.

#![warn(missing_docs)]

pub mod bus;
pub mod pass;
pub mod resource;
pub mod schedule;

pub use bus::{Artifact, ArtifactBus, BusError};
pub use pass::Pass;
pub use resource::{Builtin, Resource};
pub use schedule::{schedule, schedule_nodes, PassNode, ScheduleError};

use mangler_core::{Language, PassConfig};

/// Build the scheduler's [`PassNode`] view of a [`Pass`]. The runner calls this
/// for each enabled pass to feed [`schedule_nodes`], keeping the
/// id/reads/writes triple in lockstep with what the bus later validates against.
pub fn node_for<L: Language, C: PassConfig>(pass: &dyn Pass<L, C>) -> PassNode {
    PassNode::new(pass.id(), pass.reads(), pass.writes())
}

#[cfg(test)]
mod integration_tests {
    use super::*;
    use mangler_core::{Notes, Result, Rng};

    // A toy language whose AST records the order passes touched it.
    struct Toy;
    impl Language for Toy {
        type Ast = Vec<&'static str>;
        type ParseOpts = ();
        const ID: &'static str = "toy";
        fn parse(&self, _src: &str, _: &()) -> Result<Vec<&'static str>> {
            Ok(Vec::new())
        }
        fn print(&self, ast: &Vec<&'static str>) -> String {
            ast.join(",")
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

    // A pass parameterized by id + reads + writes; on run it appends its id and,
    // if it writes DecoderAnchor, puts a handle; if it reads one, asserts presence.
    struct P {
        id: &'static str,
        reads: Vec<Resource>,
        writes: Vec<Resource>,
    }
    impl Pass<Toy, Cfg> for P {
        fn id(&self) -> &'static str {
            self.id
        }
        fn reads(&self) -> &[Resource] {
            &self.reads
        }
        fn writes(&self) -> &[Resource] {
            &self.writes
        }
        fn run(
            &self,
            ast: &mut Vec<&'static str>,
            _cfg: &Cfg,
            _rng: &mut Rng,
            _bus: &mut ArtifactBus,
            _notes: &mut Notes,
        ) -> Result<()> {
            ast.push(self.id);
            Ok(())
        }
    }

    #[test]
    fn end_to_end_runner_loop_respects_derived_order() {
        // Two passes: a writer of DecoderAnchor and a reader; the reader must run
        // second purely from the declared edge.
        let passes: Vec<Box<dyn Pass<Toy, Cfg>>> = vec![
            Box::new(P {
                id: "reader",
                reads: vec![Resource::decoder_anchor()],
                writes: vec![],
            }),
            Box::new(P {
                id: "writer",
                reads: vec![],
                writes: vec![Resource::decoder_anchor()],
            }),
        ];
        let cfg = Cfg { seed: 42 };

        // Runner loop, exactly as documented for WP6.
        let nodes: Vec<PassNode> = passes.iter().map(|p| node_for(p.as_ref())).collect();
        let order = schedule_nodes(&nodes).unwrap();
        assert_eq!(order.iter().map(|n| n.id).collect::<Vec<_>>(), vec!["writer", "reader"]);

        let mut ast = Toy.parse("", &()).unwrap();
        let mut bus = ArtifactBus::new();
        let mut notes = Notes::new();
        for n in &order {
            let p = passes.iter().find(|p| p.id() == n.id).unwrap();
            let mut rng = Rng::for_pass(cfg.seed(), p.id());
            bus.enter_pass(p.id(), p.reads(), p.writes());
            p.run(&mut ast, &cfg, &mut rng, &mut bus, &mut notes).unwrap();
        }
        assert_eq!(ast, vec!["writer", "reader"]);
    }
}
