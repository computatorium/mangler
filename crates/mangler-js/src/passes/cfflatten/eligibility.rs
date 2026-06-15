//! Control-flow-flattening eligibility — a thin `reject` table over the shared
//! `analysis::eligibility::body_classify` walker, plus the fused gate scan
//! ([`scan_gates`]) the flattener uses on its hot path.
//!
//! The shared walker supplies the depth-first first-reason-wins traversal, the
//! nested-function skip, and the direct-`eval`/`with` policy. This module supplies
//! only cfflatten's reject set (the control-flow constructs the linearizer cannot
//! model) plus its `too_small` statement-count pre-check, which is not a body-walk
//! concern and so stays here.

use std::collections::HashSet;

use mangler_jsast::analysis::eligibility::{body_classify, Probe, SkipMethodWrappers};
use mangler_jsast::analysis::is_direct_eval_callee;
use swc_core::ecma::ast::*;
use swc_core::ecma::visit::{Visit, VisitWith};

pub use mangler_jsast::analysis::eligibility::Eligibility;

/// Classify whether a function/arrow/method body is safe for control-flow flattening.
/// Caller must already have checked that the containing Function is neither generator
/// nor async (this function only walks the body).
pub fn classify_function_body(body: &BlockStmt) -> Eligibility {
    // `too_small` is a statement-count gate, not a body-walk concern: keep it as a
    // caller-side pre-check (mirrors the spec's scope rule).
    if body.stmts.len() <= 2 {
        return Eligibility::Skip("too_small");
    }
    // cfflatten pruned the whole method-wrapper node in its original visitor.
    body_classify(body, SkipMethodWrappers::Whole, &reject)
}

/// cfflatten's structural reject set, evaluated at the shared walker's visit points.
fn reject(p: Probe) -> Option<&'static str> {
    match p {
        Probe::Try(_) => Some("try_catch"),
        Probe::Switch(_) => Some("switch"),
        Probe::DoWhile(_) => Some("do_while"),
        Probe::ForIn(_) => Some("for_in"),
        Probe::ForOf(_) => Some("for_of"),
        Probe::Labeled(_) => Some("labeled"),
        Probe::With(_) => Some("with"),
        Probe::DirectEvalCall(_) => Some("eval"),
        // For-loop with a `let` head whose init/test/update/body creates a closure
        // referencing the binding: flattening would share a single hoisted slot. This
        // is a non-structural scan (it inspects closure bodies for ident refs), so it
        // lives here in the reject table rather than in the shared walker.
        Probe::For(n) => for_let_closure(n),
        // await/yield are not in cfflatten's reject set (caller already excluded
        // async/generator), so they never bail here.
        Probe::Await(_) | Probe::Yield(_) => None,
    }
}

/// `for (let NAME = …; …; …) BODY` where some closure in init/test/update/body
/// references `NAME` — returns the bail reason, else `None`.
fn for_let_closure(n: &ForStmt) -> Option<&'static str> {
    let Some(VarDeclOrExpr::VarDecl(decl)) = &n.init else {
        return None;
    };
    if decl.kind != VarDeclKind::Let {
        return None;
    }
    let names: HashSet<&str> = decl
        .decls
        .iter()
        .filter_map(|d| match &d.name {
            Pat::Ident(bi) => Some(bi.id.sym.as_ref()),
            _ => None,
        })
        .collect();
    if names.is_empty() {
        return None;
    }
    let mut closure_scan = ClosureRefsScanner { names: &names, found: false };
    // Scan init/test/update/body so closures created in the test or update (not just
    // the body) are detected too.
    n.visit_with(&mut closure_scan);
    if closure_scan.found {
        Some("per_iter_let_with_closure")
    } else {
        None
    }
}

/// Scans for any closure body that references any of the names in `names`.
struct ClosureRefsScanner<'a> {
    names: &'a HashSet<&'a str>,
    found: bool,
}

impl<'a> Visit for ClosureRefsScanner<'a> {
    // We need to FIND closures and walk inside them. So we override the
    // function/arrow visitors to descend (NOT no-op) and check their bodies
    // for ident references to any of `names`.
    fn visit_function(&mut self, n: &Function) {
        let mut ref_scan = IdentRefScanner { names: self.names, found: false };
        n.visit_with(&mut ref_scan);
        if ref_scan.found {
            self.found = true;
        }
    }
    fn visit_arrow_expr(&mut self, n: &ArrowExpr) {
        let mut ref_scan = IdentRefScanner { names: self.names, found: false };
        n.visit_with(&mut ref_scan);
        if ref_scan.found {
            self.found = true;
        }
    }
    // Continue into non-closure children to find nested closures.
}

struct IdentRefScanner<'a> {
    names: &'a HashSet<&'a str>,
    found: bool,
}

impl<'a> Visit for IdentRefScanner<'a> {
    fn visit_ident(&mut self, n: &Ident) {
        if self.names.contains(n.sym.as_ref()) {
            self.found = true;
        }
    }
}

// ---------------------------------------------------------------------------
// Fused gate scan
// ---------------------------------------------------------------------------

/// Everything the flattener's per-body gate needs, computed by [`scan_gates`] in
/// a single traversal instead of separate walks.
pub struct BodyGates {
    /// The structural eligibility classification (identical to
    /// [`classify_function_body`], including the `too_small` pre-check).
    pub eligibility: Eligibility,
    /// Any `break`/`continue` outside nested functions/arrows (this check descends
    /// into `class` bodies).
    pub has_break_continue: bool,
    /// A `function f(){…}` declaration nested inside a control-flow construct
    /// (`if`/`while`/`for`/block) rather than directly in the body. Such a
    /// declaration has Annex-B / implementation-defined hoisting the linearizer
    /// cannot model, so the caller must bail. Direct-body declarations are fine
    /// (they are hoisted to the prologue) and do NOT set this flag.
    pub has_nested_fn_decl: bool,
    /// Any non-`var` declaration outside nested functions/arrows/classes.
    pub has_let_const: bool,
    /// No destructuring `let`/`const` bindings, no destructuring assignment
    /// targets, no class declarations — outside nested functions/arrows/classes.
    pub tdz_struct_safe: bool,
    /// Names of `let`/`const` simple bindings declared inside a loop (outside
    /// nested functions/arrows/classes). When non-empty the caller must run
    /// [`super::tdz::loop_let_captured`] before the TDZ rewrite.
    pub loop_let_names: Vec<String>,
}

/// Computes all of the flattener's read-only gate checks in one walk.
///
/// Behavior is bit-for-bit identical to running separate visitors; each check
/// keeps its own traversal boundary (see the per-handler comments on [`GateScan`]).
pub fn scan_gates(body: &BlockStmt) -> BodyGates {
    // `too_small` is a statement-count gate evaluated before any walk, exactly
    // as in `classify_function_body`; the other gate fields are never read by
    // the caller on the Skip path.
    if body.stmts.len() <= 2 {
        return BodyGates {
            eligibility: Eligibility::Skip("too_small"),
            has_break_continue: false,
            has_nested_fn_decl: false,
            has_let_const: false,
            tdz_struct_safe: true,
            loop_let_names: Vec::new(),
        };
    }
    let mut v = GateScan {
        reason: None,
        has_break_continue: false,
        has_nested_fn_decl: false,
        has_let_const: false,
        struct_safe: true,
        loop_let_names: Vec::new(),
        in_class: false,
        classify_off: false,
        loop_depth: 0,
        ctrl_depth: 0,
    };
    // Visit each top-level statement directly (rather than the enclosing
    // `BlockStmt`) so the body's OWN block does not count toward `ctrl_depth`:
    // only genuinely-nested blocks/control-flow bump it. This keeps every other
    // check bit-identical (they ignore `ctrl_depth`).
    for s in &body.stmts {
        s.visit_with(&mut v);
    }
    BodyGates {
        eligibility: match v.reason {
            Some(r) => Eligibility::Skip(r),
            None => Eligibility::Eligible,
        },
        has_break_continue: v.has_break_continue,
        has_nested_fn_decl: v.has_nested_fn_decl,
        has_let_const: v.has_let_const,
        tdz_struct_safe: v.struct_safe,
        loop_let_names: v.loop_let_names,
    }
}

/// The fused visitor. The per-check visitors had *different* traversal
/// boundaries, preserved here with two suppression flags over one full walk:
///
/// * **All checks** skip nested `Function`/`ArrowExpr` subtrees outright.
/// * **`in_class`** — let/const, struct-safety, and loop-let recording all pruned
///   `Class` subtrees wholesale, while break/continue and the eligibility walker
///   descended into them. The fused walk always descends but suppresses the three
///   class-pruned checks' record points.
/// * **`classify_off`** — the eligibility walker (`SkipMethodWrappers::Whole`)
///   pruned the *entire* class-/object-method wrapper node, never visiting even
///   computed keys — but the other checks descend into those wrappers (only
///   stopping at the inner `Function`). The fused walk descends and suppresses only
///   the eligibility probes inside wrappers.
struct GateScan {
    /// First-reason-wins eligibility skip reason.
    reason: Option<&'static str>,
    has_break_continue: bool,
    has_nested_fn_decl: bool,
    has_let_const: bool,
    struct_safe: bool,
    loop_let_names: Vec<String>,
    /// Inside a `Class` subtree: suppress let/const, struct-safety, and
    /// loop-let recording.
    in_class: bool,
    /// Inside a method-wrapper node: suppress eligibility probes.
    classify_off: bool,
    /// Loop depth: > 0 while inside a loop statement.
    loop_depth: usize,
    /// Control-flow nesting depth: > 0 while inside any block-splitting construct
    /// (`if`/`while`/`for`/block). Used to detect function declarations nested
    /// inside control flow (Annex-B hoisting the CFG cannot model).
    ctrl_depth: usize,
}

impl GateScan {
    /// Offer a probe to cfflatten's `reject` table unless suppressed; record
    /// the first reason only (same as the shared walker's `probe`).
    fn probe(&mut self, p: Probe) {
        if !self.classify_off
            && self.reason.is_none()
            && let Some(r) = reject(p)
        {
            self.reason = Some(r);
        }
    }

    fn in_loop<F: FnOnce(&mut Self)>(&mut self, f: F) {
        self.loop_depth += 1;
        self.ctrl_depth += 1;
        f(self);
        self.ctrl_depth -= 1;
        self.loop_depth -= 1;
    }

    /// Visit children inside a (non-loop) block-splitting construct, bumping the
    /// control-flow nesting depth so function declarations nested within are
    /// detected.
    fn in_ctrl<F: FnOnce(&mut Self)>(&mut self, f: F) {
        self.ctrl_depth += 1;
        f(self);
        self.ctrl_depth -= 1;
    }

    /// Visit a method-wrapper's children with eligibility probes suppressed.
    fn in_wrapper<F: FnOnce(&mut Self)>(&mut self, f: F) {
        let prev = self.classify_off;
        self.classify_off = true;
        f(self);
        self.classify_off = prev;
    }
}

impl Visit for GateScan {
    // Nested fn/arrow bodies are gated independently — no check descends.
    fn visit_function(&mut self, _n: &Function) {}
    fn visit_arrow_expr(&mut self, _n: &ArrowExpr) {}

    // --- nested function-declaration detection ------------------------------
    fn visit_fn_decl(&mut self, _n: &FnDecl) {
        // A `function f(){…}` declaration nested inside control flow (`if`/loop/
        // block) has Annex-B hoisting the linearizer cannot model; flag it so the
        // caller bails. Direct-body declarations (ctrl_depth == 0) are hoisted to
        // the prologue and are fine. We do NOT descend into the function body
        // (gated independently); but the GENERATOR/ASYNC carrier of a nested fn
        // decl is irrelevant here — only its placement matters.
        if !self.in_class && self.ctrl_depth > 0 {
            self.has_nested_fn_decl = true;
        }
        // Do not descend — `visit_function` no-ops anyway, and the declaration's
        // own body is gated as an independent body when flattened.
    }
    fn visit_if_stmt(&mut self, n: &IfStmt) {
        // No eligibility probe (`if` is flattenable). The test expression is not a
        // block-splitting context for hoisting; only the consequent/alternate
        // bodies are. Bump control depth around the whole node — a fn decl in the
        // test is impossible (expression position), so this is conservative-safe.
        self.in_ctrl(|s| n.visit_children_with(s));
    }
    fn visit_block_stmt(&mut self, n: &BlockStmt) {
        // A bare nested block: its direct statements are a nested lexical context
        // for fn-decl hoisting.
        self.in_ctrl(|s| n.visit_children_with(s));
    }

    // --- break/continue record points --------------------------------------
    fn visit_break_stmt(&mut self, _n: &BreakStmt) {
        self.has_break_continue = true;
    }
    fn visit_continue_stmt(&mut self, _n: &ContinueStmt) {
        self.has_break_continue = true;
    }

    // --- let/const presence + struct check + loop-let collection -----------
    fn visit_var_decl(&mut self, n: &VarDecl) {
        if !self.in_class && n.kind != VarDeclKind::Var {
            self.has_let_const = true;
            for d in &n.decls {
                match &d.name {
                    Pat::Ident(bi) => {
                        if self.loop_depth > 0 {
                            self.loop_let_names.push(bi.id.sym.to_string());
                        }
                    }
                    // Destructuring let/const binding: TDZ lowering can't model it.
                    _ => self.struct_safe = false,
                }
            }
        }
        n.visit_children_with(self);
    }
    fn visit_assign_expr(&mut self, n: &AssignExpr) {
        // Destructuring assignment targets are not modeled.
        if !self.in_class && matches!(n.left, AssignTarget::Pat(_)) {
            self.struct_safe = false;
        }
        n.visit_children_with(self);
    }
    fn visit_class_decl(&mut self, n: &ClassDecl) {
        // Class declarations are not modeled by the TDZ lowering.
        if !self.in_class {
            self.struct_safe = false;
        }
        // break/continue and the eligibility walker descend into class decls.
        n.visit_children_with(self);
    }
    fn visit_class(&mut self, n: &Class) {
        let prev = self.in_class;
        self.in_class = true;
        n.visit_children_with(self);
        self.in_class = prev;
    }

    // --- method wrappers: eligibility-pruned, others descend ---------------
    fn visit_class_method(&mut self, n: &ClassMethod) {
        self.in_wrapper(|s| n.visit_children_with(s));
    }
    fn visit_private_method(&mut self, n: &PrivateMethod) {
        self.in_wrapper(|s| n.visit_children_with(s));
    }
    fn visit_constructor(&mut self, n: &Constructor) {
        self.in_wrapper(|s| n.visit_children_with(s));
    }
    fn visit_method_prop(&mut self, n: &MethodProp) {
        self.in_wrapper(|s| n.visit_children_with(s));
    }
    fn visit_getter_prop(&mut self, n: &GetterProp) {
        self.in_wrapper(|s| n.visit_children_with(s));
    }
    fn visit_setter_prop(&mut self, n: &SetterProp) {
        self.in_wrapper(|s| n.visit_children_with(s));
    }

    // --- eligibility probe points (+ loop depth where applicable) ----------
    fn visit_with_stmt(&mut self, n: &WithStmt) {
        self.probe(Probe::With(n));
        n.visit_children_with(self);
    }
    fn visit_try_stmt(&mut self, n: &TryStmt) {
        self.probe(Probe::Try(n));
        n.visit_children_with(self);
    }
    fn visit_switch_stmt(&mut self, n: &SwitchStmt) {
        self.probe(Probe::Switch(n));
        n.visit_children_with(self);
    }
    fn visit_labeled_stmt(&mut self, n: &LabeledStmt) {
        self.probe(Probe::Labeled(n));
        n.visit_children_with(self);
    }
    fn visit_do_while_stmt(&mut self, n: &DoWhileStmt) {
        self.probe(Probe::DoWhile(n));
        self.in_loop(|s| n.visit_children_with(s));
    }
    fn visit_for_in_stmt(&mut self, n: &ForInStmt) {
        self.probe(Probe::ForIn(n));
        self.in_loop(|s| n.visit_children_with(s));
    }
    fn visit_for_of_stmt(&mut self, n: &ForOfStmt) {
        self.probe(Probe::ForOf(n));
        self.in_loop(|s| n.visit_children_with(s));
    }
    fn visit_for_stmt(&mut self, n: &ForStmt) {
        self.probe(Probe::For(n));
        self.in_loop(|s| n.visit_children_with(s));
    }
    fn visit_while_stmt(&mut self, n: &WhileStmt) {
        // No eligibility probe (while is flattenable) — loop depth only.
        self.in_loop(|s| n.visit_children_with(s));
    }
    fn visit_await_expr(&mut self, n: &AwaitExpr) {
        // Unreachable in practice (caller excludes async bodies); kept for parity.
        self.probe(Probe::Await(n));
        n.visit_children_with(self);
    }
    fn visit_yield_expr(&mut self, n: &YieldExpr) {
        // Unreachable in practice (caller excludes generator bodies).
        self.probe(Probe::Yield(n));
        n.visit_children_with(self);
    }
    fn visit_call_expr(&mut self, n: &CallExpr) {
        // Shared eval policy: only a *direct* eval is a reject candidate.
        if is_direct_eval_callee(&n.callee) {
            self.probe(Probe::DirectEvalCall(n));
        }
        n.visit_children_with(self);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::passes::cfflatten::test_support::parse_body;

    fn check(src: &str) -> Eligibility {
        classify_function_body(&parse_body(src))
    }

    fn reason(e: Eligibility) -> &'static str {
        match e {
            Eligibility::Eligible => "Eligible",
            Eligibility::Skip(r) => r,
        }
    }

    #[test]
    fn too_small_skipped() {
        match check("var a = 1; return a;") {
            Eligibility::Skip("too_small") => {}
            _ => panic!("expected too_small"),
        }
    }

    #[test]
    fn try_catch_skipped() {
        match check("var a = 1; var b = 2; try { a = b; } catch (e) {} return a;") {
            Eligibility::Skip("try_catch") => {}
            other => panic!("expected try_catch, got {:?}", reason(other)),
        }
    }

    #[test]
    fn for_of_skipped() {
        match check("var a = 0; var arr = [1,2,3]; for (var x of arr) a += x; return a;") {
            Eligibility::Skip("for_of") => {}
            other => panic!("expected for_of, got {:?}", reason(other)),
        }
    }

    #[test]
    fn eval_skipped() {
        match check("var x = 1; var y = 2; var z = eval(\"x + y\"); return z;") {
            Eligibility::Skip("eval") => {}
            other => panic!("expected eval, got {:?}", reason(other)),
        }
    }

    #[test]
    fn per_iter_let_with_closure_skipped() {
        let src = "var a = []; for (let i = 0; i < 3; i++) { a.push(function(){ return i; }); } return a[0]();";
        match check(src) {
            Eligibility::Skip("per_iter_let_with_closure") => {}
            other => panic!("expected per_iter_let_with_closure, got {:?}", reason(other)),
        }
    }

    #[test]
    fn for_with_let_no_closure_is_eligible() {
        let src = "var a = 0; for (let i = 0; i < 5; i++) a += i; var b = a + 1; return b;";
        match check(src) {
            Eligibility::Eligible => {}
            other => panic!("expected Eligible, got {:?}", reason(other)),
        }
    }

    #[test]
    fn plain_eligible_body() {
        let src = "var a = 1; if (a > 0) a = 2; else a = 3; var b = a + 1; return b;";
        match check(src) {
            Eligibility::Eligible => {}
            other => panic!("expected Eligible, got {:?}", reason(other)),
        }
    }

    #[test]
    fn per_iter_let_closure_in_test_is_skipped() {
        let src = "var fns=[]; for (let i=0; (fns.push(function(){return i;}), i<3); i++) {} return fns[0]();";
        match check(src) {
            Eligibility::Skip("per_iter_let_with_closure") => {}
            other => panic!("expected per_iter_let_with_closure, got {:?}", reason(other)),
        }
    }

    // --- fused gate scan (scan_gates) ---------------------------------------

    fn gates(src: &str) -> BodyGates {
        scan_gates(&parse_body(src))
    }

    /// The fused scan's classification must agree with `classify_function_body`.
    #[test]
    fn fused_eligibility_matches_classifier() {
        for src in [
            "var a = 1; return a;",
            "var a = 1; var b = 2; try { a = b; } catch (e) {} return a;",
            "var a = 0; var arr = [1,2,3]; for (var x of arr) a += x; return a;",
            "var x = 1; var y = 2; var z = eval(\"x + y\"); return z;",
            "var a = []; for (let i = 0; i < 3; i++) { a.push(function(){ return i; }); } return a[0]();",
            "var a = 0; for (let i = 0; i < 5; i++) a += i; var b = a + 1; return b;",
            "var a = 1; if (a > 0) a = 2; else a = 3; var b = a + 1; return b;",
            "var fns=[]; for (let i=0; (fns.push(function(){return i;}), i<3); i++) {} return fns[0]();",
            "var a=1; var b=2; try { a=b; } catch(e){} switch(a){ case 1: break; } return a;",
            "var a=1; var b=2; switch(a){ case 1: break; } try { a=b; } catch(e){} return a;",
            "var a=1; var b=2; class C { static { try {} catch(e) {} } } return C;",
            "var a=1; var b=2; var o = { [eval('\"k\"')]() {} }; return o;",
            "var a=1; var b=2; var o = { get g() { try {} catch(e) {} return 1; } }; return o;",
            "var a=1; var b=2; function g(){ try {} catch(e) {} } return g;",
            "var a=1; var b=2; var h = () => eval('1'); return h;",
        ] {
            let body = parse_body(src);
            assert_eq!(
                reason(classify_function_body(&body)),
                reason(scan_gates(&body).eligibility),
                "fused vs shared-walker classification diverged on: {src}"
            );
        }
    }

    #[test]
    fn fused_break_continue_boundaries() {
        assert!(gates("var a=0; for(;;){ a++; break; } return a;").has_break_continue);
        assert!(!gates("var a=1; var b=2; function g(){ for(;;) break; } return g;").has_break_continue);
        assert!(gates("var a=1; var b=2; class C { constructor(){ for(;;){ break; } } } return C;").has_break_continue);
        assert!(gates("var a=1; var b=2; class C { static { for(;;) break; } } return C;").has_break_continue);
        assert!(gates("var a=1; var b=2; var o={ get g(){ for(;;) break; return 1; } }; return o;").has_break_continue);
        assert!(!gates("var a=1; var b=2; class C { m(){ for(;;) break; } } return C;").has_break_continue);
    }

    #[test]
    fn fused_let_const_boundaries() {
        assert!(gates("let a = 1; var b = 2; return a + b;").has_let_const);
        assert!(gates("var a = 1; var b = 2; const c = 3; return c;").has_let_const);
        assert!(!gates("var a=1; var b=2; class C { static { let x = 1; } } return C;").has_let_const);
        assert!(!gates("var a=1; var b=2; function g(){ let x=1; return x; } return g;").has_let_const);
        assert!(gates("var a=1; var b=2; var o={ get g(){ let x=1; return x; } }; return o;").has_let_const);
    }

    #[test]
    fn fused_tdz_struct_safety() {
        assert!(!gates("var a=[1]; let [x] = a; var b=2; return x;").tdz_struct_safe);
        assert!(!gates("var a={x:1}; var x; ({x} = a); return x;").tdz_struct_safe);
        assert!(!gates("var a=1; var b=2; class C {} return C;").tdz_struct_safe);
        assert!(gates("var a=1; var b=2; var C = class {}; return C;").tdz_struct_safe);
        assert!(gates("var [x] = [1]; var b=2; return x + b;").tdz_struct_safe);
        assert!(gates("var a=1; var b=2; function g(){ let [x]=[1]; return x; } return g;").tdz_struct_safe);
        assert!(gates("var a=1; var b=2; var C = class { static { class D {} } }; return C;").tdz_struct_safe);
    }

    #[test]
    fn fused_loop_let_names() {
        let g = gates("var a=0; for (let i=0; i<3; i++) { let x=i; a+=x; } return a;");
        assert_eq!(g.loop_let_names, vec!["i".to_string(), "x".to_string()]);
        let g = gates("var a=0; var o={}; while(a<1){ let w=1; a+=w; } for (var k in o) { let q=2; a+=q; } return a;");
        assert_eq!(g.loop_let_names, vec!["w".to_string(), "q".to_string()]);
        assert!(gates("let a = 1; var b = 2; return a + b;").loop_let_names.is_empty());
        assert!(gates("var a=1; var b=2; class C { static { for(;;){ let x=1; } } } return C;").loop_let_names.is_empty());
        assert!(gates("var a=1; var b=2; function g(){ for(;;){ let x=1; } } return g;").loop_let_names.is_empty());
    }

    #[test]
    fn fused_too_small_short_circuits() {
        let g = gates("let a = 1; return a;");
        assert_eq!(reason(g.eligibility), "too_small");
    }

    #[test]
    fn first_reason_wins_dfs_order() {
        let src = "var a=1; var b=2; try { a=b; } catch(e){} switch(a){ case 1: break; } return a;";
        assert_eq!(reason(check(src)), "try_catch");
        let src2 = "var a=1; var b=2; switch(a){ case 1: break; } try { a=b; } catch(e){} return a;";
        assert_eq!(reason(check(src2)), "switch");
    }
}
