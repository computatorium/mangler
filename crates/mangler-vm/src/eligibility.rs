//! Virtualization eligibility — a thin `reject` table over the shared
//! `analysis::eligibility::body_classify` walker (E1), plus the D2 sloppy-mode
//! `arguments`-aliasing soundness bail.
//!
//! The structural reject set is deliberately small: after the Tier-1/Tier-2
//! expansion (try/switch/loops/for-of/destructuring/spread, D4 tagged templates,
//! D5 nested closures) the ONLY permanent structural skips are the dynamic-scope
//! constructs the flat-slot VM cannot model — `with`, direct `eval` — plus
//! `await`/`yield` (the caller has already excluded async/generators). Everything
//! else is handed to the compiler ([`crate::compile`]), which is the final
//! authority and bails on any residual unsupported shape.
//!
//! `arguments_alias` is the one NON-structural check kept here: in sloppy mode
//! `arguments[i]` aliases the i-th parameter, but the VM materializes a plain
//! snapshot array for `arguments`, which breaks that alias. Strictness is not known
//! statically (PreResolver), so the body bails conservatively when it BOTH uses
//! `arguments` AND writes a parameter or an `arguments` element — the only case the
//! snapshot would make observable.

use mangler_jsast::analysis::eligibility::{body_classify, Probe, SkipMethodWrappers};
use std::collections::HashSet;
use swc_core::ecma::ast::*;
use swc_core::ecma::visit::{Visit, VisitWith};

pub use mangler_jsast::analysis::eligibility::Eligibility;

/// Structural eligibility for the body (generator/async checked by caller).
/// Capture-mutability is enforced by `compile_body`, which bails on a write
/// to a captured binding (read-only capture only).
///
/// `params` supplies the function's parameter names so the D2 `arguments`
/// sloppy-aliasing bail can detect a write to a parameter (see `arguments_alias`).
///
/// The structural rejects (`with`/`await`/`yield`/direct-`eval`, plus the
/// nested-fn/arrow skip) run through the shared [`body_classify`] walker. The D2
/// sloppy-`arguments`-aliasing bail is a *non-structural* write-scan (it needs param
/// context and inspects writes), so per the E1 scope rule it stays a caller-side
/// post-check here, applied only when the structural walk found no reason.
pub fn classify_body(params: &[Param], body: &BlockStmt) -> Eligibility {
    // virtualize descended into method wrappers (only the inner fn/arrow was skipped).
    match body_classify(body, SkipMethodWrappers::InnerOnly, &reject) {
        Eligibility::Skip(r) => Eligibility::Skip(r),
        // §5a case 3: `arguments.callee`/`.caller` is an irreducible strict/sloppy
        // divergence — strict throws on access, but the VM materializes `arguments` as
        // a plain array so the access is silently `undefined`. There is no sound VM
        // shape for it, so bail (stay native) regardless of mode.
        Eligibility::Eligible if reads_arguments_callee_or_caller(body) => {
            Eligibility::Skip("arguments_callee")
        }
        Eligibility::Eligible if arguments_alias(params, body) => {
            Eligibility::Skip("arguments_alias")
        }
        Eligibility::Eligible => Eligibility::Eligible,
    }
}

/// §5a case 3: does `body` read `arguments.callee` or `arguments.caller`? Mirrors the
/// `arguments_alias` post-check shape: a write-free member-access scan that does NOT
/// descend into nested functions/arrows (each has its OWN `arguments` and is checked
/// when its own chunk compiles). Matches `arguments.callee`, `arguments.caller`,
/// `arguments["callee"]`, and `arguments["caller"]`.
fn reads_arguments_callee_or_caller(body: &BlockStmt) -> bool {
    struct Scan {
        hit: bool,
    }
    impl Scan {
        fn is_arguments(e: &Expr) -> bool {
            match e {
                Expr::Ident(id) => id.sym.as_ref() == "arguments",
                Expr::Paren(p) => Self::is_arguments(&p.expr),
                _ => false,
            }
        }
    }
    impl Visit for Scan {
        fn visit_member_expr(&mut self, n: &MemberExpr) {
            if Self::is_arguments(&n.obj) {
                let key = match &n.prop {
                    MemberProp::Ident(id) => Some(id.sym.as_ref().to_string()),
                    MemberProp::Computed(c) => match &*c.expr {
                        Expr::Lit(Lit::Str(s)) => s.value.as_str().map(|v| v.to_string()),
                        _ => None,
                    },
                    MemberProp::PrivateName(_) => None,
                };
                if matches!(key.as_deref(), Some("callee") | Some("caller")) {
                    self.hit = true;
                }
            }
            n.visit_children_with(self);
        }
        // Nested functions/arrows have their own `arguments`; checked in their chunk.
        fn visit_function(&mut self, _: &Function) {}
        fn visit_arrow_expr(&mut self, _: &ArrowExpr) {}
    }
    let mut s = Scan { hit: false };
    body.visit_with(&mut s);
    s.hit
}

/// virtualize's structural reject set, evaluated at the shared walker's visit points.
/// D4 removed tagged-template and D5 removed the nested-fn bails, so only the
/// permanent dynamic-scope skips (`with`, direct `eval`) plus `await`/`yield` remain.
fn reject(p: Probe) -> Option<&'static str> {
    match p {
        Probe::With(_) => Some("with"),
        Probe::Await(_) => Some("await"),
        Probe::Yield(_) => Some("yield"),
        Probe::DirectEvalCall(_) => Some("direct_eval"),
        // cfflatten's control-flow constructs are all virtualize-eligible (Tier-1).
        Probe::Try(_)
        | Probe::Switch(_)
        | Probe::DoWhile(_)
        | Probe::ForIn(_)
        | Probe::ForOf(_)
        | Probe::Labeled(_)
        | Probe::For(_) => None,
    }
}

/// Collect the simple-ident names bound by the parameter list (recursing into
/// default / destructuring / rest patterns). Only these names can be *written*
/// to a positional/aliased slot; a destructuring-leaf name still maps to a slot
/// that, in sloppy mode, does NOT alias `arguments[i]`, but we include leaves
/// conservatively — a write to one still keeps the function un-virtualized,
/// which is sound (never a miscompile).
fn collect_param_names(params: &[Param], out: &mut HashSet<String>) {
    fn walk(pat: &Pat, out: &mut HashSet<String>) {
        match pat {
            Pat::Ident(bi) => {
                out.insert(bi.id.sym.to_string());
            }
            Pat::Assign(ap) => walk(&ap.left, out),
            Pat::Rest(r) => walk(&r.arg, out),
            Pat::Array(arr) => {
                for el in arr.elems.iter().flatten() {
                    walk(el, out);
                }
            }
            Pat::Object(obj) => {
                for prop in &obj.props {
                    match prop {
                        ObjectPatProp::KeyValue(kv) => walk(&kv.value, out),
                        ObjectPatProp::Assign(a) => {
                            out.insert(a.key.id.sym.to_string());
                        }
                        ObjectPatProp::Rest(r) => walk(&r.arg, out),
                    }
                }
            }
            Pat::Expr(_) | Pat::Invalid(_) => {}
        }
    }
    for p in params {
        walk(&p.pat, out);
    }
}

/// D2 sloppy-aliasing soundness bail. In sloppy mode `arguments[i]` aliases the
/// `i`-th parameter, so mutating one is observable through the other. The VM
/// materializes a *snapshot* array for `arguments`, which breaks that alias.
/// Strictness is not statically known here, so we bail conservatively when BOTH:
///   (a) the body uses `arguments`, AND
///   (b) it could observe aliasing — it WRITES a parameter (assign / `++`/`--` /
///       compound / destructuring-assignment target on a param name) OR WRITES an
///       `arguments` element (`arguments[i] = …`, `arguments[i]++`, etc).
/// When neither write occurs (read `arguments.length`, index-read, iterate,
/// `f.apply(x, arguments)`) the snapshot is exactly equivalent.
fn arguments_alias(params: &[Param], body: &BlockStmt) -> bool {
    let mut param_names = HashSet::new();
    collect_param_names(params, &mut param_names);
    let mut a = AliasScan {
        params: &param_names,
        uses_arguments: false,
        aliasing_write: false,
    };
    body.visit_with(&mut a);
    a.uses_arguments && a.aliasing_write
}

struct AliasScan<'a> {
    params: &'a HashSet<String>,
    uses_arguments: bool,
    aliasing_write: bool,
}

impl AliasScan<'_> {
    /// True if `e` (paren-peeled) is a bare `arguments` reference.
    fn is_arguments(e: &Expr) -> bool {
        match e {
            Expr::Ident(id) => id.sym.as_ref() == "arguments",
            Expr::Paren(p) => Self::is_arguments(&p.expr),
            _ => false,
        }
    }

    /// Inspect a write target (assignment LHS or `++`/`--` operand). Marks
    /// `aliasing_write` if the target is a parameter name or an `arguments[...]`
    /// member access.
    fn mark_target(&mut self, target: &Expr) {
        match target {
            Expr::Ident(id) => {
                if self.params.contains(id.sym.as_ref()) {
                    self.aliasing_write = true;
                }
            }
            Expr::Paren(p) => self.mark_target(&p.expr),
            Expr::Member(m) if Self::is_arguments(&m.obj) => self.aliasing_write = true,
            _ => {}
        }
    }

    /// Inspect an assignment-pattern target (used for destructuring assignment
    /// `[a] = x` / `({a} = x)` and simple-ident LHS reached via `Pat`).
    fn mark_pat_target(&mut self, pat: &Pat) {
        match pat {
            Pat::Ident(bi) => {
                if self.params.contains(bi.id.sym.as_ref()) {
                    self.aliasing_write = true;
                }
            }
            Pat::Expr(e) => self.mark_target(e),
            Pat::Assign(ap) => self.mark_pat_target(&ap.left),
            Pat::Rest(r) => self.mark_pat_target(&r.arg),
            Pat::Array(arr) => {
                for el in arr.elems.iter().flatten() {
                    self.mark_pat_target(el);
                }
            }
            Pat::Object(obj) => {
                for prop in &obj.props {
                    match prop {
                        ObjectPatProp::KeyValue(kv) => self.mark_pat_target(&kv.value),
                        ObjectPatProp::Assign(a) => {
                            if self.params.contains(a.key.id.sym.as_ref()) {
                                self.aliasing_write = true;
                            }
                        }
                        ObjectPatProp::Rest(r) => self.mark_pat_target(&r.arg),
                    }
                }
            }
            Pat::Invalid(_) => {}
        }
    }
}

impl Visit for AliasScan<'_> {
    fn visit_ident(&mut self, n: &Ident) {
        if n.sym.as_ref() == "arguments" {
            self.uses_arguments = true;
        }
    }
    fn visit_assign_expr(&mut self, n: &AssignExpr) {
        match &n.left {
            AssignTarget::Simple(SimpleAssignTarget::Ident(bi)) => {
                if self.params.contains(bi.id.sym.as_ref()) {
                    self.aliasing_write = true;
                }
            }
            AssignTarget::Simple(SimpleAssignTarget::Member(m)) => {
                if Self::is_arguments(&m.obj) {
                    self.aliasing_write = true;
                }
            }
            AssignTarget::Simple(SimpleAssignTarget::Paren(p)) => self.mark_target(&p.expr),
            AssignTarget::Pat(AssignTargetPat::Array(arr)) => {
                for el in arr.elems.iter().flatten() {
                    self.mark_pat_target(el);
                }
            }
            AssignTarget::Pat(AssignTargetPat::Object(obj)) => {
                for prop in &obj.props {
                    match prop {
                        ObjectPatProp::KeyValue(kv) => self.mark_pat_target(&kv.value),
                        ObjectPatProp::Assign(a) => {
                            if self.params.contains(a.key.id.sym.as_ref()) {
                                self.aliasing_write = true;
                            }
                        }
                        ObjectPatProp::Rest(r) => self.mark_pat_target(&r.arg),
                    }
                }
            }
            _ => {}
        }
        n.visit_children_with(self);
    }
    fn visit_update_expr(&mut self, n: &UpdateExpr) {
        self.mark_target(&n.arg);
        n.visit_children_with(self);
    }
    // D5: a nested function/arrow has its OWN `arguments` and its own params; it is
    // virtualized as a separate chunk and checked there. Do not let its `arguments`
    // use or param writes count toward the OUTER function's sloppy-aliasing bail.
    fn visit_function(&mut self, _: &Function) {}
    fn visit_arrow_expr(&mut self, _: &ArrowExpr) {}
}

#[cfg(test)]
mod tests {
    use super::{classify_body, Eligibility};
    use crate::test_support::{parse_fn_body, parse_fn_with_params};

    /// Classify a body parsed via `parse_fn_body` (no params).
    fn classify(body: &swc_core::ecma::ast::BlockStmt) -> Eligibility {
        classify_body(&[], body)
    }

    #[test]
    fn rejects_remaining_unsupported() {
        // `with` is a permanent skip. try/catch and for-of are eligible (Tier-1);
        // nested functions/arrows are now eligible too (D5 closures).
        assert!(matches!(
            classify(&parse_fn_body("with(o){ x; }")),
            Eligibility::Skip("with")
        ));
        assert!(matches!(
            classify(&parse_fn_body("function g(){ return 1; }")),
            Eligibility::Eligible
        ));
        assert!(matches!(
            classify(&parse_fn_body("var h = () => 1;")),
            Eligibility::Eligible
        ));
    }

    #[test]
    fn first_reason_wins_dfs_order() {
        // Two reject-triggers in one body must report the FIRST in DFS order. Here a
        // `with` precedes a direct `eval`, so the unified path must report `with`.
        assert!(matches!(
            classify(&parse_fn_body("with(o){ x; } var r = eval(s);")),
            Eligibility::Skip("with")
        ));
        // Reversed source order: the direct `eval` now precedes the `with`.
        assert!(matches!(
            classify(&parse_fn_body("var r = eval(s); with(o){ x; }")),
            Eligibility::Skip("direct_eval")
        ));
    }

    #[test]
    fn accepts_try_catch_and_for_of() {
        assert!(matches!(
            classify(&parse_fn_body("try{ f(); }catch(e){ g(e); }")),
            Eligibility::Eligible
        ));
        assert!(matches!(
            classify(&parse_fn_body("for (var x of y) { g(x); }")),
            Eligibility::Eligible
        ));
    }

    #[test]
    fn accepts_loops_and_calls() {
        let body = parse_fn_body("var s=0; for(var i=0;i<n;i++){ s=s+i; } return s;");
        assert!(matches!(classify(&body), Eligibility::Eligible));
    }

    #[test]
    fn arguments_read_only_is_eligible() {
        // Read-only `arguments` uses no longer bail (D2): the VM snapshots a plain
        // array. length / index-read / iterate / `.apply` forwarding are all fine.
        for src in [
            "function(){ return arguments.length; }",
            "function(){ var s=0; for(var i=0;i<arguments.length;i++) s+=arguments[i]; return s; }",
            "function(){ return f.apply(null, arguments); }",
            "function(a,b){ return a + b + arguments.length; }",
        ] {
            let (params, body) = parse_fn_with_params(src);
            assert!(
                matches!(classify_body(&params, &body), Eligibility::Eligible),
                "`{src}` must be eligible"
            );
        }
    }

    #[test]
    fn arguments_with_aliasing_write_bails() {
        // Uses `arguments` AND writes a param (or an `arguments` element) => bail,
        // because sloppy-mode aliasing would be observable and a snapshot breaks it.
        for src in [
            "function(a){ a = 9; return arguments[0]; }",         // simple param write
            "function(a){ a++; return arguments.length; }",       // ++ on param
            "function(a){ a += 1; return arguments[0]; }",        // compound on param
            "function(a){ arguments[0] = 7; return a; }",         // write arguments elem
            "function(a){ [a] = [5]; return arguments[0]; }",     // destructuring target
            "function(a){ arguments[0]++; return a; }",           // ++ on arguments elem
        ] {
            let (params, body) = parse_fn_with_params(src);
            assert!(
                matches!(classify_body(&params, &body), Eligibility::Skip("arguments_alias")),
                "`{src}` must bail arguments_alias"
            );
        }
    }

    #[test]
    fn arguments_callee_caller_bails() {
        // §5a case 3: `arguments.callee`/`.caller` is an irreducible strict/sloppy
        // divergence (strict throws on access, the VM's plain-array `arguments` does
        // not), so any body reading it bails — stays native.
        for src in [
            "function(){ return arguments.callee; }",
            "function(){ return arguments.caller; }",
            "function(){ return arguments['callee']; }",
            "function(){ return arguments[\"caller\"].x; }",
        ] {
            let (params, body) = parse_fn_with_params(src);
            assert!(
                matches!(classify_body(&params, &body), Eligibility::Skip("arguments_callee")),
                "`{src}` must bail arguments_callee"
            );
        }
        // A nested function's `arguments.callee` does NOT bail the outer body (each
        // function has its own `arguments`, checked when its own chunk compiles).
        let (params, body) =
            parse_fn_with_params("function(){ var g = function(){ return arguments.callee; }; return g; }");
        assert!(matches!(classify_body(&params, &body), Eligibility::Eligible));
        // Plain `arguments.length` / element read is still eligible.
        let (params, body) = parse_fn_with_params("function(){ return arguments.length; }");
        assert!(matches!(classify_body(&params, &body), Eligibility::Eligible));
    }

    #[test]
    fn param_write_without_arguments_is_eligible() {
        // Writing a param is fine on its own — only the COMBINATION with `arguments`
        // use triggers the aliasing bail.
        let (params, body) = parse_fn_with_params("function(a){ a = 9; return a; }");
        assert!(matches!(classify_body(&params, &body), Eligibility::Eligible));
    }

    #[test]
    fn keeps_permanent_skips() {
        // The audit: after the full Tier-1/Tier-2 expansion (incl. D5 nested
        // closures), eligibility rejects ONLY the permanent skips — `with` (dynamic
        // scope) and direct `eval` (caller-scope access). Read-only `arguments` (D2),
        // tagged templates (D4), and nested functions/arrows (D5) are all eligible.
        // `await`/`yield` are also kept but don't parse in this sloppy/sync helper.
        for (src, reason) in [
            ("with(o){ x; }", "with"),
            ("var r = eval(s);", "direct_eval"),
        ] {
            match classify(&parse_fn_body(src)) {
                Eligibility::Skip(r) => assert_eq!(r, reason, "for `{src}`"),
                Eligibility::Eligible => panic!("`{src}` must still skip"),
            }
        }
    }

    #[test]
    fn nested_functions_and_arrows_are_eligible() {
        // D5: nested `function`/`arrow` are virtualized as their own chunks, so the
        // OUTER function's structural eligibility no longer rejects them. (The nested
        // body's own unsupported constructs bail when it is compiled, not here.)
        for src in [
            "function inc(){ c++; return c; } return inc;",
            "return x => y => x + y;",
            "var f = function self(n){ return n<=1?1:n*self(n-1); }; return f(5);",
            "function a(){ return 1; } function b(){ return 2; } return a()+b();",
        ] {
            assert!(
                matches!(classify(&parse_fn_body(src)), Eligibility::Eligible),
                "`{src}` must be eligible"
            );
        }
    }

    #[test]
    fn accepts_tagged_template() {
        // D4: tagged templates are now structurally eligible (lowered to a cached
        // frozen template object + call on the tag). Both bare and member tags.
        for src in [
            "var t = tag`x${1}`;",
            "var t = obj.tag`a${b}c`;",
            "var t = tag`only-quasi`;",
        ] {
            assert!(
                matches!(classify(&parse_fn_body(src)), Eligibility::Eligible),
                "`{src}` must be eligible"
            );
        }
    }

    #[test]
    fn accepts_all_tier1_constructs() {
        // Every Tier-1 construct passes the structural gate (compile_body is the
        // authority that bails on residual unsupported shapes).
        for src in [
            "switch (x) { case 1: break; default: g(); }",          // switch
            "outer: for (;;) { break outer; }",                     // labeled
            "var a = o?.b?.c;",                                     // optional chaining
            "delete o.k;",                                          // delete
            "for (var k in o) { g(k); }",                           // for-in
            "for (var v of o) { g(v); }",                           // for-of
            "try { g(); } catch (e) { h(e); } finally { k(); }",    // try/catch/finally
            "throw e;",                                             // throw
            "var { a, b: c, ...r } = o;",                           // object destructuring
            "var [p, , q = 1, ...t] = o;",                          // array destructuring
            "var z = f(...a);",                                     // call spread
            "var m = { ...o, k: 1 };",                              // object spread
            "var e = ev; e(s);",                                    // indirect eval
        ] {
            assert!(
                matches!(classify(&parse_fn_body(src)), Eligibility::Eligible),
                "`{src}` must be eligible"
            );
        }
    }
}
