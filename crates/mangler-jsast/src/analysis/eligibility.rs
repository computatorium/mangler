//! Shared body-eligibility walker.
//!
//! Two passes — `cfflatten` and `virtualize` — each decide, structurally, whether
//! a function/arrow/method *body* is safe to transform. They reject overlapping but
//! distinct sets of node kinds (control-flow constructs for cfflatten; VM-unsupported
//! constructs for virtualize), yet share the same scaffolding:
//!
//! * a depth-first **first-reason-wins** walk over the body,
//! * the canonical **direct-`eval` / `with`** dynamic-scope policy
//!   ([`is_direct_eval_callee`]),
//! * a **nested-function skip** (each nested `Function`/`ArrowExpr` body is classified
//!   independently, so the outer walk does not descend into it).
//!
//! [`body_classify`] is that scaffolding, parameterized by a `reject` predicate. Each
//! pass supplies a thin closure returning *its* reasons over a small [`Probe`] enum;
//! the traversal, nested-fn skip, and eval/with policy are shared.
//!
//! **Scope (pragmatic, sound).** Only *structural* node-kind rejects move here. Checks
//! that need param/write context rather than a node-kind probe stay caller-side pre/post
//! checks around this walker.

use crate::analysis::is_direct_eval_callee;
use swc_core::ecma::ast::*;
use swc_core::ecma::visit::{Visit, VisitWith};

/// Outcome of classifying a function body for a structural transform.
#[derive(Debug)]
pub enum Eligibility {
    /// No `reject` rule fired: the body is safe to transform.
    Eligible,
    /// A `reject` rule fired; the payload is the first reason (a stable skip tag
    /// for `--verbose` diagnostics), in depth-first first-reason-wins order.
    Skip(&'static str),
}

/// A node the shared walker offers to a pass's `reject` predicate. The walker emits
/// each variant at the same visit point the original per-pass visitors checked it, so
/// the depth-first first-reason-wins ordering is preserved.
pub enum Probe<'a> {
    With(&'a WithStmt),
    Try(&'a TryStmt),
    Switch(&'a SwitchStmt),
    DoWhile(&'a DoWhileStmt),
    ForIn(&'a ForInStmt),
    ForOf(&'a ForOfStmt),
    Labeled(&'a LabeledStmt),
    For(&'a ForStmt),
    Await(&'a AwaitExpr),
    Yield(&'a YieldExpr),
    /// A direct `eval(...)` call (callee is the bare `eval`, parens peeled).
    DirectEvalCall(&'a CallExpr),
}

/// Which nested function-like wrappers the walk prunes wholesale.
///
/// `Function` and `ArrowExpr` bodies are *always* skipped (the shared structural
/// decision: each is classified independently). This knob only controls whether the
/// class-/object-method *wrapper* nodes are pruned as a whole — which also skips their
/// computed keys — versus descended into (visiting keys) while still skipping the inner
/// `Function`/`ArrowExpr`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SkipMethodWrappers {
    /// Prune the whole method-wrapper node (cfflatten's original behavior).
    Whole,
    /// Descend into the wrapper (visit its keys); only the inner fn/arrow is skipped
    /// (virtualize's original behavior).
    InnerOnly,
}

/// Walk a function body, returning [`Eligibility::Skip`] with the **first** reason a
/// `reject` rule returns in depth-first order, else [`Eligibility::Eligible`].
pub fn body_classify(
    body: &FunctionBody,
    skip: SkipMethodWrappers,
    reject: &dyn Fn(Probe) -> Option<&'static str>,
) -> Eligibility {
    let mut w = Walker {
        reject,
        skip,
        reason: None,
    };
    body.visit_with(&mut w);
    match w.reason {
        Some(r) => Eligibility::Skip(r),
        None => Eligibility::Eligible,
    }
}

struct Walker<'r> {
    reject: &'r dyn Fn(Probe) -> Option<&'static str>,
    skip: SkipMethodWrappers,
    reason: Option<&'static str>,
}

impl Walker<'_> {
    /// Offer a probe to the `reject` table, recording the first reason only.
    fn probe(&mut self, p: Probe) {
        if self.reason.is_none()
            && let Some(r) = (self.reject)(p)
        {
            self.reason = Some(r);
        }
    }
}

impl Visit for Walker<'_> {
    fn visit_bin_expr(&mut self, binary: &BinExpr) {
        crate::deep::walk_binary(binary, self);
    }
    fn visit_with_stmt(&mut self, n: &WithStmt) {
        self.probe(Probe::With(n));
    }
    fn visit_try_stmt(&mut self, n: &TryStmt) {
        self.probe(Probe::Try(n));
    }
    fn visit_switch_stmt(&mut self, n: &SwitchStmt) {
        self.probe(Probe::Switch(n));
    }
    fn visit_do_while_stmt(&mut self, n: &DoWhileStmt) {
        self.probe(Probe::DoWhile(n));
    }
    fn visit_for_in_stmt(&mut self, n: &ForInStmt) {
        self.probe(Probe::ForIn(n));
    }
    fn visit_for_of_stmt(&mut self, n: &ForOfStmt) {
        self.probe(Probe::ForOf(n));
    }
    fn visit_labeled_stmt(&mut self, n: &LabeledStmt) {
        self.probe(Probe::Labeled(n));
    }
    fn visit_await_expr(&mut self, n: &AwaitExpr) {
        self.probe(Probe::Await(n));
    }
    fn visit_yield_expr(&mut self, n: &YieldExpr) {
        self.probe(Probe::Yield(n));
    }

    fn visit_for_stmt(&mut self, n: &ForStmt) {
        self.probe(Probe::For(n));
        n.visit_children_with(self);
    }

    fn visit_call_expr(&mut self, n: &CallExpr) {
        if is_direct_eval_callee(&n.callee) {
            self.probe(Probe::DirectEvalCall(n));
            return;
        }
        n.visit_children_with(self);
    }

    // Nested fn/arrow bodies are classified independently — never descend.
    fn visit_function(&mut self, _n: &Function) {}
    fn visit_arrow_expr(&mut self, _n: &ArrowExpr) {}

    // Method wrappers: pruned wholesale, or descended-into (keys visited) per `skip`.
    fn visit_class_method(&mut self, n: &ClassMethod) {
        if self.skip == SkipMethodWrappers::InnerOnly {
            n.visit_children_with(self);
        }
    }
    fn visit_method_prop(&mut self, n: &MethodProp) {
        if self.skip == SkipMethodWrappers::InnerOnly {
            n.visit_children_with(self);
        }
    }
    fn visit_constructor(&mut self, n: &Constructor) {
        if self.skip == SkipMethodWrappers::InnerOnly {
            n.visit_children_with(self);
        }
    }
    fn visit_getter_prop(&mut self, n: &GetterProp) {
        if self.skip == SkipMethodWrappers::InnerOnly {
            n.visit_children_with(self);
        }
    }
    fn visit_setter_prop(&mut self, n: &SetterProp) {
        if self.skip == SkipMethodWrappers::InnerOnly {
            n.visit_children_with(self);
        }
    }
    fn visit_private_method(&mut self, n: &PrivateMethod) {
        if self.skip == SkipMethodWrappers::InnerOnly {
            n.visit_children_with(self);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::{Js, ParseOpts};
    use mangler_core::Language;

    fn parse_body(src: &str) -> FunctionBody {
        let wrapped = format!("function __t() {{ {src} }}");
        let program = Js
            .parse(&wrapped, &ParseOpts::default())
            .unwrap()
            .into_program();
        let first_stmt: Stmt = match program {
            Program::Module(m) => match m.body.into_iter().next().unwrap() {
                ModuleItem::Stmt(s) => s,
                _ => panic!("expected statement"),
            },
            Program::Script(s) => s.body.into_iter().next().unwrap(),
        };
        match first_stmt {
            Stmt::Decl(Decl::Fn(fd)) => fd.function.body.unwrap(),
            _ => panic!("expected function decl"),
        }
    }

    fn reason(e: Eligibility) -> &'static str {
        match e {
            Eligibility::Eligible => "Eligible",
            Eligibility::Skip(r) => r,
        }
    }

    fn ab_reject(p: Probe) -> Option<&'static str> {
        match p {
            Probe::Try(_) => Some("a_try"),
            Probe::Switch(_) => Some("b_switch"),
            _ => None,
        }
    }

    #[test]
    fn first_reason_wins_try_before_switch() {
        let body =
            parse_body("var x = 1; try { x = 2; } catch (e) {} switch (x) { case 1: break; }");
        assert_eq!(
            reason(body_classify(&body, SkipMethodWrappers::Whole, &ab_reject)),
            "a_try"
        );
    }

    #[test]
    fn first_reason_wins_switch_before_try() {
        let body =
            parse_body("var x = 1; switch (x) { case 1: break; } try { x = 2; } catch (e) {}");
        assert_eq!(
            reason(body_classify(&body, SkipMethodWrappers::Whole, &ab_reject)),
            "b_switch"
        );
    }

    #[test]
    fn no_reject_is_eligible() {
        let body = parse_body("var a = 1; var b = a + 1; return b;");
        assert!(matches!(
            body_classify(&body, SkipMethodWrappers::Whole, &|_| None),
            Eligibility::Eligible
        ));
    }

    #[test]
    fn nested_fn_bodies_are_skipped() {
        let r = |p: Probe| match p {
            Probe::DirectEvalCall(_) => Some("eval"),
            _ => None,
        };
        for src in [
            "function g(){ var x = eval('1'); return x; } return g;",
            "var h = () => eval('1');",
        ] {
            let body = parse_body(src);
            assert!(matches!(
                body_classify(&body, SkipMethodWrappers::Whole, &r),
                Eligibility::Eligible
            ));
            assert!(matches!(
                body_classify(&body, SkipMethodWrappers::InnerOnly, &r),
                Eligibility::Eligible
            ));
        }
    }
}
