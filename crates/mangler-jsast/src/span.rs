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

use swc_core::common::{BytePos, DUMMY_SP, Span};

/// The span stamped on every AST node this crate (or a downstream pass) injects.
///
/// Returns [`DUMMY_SP`] today. This is the single place to change if injected
/// nodes should ever carry synthetic source-map spans.
#[inline]
pub fn injected_span() -> Span {
    DUMMY_SP
}

/// Compiler-owned executable bootstrap. This marker exempts its expression
/// subtree from transformations that depend on the not-yet-initialized decoder.
/// It is distinct from the VM compiler's structural class-factory marker.
pub fn runtime_span() -> Span {
    Span {
        lo: BytePos(0),
        hi: BytePos(2),
    }
}

pub fn is_runtime_span(span: Span) -> bool {
    span == runtime_span()
}

/// Generated suspension state callback. It resumes in its source function's
/// variable environment and does not introduce an observable function scope.
pub fn suspension_entry_span() -> Span {
    Span {
        lo: BytePos(0),
        hi: BytePos(3),
    }
}

pub fn is_suspension_entry_span(span: Span) -> bool {
    span == suspension_entry_span()
}

/// Clear locations obtained by parsing compiler-generated JavaScript fragments.
/// Apply at the producer before mixing the fragment with source AST nodes. Existing
/// zero-offset protocol markers remain authoritative and are preserved.
pub struct GeneratedSpans;
impl swc_core::ecma::visit::VisitMut for GeneratedSpans {
    fn visit_mut_span(&mut self, span: &mut Span) {
        if span.lo.0 != 0 {
            *span = injected_span();
        }
    }
    fn visit_mut_bin_expr(&mut self, expression: &mut swc_core::ecma::ast::BinExpr) {
        crate::deep::walk_binary_mut(expression, self);
    }
}

/// Synthetic runtime-eval carrier. It is lowered as a source environment, then
/// compiled by the eval compiler with its original completion/binding metadata.
pub fn eval_entry_span() -> Span {
    Span {
        lo: BytePos(0),
        hi: BytePos(4),
    }
}

pub fn is_eval_entry_span(span: Span) -> bool {
    span == eval_entry_span()
}

/// A fully generated callable factory whose source behavior is already bytecode.
/// This narrow certificate permits native host protocol templates inside it.
pub fn generated_factory_span() -> Span {
    Span {
        lo: BytePos(0),
        hi: BytePos(5),
    }
}

pub fn is_generated_factory_span(span: Span) -> bool {
    span == generated_factory_span()
}

/// Compiler support declaration referenced only by other generated machinery.
/// Embedding may keep it inside the private runtime closure instead of exporting
/// its name into the surrounding source scope.
pub fn private_runtime_declaration_span() -> Span {
    Span {
        lo: BytePos(0),
        hi: BytePos(7),
    }
}

pub fn is_private_runtime_declaration_span(span: Span) -> bool {
    span == private_runtime_declaration_span()
}

/// Generic compiler protocol support, certified at its producer. It contains
/// no source behavior and may be moved into the captured-intrinsics prologue.
pub fn protocol_helper_span() -> Span {
    Span {
        lo: BytePos(0),
        hi: BytePos(6),
    }
}

pub fn is_protocol_helper_span(span: Span) -> bool {
    span == protocol_helper_span()
}

/// A generated identity-tagged literal carrying only a source template object.
/// Suspension passes preserve this site; bytecode reads its canonical template
/// constant directly instead of allocating a second helper-owned cache.
pub fn template_object_span() -> Span {
    Span {
        lo: BytePos(0),
        hi: BytePos(8),
    }
}

pub fn is_template_object_span(span: Span) -> bool {
    span == template_object_span()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_parser_locations_do_not_replace_protocol_markers() {
        use swc_core::ecma::visit::VisitMut;
        let mut source = Span {
            lo: BytePos(1),
            hi: BytePos(400),
        };
        GeneratedSpans.visit_mut_span(&mut source);
        assert_eq!(source, injected_span());
        for expected in [
            injected_span(),
            runtime_span(),
            suspension_entry_span(),
            protocol_helper_span(),
            private_runtime_declaration_span(),
            lexical_class_reference_span(),
            template_object_span(),
        ] {
            let mut marker = expected;
            GeneratedSpans.visit_mut_span(&mut marker);
            assert_eq!(marker, expected);
        }
    }

    #[test]
    fn injected_span_is_dummy_today() {
        assert_eq!(injected_span(), DUMMY_SP);
    }
}

/// Reference to a caller-owned lexical class operation, without calling it.
/// The generated call carries an operation ID, never source arguments.
/// Suspension emits this marker; class lexical preparation consumes it.
pub fn lexical_class_reference_span() -> Span {
    Span {
        lo: BytePos(0),
        hi: BytePos(9),
    }
}

pub fn is_lexical_class_reference_span(span: Span) -> bool {
    span == lexical_class_reference_span()
}
