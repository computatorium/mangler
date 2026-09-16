//! D1 — mutable capture via boxed cells: the boxing analysis + the
//! enclosing-scope rewriter (a pre-pass that runs BEFORE the main virtualize
//! visit).
//!
//! ## Mechanism (classic closure conversion)
//! A captured outer binding that is *written* (by the VM body or the enclosing
//! scope) cannot be modeled by the flat-slot VM: the VM frame slot is a copy, so a
//! write is lost. The fix boxes such a binding into a one-element **cell** `[v]`
//! shared *by reference* between the enclosing scope and the VM:
//!   * the enclosing scope declares `var x = [init]` (instead of `var x = init`)
//!     and every read/write becomes `x[0]` / `x[0] = …` (this module);
//!   * the inner (virtualized) function captures the *cell* (the thunk's capture
//!     expression stays the bare name `x`, which now evaluates to the array), and
//!     its body reads/writes through the cell via `LoadCell`/`StoreCell`
//!     ([`crate::compile`], driven by the `boxed` set this module produces).
//!
//! Because arrays are reference types, a `StoreCell` in the VM mutates the same
//! array the enclosing scope holds, so the mutation propagates.
//!
//! ## Why a self-contained lexical analysis (no resolver marks)
//! The virtualize pass runs in `Phase::PreResolver` — the tree has NO resolver
//! `SyntaxContext` marks when we run. The spec phrases the eligibility test in
//! terms of resolver `Id`s, but we have none here. We therefore do a conservative
//! *lexical* analysis scoped to ONE enclosing function `F`: we box only `F`'s own
//! function-body bindings (`var` / body-top-level `let`), and we bail whenever a
//! same-named binding could make a lexical decision ambiguous (any nested
//! function that captures the name but is not one we virtualize; a name also bound
//! by an inner scope of `F`; `with`/`eval` dynamic scope). Every uncertainty bails
//! to the read-only-capture status quo — never a miscompile.
//!
//! ## Bail set (stay conservative — leave the binding un-boxed, inner fn
//! un-virtualized-for-mutation)
//! A name is NOT boxed (and any mutating inner capture of it stays bailed) when:
//!   * `F` is the **module top level** (top-level bindings are externally
//!     referenceable — we never rewrite them);
//!   * `F` has **dynamic scope** (`with` / direct `eval`) — the rewrite is unsound
//!     there;
//!   * the name is a **param** of `F` (D1 does not seed a cell for a param — that
//!     needs the `MakeCell` enclosing-JS rewrite reserved for D5);
//!   * the name is a `const`, or is never written, or is not captured by any inner
//!     function we will virtualize;
//!   * the name is captured/referenced by a nested function we are **not**
//!     virtualizing in this pass (it would read the raw cell array, not `x[0]`) —
//!     the D1-alone bail; D5 will lift it;
//!   * the name is also bound by a **distinct** inner block/function scope of `F`
//!     (a shadow), which would make the by-name rewrite imprecise.

use std::collections::{HashMap, HashSet};

use swc_core::common::BytePos;
use swc_core::ecma::ast::*;
use swc_core::ecma::visit::{Visit, VisitMut, VisitMutWith, VisitWith};

use mangler_jsast::analysis::{binding_names, is_direct_eval_callee};

fn walk_binary_chain_mut<V: VisitMut>(binary: &mut BinExpr, visitor: &mut V) {
    let mut pending: Vec<&mut Expr> = vec![&mut binary.right, &mut binary.left];
    while let Some(expr) = pending.pop() {
        if let Expr::Bin(binary) = expr {
            pending.push(&mut binary.right);
            pending.push(&mut binary.left);
        } else {
            expr.visit_mut_with(visitor);
        }
    }
}

/// Returns true if `name` matches `glob`. Invalid globs match nothing. (Ported
/// from the legacy `matcher::matches`; the VM's only consumer of glob matching.)
fn glob_matches(glob: &str, name: &str) -> bool {
    glob::Pattern::new(glob)
        .map(|p| p.matches(name))
        .unwrap_or(false)
}

/// True if a block body begins with its own `"use strict"` directive. Mirrors the
/// legacy `virtualize::has_use_strict_directive`, kept local to the VM crate.
fn has_use_strict_directive(body: &FunctionBody) -> bool {
    mangler_jsast::directives::has_use_strict(&body.stmts)
}

/// True if `body` introduces dynamic scope the rewrite cannot model — a `with`
/// statement or a direct `eval(...)` anywhere in it (including nested functions,
/// which could `eval` into F's scope). Mirrors `analysis::has_dynamic_scope` but
/// operates on a `BlockStmt` (the shared predicate takes a whole `Program`).
fn block_has_dynamic_scope(body: &FunctionBody) -> bool {
    struct V(bool);
    impl Visit for V {
        fn visit_bin_expr(&mut self, n: &BinExpr) {
            crate::compile::walk_binary_chain(n, self);
        }
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
    body.visit_with(&mut v);
    v.0
}

/// The plan produced by the pre-pass: for each inner function we will virtualize
/// (keyed by its body's start `BytePos`), the set of capture names that are boxed
/// cells. `compile_body_boxed` consumes the set for that function. A function with
/// no boxed captures simply has no entry (or an empty set).
#[derive(Debug, Default)]
pub struct BoxPlan {
    by_fn_body: HashMap<BytePos, HashSet<String>>,
}

impl BoxPlan {
    /// The boxed-capture names for the inner function whose body starts at `pos`.
    pub fn boxed_for(&self, pos: BytePos) -> HashSet<String> {
        self.by_fn_body.get(&pos).cloned().unwrap_or_default()
    }

    pub fn is_empty(&self) -> bool {
        self.by_fn_body.is_empty()
    }
}

/// Run the D1 boxing pre-pass over `program`: analyze every (non-top-level)
/// enclosing function, rewrite its body to cell-ify the boxed bindings, and return
/// the `BoxPlan` mapping each virtualized inner function to its boxed-capture set.
///
/// `glob` is the same function-name glob the virtualizer matches on, so we only
/// consider boxing for inner functions the pass will actually virtualize.
pub fn plan_and_rewrite(program: &mut Program, glob: &str) -> BoxPlan {
    let mut plan = BoxPlan::default();
    let mut v = Driver {
        glob,
        plan: &mut plan,
    };
    program.visit_mut_with(&mut v);
    plan
}

/// Walks the program looking for enclosing functions. The module/script top level
/// is NOT an enclosing scope we rewrite (top-level bindings are externally
/// referenceable), so we only act on `Function`/`ArrowExpr` *bodies*.
struct Driver<'a> {
    glob: &'a str,
    plan: &'a mut BoxPlan,
}

impl VisitMut for Driver<'_> {
    fn visit_mut_bin_expr(&mut self, n: &mut BinExpr) {
        walk_binary_chain_mut(n, self);
    }
    fn visit_mut_function(&mut self, f: &mut Function) {
        // Analyze this function AS AN ENCLOSING scope first (so a boxed binding's
        // own body is rewritten), then recurse so deeper nestings are handled too.
        if let Some(body) = &mut f.body {
            analyze_enclosing(&f.params, body, self.glob, self.plan);
        }
        f.visit_mut_children_with(self);
    }
    // Arrow functions can also be enclosing scopes; their expression-or-block body
    // is handled the same way when it is a block. (An expression-bodied arrow has
    // no statements to rewrite and no `var`/`let` to box, so it is skipped.)
    fn visit_mut_arrow_expr(&mut self, a: &mut ArrowExpr) {
        if let ArrowFunctionBody::FunctionBody(body) = &mut *a.body {
            // Arrows have their own param list (`Pat`, not `Param`); D1 does not box
            // params, so an empty param slice keeps the param-exclusion conservative.
            analyze_enclosing(&[], body, self.glob, self.plan);
        }
        a.visit_mut_children_with(self);
    }
}

/// Analyze one enclosing function body `F` and, if any of its bindings are
/// safely boxable, rewrite the body in place and record the per-inner-fn boxed
/// sets in `plan`.
fn analyze_enclosing(params: &[Param], body: &mut FunctionBody, glob: &str, plan: &mut BoxPlan) {
    // 0. Dynamic scope (with / direct eval) anywhere in F defeats static reasoning.
    if block_has_dynamic_scope(body) {
        return;
    }

    // 1. The candidate boxable names: F's function-scoped `var`s + body-top-level
    //    `let` (NOT `const`, NOT params, NOT nested-block lets). These are F's own
    //    bindings whose declarations we control and can cell-ify in place.
    let mut decl = DeclScan {
        var_names: HashSet::new(),
        let_names: HashSet::new(),
        const_names: HashSet::new(),
        shadowed: HashSet::new(),
    };
    decl.scan_body_top_level(body);
    let param_names = collect_param_names(params);

    // 2. Find the inner functions of F that the virtualizer WILL virtualize
    //    (match the glob, structurally eligible, not generator/async/use-strict),
    //    and the set of all OTHER nested functions (any depth). Also collect, per
    //    candidate, the free names it references (its captures) and which of those
    //    it writes.
    //
    // The analysis (immutable borrow of `body`) produces two OWNED results — the
    // final boxed set and the per-inner-fn plan entries — so the borrow can be
    // dropped before the mutable rewrite below.
    let (boxed, plan_entries) = {
        let mut inner = InnerScan {
            glob: glob.to_string(),
            virt: Vec::new(),
            non_virt_refs: HashSet::new(),
        };
        body.visit_with(&mut inner);

        if inner.virt.is_empty() {
            return; // nothing to virtualize here -> nothing to box
        }

        // 3. Names written somewhere F can see them: F's own body writes + inner-fn
        //    writes. (Read-only captures are never boxed.)
        let mut writes = WriteScan {
            written: HashSet::new(),
        };
        body.visit_with(&mut writes);

        // 4. Decide the boxed set for F. A candidate name `x` is boxed iff:
        //    (a) it is a `var` or body-top-level `let` of F (not const/param);
        //    (b) it is written somewhere in F's scope;
        //    (c) it is captured by >=1 inner fn we virtualize AND mutated (by an
        //        inner fn or the enclosing scope) — a cell is only needed for a
        //        capture that is actually written somewhere;
        //    (d) it is NOT referenced by any non-virtualized nested fn (D1-alone);
        //    (e) it is NOT shadowed by a distinct inner scope binding of the name.
        let mut boxed: HashSet<String> = HashSet::new();
        let boxable_decls: HashSet<&String> =
            decl.var_names.iter().chain(decl.let_names.iter()).collect();
        for name in boxable_decls {
            if param_names.contains(name) || decl.const_names.contains(name) {
                continue;
            }
            if !writes.written.contains(name) {
                continue; // read-only -> stay plain capture
            }
            if !inner.virt.iter().any(|g| g.captures.contains(name)) {
                continue; // not captured by any fn we virtualize
            }
            // D1-alone bail: referenced by a non-virtualized nested function.
            if inner.non_virt_refs.contains(name) {
                continue;
            }
            // shadow bail: F declares the same name in a distinct inner scope.
            if decl.shadowed.contains(name) {
                continue;
            }
            boxed.insert(name.clone());
        }

        if boxed.is_empty() {
            return;
        }

        // 5. SOUNDNESS GATE. Boxing rewrites F's body so a boxed binding is a cell.
        //    For that to be sound, EVERY inner fn that captures a boxed name MUST be
        //    virtualized (else it would read the raw cell array, not the value). The
        //    structural eligibility check in `InnerScan` is necessary but not
        //    sufficient — `compile_body` can still bail for reasons it can't see
        //    (surrogate string, too-large, dup param, default-scope, …). So a second
        //    immutable walk (`CompileVerify`) actually compiles each consuming
        //    candidate with its boxed set; if ANY fails, we ABORT boxing for the
        //    whole of F (leave it the read-only-capture status quo) rather than risk
        //    a lost-mutation miscompile.
        let consumer_boxed: HashMap<BytePos, HashSet<String>> = inner
            .virt
            .iter()
            .filter(|g| !g.captures.is_disjoint(&boxed))
            .map(|g| {
                (
                    g.body_pos,
                    g.captures.intersection(&boxed).cloned().collect(),
                )
            })
            .collect();
        let mut verify = CompileVerify {
            want: &consumer_boxed,
            all_ok: true,
        };
        body.visit_with(&mut verify);
        if !verify.all_ok {
            return; // a consumer won't virtualize -> do not box anything for F
        }

        let entries: Vec<(BytePos, HashSet<String>)> = consumer_boxed.into_iter().collect();
        (boxed, entries)
    };

    // 6. Commit: rewrite F's body so every boxed binding becomes a cell, then record
    //    each consuming inner fn's boxed-capture set in the plan.
    rewrite_boxed(body, &boxed);
    for (pos, g_boxed) in plan_entries {
        plan.by_fn_body.insert(pos, g_boxed);
    }
}

/// Collect the simple param names of `F` (recursing destructuring). D1 never boxes
/// a param, so these are excluded from the boxable set.
fn collect_param_names(params: &[Param]) -> HashSet<String> {
    let mut out = HashSet::new();
    for p in params {
        binding_names(&p.pat, &mut |id| {
            out.insert(id.sym.to_string());
        });
    }
    out
}

/// Scans F's body for its own bindings, distinguishing function-scoped `var`,
/// body-top-level `let`, `const`, and names re-bound by a DISTINCT inner scope
/// (block / nested function param/var/let) — a shadow that makes a by-name rewrite
/// imprecise. Does NOT descend into nested function bodies for the var/let/const
/// classification (those are different scopes), but DOES track shadowing names
/// declared anywhere nested.
struct DeclScan {
    var_names: HashSet<String>,
    let_names: HashSet<String>,
    const_names: HashSet<String>,
    /// Names re-bound by a DISTINCT inner scope of F — a nested-block `let`/`const`,
    /// a `catch` binding, or a nested function's name/params/locals — that collide
    /// with a body-top-level candidate name. Such a shadow makes the by-name rewrite
    /// ambiguous, so a shadowed candidate is never boxed.
    shadowed: HashSet<String>,
}

impl DeclScan {
    fn scan_body_top_level(&mut self, body: &FunctionBody) {
        // `var` is function-scoped: collect from the whole body (including nested
        // blocks, but NOT nested functions). `let`/`const` only at the body's top
        // statement level (a nested-block `let` is a different scope we don't box).
        let mut vars = VarScan {
            names: HashSet::new(),
        };
        body.visit_with(&mut vars);
        self.var_names = vars.names;

        for stmt in &body.stmts {
            if let Stmt::Decl(Decl::Var(v)) = stmt {
                let target = match v.kind {
                    VarDeclKind::Let => Some(&mut self.let_names),
                    VarDeclKind::Const => Some(&mut self.const_names),
                    VarDeclKind::Var => None,
                };
                if let Some(set) = target {
                    for d in &v.decls {
                        binding_names(&d.name, &mut |id| {
                            set.insert(id.sym.to_string());
                        });
                    }
                }
            }
        }

        // Shadow detection: any name bound by a nested block (`let`/`const` not at
        // the body top level), a `catch` clause, or a nested function (its own
        // name / params / locals) that ALSO appears as a candidate name above.
        let candidates: HashSet<String> = self
            .var_names
            .iter()
            .chain(self.let_names.iter())
            .cloned()
            .collect();
        let mut sh = ShadowScan {
            candidates: &candidates,
            top_level: true,
            shadowed: HashSet::new(),
        };
        // Visit the body's CHILDREN at top level (visiting `body` itself would fire
        // `visit_block_stmt` on F's own body and wrongly flip `top_level` to false,
        // marking every top-level `let` as a shadow).
        body.visit_children_with(&mut sh);
        self.shadowed = sh.shadowed;
    }
}

/// Detects a candidate name re-bound by a DISTINCT inner scope of F (nested-block
/// `let`/`const`, `catch`, nested-function name/params/locals). Body-top-level
/// `let`/`const` (the candidate declarations themselves) and function-scoped `var`
/// are NOT shadows.
struct ShadowScan<'a> {
    candidates: &'a HashSet<String>,
    /// True only while visiting the body's top statement list (not inside any
    /// nested block / function). A `let`/`const` here is a candidate, not a shadow.
    top_level: bool,
    shadowed: HashSet<String>,
}
impl ShadowScan<'_> {
    fn mark(&mut self, name: &str) {
        if self.candidates.contains(name) {
            self.shadowed.insert(name.to_string());
        }
    }
}
impl Visit for ShadowScan<'_> {
    fn visit_bin_expr(&mut self, n: &BinExpr) {
        crate::compile::walk_binary_chain(n, self);
    }
    fn visit_block_stmt(&mut self, b: &BlockStmt) {
        let prev = self.top_level;
        self.top_level = false;
        b.visit_children_with(self);
        self.top_level = prev;
    }
    fn visit_var_decl(&mut self, v: &VarDecl) {
        // A non-top-level `let`/`const` is an inner-block binding -> shadow.
        if !self.top_level && matches!(v.kind, VarDeclKind::Let | VarDeclKind::Const) {
            for d in &v.decls {
                binding_names(&d.name, &mut |id| self.mark(id.sym.as_ref()));
            }
        }
        v.visit_children_with(self);
    }
    fn visit_catch_clause(&mut self, c: &CatchClause) {
        if let Some(p) = &c.param {
            binding_names(p, &mut |id| self.mark(id.sym.as_ref()));
        }
        c.visit_children_with(self);
    }
    fn visit_fn_decl(&mut self, n: &FnDecl) {
        self.mark(n.ident.sym.as_ref());
        for p in &n.function.params {
            binding_names(&p.pat, &mut |id| self.mark(id.sym.as_ref()));
        }
        // A nested fn's own locals shadow within its body, but they cannot be the
        // SAME slot as F's candidate (different scope) — still, by-name they would
        // collide, so mark them too. Descend to find deeper nested decls.
        n.visit_children_with(self);
    }
    fn visit_fn_expr(&mut self, n: &FnExpr) {
        if let Some(id) = &n.ident {
            self.mark(id.sym.as_ref());
        }
        for p in &n.function.params {
            binding_names(&p.pat, &mut |id| self.mark(id.sym.as_ref()));
        }
        n.visit_children_with(self);
    }
    fn visit_arrow_expr(&mut self, n: &ArrowExpr) {
        for p in &n.params {
            binding_names(p, &mut |id| self.mark(id.sym.as_ref()));
        }
        n.visit_children_with(self);
    }
}

/// `var`-name collector that does NOT descend into nested functions (a nested fn's
/// `var` is its own scope).
struct VarScan {
    names: HashSet<String>,
}
impl Visit for VarScan {
    fn visit_bin_expr(&mut self, n: &BinExpr) {
        crate::compile::walk_binary_chain(n, self);
    }
    fn visit_var_decl(&mut self, v: &VarDecl) {
        if matches!(v.kind, VarDeclKind::Var) {
            for d in &v.decls {
                binding_names(&d.name, &mut |id| {
                    self.names.insert(id.sym.to_string());
                });
            }
        }
        v.visit_children_with(self);
    }
    fn visit_function(&mut self, _: &Function) {}
    fn visit_arrow_expr(&mut self, _: &ArrowExpr) {}
}

/// Per-candidate-inner-function record. (swc's `Visit` does not let a visitor store
/// references to the nodes it visits, so the params/body needed for the compile
/// verification are re-found by `body_pos` in a second immutable walk —
/// `CompileVerify` — rather than stored here.)
struct VirtFn {
    /// Start `BytePos` of the inner function body (the plan key, matched by the
    /// virtualizer against `function.body.span.lo`).
    body_pos: BytePos,
    /// Free names this inner fn references (its capture set, by name). Whether a
    /// captured name is WRITTEN is determined function-wide by `WriteScan` (which
    /// descends into nested fns), so it is not tracked per-candidate here.
    captures: HashSet<String>,
}

/// Scans F's body for nested functions, partitioning them into "will be
/// virtualized" (matches glob + structurally eligible) and "everything else", and
/// computing each candidate's free-name capture/write sets. Only DIRECT-or-deeper
/// nested functions are considered; the candidate set is restricted to those the
/// virtualizer can actually take.
struct InnerScan {
    glob: String,
    virt: Vec<VirtFn>,
    /// Names referenced by a non-virtualized nested function (any depth).
    non_virt_refs: HashSet<String>,
}

impl InnerScan {
    /// Is this inner function one the virtualizer will take? It must match the glob,
    /// be structurally eligible, and not be a generator/async/use-strict body. We do
    /// NOT attempt the full `compile_body` here (that depends on the boxed set);
    /// instead we use the *structural* eligibility, and the virtualizer's own
    /// compile bail remains the final authority. To keep boxing SOUND when a
    /// candidate later fails to compile, we additionally require that the candidate
    /// has a real (non-dummy) body span so the plan key is stable and unique.
    fn is_virtualizable(name: Option<&str>, f: &Function) -> bool {
        if name.is_none() {
            return false; // an anonymous fn expr is never matched/virtualized
        }
        if f.is_generator || f.is_async {
            return false;
        }
        let Some(body) = &f.body else { return false };
        if body.span.lo == BytePos(0) {
            return false; // synthetic body -> no stable plan key
        }
        if has_use_strict_directive(body) {
            return false;
        }
        matches!(
            crate::eligibility::classify_body(&f.params, body),
            crate::eligibility::Eligibility::Eligible
        )
    }

    /// Collect a function's body free-name refs/writes (its capture set), with its
    /// own params+locals excluded.
    fn scan_fn_refs(params: &[Param], body: &FunctionBody) -> FreeRefScan {
        let mut cap = FreeRefScan::new(params);
        cap.collect_locals(body);
        body.visit_with(&mut cap);
        cap
    }
}

impl Visit for InnerScan {
    fn visit_bin_expr(&mut self, n: &BinExpr) {
        crate::compile::walk_binary_chain(n, self);
    }
    // `InnerScan` looks ONLY at the functions directly nested in F (one level); it
    // does NOT recurse into their bodies, because the `Driver` visits every function
    // as a candidate enclosing scope in its own right (so a doubly-nested candidate
    // is handled when its OWN enclosing scope is analyzed). A nested function is
    // either a virtualizable candidate (recorded in `virt`) or a non-virtualized
    // scope whose free refs are the D1-alone bail set (`non_virt_refs`).
    fn visit_fn_decl(&mut self, n: &FnDecl) {
        let name = n.ident.sym.to_string();
        let Some(body) = &n.function.body else { return };
        let cap = Self::scan_fn_refs(&n.function.params, body);
        if InnerScan::is_virtualizable(Some(&name), &n.function) && glob_matches(&self.glob, &name)
        {
            self.virt.push(VirtFn {
                body_pos: body.span.lo,
                captures: cap.refs,
            });
        } else {
            self.non_virt_refs.extend(cap.refs);
        }
    }
    fn visit_fn_expr(&mut self, n: &FnExpr) {
        let name = n.ident.as_ref().map(|i| i.sym.to_string());
        let Some(body) = &n.function.body else { return };
        let cap = Self::scan_fn_refs(&n.function.params, body);
        let virt = name.as_deref().is_some_and(|nm| {
            InnerScan::is_virtualizable(Some(nm), &n.function) && glob_matches(&self.glob, nm)
        });
        if virt {
            self.virt.push(VirtFn {
                body_pos: body.span.lo,
                captures: cap.refs,
            });
        } else {
            self.non_virt_refs.extend(cap.refs);
        }
    }
    fn visit_arrow_expr(&mut self, n: &ArrowExpr) {
        // Arrows are never virtualized -> every free name is a non-virt ref.
        let mut cap = FreeRefScan::new(&[]);
        cap.collect_arrow_params(&n.params);
        match &*n.body {
            ArrowFunctionBody::FunctionBody(body) => {
                cap.collect_locals(body);
                body.visit_with(&mut cap);
            }
            ArrowFunctionBody::Expr(e) => e.visit_with(&mut cap),
        }
        self.non_virt_refs.extend(cap.refs);
    }
    // A bare anonymous `Function` (object/class method, getter/setter) is never
    // virtualized; its free names are non-virt refs. (Named fn decls/exprs are
    // intercepted above before this fires.)
    fn visit_function(&mut self, f: &Function) {
        if let Some(body) = &f.body {
            self.non_virt_refs
                .extend(Self::scan_fn_refs(&f.params, body).refs);
        }
    }
}

/// Second immutable walk that actually COMPILES each consuming candidate (a nested
/// fn whose body `BytePos` is a key of `want`) with its boxed set, to confirm it
/// will virtualize. `all_ok` stays true only if every candidate compiles. This is
/// the soundness gate: boxing is committed only when every fn that captures a boxed
/// name is guaranteed to virtualize (otherwise it would read the raw cell array).
struct CompileVerify<'a> {
    want: &'a HashMap<BytePos, HashSet<String>>,
    all_ok: bool,
}
impl CompileVerify<'_> {
    fn check(&mut self, f: &Function) {
        if let Some(body) = &f.body
            && let Some(g_boxed) = self.want.get(&body.span.lo)
            && crate::compile::compile_body_boxed(&f.params, body, g_boxed).is_err()
        {
            self.all_ok = false;
        }
    }
}
impl Visit for CompileVerify<'_> {
    fn visit_bin_expr(&mut self, n: &BinExpr) {
        crate::compile::walk_binary_chain(n, self);
    }
    fn visit_function(&mut self, f: &Function) {
        self.check(f);
        f.visit_children_with(self);
    }
}

/// Collects the free value-position names a function body references and which of
/// them it writes. "Free" is relative to the function's OWN params + locals: a name
/// the body declares (param / `var` / `let` / `const` / `catch` / inner-fn name) is
/// NOT free. This is a conservative lexical approximation (no resolver) sufficient
/// for the by-name boxing decision; any ambiguity is resolved by the broader bails.
struct FreeRefScan {
    /// Names bound locally (params + body decls) — excluded from `refs`.
    local: HashSet<String>,
    /// Free names this body references (a capture, whether read or written — a
    /// write is also a reference). Used as the inner fn's capture set.
    refs: HashSet<String>,
}

impl FreeRefScan {
    fn new(params: &[Param]) -> Self {
        let mut local = HashSet::new();
        for p in params {
            binding_names(&p.pat, &mut |id| {
                local.insert(id.sym.to_string());
            });
        }
        FreeRefScan {
            local,
            refs: HashSet::new(),
        }
    }
    fn note_ref(&mut self, name: &str) {
        if !self.local.contains(name) {
            self.refs.insert(name.to_string());
        }
    }
    /// A write target is also a reference (the name is captured).
    fn note_write(&mut self, name: &str) {
        self.note_ref(name);
    }
    /// Pre-collect ALL names this body binds (params already added; here we add
    /// `var`/function-decl names from the whole body and block-scoped names too —
    /// conservatively treating every locally-declared name as non-free). This runs
    /// before the ref walk so an assignment to a local is not mistaken for a
    /// capture write.
    fn collect_locals(&mut self, body: &FunctionBody) {
        let mut d = LocalDeclScan {
            names: std::mem::take(&mut self.local),
        };
        body.visit_with(&mut d);
        self.local = d.names;
    }
    /// Seed the local set with an arrow function's parameter names (arrows take a
    /// `Pat` list, not `Param`).
    fn collect_arrow_params(&mut self, params: &[Pat]) {
        for p in params {
            binding_names(p, &mut |id| {
                self.local.insert(id.sym.to_string());
            });
        }
    }
}

impl Visit for FreeRefScan {
    fn visit_bin_expr(&mut self, n: &BinExpr) {
        crate::compile::walk_binary_chain(n, self);
    }
    fn visit_assign_expr(&mut self, n: &AssignExpr) {
        match &n.left {
            AssignTarget::Simple(SimpleAssignTarget::Ident(bi)) => {
                self.note_write(bi.id.sym.as_ref());
            }
            AssignTarget::Pat(AssignTargetPat::Array(arr)) => {
                self.note_pat_targets_array(arr);
            }
            AssignTarget::Pat(AssignTargetPat::Object(obj)) => {
                self.note_pat_targets_object(obj);
            }
            _ => {}
        }
        // The RHS (and any member/computed parts of the LHS) are plain refs.
        n.right.visit_with(self);
        if let AssignTarget::Simple(SimpleAssignTarget::Member(m)) = &n.left {
            m.visit_with(self);
        }
    }
    fn visit_update_expr(&mut self, n: &UpdateExpr) {
        if let Expr::Ident(id) = &*n.arg {
            self.note_write(id.sym.as_ref());
        } else {
            n.arg.visit_with(self);
        }
    }
    fn visit_for_in_stmt(&mut self, n: &ForInStmt) {
        if let ForHead::Pat(p) = &n.left
            && let Pat::Ident(bi) = &**p
        {
            self.note_write(bi.id.sym.as_ref());
        }
        n.left.visit_with(self);
        n.right.visit_with(self);
        n.body.visit_with(self);
    }
    fn visit_for_of_stmt(&mut self, n: &ForOfStmt) {
        if let ForHead::Pat(p) = &n.left
            && let Pat::Ident(bi) = &**p
        {
            self.note_write(bi.id.sym.as_ref());
        }
        n.left.visit_with(self);
        n.right.visit_with(self);
        n.body.visit_with(self);
    }
    fn visit_ident(&mut self, id: &Ident) {
        self.note_ref(id.sym.as_ref());
    }
    // Member property and object-literal keys are not value refs.
    fn visit_member_expr(&mut self, m: &MemberExpr) {
        m.obj.visit_with(self);
        if let MemberProp::Computed(c) = &m.prop {
            c.visit_with(self);
        }
    }
    fn visit_prop_name(&mut self, p: &PropName) {
        if let PropName::Computed(c) = p {
            c.visit_with(self);
        }
    }
    // Nested functions inside this body are their own scope; their free refs are
    // this body's refs too (transitively a capture), so DO descend, but a name they
    // bind locally shadows. For the conservative D1 analysis we descend normally —
    // any over-approximation only makes us box less (via the non-virt-ref bail) or
    // is caught by the shadow bail.
}

impl FreeRefScan {
    fn note_pat_targets_array(&mut self, arr: &ArrayPat) {
        for el in arr.elems.iter().flatten() {
            self.note_pat_target(el);
        }
    }
    fn note_pat_targets_object(&mut self, obj: &ObjectPat) {
        for prop in &obj.props {
            match prop {
                ObjectPatProp::KeyValue(kv) => self.note_pat_target(&kv.value),
                ObjectPatProp::Assign(a) => self.note_write(a.key.id.sym.as_ref()),
                ObjectPatProp::Rest(r) => self.note_pat_target(&r.arg),
            }
        }
    }
    fn note_pat_target(&mut self, pat: &Pat) {
        match pat {
            Pat::Ident(bi) => self.note_write(bi.id.sym.as_ref()),
            Pat::Expr(e) => {
                if let Expr::Ident(id) = &**e {
                    self.note_write(id.sym.as_ref());
                } else {
                    e.visit_with(self);
                }
            }
            Pat::Assign(ap) => {
                self.note_pat_target(&ap.left);
                ap.right.visit_with(self);
            }
            Pat::Array(arr) => self.note_pat_targets_array(arr),
            Pat::Object(obj) => self.note_pat_targets_object(obj),
            Pat::Rest(r) => self.note_pat_target(&r.arg),
            Pat::Invalid(_) => {}
        }
    }
}

/// Collects every name a function body binds locally (params already seeded):
/// `var`, function declarations, `let`/`const`, `catch` params, destructuring
/// leaves — at any nesting WITHIN this function (not descending into nested
/// functions, whose bindings are their own scope). Used to exclude locals from the
/// free-ref set.
struct LocalDeclScan {
    names: HashSet<String>,
}
impl Visit for LocalDeclScan {
    fn visit_bin_expr(&mut self, n: &BinExpr) {
        crate::compile::walk_binary_chain(n, self);
    }
    fn visit_var_decl(&mut self, v: &VarDecl) {
        for d in &v.decls {
            binding_names(&d.name, &mut |id| {
                self.names.insert(id.sym.to_string());
            });
        }
        v.visit_children_with(self);
    }
    fn visit_fn_decl(&mut self, n: &FnDecl) {
        self.names.insert(n.ident.sym.to_string());
        // Do not descend into the nested fn's body (its own scope).
    }
    fn visit_catch_clause(&mut self, c: &CatchClause) {
        if let Some(p) = &c.param {
            binding_names(p, &mut |id| {
                self.names.insert(id.sym.to_string());
            });
        }
        c.body.visit_with(self);
    }
    fn visit_function(&mut self, _: &Function) {}
    fn visit_arrow_expr(&mut self, _: &ArrowExpr) {}
}

/// Scans F's body for WRITES to a name (assign / `++`/`--` / compound / for-head /
/// destructuring target), at any depth INCLUDING nested functions — because a
/// boxed binding may be written by an inner virtualized function. Records just the
/// written names.
struct WriteScan {
    written: HashSet<String>,
}
impl WriteScan {
    fn note_pat(&mut self, pat: &Pat) {
        match pat {
            Pat::Ident(bi) => {
                self.written.insert(bi.id.sym.to_string());
            }
            Pat::Expr(e) => {
                if let Expr::Ident(id) = &**e {
                    self.written.insert(id.sym.to_string());
                }
            }
            Pat::Assign(ap) => self.note_pat(&ap.left),
            Pat::Array(arr) => {
                for el in arr.elems.iter().flatten() {
                    self.note_pat(el);
                }
            }
            Pat::Object(obj) => {
                for prop in &obj.props {
                    match prop {
                        ObjectPatProp::KeyValue(kv) => self.note_pat(&kv.value),
                        ObjectPatProp::Assign(a) => {
                            self.written.insert(a.key.id.sym.to_string());
                        }
                        ObjectPatProp::Rest(r) => self.note_pat(&r.arg),
                    }
                }
            }
            Pat::Rest(r) => self.note_pat(&r.arg),
            Pat::Invalid(_) => {}
        }
    }
}
impl Visit for WriteScan {
    fn visit_bin_expr(&mut self, n: &BinExpr) {
        crate::compile::walk_binary_chain(n, self);
    }
    fn visit_assign_expr(&mut self, n: &AssignExpr) {
        match &n.left {
            AssignTarget::Simple(SimpleAssignTarget::Ident(bi)) => {
                self.written.insert(bi.id.sym.to_string());
            }
            AssignTarget::Pat(AssignTargetPat::Array(arr)) => {
                for el in arr.elems.iter().flatten() {
                    self.note_pat(el);
                }
            }
            AssignTarget::Pat(AssignTargetPat::Object(obj)) => {
                for prop in &obj.props {
                    match prop {
                        ObjectPatProp::KeyValue(kv) => self.note_pat(&kv.value),
                        ObjectPatProp::Assign(a) => {
                            self.written.insert(a.key.id.sym.to_string());
                        }
                        ObjectPatProp::Rest(r) => self.note_pat(&r.arg),
                    }
                }
            }
            _ => {}
        }
        n.visit_children_with(self);
    }
    fn visit_update_expr(&mut self, n: &UpdateExpr) {
        if let Expr::Ident(id) = &*n.arg {
            self.written.insert(id.sym.to_string());
        }
        n.visit_children_with(self);
    }
    fn visit_for_in_stmt(&mut self, n: &ForInStmt) {
        if let ForHead::Pat(p) = &n.left {
            self.note_pat(p);
        }
        n.visit_children_with(self);
    }
    fn visit_for_of_stmt(&mut self, n: &ForOfStmt) {
        if let ForHead::Pat(p) = &n.left {
            self.note_pat(p);
        }
        n.visit_children_with(self);
    }
}

// ---------------------------------------------------------------------------
// The enclosing-scope rewriter: cell-ify every boxed binding in F's body.
// ---------------------------------------------------------------------------

/// Rewrite F's body in place so each boxed name `x` is a one-element cell:
///   * its declaration `var x = init` / `let x = init` becomes `var/let x = [init]`
///     (a bare `var x;` becomes `var x = [undefined]`); TDZ is preserved by boxing
///     AT the declaration point and keeping the `let` binding (a pre-decl read
///     still hits the TDZ on `x` itself);
///   * every value-position read `x` becomes the member expression `x[0]`;
///   * every write target `x = v` / `x += v` / `++x` / destructuring/for-head
///     becomes `x[0]`.
///
/// Nested function bodies are NOT rewritten: a virtualized inner fn captures the
/// CELL (its thunk's capture expression stays the bare `x`), and a name bound by a
/// nested scope shadows (the boxed name is F's; the rewrite is keyed by name and we
/// have already bailed on any same-name shadow, so a bare `x` everywhere in F's own
/// statements unambiguously refers to the boxed binding).
fn rewrite_boxed(body: &mut FunctionBody, boxed: &HashSet<String>) {
    let mut rw = CellRewriter { boxed };
    body.visit_mut_with(&mut rw);
}

struct CellRewriter<'a> {
    boxed: &'a HashSet<String>,
}

impl CellRewriter<'_> {
    /// `x` -> `x[0]` (a computed member access with numeric index 0).
    fn cellify_ident(id: &Ident) -> Expr {
        Expr::Member(MemberExpr {
            span: id.span,
            obj: Box::new(Expr::Ident(id.clone())),
            prop: MemberProp::Computed(ComputedPropName {
                span: id.span,
                expr: Box::new(Expr::Lit(Lit::Num(Number {
                    span: id.span,
                    value: 0.0,
                    raw: None,
                }))),
            }),
        })
    }
}

impl VisitMut for CellRewriter<'_> {
    fn visit_mut_bin_expr(&mut self, n: &mut BinExpr) {
        walk_binary_chain_mut(n, self);
    }
    // Stop at a nested function/arrow: its body is a different scope. A virtualized
    // inner captures the cell via the bare name (handled by the thunk), and a
    // non-virtualized inner referencing a boxed name was already bailed out of the
    // boxed set, so no boxed name appears free in a nested body we must rewrite.
    fn visit_mut_function(&mut self, _: &mut Function) {}
    fn visit_mut_arrow_expr(&mut self, _: &mut ArrowExpr) {}

    // Declaration: `var x = init` -> `var x = [init]`; `var x;` -> `var x = [undefined]`.
    fn visit_mut_var_declarator(&mut self, d: &mut VarDeclarator) {
        // Only a simple-ident boxed declarator is cell-ified (the analysis only
        // boxes simple-ident `var`/`let`, never a destructuring leaf).
        if let Pat::Ident(bi) = &d.name
            && self.boxed.contains(bi.id.sym.as_ref())
        {
            let init = d
                .init
                .take()
                .unwrap_or_else(|| Box::new(undef_expr(bi.id.span)));
            // Rewrite inside the initializer FIRST (it runs in F's scope and may
            // reference other boxed names), then wrap it in a one-element array.
            let mut init = init;
            init.visit_mut_with(self);
            d.init = Some(Box::new(Expr::Array(ArrayLit {
                span: bi.id.span,
                elems: vec![Some(ExprOrSpread {
                    spread: None,
                    expr: init,
                })],
            })));
            return;
        }
        d.visit_mut_children_with(self);
    }

    // Assignment / compound-assignment target `x` / `x op= v` -> `x[0] op= v`.
    fn visit_mut_assign_expr(&mut self, n: &mut AssignExpr) {
        if let AssignTarget::Simple(SimpleAssignTarget::Ident(bi)) = &n.left
            && self.boxed.contains(bi.id.sym.as_ref())
        {
            let member = match Self::cellify_ident(&bi.id) {
                Expr::Member(m) => m,
                _ => unreachable!(),
            };
            n.left = AssignTarget::Simple(SimpleAssignTarget::Member(member));
        } else {
            n.left.visit_mut_with(self);
        }
        n.right.visit_mut_with(self);
    }

    // `++x` / `x--` -> `++x[0]` / `x[0]--`.
    fn visit_mut_update_expr(&mut self, n: &mut UpdateExpr) {
        if let Expr::Ident(id) = &*n.arg
            && self.boxed.contains(id.sym.as_ref())
        {
            *n.arg = Self::cellify_ident(id);
            return;
        }
        n.arg.visit_mut_with(self);
    }

    // for-head bare-ident target `for (x in/of …)` -> `for (x[0] in/of …)`.
    fn visit_mut_for_in_stmt(&mut self, n: &mut ForInStmt) {
        self.cellify_for_head(&mut n.left);
        n.right.visit_mut_with(self);
        n.body.visit_mut_with(self);
    }
    fn visit_mut_for_of_stmt(&mut self, n: &mut ForOfStmt) {
        self.cellify_for_head(&mut n.left);
        n.right.visit_mut_with(self);
        n.body.visit_mut_with(self);
    }

    // Value-position read `x` -> `x[0]`. This is the catch-all for expressions; the
    // target-position rewrites above run first (they don't recurse into the bare
    // target ident), so a read here is always a genuine value read.
    fn visit_mut_expr(&mut self, e: &mut Expr) {
        if let Expr::Ident(id) = e {
            if self.boxed.contains(id.sym.as_ref()) {
                *e = Self::cellify_ident(id);
                return;
            }
            return;
        }
        e.visit_mut_children_with(self);
    }

    // Object-literal shorthand `{ x }` would mean `{ x: x }`; if `x` is boxed it
    // must become `{ x: x[0] }`. Rewrite shorthand props explicitly.
    fn visit_mut_prop(&mut self, p: &mut Prop) {
        if let Prop::Shorthand(id) = p
            && self.boxed.contains(id.sym.as_ref())
        {
            *p = Prop::KeyValue(KeyValueProp {
                key: PropName::Ident(IdentName::new(id.sym.clone(), id.span)),
                value: Box::new(Self::cellify_ident(id)),
            });
            return;
        }
        p.visit_mut_children_with(self);
    }
}

impl CellRewriter<'_> {
    /// Rewrite a `for-in`/`for-of` head whose target is a bare boxed ident
    /// (`for (x of …)`) to a member target (`for (x[0] of …)`). A `var`/`let` head
    /// declares a fresh local (never boxed) and is left alone except for cell-ifying
    /// any boxed initializer references, which the normal walk handles.
    fn cellify_for_head(&mut self, head: &mut ForHead) {
        if let ForHead::Pat(p) = head
            && let Pat::Ident(bi) = &**p
            && self.boxed.contains(bi.id.sym.as_ref())
        {
            let member = match Self::cellify_ident(&bi.id) {
                Expr::Member(m) => m,
                _ => unreachable!(),
            };
            *head = ForHead::Pat(Box::new(Pat::Expr(Box::new(Expr::Member(member)))));
        } else {
            head.visit_mut_with(self);
        }
    }
}

fn undef_expr(span: swc_core::common::Span) -> Expr {
    Expr::Ident(Ident::new("undefined".into(), span, Default::default()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use swc_core::common::sync::Lrc;
    use swc_core::common::{FileName, SourceMap};
    use swc_core::ecma::ast::EsVersion;
    use swc_core::ecma::codegen::Emitter;
    use swc_core::ecma::codegen::text_writer::JsWriter;
    use swc_core::ecma::parser::{EsSyntax, Parser, StringInput, Syntax, lexer::Lexer};

    fn parse(src: &str) -> Program {
        let cm: Lrc<SourceMap> = Default::default();
        let fm = cm.new_source_file(Lrc::new(FileName::Custom("t.js".into())), src.to_string());
        let lexer = Lexer::new(
            Syntax::Es(EsSyntax::default()),
            EsVersion::EsNext,
            StringInput::from(&*fm),
            None,
        );
        Parser::new_from(lexer).parse_program().unwrap()
    }

    fn emit(program: &Program) -> String {
        let cm: Lrc<SourceMap> = Default::default();
        let mut buf = Vec::new();
        {
            let mut emitter = Emitter {
                cfg: Default::default(),
                cm: cm.clone(),
                comments: None,
                wr: JsWriter::new(cm, "", &mut buf, None),
            };
            emitter.emit_program(program).unwrap();
        }
        String::from_utf8(buf).unwrap()
    }

    /// Run the pre-pass and return (rewritten source, boxed-set size).
    fn run(src: &str, glob: &str) -> (String, usize) {
        let mut program = parse(src);
        let plan = plan_and_rewrite(&mut program, glob);
        let n: usize = plan.by_fn_body.values().map(|s| s.len()).sum();
        (emit(&program), n)
    }

    #[test]
    fn boxes_mutated_captured_var() {
        // The counter closure: `inc` mutates captured `c` -> `c` boxed, `mk`
        // rewritten so `var c=[0]`, reads/writes become `c[0]`.
        let (out, boxed) = run(
            "function mk(){ var c=0; function inc(){ c=c+1; return c; } return inc()+c; }",
            "inc",
        );
        assert!(boxed >= 1, "`c` must be boxed: {out}");
        assert!(
            out.contains("var c = [") || out.contains("var c=["),
            "decl cell-ified: {out}"
        );
        assert!(out.contains("c[0]"), "reads/writes cell-ified: {out}");
    }

    #[test]
    fn read_only_capture_not_boxed() {
        // `addBase` only READS captured `base` -> no box (no regression).
        let (out, boxed) = run(
            "function mk(base){ function addBase(x){ return x+base; } return addBase(1); }",
            "addBase",
        );
        assert_eq!(boxed, 0, "read-only capture must not box: {out}");
        assert!(!out.contains("[0]"), "no cell rewrite: {out}");
    }

    #[test]
    fn param_capture_not_boxed() {
        // A captured-and-mutated PARAM of the enclosing scope is NOT boxed in D1
        // (param boxing needs MakeCell seeding, reserved for D5).
        let (_out, boxed) = run(
            "function mk(c){ function inc(){ c=c+1; return c; } return inc(); }",
            "inc",
        );
        assert_eq!(boxed, 0, "an enclosing param is never boxed in D1");
    }

    #[test]
    fn top_level_binding_not_boxed() {
        // A module-top-level `var c` is never rewritten (externally referenceable).
        let (_out, boxed) = run("var c=0; function inc(){ c=c+1; return c; }", "inc");
        assert_eq!(boxed, 0, "top-level binding must never be boxed");
    }

    #[test]
    fn non_virtualized_sibling_capture_bails() {
        // `c` is also captured by a NON-virtualized sibling `other` (it would read
        // the raw cell array) -> D1-alone bail: do not box.
        let (_out, boxed) = run(
            "function mk(){ var c=0; function inc(){ c=c+1; return c; } \
             function other(){ return c; } return inc()+other(); }",
            "inc",
        );
        assert_eq!(
            boxed, 0,
            "a non-virtualized sibling capture must block boxing"
        );
    }

    #[test]
    fn shadowed_name_not_boxed() {
        // A nested block re-binds `c` with `let` -> shadow -> imprecise by-name
        // rewrite -> do not box.
        let (_out, boxed) = run(
            "function mk(){ var c=0; function inc(){ c=c+1; return c; } \
             { let c=9; c=c+1; } return inc(); }",
            "inc",
        );
        assert_eq!(boxed, 0, "a shadowed candidate must not be boxed");
    }

    #[test]
    fn const_capture_not_boxed_and_no_mutation() {
        // A captured `const` is never written (can't be), so it stays a read-only
        // capture (not boxed).
        let (_out, boxed) = run(
            "function mk(){ const c=0; function get(){ return c; } return get(); }",
            "get",
        );
        assert_eq!(boxed, 0, "const capture is read-only -> not boxed");
    }

    #[test]
    fn dynamic_scope_blocks_boxing() {
        // A `with`/`eval` in the enclosing scope defeats static reasoning -> bail.
        let (_out, boxed) = run(
            "function mk(o){ with(o){} var c=0; function inc(){ c=c+1; return c; } return inc(); }",
            "inc",
        );
        assert_eq!(boxed, 0, "dynamic scope must block boxing");
    }
}
