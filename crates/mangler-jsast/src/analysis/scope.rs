//! Dynamic-scope policy and resolver-mark identifier classification.

use swc_core::common::{Mark, SyntaxContext};
use swc_core::ecma::ast::*;
use swc_core::ecma::visit::{Visit, VisitWith};

/// The ONE direct-`eval` policy, shared by every pass that must bail on dynamic
/// scope. A call is a *direct* eval iff its callee, after peeling redundant
/// parentheses, is the bare identifier `eval`.
///
/// Parentheses do not change direct-eval status — `(eval)(x)` and `((eval))(x)`
/// are still direct evals that can see the caller's locals — so they are peeled.
/// The comma operator DOES change it: `(0, eval)(x)` is an *indirect* eval (its
/// callee is a `SeqExpr`, evaluated in global scope), so it is not matched and is
/// treated as an ordinary captured-global call. A binding literally named `eval`
/// also matches; that is intentionally conservative — it only forgoes an optional
/// transform for that file, it never miscompiles.
pub fn is_direct_eval_callee(callee: &Callee) -> bool {
    let Callee::Expr(e) = callee else {
        return false;
    };
    let mut e: &Expr = e;
    while let Expr::Paren(p) = e {
        e = &p.expr;
    }
    matches!(e, Expr::Ident(id) if id.sym.as_ref() == "eval")
}

/// True if `program` introduces dynamic scope the resolver cannot model — a
/// `with` statement or a direct `eval(...)` (per [`is_direct_eval_callee`])
/// anywhere in the file. Standalone whole-program walk for callers that do not
/// already traverse the tree; passes with a fused collection walk should instead
/// call [`is_direct_eval_callee`] inline to avoid a second traversal.
pub fn has_dynamic_scope(program: &Program) -> bool {
    struct V(bool);
    impl Visit for V {
        fn visit_with_stmt(&mut self, n: &WithStmt) {
            self.0 = true;
            n.visit_children_with(self);
        }
        fn visit_call_expr(&mut self, n: &CallExpr) {
            if is_direct_eval_callee(&n.callee) {
                self.0 = true;
            }
            n.visit_children_with(self);
        }
    }
    let mut v = V(false);
    program.visit_with(&mut v);
    v.0
}

/// A value-namespace `Ident` is a renamable LOCAL iff its context is non-empty
/// (property names / labels keep the empty context) and its outer mark is neither
/// the unresolved (free-global) mark nor the top-level mark — exactly the set swc
/// would mangle under `top_level = false`. Requires a resolved tree.
pub fn is_local(ctxt: SyntaxContext, unresolved: Mark, top_level: Mark) -> bool {
    if ctxt == SyntaxContext::empty() {
        return false;
    }
    let outer = ctxt.outer();
    outer != unresolved && outer != top_level
}

/// A free global: an `Ident` carrying the resolver's `unresolved_mark` — never
/// declared anywhere the resolver could see. Requires a resolved tree.
pub fn is_free_global(ctxt: SyntaxContext, unresolved: Mark) -> bool {
    ctxt != SyntaxContext::empty() && ctxt.outer() == unresolved
}

/// A top-level binding: an `Ident` carrying `top_level_mark`. These are never
/// renamed (external code may reference them). Requires a resolved tree.
pub fn is_top_level(ctxt: SyntaxContext, top_level: Mark) -> bool {
    ctxt != SyntaxContext::empty() && ctxt.outer() == top_level
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::{Js, ParseOpts};
    use mangler_core::Language;

    fn parse(src: &str) -> Program {
        Js.parse(src, &ParseOpts::default()).unwrap().into_program()
    }

    #[test]
    fn direct_eval_forms() {
        assert!(has_dynamic_scope(&parse("eval('x')")));
        assert!(has_dynamic_scope(&parse("(eval)('x')")));
        assert!(has_dynamic_scope(&parse("((eval))('x')")));
    }

    #[test]
    fn indirect_eval_is_not_dynamic() {
        assert!(!has_dynamic_scope(&parse("(0, eval)('x')")));
        assert!(!has_dynamic_scope(&parse("var e = eval; e('x')")));
    }

    #[test]
    fn with_is_dynamic() {
        assert!(has_dynamic_scope(&parse("with (o) { x; }")));
    }

    #[test]
    fn plain_program_is_static() {
        assert!(!has_dynamic_scope(&parse("function f(a){ return a + 1; } f(2);")));
    }
}
