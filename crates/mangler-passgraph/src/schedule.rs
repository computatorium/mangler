//! The topological scheduler — execution order *derived* from declared
//! reads/writes rather than written by hand.
//!
//! Given a set of passes, each declaring [`reads`](crate::Pass::reads) and
//! [`writes`](crate::Pass::writes) over the [`Resource`] vocabulary, the
//! scheduler builds a dependency graph and returns a valid linear order: a pass
//! that reads resource `R` is placed after every enabled pass that writes `R`.
//! Cycles are rejected with a clear error rather than a panic or hang.
//!
//! # How the old structure falls out
//!
//! The original pipeline hardcoded two things: a PreResolver/PostResolver phase
//! split, and a registration order whose comments enumerated the real edges
//! (member-access → global-ref → strings, expr → cf-flatten, …). Both now emerge
//! from the same sort:
//!
//! * The **resolver is a pseudo-pass** that writes
//!   [`ResolvedScopes`](crate::resource::Builtin::ResolvedScopes). Any pass that
//!   used to be "PostResolver" simply reads it; the Pre/Post boundary is wherever
//!   the sort places the resolver pseudo-pass.
//! * **Minify, anti-tamper and emit-patch** are likewise ordinary passes in the
//!   graph (they read/write resources such as
//!   [`MangleControl`](crate::resource::Builtin::MangleControl)).
//!
//! # Determinism
//!
//! Ties (passes with no ordering constraint between them) are broken by
//! [`id`](crate::Pass::id), so the same pass set always yields the same order.
//! The sort is Kahn's algorithm with a deterministic ready-set: at each step the
//! ready pass with the smallest id is emitted.

use crate::resource::Resource;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

/// The scheduler-visible facts about one pass: its stable id and its declared
/// reads/writes. Decoupled from the [`Pass`](crate::Pass) trait (which is generic
/// over `Language`/`Config`) so the sort is a plain data algorithm, unit-testable
/// without constructing real passes or ASTs.
#[derive(Debug, Clone)]
pub struct PassNode {
    /// The pass's stable, unique id — the tie-break key and (for the runner) the
    /// RNG `pass_id`.
    pub id: &'static str,
    /// Resources this pass consumes.
    pub reads: Vec<Resource>,
    /// Resources this pass produces.
    pub writes: Vec<Resource>,
}

impl PassNode {
    /// Construct a node from its id and declared resources.
    pub fn new(id: &'static str, reads: &[Resource], writes: &[Resource]) -> Self {
        PassNode {
            id,
            reads: reads.to_vec(),
            writes: writes.to_vec(),
        }
    }
}

/// Why a schedule could not be produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScheduleError {
    /// Two or more passes form a read/write cycle (each reads a resource another
    /// writes, transitively back to itself). Carries the ids still unscheduled
    /// when progress stalled, sorted for a stable message.
    Cycle {
        /// The ids of the passes left in the cycle, sorted.
        involved: Vec<&'static str>,
    },
    /// Two passes share the same `id`. Ids must be unique within a schedule
    /// because they key both the tie-break order and the RNG.
    DuplicateId(&'static str),
}

impl fmt::Display for ScheduleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScheduleError::Cycle { involved } => write!(
                f,
                "pass schedule has a dependency cycle among: {}",
                involved.join(", ")
            ),
            ScheduleError::DuplicateId(id) => {
                write!(f, "duplicate pass id `{id}` in schedule")
            }
        }
    }
}

impl std::error::Error for ScheduleError {}

/// Produce a valid execution order for `nodes`, or a [`ScheduleError`].
///
/// Edges: for every resource `R`, every writer of `R` is ordered before every
/// reader of `R`. Among passes with no constraint between them, the one with the
/// smaller [`id`](PassNode::id) comes first (deterministic tie-break). Self-edges
/// (a pass that both reads and writes the same resource) are ignored — a pass
/// does not depend on itself.
///
/// Returns the ids in execution order. Use [`schedule_nodes`] if you want the
/// nodes back instead.
pub fn schedule(nodes: &[PassNode]) -> Result<Vec<&'static str>, ScheduleError> {
    Ok(schedule_nodes(nodes)?.into_iter().map(|n| n.id).collect())
}

/// Like [`schedule`] but returns the ordered [`PassNode`]s (cloned), so a runner
/// can keep the reads/writes alongside the order.
pub fn schedule_nodes(nodes: &[PassNode]) -> Result<Vec<PassNode>, ScheduleError> {
    // Reject duplicate ids up front — they break both the tie-break and the RNG.
    let mut seen = BTreeSet::new();
    for n in nodes {
        if !seen.insert(n.id) {
            return Err(ScheduleError::DuplicateId(n.id));
        }
    }

    // Index every writer of each resource, keyed by the resource's canonical
    // string key (Resource itself is intentionally not `Ord` — its identity is
    // the key, so we map over that).
    let mut writers_of: BTreeMap<&'static str, Vec<&'static str>> = BTreeMap::new();
    for n in nodes {
        for &w in &n.writes {
            writers_of.entry(w.key()).or_default().push(n.id);
        }
    }

    // Build edges writer -> reader for each resource a node reads. Dedup edges
    // and skip self-edges (a pass reading what it also writes).
    let mut succ: BTreeMap<&'static str, BTreeSet<&'static str>> = BTreeMap::new();
    let mut indegree: BTreeMap<&'static str, usize> = BTreeMap::new();
    for n in nodes {
        indegree.entry(n.id).or_insert(0);
        succ.entry(n.id).or_default();
    }
    for reader in nodes {
        for r in &reader.reads {
            if let Some(writers) = writers_of.get(r.key()) {
                for &writer in writers {
                    if writer == reader.id {
                        continue; // self-edge: ignore
                    }
                    // Insert edge writer -> reader; bump reader indegree once.
                    if succ.get_mut(writer).unwrap().insert(reader.id) {
                        *indegree.get_mut(reader.id).unwrap() += 1;
                    }
                }
            }
        }
    }

    // Kahn's algorithm with a deterministic ready set (BTreeSet ⇒ smallest id
    // first), giving a stable, reproducible order for any tie.
    let mut ready: BTreeSet<&'static str> = indegree
        .iter()
        .filter(|&(_, &d)| d == 0)
        .map(|(&id, _)| id)
        .collect();

    let by_id: BTreeMap<&'static str, &PassNode> = nodes.iter().map(|n| (n.id, n)).collect();
    let mut order: Vec<PassNode> = Vec::with_capacity(nodes.len());

    while let Some(&id) = ready.iter().next() {
        ready.remove(id);
        order.push((*by_id[id]).clone());
        // Relax successors in id order (BTreeSet iteration is sorted).
        let succs: Vec<&'static str> = succ[id].iter().copied().collect();
        for s in succs {
            let d = indegree.get_mut(s).unwrap();
            *d -= 1;
            if *d == 0 {
                ready.insert(s);
            }
        }
    }

    if order.len() != nodes.len() {
        // Whatever still has indegree > 0 is part of (or downstream of) a cycle.
        let scheduled: BTreeSet<&'static str> = order.iter().map(|n| n.id).collect();
        let mut involved: Vec<&'static str> = nodes
            .iter()
            .map(|n| n.id)
            .filter(|id| !scheduled.contains(id))
            .collect();
        involved.sort_unstable();
        return Err(ScheduleError::Cycle { involved });
    }

    Ok(order)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource::Resource;

    /// Index of `id` in `order`. Panics if absent (test bug).
    fn pos(order: &[&'static str], id: &str) -> usize {
        order.iter().position(|x| *x == id).expect("id present")
    }

    /// The real JS edge set, encoded as a fixture (mirrors the ordering comments
    /// in `lang/js/mod.rs`). This is the load-bearing acceptance test.
    fn js_fixture() -> Vec<PassNode> {
        vec![
            // memberaccess writes PropertyLiterals.
            PassNode::new("memberaccess", &[], &[Resource::property_literals()]),
            // globalref reads PropertyLiterals, writes GlobalNameLiterals.
            PassNode::new(
                "globalref",
                &[Resource::property_literals()],
                &[Resource::global_name_literals()],
            ),
            // strings reads both literal sets, writes DecoderAnchor.
            PassNode::new(
                "strings",
                &[Resource::property_literals(), Resource::global_name_literals()],
                &[Resource::decoder_anchor()],
            ),
            // resolver pseudo-pass writes ResolvedScopes.
            PassNode::new("resolver", &[], &[Resource::resolved_scopes()]),
            // expr reads DecoderAnchor.
            PassNode::new("expr", &[Resource::decoder_anchor()], &[]),
            // cfflatten reads DecoderAnchor + ResolvedScopes.
            PassNode::new(
                "cfflatten",
                &[Resource::decoder_anchor(), Resource::resolved_scopes()],
                &[],
            ),
            // deadcode reads DecoderAnchor.
            PassNode::new("deadcode", &[Resource::decoder_anchor()], &[]),
            // idnames reads ResolvedScopes, writes MangleControl.
            PassNode::new(
                "idnames",
                &[Resource::resolved_scopes()],
                &[Resource::mangle_control()],
            ),
            // minify reads MangleControl (runs after idnames decides).
            PassNode::new("minify", &[Resource::mangle_control()], &[]),
        ]
    }

    #[test]
    fn reproduces_valid_order_for_real_edge_set() {
        let order = schedule(&js_fixture()).expect("acyclic");

        // The enumerated real ordering edges all hold.
        assert!(pos(&order, "memberaccess") < pos(&order, "globalref"));
        assert!(pos(&order, "globalref") < pos(&order, "strings"));
        assert!(pos(&order, "memberaccess") < pos(&order, "strings"));
        assert!(pos(&order, "strings") < pos(&order, "expr"));
        assert!(pos(&order, "strings") < pos(&order, "cfflatten"));
        assert!(pos(&order, "strings") < pos(&order, "deadcode"));
        assert!(pos(&order, "resolver") < pos(&order, "cfflatten"));
        assert!(pos(&order, "resolver") < pos(&order, "idnames"));
        assert!(pos(&order, "idnames") < pos(&order, "minify"));
    }

    #[test]
    fn order_is_deterministic_across_runs() {
        let a = schedule(&js_fixture()).unwrap();
        let b = schedule(&js_fixture()).unwrap();
        assert_eq!(a, b, "same pass set must give identical order");
    }

    #[test]
    fn independent_passes_tie_break_by_id() {
        // Two passes with no constraint between them: order is purely by id.
        let nodes = vec![
            PassNode::new("zeta", &[], &[]),
            PassNode::new("alpha", &[], &[]),
            PassNode::new("mu", &[], &[]),
        ];
        let order = schedule(&nodes).unwrap();
        assert_eq!(order, vec!["alpha", "mu", "zeta"]);
    }

    #[test]
    fn cycle_is_rejected_not_panicked() {
        // a reads R1 (written by b); b reads R2 (written by a) → cycle.
        let r1 = Resource::custom("r1");
        let r2 = Resource::custom("r2");
        let nodes = vec![
            PassNode::new("a", &[r1], &[r2]),
            PassNode::new("b", &[r2], &[r1]),
        ];
        let err = schedule(&nodes).unwrap_err();
        match err {
            ScheduleError::Cycle { involved } => assert_eq!(involved, vec!["a", "b"]),
            other => panic!("expected cycle, got {other:?}"),
        }
    }

    #[test]
    fn duplicate_id_is_rejected() {
        let nodes = vec![
            PassNode::new("dup", &[], &[]),
            PassNode::new("dup", &[], &[]),
        ];
        assert_eq!(
            schedule(&nodes).unwrap_err(),
            ScheduleError::DuplicateId("dup")
        );
    }

    #[test]
    fn self_read_write_is_not_a_cycle() {
        // A pass that both reads and writes the same resource does not depend on
        // itself — it must still schedule.
        let r = Resource::custom("acc");
        let nodes = vec![PassNode::new("solo", &[r], &[r])];
        assert_eq!(schedule(&nodes).unwrap(), vec!["solo"]);
    }

    #[test]
    fn multiple_writers_all_precede_a_reader() {
        // Two passes write R; the reader must follow both.
        let r = Resource::custom("multi");
        let nodes = vec![
            PassNode::new("reader", &[r], &[]),
            PassNode::new("writer_b", &[], &[r]),
            PassNode::new("writer_a", &[], &[r]),
        ];
        let order = schedule(&nodes).unwrap();
        assert!(pos(&order, "writer_a") < pos(&order, "reader"));
        assert!(pos(&order, "writer_b") < pos(&order, "reader"));
    }
}
