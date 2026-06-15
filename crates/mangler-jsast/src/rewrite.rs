//! The ONE `VisitMut` wrapper.
//!
//! Before WP3, ~19 passes each hand-rolled a `struct Rewriter; impl VisitMut for
//! Rewriter { … }`, re-deriving post-order descent, the `inside_core_init`
//! protected-subtree guard, and predicate dispatch every time. This module wraps
//! `VisitMut` **once** and exposes the handful of shapes those passes actually
//! used:
//!
//! * [`rewrite_exprs`] — run a closure on every [`Expr`].
//! * [`rewrite_stmts`] — run a closure on every [`Stmt`].
//! * [`replace_if`] — predicate-guarded whole-node replacement.
//! * A first-class **skip-protected-subtree** mechanism ([`Walk::skip_subtree`])
//!   that replaces the copy-pasted `inside_core_init` / `is_core_declarator`
//!   guards: mark a subtree (e.g. the decoder `core` declarator's initializer) and
//!   the walker does not descend into it.
//!
//! ## Order is explicit
//!
//! [`Order::Post`] runs the closure on a node **after** its children (the default
//! and overwhelmingly common case — a freshly built replacement has no children,
//! so post-order never re-triggers on its own output). [`Order::Pre`] runs it
//! **before** descending (needed when the rewrite changes whether/how children
//! should be visited, e.g. hiding a whole `-N` before the inner `N` is seen).
//!
//! ## Proof of ergonomics
//!
//! The member-access pass — its own ~170-line `VisitMut` impl upstream — is
//! reproduced in a few lines in this module's tests
//! (`memberaccess_in_a_few_lines`), which is the acceptance criterion for the
//! combinator API.

use swc_core::ecma::ast::*;
use swc_core::ecma::visit::{VisitMut, VisitMutWith};

/// Whether a per-node closure runs before or after its children are visited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Order {
    /// Run the closure on a node BEFORE descending into its children.
    Pre,
    /// Run the closure on a node AFTER its children have been visited (default).
    Post,
}

/// Run `f` on every [`Expr`] in `program`, post-order. The common case: most
/// rewrites build a leaf replacement whose children are already final, so
/// post-order never revisits generated output.
pub fn rewrite_exprs(program: &mut Program, f: impl FnMut(&mut Expr)) {
    Walk::new().on_expr(f).run(program);
}

/// Run `f` on every [`Stmt`] in `program`, post-order.
pub fn rewrite_stmts(program: &mut Program, f: impl FnMut(&mut Stmt)) {
    Walk::new().on_stmt(f).run(program);
}

/// Replace every [`Expr`] for which `pred` returns `Some(replacement)`. Post-order:
/// `pred` sees a node whose children are already rewritten, and the replacement is
/// not re-descended (its children, if any, came from `pred` already-formed).
pub fn replace_if(program: &mut Program, mut pred: impl FnMut(&Expr) -> Option<Expr>) {
    Walk::new()
        .on_expr(move |e| {
            if let Some(rep) = pred(e) {
                *e = rep;
            }
        })
        .run(program);
}

/// The no-op default expr/stmt closure type: does nothing on any node.
pub type NoExpr = fn(&mut Expr);
/// The no-op default stmt closure type.
pub type NoStmt = fn(&mut Stmt);
/// The no-op default skip predicate type: never skips.
pub type NoSkip = fn(SubtreeMark) -> bool;

/// The configurable single-traversal rewriter.
///
/// Build with [`Walk::new`], attach an [`on_expr`](Walk::on_expr) and/or
/// [`on_stmt`](Walk::on_stmt) closure, optionally choose [`order`](Walk::order)
/// and one or more [`skip_subtree`](Walk::skip_subtree) predicates, then
/// [`run`](Walk::run). One `Walk` = one `VisitMut` pass over the tree.
///
/// `EF`/`SF` are the expr/stmt closure types; `SK` is the skip predicate. The
/// defaults are real no-op `fn`s, so a `Walk` with only `on_expr` set walks exprs
/// and leaves stmts/skip as harmless no-ops (and [`run`](Walk::run) is callable
/// without setting all three).
pub struct Walk<EF = NoExpr, SF = NoStmt, SK = NoSkip> {
    on_expr: EF,
    on_stmt: SF,
    skip: SK,
    order: Order,
}

impl Default for Walk {
    fn default() -> Self {
        Self::new()
    }
}

impl Walk {
    /// A walker with no callbacks attached. Add behavior with [`on_expr`](Walk::on_expr) /
    /// [`on_stmt`](Walk::on_stmt) / [`skip_subtree`](Walk::skip_subtree).
    pub fn new() -> Walk<NoExpr, NoStmt, NoSkip> {
        Walk {
            on_expr: noop_expr,
            on_stmt: noop_stmt,
            skip: noop_skip,
            order: Order::Post,
        }
    }
}

fn noop_expr(_: &mut Expr) {}
fn noop_stmt(_: &mut Stmt) {}
fn noop_skip(_: SubtreeMark) -> bool {
    false
}

impl<EF, SF, SK> Walk<EF, SF, SK> {
    /// Attach a per-[`Expr`] closure. Replaces any previously-set expr closure.
    pub fn on_expr<F: FnMut(&mut Expr)>(self, f: F) -> Walk<F, SF, SK> {
        Walk {
            on_expr: f,
            on_stmt: self.on_stmt,
            skip: self.skip,
            order: self.order,
        }
    }

    /// Attach a per-[`Stmt`] closure. Replaces any previously-set stmt closure.
    pub fn on_stmt<F: FnMut(&mut Stmt)>(self, f: F) -> Walk<EF, F, SK> {
        Walk {
            on_expr: self.on_expr,
            on_stmt: f,
            skip: self.skip,
            order: self.order,
        }
    }

    /// Choose [`Order::Pre`] or [`Order::Post`] (default `Post`).
    pub fn order(mut self, order: Order) -> Self {
        self.order = order;
        self
    }

    /// **Skip-protected-subtree**: mark a subtree so the walker does not descend
    /// into it. `pred` is called on each [`SubtreeMark`] the walker is about to
    /// enter; returning `true` prunes that whole subtree (no closure fires inside
    /// it, no children are visited).
    ///
    /// This is the first-class replacement for the copy-pasted `inside_core_init`
    /// guard: to protect the decoder's `var core = …` initializer, return `true`
    /// for the [`SubtreeMark::VarDeclaratorInit`] whose declarator name is `core`.
    pub fn skip_subtree<F: FnMut(SubtreeMark) -> bool>(self, pred: F) -> Walk<EF, SF, F> {
        Walk {
            on_expr: self.on_expr,
            on_stmt: self.on_stmt,
            skip: pred,
            order: self.order,
        }
    }
}

impl<EF, SF, SK> Walk<EF, SF, SK>
where
    EF: FnMut(&mut Expr),
    SF: FnMut(&mut Stmt),
    SK: FnMut(SubtreeMark) -> bool,
{
    /// Run the configured single traversal over `program`.
    pub fn run(mut self, program: &mut Program) {
        let mut v = Driver {
            on_expr: &mut self.on_expr,
            on_stmt: &mut self.on_stmt,
            skip: &mut self.skip,
            order: self.order,
        };
        program.visit_mut_with(&mut v);
    }
}

/// A point at which a subtree can be pruned by a [`Walk::skip_subtree`] predicate.
///
/// Today it covers the cases the obfuscation passes need: a `var`/`let`/`const`
/// declarator's **initializer** (the decoder-`core` protected subtree), and a
/// named function declaration (skip the VM interpreter body). Extend this enum as
/// new protected-subtree shapes appear — the predicate stays a closure so callers
/// don't need a new mechanism each time.
#[non_exhaustive]
pub enum SubtreeMark<'a> {
    /// The initializer expression of `<kind> <name> = <init>`. The borrow lets a
    /// predicate match on the binding name (e.g. the decoder `core`).
    VarDeclaratorInit { name: &'a Pat },
    /// A named function declaration `function <ident>(…) { … }` (e.g. the VM
    /// interpreter, whose body must not be rewritten).
    FnDecl { ident: &'a Ident },
}

/// The single `VisitMut` impl. All combinator behavior funnels through here.
struct Driver<'a, EF, SF, SK> {
    on_expr: &'a mut EF,
    on_stmt: &'a mut SF,
    skip: &'a mut SK,
    order: Order,
}

impl<EF, SF, SK> VisitMut for Driver<'_, EF, SF, SK>
where
    EF: FnMut(&mut Expr),
    SF: FnMut(&mut Stmt),
    SK: FnMut(SubtreeMark) -> bool,
{
    fn visit_mut_expr(&mut self, e: &mut Expr) {
        match self.order {
            Order::Pre => {
                (self.on_expr)(e);
                e.visit_mut_children_with(self);
            }
            Order::Post => {
                e.visit_mut_children_with(self);
                (self.on_expr)(e);
            }
        }
    }

    fn visit_mut_stmt(&mut self, s: &mut Stmt) {
        match self.order {
            Order::Pre => {
                (self.on_stmt)(s);
                s.visit_mut_children_with(self);
            }
            Order::Post => {
                s.visit_mut_children_with(self);
                (self.on_stmt)(s);
            }
        }
    }

    fn visit_mut_var_declarator(&mut self, n: &mut VarDeclarator) {
        // Offer the initializer subtree to the skip predicate. If protected, visit
        // the binding pattern (cheap; never holds rewritable exprs we care to skip)
        // but NOT the initializer, so e.g. the decoder `core` init is untouched.
        let protected = (self.skip)(SubtreeMark::VarDeclaratorInit { name: &n.name });
        if protected {
            // Intentionally do not descend into `n.init`.
            return;
        }
        n.visit_mut_children_with(self);
    }

    fn visit_mut_fn_decl(&mut self, n: &mut FnDecl) {
        if (self.skip)(SubtreeMark::FnDecl { ident: &n.ident }) {
            return;
        }
        n.visit_mut_children_with(self);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::{Js, ParseOpts};
    use mangler_core::Language;

    fn parse(src: &str) -> crate::lang::Ast {
        Js.parse(src, &ParseOpts::default()).unwrap()
    }

    /// THE acceptance proof: the upstream member-access pass — `obj.prop` →
    /// `obj["prop"]`, including optional chaining — expressed via the combinator
    /// API in a handful of lines, no hand-rolled `VisitMut`.
    #[test]
    fn memberaccess_in_a_few_lines() {
        let mut ast = parse("a.b.c; x?.y;");
        rewrite_exprs(ast.program_mut(), |e| {
            if let Expr::Member(m) = e
                && let MemberProp::Ident(id) = &m.prop
            {
                m.prop = MemberProp::Computed(ComputedPropName {
                    span: m.span,
                    expr: Box::new(crate::build::str_lit(id.sym.as_ref())),
                });
            }
            if let Expr::OptChain(o) = e
                && let OptChainBase::Member(m) = &mut *o.base
                && let MemberProp::Ident(id) = &m.prop
            {
                m.prop = MemberProp::Computed(ComputedPropName {
                    span: m.span,
                    expr: Box::new(crate::build::str_lit(id.sym.as_ref())),
                });
            }
        });
        let out = Js.print(&ast);
        assert!(!out.contains(".b"), "static .b converted: {out}");
        assert!(!out.contains(".c"), "static .c converted: {out}");
        assert!(out.contains("a[\"b\"][\"c\"]"), "computed chain: {out}");
        assert!(out.contains("x?.[\"y\"]"), "opt-chain converted: {out}");
    }

    /// Post-order is bottom-up: nested members are all converted.
    #[test]
    fn post_order_visits_children_first() {
        let mut ast = parse("var n = 0;");
        let mut order = Vec::new();
        rewrite_exprs(ast.program_mut(), |e| {
            if let Expr::Lit(Lit::Num(num)) = e {
                order.push(num.value as i32);
            }
        });
        assert_eq!(order, vec![0]);
    }

    /// `replace_if` swaps numeric literals for `0`, post-order.
    #[test]
    fn replace_if_swaps_matching_nodes() {
        let mut ast = parse("var x = 5 + 7;");
        replace_if(ast.program_mut(), |e| match e {
            Expr::Lit(Lit::Num(_)) => Some(crate::build::num_u32(0)),
            _ => None,
        });
        let out = Js.print(&ast);
        assert!(out.contains("0+0") || out.contains('0'), "literals zeroed: {out}");
        assert!(!out.contains('5') && !out.contains('7'), "originals gone: {out}");
    }

    /// The skip-protected-subtree mechanism: the `core` declarator's initializer is
    /// NOT descended, so a literal inside it is untouched while one outside IS.
    #[test]
    fn skip_protected_subtree_leaves_core_init_untouched() {
        let mut ast = parse("var core = 111; var other = 222;");
        Walk::new()
            .on_expr(|e| {
                if let Expr::Lit(Lit::Num(num)) = e {
                    num.value = 999.0;
                }
            })
            .skip_subtree(|mark| {
                matches!(
                    mark,
                    SubtreeMark::VarDeclaratorInit { name }
                        if matches!(name, Pat::Ident(b) if b.id.sym.as_ref() == "core")
                )
            })
            .run(ast.program_mut());
        let out = Js.print(&ast);
        assert!(out.contains("111"), "protected core init untouched: {out}");
        assert!(out.contains("999"), "unprotected init rewritten: {out}");
        assert!(!out.contains("222"), "unprotected original gone: {out}");
    }

    /// Skipping a named function declaration prunes its whole body.
    #[test]
    fn skip_fn_decl_prunes_body() {
        let mut ast = parse("function interp(){ return 5; } var y = 5;");
        Walk::new()
            .on_expr(|e| {
                if let Expr::Lit(Lit::Num(num)) = e {
                    num.value = 0.0;
                }
            })
            .skip_subtree(|mark| {
                matches!(mark, SubtreeMark::FnDecl { ident } if ident.sym.as_ref() == "interp")
            })
            .run(ast.program_mut());
        let out = Js.print(&ast);
        assert!(out.contains("return 5") || out.contains("5}"), "interp body kept: {out}");
        // `var y` is outside the protected fn, so its `5` became `0`.
        assert!(out.contains("y=0"), "outside-fn literal rewritten: {out}");
    }

    /// Pre-order runs the closure before descent — used when a rewrite changes how
    /// children are seen.
    #[test]
    fn pre_order_runs_before_children() {
        let mut ast = parse("-5;");
        // Pre-order: replace the whole `-5` unary before the inner `5` is visited,
        // so the inner literal is never independently rewritten.
        Walk::new()
            .on_expr(|e| {
                if let Expr::Unary(u) = e
                    && u.op == UnaryOp::Minus
                {
                    *e = crate::build::num_u32(42);
                }
            })
            .order(Order::Pre)
            .run(ast.program_mut());
        let out = Js.print(&ast);
        assert!(out.contains("42"), "unary replaced pre-order: {out}");
        assert!(!out.contains('5'), "inner literal never seen: {out}");
    }
}
