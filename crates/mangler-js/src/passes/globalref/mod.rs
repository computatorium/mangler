//! Global-reference indirection pass.
//!
//! Hoists provably-free globals into load-time-cached locals and rewrites their
//! use sites to bare local reads, so use sites cost a local read + property
//! lookup with no per-operation decode. Ordered `member-access → globalref →
//! strings` by its declared reads/writes: it `reads()` [`PropertyLiterals`] (so
//! it runs after member-access and sees `document["getElementById"]`) and
//! `writes()` [`GlobalNameLiterals`] (so the strings pass — which reads it —
//! runs after, encoding the global-name string literals this pass injects).
//!
//! ## Detection
//!
//! A name is *indirectable* iff it is **never declared anywhere in the file**
//! (see [`detect`]). Any shadow of `X` anywhere disables indirection of `X`
//! file-wide. Files containing a direct `eval(` call or a `with` statement bail
//! entirely (those can introduce dynamic bindings, making free-global detection
//! unsound).
//!
//! ## Selection
//!
//! * `Safe` (default): only names on the curated [`allowlist`].
//! * `Aggressive`: every never-declared free name. `globalThis` is excluded in
//!   both modes (it is the anchor).
//!
//! ## Whole-name write-exclusion (correctness-critical)
//!
//! Because the hoisted alias `_Ga = _G["X"]` captures the global's value *once at
//! load*, a name is indirectable only if EVERY occurrence in the file is a
//! *read*. If a free-global name `X` appears in ANY write / mutation-target
//! position — assignment target, `++`/`--`, bare `delete X`, or a non-`VarDecl`
//! for-in/for-of head — the ENTIRE name `X` is excluded (detected up front in
//! [`detect`] via `Detection::written`, enforced by `Rewriter::is_selected`).
//!
//! **Why hoist-and-cache preserves `this`:** calling the hoisted local bare
//! (`_Ga(args)`) is an *unqualified* call of the same function value the original
//! bare `X(args)` resolved to, so `this` is identical (undefined strict / global
//! sloppy).
//!
//! ## Injection
//!
//! At the top of the program body (after any leading directive prologue so
//! `"use strict"` stays first) we inject the anchor `var _G = globalThis;`
//! (anchor hardening may rewrite this) plus, depending on the combined entry
//! count, either a one-hop `var _Ga = _G["<name>"]` table or a dispatcher
//! (`_GN`/`_perm`/`_Gd`) that hides the alias→global mapping. The `"<name>"`
//! literals are plain string literals the strings pass (ordered after us) then
//! encodes. We also inject seeded, never-referenced **decoy** aliases for
//! plausible allowlisted globals the file does NOT use.

mod allowlist;
mod detect;

use crate::artifacts::{GlobalNameLiteralsArtifact, PropertyLiteralsArtifact};
use crate::config::FileConfig;
use mangler_config::{GlobalIndirect, Intensity};
use mangler_core::{Language, Notes, Result, Rng};
use mangler_jsast::Js;
use mangler_passgraph::{ArtifactBus, Pass, Resource};
use std::collections::{HashMap, HashSet};
use swc_core::common::{SyntaxContext, DUMMY_SP};
use swc_core::ecma::ast::*;
use swc_core::ecma::visit::{VisitMut, VisitMutWith};

/// Indirects provably-free globals through load-time-cached locals.
pub struct GlobalRefPass;

impl Pass<Js, FileConfig> for GlobalRefPass {
    fn id(&self) -> &'static str {
        "globalref"
    }

    /// Reads property-name literals: orders this pass AFTER member-access, so a
    /// `document.getElementById` is already `document["getElementById"]` when we
    /// rewrite its base. The payload is empty — only the scheduler edge matters.
    fn reads(&self) -> &[Resource] {
        const R: &[Resource] = &[Resource::property_literals()];
        R
    }

    /// Writes global-name literals: orders this pass BEFORE strings, so the
    /// global-name string literals it injects get encoded by the strings pass.
    fn writes(&self) -> &[Resource] {
        const W: &[Resource] = &[Resource::global_name_literals()];
        W
    }

    /// Gated by the `global_indirect.mode` knob (Off in Minify/Low presets).
    fn enabled(&self, cfg: &FileConfig) -> bool {
        cfg.resolved().passes.global_indirect.mode != GlobalIndirect::Off
    }

    fn run(
        &self,
        ast: &mut <Js as Language>::Ast,
        cfg: &FileConfig,
        rng: &mut Rng,
        bus: &mut ArtifactBus,
        notes: &mut Notes,
    ) -> Result<()> {
        // Read the (declared) property-literals marker purely to satisfy the
        // declared edge surface; its absence is a soft no-op (member-access may
        // have been disabled). We do not need its payload.
        let _ = bus.get::<PropertyLiteralsArtifact>();

        run(ast.program_mut(), cfg, rng, notes);

        // Announce that global-name string literals now exist so the strings pass
        // (which reads this resource) is ordered after us and encodes them.
        bus.put(GlobalNameLiteralsArtifact)
            .map_err(|e| mangler_core::Error::transform(self.id(), e.to_string()))?;
        Ok(())
    }
}

/// The anchor name (`globalThis`) is never itself indirected.
const ANCHOR_GLOBAL: &str = "globalThis";

fn run(program: &mut Program, cfg: &FileConfig, rng: &mut Rng, notes: &mut Notes) {
    // 1. Detection (declared-set + bail flag). A direct `eval(` / `with` makes
    //    free-global detection unsound, so we bail entirely.
    let detection = detect::detect(program);
    if detection.bail {
        notes.push(mangler_core::Note::from(
            "globalref",
            "dynamic scope (eval/with); skipping",
        ));
        return;
    }

    let aggressive = cfg.resolved().passes.global_indirect.mode == GlobalIndirect::Aggressive;

    let mut all_idents: HashSet<String> = detection.all_idents;
    all_idents.extend(detection.declared.iter().cloned());

    // 2. Rewrite references, recording which selected globals were actually
    // indirected (so we only hoist aliases that are used).
    let mut rewriter = Rewriter {
        declared: &detection.declared,
        written: &detection.written,
        aggressive,
        aliases: HashMap::new(),
        order: Vec::new(),
        cfg,
        all_idents: &all_idents,
    };
    program.visit_mut_with(&mut rewriter);

    let used: Vec<(String, String)> = rewriter
        .order
        .iter()
        .map(|name| (name.clone(), rewriter.aliases[name].clone()))
        .collect();

    // Reserve names already chosen as real aliases + the global names themselves.
    for (g, a) in &used {
        all_idents.insert(g.clone());
        all_idents.insert(a.clone());
    }

    // If no real global was indirected there is nothing to hoist and no table to
    // pad: emit nothing (a table of pure decoys would be pointless noise and would
    // inject string literals into an otherwise string-free file).
    if used.is_empty() {
        notes.push(mangler_core::Note::from(
            "globalref",
            "no indirectable free globals; skipping",
        ));
        return;
    }

    let anchor_name = fresh_unique(cfg, &mut all_idents);

    // Decoy aliases for plausible allowlisted globals the file does not use.
    let decoys = build_decoys(cfg, rng, &detection.declared, &used, &mut all_idents);

    inject_table(program, cfg, rng, &anchor_name, &used, &decoys);
}

/// Allocate a fresh `cfg` name not colliding with `reserved`, inserting it.
fn fresh_unique(cfg: &FileConfig, reserved: &mut HashSet<String>) -> String {
    loop {
        let n = cfg.fresh_name();
        if !reserved.contains(&n) {
            reserved.insert(n.clone());
            return n;
        }
    }
}

// ---------------------------------------------------------------------------
// Reference rewriter
// ---------------------------------------------------------------------------

struct Rewriter<'a> {
    declared: &'a HashSet<String>,
    /// Names that appear in ANY write/mutation-target position file-wide. Per
    /// the whole-name write-exclusion rule, such names are never indirected.
    written: &'a HashSet<String>,
    aggressive: bool,
    /// global name → alias ident name.
    aliases: HashMap<String, String>,
    /// global names in first-use order (drives stable hoist order before shuffle).
    order: Vec<String>,
    cfg: &'a FileConfig,
    all_idents: &'a HashSet<String>,
}

impl Rewriter<'_> {
    /// Whether the bare identifier `name` is a selected free global to indirect.
    fn is_selected(&self, name: &str) -> bool {
        if name == ANCHOR_GLOBAL {
            return false;
        }
        if self.declared.contains(name) {
            return false;
        }
        // Whole-name write-exclusion.
        if self.written.contains(name) {
            return false;
        }
        if self.aggressive {
            true
        } else {
            allowlist::is_allowlisted(name)
        }
    }

    /// Return (allocating if needed) the alias ident name for global `name`.
    fn alias_for(&mut self, name: &str) -> String {
        if let Some(a) = self.aliases.get(name) {
            return a.clone();
        }
        let alias = loop {
            let cand = self.cfg.fresh_name();
            if !self.all_idents.contains(&cand) && !self.aliases.values().any(|v| v == &cand) {
                break cand;
            }
        };
        self.aliases.insert(name.to_string(), alias.clone());
        self.order.push(name.to_string());
        alias
    }

    /// If `expr` is a bare selected-global ident, replace it with its alias.
    fn try_rewrite_ident(&mut self, expr: &mut Expr) -> bool {
        if let Expr::Ident(id) = expr {
            let name = id.sym.to_string();
            if self.is_selected(&name) {
                let alias = self.alias_for(&name);
                *expr = Expr::Ident(Ident::new(alias.into(), DUMMY_SP, SyntaxContext::empty()));
                return true;
            }
        }
        false
    }
}

impl VisitMut for Rewriter<'_> {
    fn visit_mut_expr(&mut self, expr: &mut Expr) {
        // Post-order: descend first so nested references are rewritten before we
        // consider this node.
        expr.visit_mut_children_with(self);
        self.try_rewrite_ident(expr);
    }

    /// Assignment: leave a bare-ident LHS target untouched (writing the alias
    /// would write the local, not the global). Still rewrite the RHS and any
    /// member/computed sub-expressions of the LHS.
    fn visit_mut_assign_expr(&mut self, n: &mut AssignExpr) {
        n.right.visit_mut_with(self);
        match &mut n.left {
            AssignTarget::Simple(SimpleAssignTarget::Ident(_)) => {
                // bare ident assignment target — skip.
            }
            other => other.visit_mut_with(self),
        }
    }

    /// `++X` / `--X` / `X++` / `X--`: leave a bare-ident operand untouched.
    fn visit_mut_update_expr(&mut self, n: &mut UpdateExpr) {
        if matches!(&*n.arg, Expr::Ident(_)) {
            return;
        }
        n.arg.visit_mut_with(self);
    }

    /// `delete X`: leave a bare-ident operand untouched. `typeof X` / `void X`
    /// are ordinary reference positions — descend so the bare ident IS indirected.
    fn visit_mut_unary_expr(&mut self, n: &mut UnaryExpr) {
        if n.op == UnaryOp::Delete && matches!(&*n.arg, Expr::Ident(_)) {
            return;
        }
        n.arg.visit_mut_with(self);
    }

    /// `var`/`let`/`const` names are binding positions; only descend into the
    /// initializer (a reference position). By construction a selected name is
    /// never declared, so the name side never holds a selected ident anyway.
    fn visit_mut_var_declarator(&mut self, n: &mut VarDeclarator) {
        if let Some(init) = n.init.as_mut() {
            init.visit_mut_with(self);
        }
    }

    /// Object-literal shorthand `{ x }`: `x` is a *reference*, so it is a valid
    /// indirection target. Expand a selected shorthand to a key/value pair so the
    /// value can be the alias.
    fn visit_mut_prop(&mut self, n: &mut Prop) {
        n.visit_mut_children_with(self);
        if let Prop::Shorthand(id) = n {
            let name = id.sym.to_string();
            if self.is_selected(&name) {
                let alias = self.alias_for(&name);
                *n = Prop::KeyValue(KeyValueProp {
                    key: PropName::Ident(IdentName::new(name.into(), DUMMY_SP)),
                    value: Box::new(Expr::Ident(Ident::new(
                        alias.into(),
                        DUMMY_SP,
                        SyntaxContext::empty(),
                    ))),
                });
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Decoy generation
// ---------------------------------------------------------------------------

/// Decoy count for the resolved intensity. globalref is Off at Minify/Low (gated
/// by `enabled`), so those branches are inert.
fn decoy_count(level: Intensity) -> usize {
    match level {
        Intensity::Minify | Intensity::Low => 0,
        Intensity::Medium => 2,
        Intensity::High => 4,
        Intensity::Max => 6,
    }
}

/// Build decoy aliases: seeded, never-referenced reads of real allowlisted
/// globals the file does NOT use (so they are inert). Count scales with intensity.
fn build_decoys(
    cfg: &FileConfig,
    rng: &mut Rng,
    declared: &HashSet<String>,
    used: &[(String, String)],
    all_idents: &mut HashSet<String>,
) -> Vec<(String, String)> {
    let count = decoy_count(cfg.resolved().engine.level);
    if count == 0 {
        return Vec::new();
    }

    // Candidate global names: allowlisted, not declared in the file, and not
    // already used as a real indirection.
    let used_names: HashSet<&str> = used.iter().map(|(g, _)| g.as_str()).collect();
    let candidates: Vec<&'static str> = allowlist::names()
        .iter()
        .copied()
        .filter(|n| !declared.contains(*n) && !used_names.contains(*n))
        .collect();
    if candidates.is_empty() {
        return Vec::new();
    }

    // Pick `count` distinct candidates via a seeded permutation.
    let perm = rng.random_perm(candidates.len());
    let mut decoys = Vec::new();
    for &idx in perm.iter().take(count) {
        let gname = candidates[idx].to_string();
        let alias = fresh_unique(cfg, all_idents);
        decoys.push((gname, alias));
    }
    decoys
}

// ---------------------------------------------------------------------------
// Table injection
// ---------------------------------------------------------------------------

/// Minimum combined (real + decoy) entry count at/above which the dispatcher
/// shape is emitted; below it the one-hop fallback is used.
const DISPATCHER_MIN_ENTRIES: usize = 3;

/// Build and splice the hoisted decl table at the top of the program body (after
/// the leading directive prologue).
fn inject_table(
    program: &mut Program,
    cfg: &FileConfig,
    rng: &mut Rng,
    anchor_name: &str,
    used: &[(String, String)],
    decoys: &[(String, String)],
) {
    let total = used.len() + decoys.len();
    if total >= DISPATCHER_MIN_ENTRIES {
        inject_dispatcher_table(program, cfg, rng, anchor_name, used, decoys);
    } else {
        inject_onehop_table(program, cfg, rng, anchor_name, used, decoys);
    }
}

/// One-hop fallback shape: `var _Ga = _G[name]` per alias, shuffled with decoys
/// under one anchor. Used only for tiny combined entry counts.
fn inject_onehop_table(
    program: &mut Program,
    cfg: &FileConfig,
    rng: &mut Rng,
    anchor_name: &str,
    used: &[(String, String)],
    decoys: &[(String, String)],
) {
    let anchor_stmt = build_anchor_decl(cfg, anchor_name);

    let mut alias_stmts: Vec<Stmt> = Vec::with_capacity(used.len() + decoys.len());
    for (gname, alias) in used.iter().chain(decoys.iter()) {
        alias_stmts.push(build_alias_decl(anchor_name, alias, gname));
    }
    let perm = rng.random_perm(alias_stmts.len());
    let mut shuffled: Vec<Stmt> = Vec::with_capacity(alias_stmts.len());
    let mut taken: Vec<Option<Stmt>> = alias_stmts.into_iter().map(Some).collect();
    for &src in &perm {
        shuffled.push(taken[src].take().expect("perm is a bijection"));
    }

    let mut table: Vec<Stmt> = Vec::with_capacity(1 + shuffled.len());
    table.push(anchor_stmt);
    table.extend(shuffled);

    splice_stmts_at_prologue(program, table);
}

/// Dispatcher shape. `store[e]` is the names-array position of entry `e`; `perm`
/// is a seeded involution baked as `_perm`. The dispatcher computes
/// `_G[_GN[_perm[i]]]`; for a used entry `e` the per-site key is
/// `K = perm[store[e]]`, which (since `perm` is an involution) round-trips to
/// `_G[name_of(e)]`. Decoys occupy `_GN`/`_perm` slots but get no cached call.
fn inject_dispatcher_table(
    program: &mut Program,
    cfg: &FileConfig,
    rng: &mut Rng,
    anchor_name: &str,
    used: &[(String, String)],
    decoys: &[(String, String)],
) {
    let entries: Vec<&(String, String)> = used.iter().chain(decoys.iter()).collect();
    let n = entries.len();

    let store = rng.random_perm(n);
    let perm = dispatcher_involution(n, rng);

    let entry_names: Vec<&str> = entries.iter().map(|(g, _)| g.as_str()).collect();
    let name_at_pos = dispatcher_name_at_pos(&entry_names, &store);
    let gn_name = cfg.fresh_name();
    let perm_name = cfg.fresh_name();
    let gd_name = cfg.fresh_name();

    let anchor_stmt = build_anchor_decl(cfg, anchor_name);
    let names_arr_stmt = build_names_array_decl(&gn_name, &name_at_pos);
    let perm_arr_stmt = build_perm_array_decl(&perm_name, &perm);
    let dispatcher_stmt = build_dispatcher_decl(&gd_name, anchor_name, &gn_name, &perm_name);

    let mut cached: Vec<Stmt> = Vec::with_capacity(used.len());
    for (e, (_gname, alias)) in used.iter().enumerate() {
        let k = dispatcher_key(&perm, &store, e);
        cached.push(build_cached_call_decl(alias, &gd_name, k));
    }
    let cperm = rng.random_perm(cached.len());
    let mut cached_shuffled: Vec<Stmt> = Vec::with_capacity(cached.len());
    let mut taken: Vec<Option<Stmt>> = cached.into_iter().map(Some).collect();
    for &src in &cperm {
        cached_shuffled.push(taken[src].take().expect("perm is a bijection"));
    }

    let mut table: Vec<Stmt> = Vec::with_capacity(4 + cached_shuffled.len());
    table.push(anchor_stmt);
    table.push(names_arr_stmt);
    table.push(perm_arr_stmt);
    table.push(dispatcher_stmt);
    table.extend(cached_shuffled);

    splice_stmts_at_prologue(program, table);
}

/// Seeded **involution** over `0..n` (`perm[perm[i]] == i`), built from random
/// pair swaps. Self-inverse so the dispatcher index math round-trips without a
/// separate inverse table.
fn dispatcher_involution(n: usize, rng: &mut Rng) -> Vec<usize> {
    let mut perm: Vec<usize> = (0..n).collect();
    if n < 2 {
        return perm;
    }
    let mut unpaired: Vec<usize> = (0..n).collect();
    while unpaired.len() >= 2 {
        let i = unpaired.remove(0);
        // 50/50 leave it as a fixed point.
        if rng.pick(2) == 0 {
            continue;
        }
        let j_pos = rng.pick(unpaired.len());
        let j = unpaired.remove(j_pos);
        perm.swap(i, j);
    }
    perm
}

/// The per-site index `K` for entry `e`: `perm[store[e]]`.
fn dispatcher_key(perm: &[usize], store: &[usize], e: usize) -> usize {
    perm[store[e]]
}

/// Mirror of the runtime dispatcher `_G[_GN[_perm[i]]]`, returning the *name* the
/// dispatcher resolves for argument `i`. Used by the bijection unit test.
#[cfg(test)]
fn dispatcher_resolve<'a>(name_at_pos: &[&'a str], perm: &[usize], i: usize) -> &'a str {
    name_at_pos[perm[i]]
}

/// Build the `name_at_pos` mapping (`_GN` contents) from `store`: position `p`
/// holds the name of the entry `e` with `store[e] == p`.
fn dispatcher_name_at_pos<'a>(entries: &[&'a str], store: &[usize]) -> Vec<&'a str> {
    let mut name_at_pos: Vec<&str> = vec![""; store.len()];
    for (e, &p) in store.iter().enumerate() {
        name_at_pos[p] = entries[e];
    }
    name_at_pos
}

/// `var <gn> = ["<name@0>", "<name@1>", …];` — plain string literals the strings
/// pass encodes later.
fn build_names_array_decl(gn_name: &str, names: &[&str]) -> Stmt {
    let elems: Vec<Option<ExprOrSpread>> = names
        .iter()
        .map(|name| {
            Some(ExprOrSpread {
                spread: None,
                expr: Box::new(Expr::Lit(Lit::Str(Str {
                    span: DUMMY_SP,
                    value: (*name).into(),
                    raw: None,
                }))),
            })
        })
        .collect();
    let arr = Expr::Array(ArrayLit { span: DUMMY_SP, elems });
    single_var_decl(gn_name, arr)
}

/// `var <perm> = [p0, p1, …];` — the baked involution as a literal number array.
fn build_perm_array_decl(perm_name: &str, perm: &[usize]) -> Stmt {
    let elems: Vec<Option<ExprOrSpread>> = perm
        .iter()
        .map(|&p| {
            Some(ExprOrSpread {
                spread: None,
                expr: Box::new(Expr::Lit(Lit::Num(Number {
                    span: DUMMY_SP,
                    value: p as f64,
                    raw: None,
                }))),
            })
        })
        .collect();
    let arr = Expr::Array(ArrayLit { span: DUMMY_SP, elems });
    single_var_decl(perm_name, arr)
}

/// `var <gd> = function(i){ return <anchor>[<gn>[<perm>[i]]]; };`
fn build_dispatcher_decl(gd_name: &str, anchor_name: &str, gn_name: &str, perm_name: &str) -> Stmt {
    let param_name = "i";
    let perm_i = index(ident_expr(perm_name), ident_expr(param_name));
    let gn_idx = index(ident_expr(gn_name), perm_i);
    let body_expr = index(ident_expr(anchor_name), gn_idx);

    let func = Function {
        params: vec![Param {
            span: DUMMY_SP,
            decorators: vec![],
            pat: Pat::Ident(BindingIdent {
                id: Ident::new(param_name.into(), DUMMY_SP, SyntaxContext::empty()),
                type_ann: None,
            }),
        }],
        decorators: vec![],
        span: DUMMY_SP,
        ctxt: SyntaxContext::empty(),
        body: Some(BlockStmt {
            span: DUMMY_SP,
            ctxt: SyntaxContext::empty(),
            stmts: vec![Stmt::Return(ReturnStmt {
                span: DUMMY_SP,
                arg: Some(Box::new(body_expr)),
            })],
        }),
        is_generator: false,
        is_async: false,
        type_params: None,
        return_type: None,
    };
    let fn_expr = Expr::Fn(FnExpr { ident: None, function: Box::new(func) });
    single_var_decl(gd_name, fn_expr)
}

/// `var <alias> = <gd>(<k>);` — cache the resolved global once at load.
fn build_cached_call_decl(alias: &str, gd_name: &str, k: usize) -> Stmt {
    let call = Expr::Call(CallExpr {
        span: DUMMY_SP,
        ctxt: SyntaxContext::empty(),
        callee: Callee::Expr(Box::new(ident_expr(gd_name))),
        args: vec![ExprOrSpread {
            spread: None,
            expr: Box::new(Expr::Lit(Lit::Num(Number {
                span: DUMMY_SP,
                value: k as f64,
                raw: None,
            }))),
        }],
        type_args: None,
    });
    single_var_decl(alias, call)
}

/// `<obj>[<prop>]` computed member access.
fn index(obj: Expr, prop: Expr) -> Expr {
    Expr::Member(MemberExpr {
        span: DUMMY_SP,
        obj: Box::new(obj),
        prop: MemberProp::Computed(ComputedPropName {
            span: DUMMY_SP,
            expr: Box::new(prop),
        }),
    })
}

/// Bare identifier expression.
fn ident_expr(name: &str) -> Expr {
    Expr::Ident(Ident::new(name.into(), DUMMY_SP, SyntaxContext::empty()))
}

/// `var <anchor> = globalThis;` (default) or the hardened derivation.
fn build_anchor_decl(cfg: &FileConfig, anchor_name: &str) -> Stmt {
    let init: Expr = if cfg.resolved().passes.global_indirect.harden_anchor {
        // Hardened anchor: `(function(){return this})() || globalThis`.
        build_hardened_anchor_init()
    } else {
        Expr::Ident(Ident::new(ANCHOR_GLOBAL.into(), DUMMY_SP, SyntaxContext::empty()))
    };
    single_var_decl(anchor_name, init)
}

/// Build `(function(){return this})() || globalThis`.
fn build_hardened_anchor_init() -> Expr {
    let func = Function {
        params: vec![],
        decorators: vec![],
        span: DUMMY_SP,
        ctxt: SyntaxContext::empty(),
        body: Some(BlockStmt {
            span: DUMMY_SP,
            ctxt: SyntaxContext::empty(),
            stmts: vec![Stmt::Return(ReturnStmt {
                span: DUMMY_SP,
                arg: Some(Box::new(Expr::This(ThisExpr { span: DUMMY_SP }))),
            })],
        }),
        is_generator: false,
        is_async: false,
        type_params: None,
        return_type: None,
    };
    let fn_expr = Expr::Fn(FnExpr { ident: None, function: Box::new(func) });
    let call = Expr::Call(CallExpr {
        span: DUMMY_SP,
        ctxt: SyntaxContext::empty(),
        callee: Callee::Expr(Box::new(Expr::Paren(ParenExpr {
            span: DUMMY_SP,
            expr: Box::new(fn_expr),
        }))),
        args: vec![],
        type_args: None,
    });
    Expr::Bin(BinExpr {
        span: DUMMY_SP,
        op: BinaryOp::LogicalOr,
        left: Box::new(call),
        right: Box::new(Expr::Ident(Ident::new(
            ANCHOR_GLOBAL.into(),
            DUMMY_SP,
            SyntaxContext::empty(),
        ))),
    })
}

/// `var <alias> = <anchor>["<gname>"];`
fn build_alias_decl(anchor_name: &str, alias: &str, gname: &str) -> Stmt {
    let member = Expr::Member(MemberExpr {
        span: DUMMY_SP,
        obj: Box::new(Expr::Ident(Ident::new(
            anchor_name.into(),
            DUMMY_SP,
            SyntaxContext::empty(),
        ))),
        prop: MemberProp::Computed(ComputedPropName {
            span: DUMMY_SP,
            expr: Box::new(Expr::Lit(Lit::Str(Str {
                span: DUMMY_SP,
                value: gname.into(),
                raw: None,
            }))),
        }),
    });
    single_var_decl(alias, member)
}

/// `var <name> = <init>;`
fn single_var_decl(name: &str, init: Expr) -> Stmt {
    Stmt::Decl(Decl::Var(Box::new(VarDecl {
        span: DUMMY_SP,
        ctxt: SyntaxContext::empty(),
        kind: VarDeclKind::Var,
        declare: false,
        decls: vec![VarDeclarator {
            span: DUMMY_SP,
            name: Pat::Ident(BindingIdent {
                id: Ident::new(name.into(), DUMMY_SP, SyntaxContext::empty()),
                type_ann: None,
            }),
            init: Some(Box::new(init)),
            definite: false,
        }],
    })))
}

// ---------------------------------------------------------------------------
// Prologue-aware splicing
// ---------------------------------------------------------------------------

/// Is `stmt` a leading directive-prologue statement (`"use strict";` etc.)?
/// A directive is an expression statement whose expression is a bare string
/// literal. Only a *contiguous leading run* of these counts as the prologue.
fn is_directive_stmt(stmt: &Stmt) -> bool {
    matches!(
        stmt,
        Stmt::Expr(ExprStmt { expr, .. }) if matches!(&**expr, Expr::Lit(Lit::Str(_)))
    )
}

fn leading_directive_count(stmts: &[Stmt]) -> usize {
    stmts.iter().take_while(|s| is_directive_stmt(s)).count()
}

/// Splice `stmts` into `program` at the leading-directive boundary, so a
/// `"use strict"` prologue stays first.
fn splice_stmts_at_prologue(program: &mut Program, stmts: Vec<Stmt>) {
    match program {
        Program::Script(s) => {
            let at = leading_directive_count(&s.body);
            s.body.splice(at..at, stmts);
        }
        Program::Module(m) => {
            // A module's leading directives are `ModuleItem::Stmt(ExprStmt(str))`.
            let at = m
                .body
                .iter()
                .take_while(|it| match it {
                    ModuleItem::Stmt(s) => is_directive_stmt(s),
                    ModuleItem::ModuleDecl(_) => false,
                })
                .count();
            let items: Vec<ModuleItem> = stmts.into_iter().map(ModuleItem::Stmt).collect();
            m.body.splice(at..at, items);
        }
    }
}

#[cfg(test)]
mod tests;
