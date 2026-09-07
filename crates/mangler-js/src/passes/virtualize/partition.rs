//! Phase 2 — whole-program PARTITION + adaptive bisection (§3.1, §3.3, §2.1).
//!
//! Phase 1 wrapped the ENTIRE top level as one all-or-nothing chunk. Phase 2
//! replaces that with partitioning so coverage is maximal:
//!
//! 1. **Classify** (§3.1) each top-level item as [`Class::Native`] (a `ModuleDecl`
//!    import/export, or a statement containing top-level `await`) or
//!    [`Class::Wrappable`] (everything else).
//! 2. **Split** into maximal contiguous Wrappable RUNS separated by Native
//!    boundaries; Native items stay where they are, in program order.
//! 3. **Cross-run bindings** (§2.1 default self-contained mode): a `var`/`function`/
//!    `let`/`const` name declared in one run but referenced *outside* its declaring
//!    run (another run, a Native item, or an `export` specifier) is hoisted to a
//!    native one-element CELL `var <name> = [undefined];` spliced above the first run,
//!    and every reference to it (in any run) is rewritten to `<name>[0]`. The chunk
//!    then captures the cell BY NAME (a free read-only capture of the array) and
//!    reads/writes through it — the same by-reference cell trick the D1 machinery
//!    ([`mangler_vm::cells`]) uses for nested mutable captures, applied at module
//!    scope. A run with NO cross-run bindings needs no hoisting (the common
//!    single-IIFE case stays one clean chunk, byte-identical to Phase 1 output).
//! 4. **Compile each run** to its own chunk and emit a §2.1 re-entry interpreter call
//!    in its place. If a run fails to compile as one chunk, **bisect** it (§3.3) into
//!    smaller contiguous sub-runs and retry, down to single statements; a single
//!    statement that still fails stays NATIVE. Every decision has a native fallback.
//!
//! Determinism (§7): the classification, run boundaries, and the bisection split
//! order are a pure function of the input AST, so the same seed yields byte-identical
//! output including partition boundaries and the bisection sequence. Bisection splits
//! at a fixed point (the midpoint, left half first), so the offender-isolation
//! sequence is reproducible.

use std::collections::{HashMap, HashSet};

use mangler_jsast::analysis::binding_names;
use swc_core::common::DUMMY_SP;
use swc_core::ecma::ast::*;
use swc_core::ecma::visit::{Visit, VisitMut, VisitMutWith, VisitWith};

/// One top-level item paired with its §3.1 classification.
pub(crate) enum Class {
    /// Must stay native: a `ModuleDecl` (import/export) or a statement containing a
    /// top-level `await`. Never wrapped; acts as a run boundary.
    Native(ModuleItem),
    /// Wrappable: an ordinary statement. May still *contain* nested ineligible
    /// functions (those bail the run's compile, which then bisects — Phase 3 adds the
    /// native-closure escape hatch).
    Wrappable(Stmt),
}

/// Classify each top-level `ModuleItem` (§3.1). `ModuleDecl`s are native boundaries;
/// a `Stmt` containing top-level `await` is native; every other `Stmt` is wrappable.
/// Test-only convenience wrapper; production code calls [`classify_with_protected`].
#[cfg(test)]
pub(crate) fn classify(items: Vec<ModuleItem>) -> Vec<Class> {
    classify_with_protected(items, &[])
}

/// As [`classify`], but additionally forces a statement NATIVE when it DECLARES a
/// `protected` name at its top level — used to keep the strings-decoder stub
/// (`var <core> = (function(){…})()`) out of the VM. Virtualized user code may still
/// CALL `core(idx)` (it captures `core` as a free global); only the declaration must
/// remain a native module binding so that capture resolves.
pub(crate) fn classify_with_protected(items: Vec<ModuleItem>, protected: &[String]) -> Vec<Class> {
    items
        .into_iter()
        .map(|it| match it {
            ModuleItem::ModuleDecl(_) => Class::Native(it),
            ModuleItem::Stmt(s) => {
                if stmt_has_top_level_await(&s) || stmt_declares_any(&s, protected) {
                    Class::Native(ModuleItem::Stmt(s))
                } else {
                    Class::Wrappable(s)
                }
            }
        })
        .collect()
}

/// True if `s` declares (at its own top level: `var`/`let`/`const`/`function`) any
/// name in `names`. Does not descend into nested functions/blocks (a protected name
/// is declared at module top by the strings stub).
fn stmt_declares_any(s: &Stmt, names: &[String]) -> bool {
    if names.is_empty() {
        return false;
    }
    match s {
        Stmt::Decl(Decl::Var(v)) => v.decls.iter().any(|d| {
            let mut hit = false;
            mangler_jsast::analysis::binding_names(&d.name, &mut |id| {
                if names.iter().any(|n| n == id.sym.as_ref()) {
                    hit = true;
                }
            });
            hit
        }),
        Stmt::Decl(Decl::Fn(f)) => names.iter().any(|n| n == f.ident.sym.as_ref()),
        _ => false,
    }
}

/// True if `s` contains an `await` expression that is NOT nested inside a function or
/// arrow (i.e. a *module* top-level await that cannot be wrapped). `for await` heads
/// count too.
fn stmt_has_top_level_await(s: &Stmt) -> bool {
    struct Scan {
        found: bool,
    }
    impl Visit for Scan {
        // Do not descend into nested functions/arrows: their `await` belongs to an
        // async function, not the module top level.
        fn visit_function(&mut self, _: &Function) {}
        fn visit_arrow_expr(&mut self, _: &ArrowExpr) {}
        fn visit_await_expr(&mut self, _: &AwaitExpr) {
            self.found = true;
        }
        fn visit_for_of_stmt(&mut self, n: &ForOfStmt) {
            if n.is_await {
                self.found = true;
            }
            n.visit_children_with(self);
        }
    }
    let mut sc = Scan { found: false };
    s.visit_with(&mut sc);
    sc.found
}

/// A maximal contiguous run of wrappable statements, recorded by its position in the
/// classified item list. `Native` segments are emitted verbatim between runs.
pub(crate) enum Segment {
    Native(ModuleItem),
    /// A contiguous run of wrappable statements (in program order).
    Run(Vec<Stmt>),
}

/// Group the classified items into native segments and maximal wrappable runs, in
/// program order.
pub(crate) fn segment(classes: Vec<Class>) -> Vec<Segment> {
    let mut segs: Vec<Segment> = Vec::new();
    let mut run: Vec<Stmt> = Vec::new();
    for c in classes {
        match c {
            Class::Wrappable(s) => run.push(s),
            Class::Native(it) => {
                if !run.is_empty() {
                    segs.push(Segment::Run(std::mem::take(&mut run)));
                }
                segs.push(Segment::Native(it));
            }
        }
    }
    if !run.is_empty() {
        segs.push(Segment::Run(run));
    }
    segs
}

// ---------------------------------------------------------------------------
// Cross-run binding analysis (§2.1 default self-contained mode)
// ---------------------------------------------------------------------------

/// The names that must be hoisted to native cells: a binding declared at the top
/// level of one run (or by a native item) and referenced OUTSIDE its declaring run
/// (another run, a native item, or an `export` specifier).
///
/// We work conservatively in terms of names (no resolver marks — the pass is
/// pre-resolver). A name that is hazardous to cell-ify (see [`is_safe_to_cell`]) is
/// NOT cell-ified; instead its declaring run is split off so the binding stays
/// native — handled by the caller via bisection/boundary, never a miscompile.
pub(crate) struct CrossRun {
    /// Names to hoist to native `var <name> = [undefined];` cells and rewrite to
    /// `<name>[0]` everywhere.
    pub(crate) cells: HashSet<String>,
}

/// Compute the cross-run cell set over the segmented program.
///
/// A name is a cross-run cell candidate iff it is declared at the top level of some
/// run AND referenced by a *different* segment (run/native/export). Export-bound
/// names (§5) are always treated as cross-run (the export reads them outside any run).
pub(crate) fn analyze_cross_run(segs: &[Segment], export_names: &HashSet<String>) -> CrossRun {
    // For each run index, the top-level declared names and the referenced names.
    let mut run_decls: Vec<HashSet<String>> = Vec::new();
    let mut run_refs: Vec<HashSet<String>> = Vec::new();
    // Names referenced by ANY native item (imports never reference; exports do, but
    // those are folded in via `export_names`; a native statement with top-level await
    // may also reference a run binding).
    let mut native_refs: HashSet<String> = HashSet::new();

    for seg in segs {
        match seg {
            Segment::Run(stmts) => {
                run_decls.push(top_level_decl_names(stmts));
                run_refs.push(referenced_names(stmts));
            }
            Segment::Native(it) => {
                if let ModuleItem::Stmt(s) = it {
                    let mut refs = HashSet::new();
                    collect_refs_stmt(s, &mut refs);
                    native_refs.extend(refs);
                }
            }
        }
    }

    let mut cells = HashSet::new();
    for (i, decls) in run_decls.iter().enumerate() {
        for name in decls {
            // Referenced by a DIFFERENT run?
            let other_run = run_refs
                .iter()
                .enumerate()
                .any(|(j, refs)| j != i && refs.contains(name));
            let by_native = native_refs.contains(name);
            let by_export = export_names.contains(name);
            if other_run || by_native || by_export {
                cells.insert(name.clone());
            }
        }
    }
    CrossRun { cells }
}

/// The names a run's TOP-LEVEL statements declare: `var` (function-scoped, so any
/// depth within the run but not inside nested functions), top-level `function`
/// declarations, and top-level `let`/`const`. These are the bindings whose storage we
/// can hoist to a cell.
fn top_level_decl_names(stmts: &[Stmt]) -> HashSet<String> {
    let mut out = HashSet::new();
    // `var` (whole run, not descending into functions).
    struct VarScan<'a>(&'a mut HashSet<String>);
    impl Visit for VarScan<'_> {
        fn visit_var_decl(&mut self, v: &VarDecl) {
            if matches!(v.kind, VarDeclKind::Var) {
                for d in &v.decls {
                    binding_names(&d.name, &mut |id| {
                        self.0.insert(id.sym.to_string());
                    });
                }
            }
            v.visit_children_with(self);
        }
        fn visit_function(&mut self, _: &Function) {}
        fn visit_arrow_expr(&mut self, _: &ArrowExpr) {}
    }
    for s in stmts {
        s.visit_with(&mut VarScan(&mut out));
    }
    // Top-level `function` / `let` / `const` (run's statement list only).
    for s in stmts {
        match s {
            Stmt::Decl(Decl::Fn(f)) => {
                out.insert(f.ident.sym.to_string());
            }
            Stmt::Decl(Decl::Var(v)) if matches!(v.kind, VarDeclKind::Let | VarDeclKind::Const) => {
                for d in &v.decls {
                    binding_names(&d.name, &mut |id| {
                        out.insert(id.sym.to_string());
                    });
                }
            }
            _ => {}
        }
    }
    out
}

/// Every identifier referenced (read or written) anywhere in a run's statements,
/// including inside nested functions (a nested closure that reads a cross-run name
/// captures it).
fn referenced_names(stmts: &[Stmt]) -> HashSet<String> {
    let mut out = HashSet::new();
    for s in stmts {
        collect_refs_stmt(s, &mut out);
    }
    out
}

fn collect_refs_stmt(s: &Stmt, out: &mut HashSet<String>) {
    struct RefScan<'a>(&'a mut HashSet<String>);
    impl Visit for RefScan<'_> {
        fn visit_ident(&mut self, id: &Ident) {
            self.0.insert(id.sym.to_string());
        }
        // Member property names are not value refs.
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
    }
    s.visit_with(&mut RefScan(out));
}

/// Collect the names referenced by an `export { a, b }` / `export default x` /
/// `export const x = …` declaration's specifiers — the names that must remain
/// resolvable as module bindings (§5). For `export const/var/function`, the declared
/// name is exported; for `export { local as exported }`, the LOCAL name is referenced.
pub(crate) fn export_bound_names(items: &[ModuleItem]) -> HashSet<String> {
    let mut out = HashSet::new();
    for it in items {
        let ModuleItem::ModuleDecl(d) = it else {
            continue;
        };
        match d {
            ModuleDecl::ExportNamed(named) => {
                // `export { local as exported }` — the LOCAL name is what a run must
                // expose. (Re-exports `export { x } from 'm'` reference no local
                // binding; their `src` is Some, so skip those.)
                if named.src.is_none() {
                    for spec in &named.specifiers {
                        if let ExportSpecifier::Named(n) = spec
                            && let ModuleExportName::Ident(id) = &n.orig
                        {
                            out.insert(id.sym.to_string());
                        }
                    }
                }
            }
            ModuleDecl::ExportDecl(ed) => {
                // `export const x = …` / `export function f(){}` — declared inline,
                // so the binding is already native (it lives in the export item, a
                // boundary). Record its name so a run referencing it cell-reads it.
                match &ed.decl {
                    Decl::Var(v) => {
                        for dcl in &v.decls {
                            binding_names(&dcl.name, &mut |id| {
                                out.insert(id.sym.to_string());
                            });
                        }
                    }
                    Decl::Fn(f) => {
                        out.insert(f.ident.sym.to_string());
                    }
                    Decl::Class(c) => {
                        out.insert(c.ident.sym.to_string());
                    }
                    _ => {}
                }
            }
            ModuleDecl::ExportDefaultExpr(e) => {
                // `export default name;` references `name`.
                if let Expr::Ident(id) = &*e.expr {
                    out.insert(id.sym.to_string());
                }
            }
            _ => {}
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Top-level cell rewriting
// ---------------------------------------------------------------------------

/// Rewrite a run's statements so each cross-run cell name `x`:
///   * its top-level declaration is split into a hoisted native cell + an in-place
///     initializer that writes THROUGH the cell. We do this at the SPLICE step
///     (the caller emits the native `var x = [undefined];`), so here we only:
///   * rewrite `var x = init;` → `x[0] = init;` (an expression statement) — the
///     storage is the hoisted cell, the initializer runs in place;
///   * rewrite `function f(){…}` (a cross-run cell) → `f[0] = function f(){…};` and
///     KEEP a native hoisted assignment for forward-reference safety (handled by the
///     caller: a cross-run `function` name is hoisted with its value, see
///     [`hoist_function_values`]);
///   * rewrite `let x = init;` / `const x = init;` → `x[0] = init;`;
///   * rewrite every value-position read `x` → `x[0]` and every write target.
///
/// Names NOT in `cells` are untouched (they are run-local — they live as VM frame
/// locals and never escape, exactly the self-contained Phase-1 behavior).
pub(crate) fn cellify_run(stmts: &mut Vec<Stmt>, cells: &HashSet<String>) {
    if cells.is_empty() {
        return;
    }
    // First: rewrite all reads/writes/declarations within the run.
    let mut rw = CellRewriter { cells };
    for s in stmts.iter_mut() {
        s.visit_mut_with(&mut rw);
    }
    // Then: lower top-level declarations of cell names to in-place cell writes. This
    // runs AFTER the read/write rewrite so the initializer expressions are already
    // cell-rewritten.
    let mut out: Vec<Stmt> = Vec::with_capacity(stmts.len());
    for s in std::mem::take(stmts) {
        match s {
            Stmt::Decl(Decl::Var(v)) => {
                lower_var_decl(*v, cells, &mut out);
            }
            Stmt::Decl(Decl::Fn(f)) if cells.contains(f.ident.sym.as_ref()) => {
                // `function f(){…}` for a cross-run cell → `f[0] = function f(){…};`.
                // The cell already holds the hoisted value (the caller seeds it), so
                // this in-place store keeps any later mutation visible; the function
                // VALUE is available to earlier runs via the hoist (see caller).
                let name = f.ident.sym.to_string();
                let fn_expr = Expr::Fn(FnExpr {
                    ident: Some(f.ident.clone()),
                    function: f.function,
                });
                out.push(cell_store_stmt(&name, fn_expr));
            }
            other => out.push(other),
        }
    }
    *stmts = out;
}

/// Lower a `var`/`let`/`const` declaration, splitting cell-name declarators into
/// in-place `x[0] = init;` stores and keeping non-cell declarators as a (possibly
/// smaller) declaration. A bare `var x;` for a cell needs no store (the hoisted cell
/// is already `[undefined]`).
fn lower_var_decl(v: VarDecl, cells: &HashSet<String>, out: &mut Vec<Stmt>) {
    let mut keep: Vec<VarDeclarator> = Vec::new();
    for d in v.decls {
        let cell_name = match &d.name {
            Pat::Ident(bi) if cells.contains(bi.id.sym.as_ref()) => Some(bi.id.sym.to_string()),
            _ => None,
        };
        match cell_name {
            Some(name) => {
                if let Some(init) = d.init {
                    out.push(cell_store_stmt(&name, *init));
                }
                // bare `var x;` / `let x;` → nothing (cell is pre-seeded undefined).
            }
            None => keep.push(d),
        }
    }
    if !keep.is_empty() {
        out.push(Stmt::Decl(Decl::Var(Box::new(VarDecl {
            span: v.span,
            kind: v.kind,
            declare: v.declare,
            decls: keep,
            ctxt: v.ctxt,
        }))));
    }
}

/// Build the statement `name[0] = value;`.
fn cell_store_stmt(name: &str, value: Expr) -> Stmt {
    Stmt::Expr(ExprStmt {
        span: DUMMY_SP,
        expr: Box::new(Expr::Assign(AssignExpr {
            span: DUMMY_SP,
            op: AssignOp::Assign,
            left: AssignTarget::Simple(SimpleAssignTarget::Member(cell_member(name))),
            right: Box::new(value),
        })),
    })
}

/// Build the member expression `name[0]`.
fn cell_member(name: &str) -> MemberExpr {
    MemberExpr {
        span: DUMMY_SP,
        obj: Box::new(Expr::Ident(Ident::new(
            name.into(),
            DUMMY_SP,
            Default::default(),
        ))),
        prop: MemberProp::Computed(ComputedPropName {
            span: DUMMY_SP,
            expr: Box::new(Expr::Lit(Lit::Num(Number {
                span: DUMMY_SP,
                value: 0.0,
                raw: None,
            }))),
        }),
    }
}

/// Rewrites bare reads/writes of a cell name `x` to `x[0]` within a run. Mirrors the
/// D1 `CellRewriter` (`mangler_vm::cells`) but is scoped to a top-level run and works
/// on the cross-run cell set. Declarations are handled separately by [`cellify_run`].
struct CellRewriter<'a> {
    cells: &'a HashSet<String>,
}

impl CellRewriter<'_> {
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
    // Assignment / compound target `x …= v` → `x[0] …= v`.
    fn visit_mut_assign_expr(&mut self, n: &mut AssignExpr) {
        if let AssignTarget::Simple(SimpleAssignTarget::Ident(bi)) = &n.left
            && self.cells.contains(bi.id.sym.as_ref())
        {
            n.left =
                AssignTarget::Simple(SimpleAssignTarget::Member(cell_member(bi.id.sym.as_ref())));
            n.right.visit_mut_with(self);
            return;
        }
        n.left.visit_mut_with(self);
        n.right.visit_mut_with(self);
    }

    fn visit_mut_update_expr(&mut self, n: &mut UpdateExpr) {
        if let Expr::Ident(id) = &*n.arg
            && self.cells.contains(id.sym.as_ref())
        {
            *n.arg = Self::cellify_ident(id);
            return;
        }
        n.arg.visit_mut_with(self);
    }

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

    fn visit_mut_expr(&mut self, e: &mut Expr) {
        if let Expr::Ident(id) = e {
            if self.cells.contains(id.sym.as_ref()) {
                *e = Self::cellify_ident(id);
            }
            return;
        }
        e.visit_mut_children_with(self);
    }

    fn visit_mut_prop(&mut self, p: &mut Prop) {
        if let Prop::Shorthand(id) = p
            && self.cells.contains(id.sym.as_ref())
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
    fn cellify_for_head(&mut self, head: &mut ForHead) {
        if let ForHead::Pat(p) = head
            && let Pat::Ident(bi) = &**p
            && self.cells.contains(bi.id.sym.as_ref())
        {
            *head = ForHead::Pat(Box::new(Pat::Expr(Box::new(Expr::Member(cell_member(
                bi.id.sym.as_ref(),
            ))))));
            return;
        }
        head.visit_mut_with(self);
    }
}

// ---------------------------------------------------------------------------
// Safety: which cross-run names can be cell-ified soundly
// ---------------------------------------------------------------------------

/// A cross-run name is UNSAFE to cell-ify (and so its declaring run must stay native)
/// when cell-ifying it would change observable semantics the VM can't preserve:
///
///   * the name is declared at the top level of MORE THAN ONE run (an ambiguous
///     by-name rewrite — we'd cell two distinct declarations), OR
///   * the name is declared with `let`/`const`. The cell hoist is a `var x=[undefined]`,
///     so a cross-run read BEFORE the declaring run's initializer runs would yield
///     `undefined` instead of throwing a TDZ `ReferenceError` (and a `const` would
///     become writable through `x[0]=…`). A `var` cell preserves semantics (read-before-
///     init is `undefined` in BOTH forms; `var` is reassignable), so only `var` (and
///     hoisted `function`, below) are lexically safe to cell-ify, OR
///   * the name is a `function` declaration whose FIRST reference (in flattened
///     program order over the wrappable runs) is BEFORE its declaration. JS hoists a
///     function value to the top of its scope, but our cell lowering turns the decl
///     into an in-place `f[0] = function f(){…}` store; a read before that store would
///     see the pre-seeded `undefined`. (Forward references — declare-then-read — are
///     fine.), OR
///   * the name is referenced by a NATIVE segment statement or by an `export` (it is in
///     `export_names`). Cell-ification only rewrites reads/writes INSIDE wrappable runs
///     to `x[0]`; a native statement keeps its bare `x` (which would now resolve to the
///     `[value]` cell array, not the value) and `export { x }` cannot be rewritten to
///     `export { x[0] }`. Such names must stay native module bindings (§5).
///
/// Any name not deemed safe is dropped from the cell set, which means its declaring
/// run cannot fully virtualize that binding — the caller keeps the offending run (or
/// a sub-run) native via bisection. Determinism: the safe set is a pure function of
/// the segment list.
pub(crate) fn restrict_to_safe(
    segs: &[Segment],
    candidates: &HashSet<String>,
    export_names: &HashSet<String>,
) -> HashSet<String> {
    if candidates.is_empty() {
        return HashSet::new();
    }

    // Names referenced by ANY native segment statement: cell-ification never rewrites
    // these sites, so the name must stay a native binding (Bug A — cross-run native
    // cell reads). Exports are folded in separately via `export_names`.
    let mut native_refs: HashSet<String> = HashSet::new();
    for seg in segs {
        if let Segment::Native(ModuleItem::Stmt(s)) = seg {
            collect_refs_stmt(s, &mut native_refs);
        }
    }

    // Flatten the wrappable runs into a single program-ordered statement stream so we
    // can reason about declaration-vs-reference ordering uniformly (within and across
    // runs). Each entry is one top-level statement of a run.
    let flat: Vec<&Stmt> = segs
        .iter()
        .filter_map(|s| match s {
            Segment::Run(stmts) => Some(stmts),
            Segment::Native(_) => None,
        })
        .flatten()
        .collect();

    // For each candidate: how many distinct runs declare it, the flattened index of
    // its (single) declaration statement, whether that declaration is a `function`,
    // and the flattened index of its first reference.
    let mut decl_run_count: HashMap<String, HashSet<usize>> = HashMap::new();
    let mut decl_index: HashMap<String, usize> = HashMap::new();
    let mut is_fn_decl: HashSet<String> = HashSet::new();
    let mut is_lexical: HashSet<String> = HashSet::new();
    let mut first_ref: HashMap<String, usize> = HashMap::new();

    // Per-run declaration multiplicity (a name declared in two different runs).
    let mut run_no = 0usize;
    for seg in segs {
        let Segment::Run(stmts) = seg else { continue };
        for name in top_level_decl_names(stmts) {
            if candidates.contains(&name) {
                decl_run_count.entry(name).or_default().insert(run_no);
            }
        }
        run_no += 1;
    }

    for (idx, s) in flat.iter().enumerate() {
        // Record a declaration at this flattened index (top-level decls only).
        match s {
            Stmt::Decl(Decl::Fn(f)) if candidates.contains(f.ident.sym.as_ref()) => {
                let name = f.ident.sym.to_string();
                decl_index.entry(name.clone()).or_insert(idx);
                is_fn_decl.insert(name);
            }
            Stmt::Decl(Decl::Var(v)) => {
                let lexical = matches!(v.kind, VarDeclKind::Let | VarDeclKind::Const);
                for d in &v.decls {
                    binding_names(&d.name, &mut |id| {
                        let n = id.sym.to_string();
                        if candidates.contains(&n) {
                            decl_index.entry(n.clone()).or_insert(idx);
                            if lexical {
                                is_lexical.insert(n);
                            }
                        }
                    });
                }
            }
            _ => {}
        }
        // Record first reference at this index.
        let mut refs = HashSet::new();
        collect_refs_stmt(s, &mut refs);
        for name in &refs {
            if candidates.contains(name) {
                first_ref.entry(name.clone()).or_insert(idx);
            }
        }
    }

    let mut safe = HashSet::new();
    'cand: for name in candidates {
        // Referenced by a native statement or an export → must stay a native binding
        // (cell-ification never rewrites those sites). Bug A.
        if native_refs.contains(name) || export_names.contains(name) {
            continue 'cand;
        }
        // `let`/`const` cells lose TDZ-throw and const-immutability. Bug B.
        if is_lexical.contains(name) {
            continue 'cand;
        }
        // One declaring run only.
        match decl_run_count.get(name) {
            Some(runs) if runs.len() == 1 => {}
            _ => continue 'cand, // declared by 0 or >1 runs → ambiguous/unsafe.
        }
        // Function-decl forward-reference hazard: a read before the in-place store.
        if is_fn_decl.contains(name)
            && let (Some(&di), Some(&ri)) = (decl_index.get(name), first_ref.get(name))
            && ri < di
        {
            continue 'cand;
        }
        safe.insert(name.clone());
    }
    safe
}

/// Build the native hoisted cell declarations `var a=[undefined],b=[undefined];` for
/// the cell set, in a DETERMINISTIC (sorted) order, to be spliced ABOVE the first run.
pub(crate) fn cell_hoist_decls(cells: &HashSet<String>) -> Option<Stmt> {
    if cells.is_empty() {
        return None;
    }
    let mut names: Vec<&String> = cells.iter().collect();
    names.sort();
    let decls: Vec<VarDeclarator> = names
        .into_iter()
        .map(|name| VarDeclarator {
            span: DUMMY_SP,
            name: Pat::Ident(BindingIdent {
                id: Ident::new(name.as_str().into(), DUMMY_SP, Default::default()),
                type_ann: None,
            }),
            init: Some(Box::new(Expr::Array(ArrayLit {
                span: DUMMY_SP,
                elems: vec![Some(ExprOrSpread {
                    spread: None,
                    expr: Box::new(Expr::Ident(Ident::new(
                        "undefined".into(),
                        DUMMY_SP,
                        Default::default(),
                    ))),
                })],
            }))),
            definite: false,
        })
        .collect();
    Some(Stmt::Decl(Decl::Var(Box::new(VarDecl {
        span: DUMMY_SP,
        kind: VarDeclKind::Var,
        declare: false,
        decls,
        ctxt: Default::default(),
    }))))
}

// Note on cross-run `function` cells: SAFE function cells are declared no later than
// their first reference (see `restrict_to_safe`), so the in-place
// `f[0] = function f(){…}` store in the declaring run runs before any cross-run read.
// The function declaration's value-hoisting WITHIN its own run is preserved by
// `compile_body` (the VM hoists fn-decls in a chunk). Cross-run forward references
// therefore observe the value as soon as the declaring run executes, matching the
// original program order for self-contained code — no separate value hoist needed.

#[cfg(test)]
mod tests {
    use super::*;
    use mangler_core::Language;
    use mangler_jsast::lang::{Js, ParseOpts};

    fn items(src: &str) -> Vec<ModuleItem> {
        match Js.parse(src, &ParseOpts::default()).unwrap().into_program() {
            Program::Module(m) => m.body,
            Program::Script(s) => s.body.into_iter().map(ModuleItem::Stmt).collect(),
        }
    }

    /// §3.1: ModuleDecls and top-level-await statements are Native; the rest Wrappable.
    #[test]
    fn classify_native_vs_wrappable() {
        let cs = classify(items("import 'm'; var a = 1; export const b = 2; a + b;"));
        let kinds: Vec<bool> = cs.iter().map(|c| matches!(c, Class::Native(_))).collect();
        // import=native, var=wrappable, export=native, expr=wrappable.
        assert_eq!(kinds, vec![true, false, true, false]);
    }

    /// A top-level `await` statement (module) is Native; an `await` inside a nested
    /// async function is NOT a top-level await (the statement stays wrappable).
    #[test]
    fn classify_top_level_await_is_native() {
        let cs = classify(items("await p; async function f(){ await q; }"));
        assert!(
            matches!(cs[0], Class::Native(_)),
            "top-level await → native"
        );
        assert!(
            matches!(cs[1], Class::Wrappable(_)),
            "await-in-async-fn → wrappable"
        );
    }

    /// §3.1: maximal contiguous wrappable runs separated by native boundaries.
    #[test]
    fn segment_groups_maximal_runs() {
        let segs = segment(classify(items(
            "var a=1; a+1; import 'm'; var b=2; b+2; export {b};",
        )));
        // run(2 stmts), native(import), run(2 stmts), native(export).
        assert_eq!(segs.len(), 4);
        assert!(matches!(&segs[0], Segment::Run(s) if s.len() == 2));
        assert!(matches!(&segs[1], Segment::Native(_)));
        assert!(matches!(&segs[2], Segment::Run(s) if s.len() == 2));
        assert!(matches!(&segs[3], Segment::Native(_)));
    }

    /// §2.1: a name declared in one run and read in another is a cross-run cell; a
    /// run-local name is not.
    #[test]
    fn cross_run_detects_shared_binding() {
        let segs = segment(classify(items(
            "var x=1; var local=9; import 'm'; globalThis.o = x;",
        )));
        let cross = analyze_cross_run(&segs, &HashSet::new());
        assert!(
            cross.cells.contains("x"),
            "x is read across the import → cell"
        );
        assert!(
            !cross.cells.contains("local"),
            "local is run-local → not a cell"
        );
    }

    /// §5: export-bound names are treated as cross-run (read by the export boundary).
    #[test]
    fn export_bound_names_are_cross_run() {
        let it = items("var x = 1; export { x };");
        let exports = export_bound_names(&it);
        assert!(exports.contains("x"));
        let segs = segment(classify(it));
        let cross = analyze_cross_run(&segs, &exports);
        assert!(cross.cells.contains("x"), "exported `x` is cross-run");
    }

    /// A `function` referenced before its declaration across runs is UNSAFE to cell.
    #[test]
    fn restrict_to_safe_rejects_backward_fn_ref() {
        // run A reads `f`, import boundary, run B declares `function f`.
        let segs = segment(classify(items(
            "globalThis.o = f(); import 'm'; function f(){ return 1; }",
        )));
        let mut cand = HashSet::new();
        cand.insert("f".to_string());
        let safe = restrict_to_safe(&segs, &cand, &HashSet::new());
        assert!(!safe.contains("f"), "backward fn ref → not safe to cell");
    }

    /// A forward function reference (declare then read) IS safe to cell.
    #[test]
    fn restrict_to_safe_accepts_forward_fn_ref() {
        let segs = segment(classify(items(
            "function f(){ return 1; } import 'm'; globalThis.o = f();",
        )));
        let mut cand = HashSet::new();
        cand.insert("f".to_string());
        let safe = restrict_to_safe(&segs, &cand, &HashSet::new());
        assert!(safe.contains("f"), "forward fn ref → safe to cell");
    }

    /// A name declared in TWO runs is ambiguous → not safe to cell.
    #[test]
    fn restrict_to_safe_rejects_multi_run_decl() {
        let segs = segment(classify(items(
            "var x=1; import 'm'; var x=2; import 'n'; x;",
        )));
        let mut cand = HashSet::new();
        cand.insert("x".to_string());
        let safe = restrict_to_safe(&segs, &cand, &HashSet::new());
        assert!(
            !safe.contains("x"),
            "declared in two runs → ambiguous → unsafe"
        );
    }

    /// Bug B: a cross-run `let`/`const` is UNSAFE to cell — a `var x=[undefined]` cell
    /// loses TDZ-throw and const-immutability. An equivalent `var` IS safe.
    #[test]
    fn restrict_to_safe_rejects_lexical_let_const() {
        for kw in ["let", "const"] {
            let src = format!("{kw} x=1; import 'm'; globalThis.o = x;");
            let segs = segment(classify(items(&src)));
            let mut cand = HashSet::new();
            cand.insert("x".to_string());
            let safe = restrict_to_safe(&segs, &cand, &HashSet::new());
            assert!(
                !safe.contains("x"),
                "{kw} cross-run binding must not cell (TDZ)"
            );
        }
        // `var` of the same shape stays safe (semantics preserved by the cell).
        let segs = segment(classify(items("var x=1; import 'm'; globalThis.o = x;")));
        let mut cand = HashSet::new();
        cand.insert("x".to_string());
        let safe = restrict_to_safe(&segs, &cand, &HashSet::new());
        assert!(safe.contains("x"), "var cross-run binding is safe to cell");
    }

    /// Bug A: a run-declared name READ by a native statement (here a top-level-await
    /// boundary) is UNSAFE — cell-ification never rewrites the native site to `x[0]`.
    #[test]
    fn restrict_to_safe_rejects_native_referenced() {
        // `await x` is a module top-level await → native; it reads `x` declared in run A.
        let segs = segment(classify(items("var x=1; await x;")));
        let mut cand = HashSet::new();
        cand.insert("x".to_string());
        let safe = restrict_to_safe(&segs, &cand, &HashSet::new());
        assert!(
            !safe.contains("x"),
            "name read by a native stmt must stay native"
        );
    }

    /// Bug A: an export-bound name is UNSAFE — `export { x }` cannot become
    /// `export { x[0] }`, so `x` must remain a native module binding (§5).
    #[test]
    fn restrict_to_safe_rejects_export_referenced() {
        let it = items("var x = 1; export { x };");
        let exports = export_bound_names(&it);
        let segs = segment(classify(it));
        let mut cand = HashSet::new();
        cand.insert("x".to_string());
        let safe = restrict_to_safe(&segs, &cand, &exports);
        assert!(
            !safe.contains("x"),
            "exported name must stay a native binding"
        );
    }

    /// Cell hoist decls are deterministic (sorted) and one-element `[undefined]` cells.
    #[test]
    fn cell_hoist_is_sorted_and_undefined() {
        let mut cells = HashSet::new();
        cells.insert("zeta".to_string());
        cells.insert("alpha".to_string());
        let decl = cell_hoist_decls(&cells).expect("decl");
        let rendered = render_stmt(&decl);
        let a = rendered.find("alpha").unwrap();
        let z = rendered.find("zeta").unwrap();
        assert!(a < z, "cells sorted deterministically: {rendered}");
        assert_eq!(
            rendered.matches("[undefined]").count(),
            2,
            "two undefined cells: {rendered}"
        );
    }

    fn render_stmt(s: &Stmt) -> String {
        use swc_core::ecma::ast::Script;
        let mut ast = Js.parse("0;", &ParseOpts::default()).unwrap();
        *ast.program_mut() = Program::Script(Script {
            span: DUMMY_SP,
            body: vec![s.clone()],
            shebang: None,
        });
        Js.print(&ast)
    }
}
