//! Shared scope / binding / eligibility queries.
//!
//! These are the canonical predicates WP6 passes call instead of re-deriving the
//! same scope reasoning. Three concerns, one source of truth each:
//!
//! 1. **Dynamic scope** ([`scope`]) — direct `eval` / `with` policy, and the
//!    resolver-mark local/free-global/top-level classification.
//! 2. **Binding names** ([`bindings`]) — which idents a (possibly destructuring)
//!    pattern introduces.
//! 3. **Body eligibility** ([`eligibility`]) — the shared first-reason-wins body
//!    classifier with a nested-fn skip, parameterized by a per-pass `reject`.
//!
//! Ported verbatim (modulo crate paths) from the legacy `src/lang/js/analysis`
//! so the WP6 passes get byte-identical behavior on the new substrate.

pub mod bindings;
pub mod eligibility;
pub mod scope;

pub use bindings::binding_names;
pub use eligibility::{body_classify, Eligibility, Probe, SkipMethodWrappers};
pub use scope::{
    has_dynamic_scope, is_direct_eval_callee, is_free_global, is_local, is_top_level,
};
