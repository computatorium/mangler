//! Span policy — the ONE seam every injected node's span flows through.
//!
//! Passes and the AST builders never write `DUMMY_SP` directly. They call
//! [`injected_span`] instead, so the day we want injected nodes to carry a
//! synthetic source-map position (for an output source map, or for debugging
//! which pass emitted a node), it is a single-line change here rather than a
//! sweep across every builder and pass.
//!
//! Today it returns [`DUMMY_SP`] — swc's "no real source location" sentinel —
//! which is exactly what every hand-built node uses now. The swc `fixer` pass
//! (run in codegen) is the safety net that repairs any parenthesization/precedence
//! a builder gets subtly wrong, so builders may stay span-agnostic.

use swc_core::common::{Span, DUMMY_SP};

/// The span stamped on every AST node this crate (or a downstream pass) injects.
///
/// Returns [`DUMMY_SP`] today. This is the single place to change if injected
/// nodes should ever carry synthetic source-map spans.
#[inline]
pub fn injected_span() -> Span {
    DUMMY_SP
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injected_span_is_dummy_today() {
        assert_eq!(injected_span(), DUMMY_SP);
    }
}
