//! Control-flow flattening: rewrite each eligible function/arrow/method body into
//! a dispatch-loop state machine (`while (1) switch (state) { ... }`).
//!
//! # Pass shape
//!
//! * `id() = "cfflatten"`
//! * `reads() = [decoder_anchor (optional), resolved_scopes (required)]`
//! * `writes() = []`
//! * `enabled() = cf_flatten.enabled`
//!
//! The decoder anchor is OPTIONAL — obtained via
//! [`crate::opaque::anchor_from_bus_or_inject`], which prefers the strings decoder
//! and otherwise injects an independent non-foldable fallback anchor. The opaque
//! state values that defeat the compressor's dispatch-folding therefore work with
//! AND without the strings pass.
//!
//! [`Resource::resolved_scopes`](mangler_passgraph::Resource::resolved_scopes) is
//! REQUIRED: the TDZ rewrite (see [`tdz`]) keys bindings on
//! `(name, SyntaxContext)`, and the resolver is what assigns distinct
//! `SyntaxContext`s to same-named `let`/`const` across scopes. Declaring this read
//! lands the pass POST-resolver so those marks are meaningful.
//!
//! # Bail-to-safe
//!
//! ANY unsupported construct leaves the body un-flattened — never miscompiled. The
//! gate is the fused [`eligibility::scan_gates`] (rejecting try/switch/do-while/
//! for-in/for-of/labeled/with/eval/break/continue and unmodellable TDZ shapes)
//! plus the closure-capture scan [`tdz::loop_let_captured`].

pub mod cfg;
pub mod eligibility;
pub mod emit;
pub mod helpers;
pub mod tdz;

#[cfg(test)]
pub(crate) mod test_support;
#[cfg(test)]
pub(crate) use test_support::test_support_cfg;

use crate::artifacts::DecoderAnchorArtifact;
use crate::config::FileConfig;
use crate::opaque::{anchor_from_bus_or_inject, OpaqueAnchor};
use eligibility::{scan_gates, Eligibility};
use helpers::TdzHelpers;
use mangler_core::{Language, Note, Notes, PassConfig, Result, Rng};
use mangler_jsast::Js;
use mangler_passgraph::{ArtifactBus, Pass, Resource};
use swc_core::common::SyntaxContext;
use swc_core::ecma::ast::*;
use swc_core::ecma::visit::{Visit, VisitMut, VisitMutWith, VisitWith};

/// The control-flow-flattening pass.
pub struct CfFlattenPass;

impl Pass<Js, FileConfig> for CfFlattenPass {
    fn id(&self) -> &'static str {
        "cfflatten"
    }

    /// Reads the decoder anchor (OPTIONAL — falls back to an injected anchor) and
    /// the resolver marks (REQUIRED — lands the pass post-resolver for the TDZ
    /// disambiguation).
    fn reads(&self) -> &[Resource] {
        const R: &[Resource] = &[Resource::decoder_anchor(), Resource::resolved_scopes()];
        R
    }

    fn enabled(&self, cfg: &FileConfig) -> bool {
        cfg.resolved().passes.cf_flatten.enabled
    }

    fn run(
        &self,
        ast: &mut <Js as Language>::Ast,
        cfg: &FileConfig,
        rng: &mut Rng,
        bus: &mut ArtifactBus,
        notes: &mut Notes,
    ) -> Result<()> {
        // Reserve the TDZ guard helper names + seeded surface-form choices up front
        // so per-body name allocation is deterministic regardless of whether the
        // helpers are used (fixed RNG draw order).
        let helpers = TdzHelpers::new(cfg, rng);

        // Resolve the anchor: the strings decoder if present, else an independent
        // injected fallback. The fallback function is spliced at the top of the
        // program once and referenced by every opaque transition.
        let seed_word = format!("c{:x}", cfg.seed() & 0xffff);
        let (anchor, injected) =
            anchor_from_bus_or_inject(ast.program_mut(), bus, || cfg.fresh_name(), &seed_word)
                .map_err(|e| mangler_core::Error::transform(self.id(), e.to_string()))?;
        if injected {
            notes.push(Note::from(
                self.id(),
                "no decoder anchor present; injected an independent opaque anchor",
            ));
        }

        // The declarator whose initializer must NOT emit an `anchor(0)` transition:
        // the decoder `core` if present (calling it before it is assigned would
        // throw / recurse), else the injected fallback anchor name (whose body
        // returns a string — nothing to flatten, but skip uniformly for clarity).
        let core_name: Option<String> = match bus.get::<DecoderAnchorArtifact>() {
            Ok(Some(d)) => Some(d.core_name.clone()),
            _ => None,
        };
        let protect_name = core_name.unwrap_or_else(|| anchor.name().to_string());

        let dead_state_rate = cfg.resolved().passes.cf_flatten.dead_state_rate;
        let state_vars = cfg.resolved().passes.cf_flatten.state_vars;
        // No dedicated config field for the data-dependent ("offset trick") rate
        // (config.rs is owned elsewhere); tie it to `dead_state_rate` exactly as the
        // legacy pass did — both scale control-flow obfuscation aggressiveness and
        // both are `0.0` where `cf_flatten` is off anyway.
        let data_dep_rate = dead_state_rate;

        let used_tdz = {
            let mut v = Flattener {
                cfg,
                rng,
                helpers: &helpers,
                used_tdz: false,
                anchor: &anchor,
                protect_name: &protect_name,
                inside_core_init: false,
                state_vars,
                dead_state_rate,
                data_dep_rate,
            };
            ast.program_mut().visit_mut_with(&mut v);
            v.used_tdz
        };
        if used_tdz {
            helpers::inject(ast.program_mut(), &helpers);
        }
        Ok(())
    }
}

struct Flattener<'a> {
    cfg: &'a FileConfig,
    rng: &'a mut Rng,
    helpers: &'a TdzHelpers,
    used_tdz: bool,
    /// The opaque anchor (decoder core or injected fallback) every opaque
    /// transition couples to.
    anchor: &'a OpaqueAnchor,
    /// The anchor function's binding name; its initializer subtree must not emit an
    /// `anchor(0)`-coupled transition (the anchor is not assigned yet there).
    protect_name: &'a str,
    /// True while traversing inside the anchor's own initializer (`var <core> =
    /// …`). Inside that scope opaque transitions MUST NOT be emitted — they would
    /// call an uninitialized value and throw / recurse. Bodies flattened here fall
    /// back to bare numeric literals.
    inside_core_init: bool,
    state_vars: u8,
    dead_state_rate: f32,
    data_dep_rate: f32,
}

impl Flattener<'_> {
    /// Attempts to flatten one function body in place. Returns whether it did.
    fn try_flatten_body(&mut self, body: &mut BlockStmt, is_simple: bool) -> bool {
        if !is_simple {
            return false;
        }
        // All read-only gate checks are computed in ONE fused walk; the
        // short-circuit ordering below mirrors the original separate checks.
        let gates = scan_gates(body);
        match gates.eligibility {
            Eligibility::Eligible => {}
            Eligibility::Skip(_reason) => return false,
        }
        // The linearizer does not model break/continue — skip to stay sound.
        if gates.has_break_continue {
            return false;
        }
        // A function declaration nested inside control flow has Annex-B /
        // implementation-defined hoisting the linearizer cannot reproduce — bail.
        // (Direct-body declarations ARE handled, by `hoist_fn_decls` below.)
        if gates.has_nested_fn_decl {
            return false;
        }
        // let/const bodies: lower to TDZ-guarded vars first, but only when the usage
        // is within the soundly-handled subset. Otherwise skip.
        if gates.has_let_const {
            if !gates.tdz_struct_safe {
                return false;
            }
            // Loop-declared let/const captured by a closure would need
            // per-iteration freshness the lowering does not emulate.
            if !gates.loop_let_names.is_empty()
                && tdz::loop_let_captured(body, &gates.loop_let_names)
            {
                return false;
            }
            if tdz::rewrite(body, self.cfg, self.helpers) {
                self.used_tdz = true;
            }
            // `body` is now var-only.
        }

        // Extract this body's DIRECT (top-level) function declarations. In JS a
        // function declaration is hoisted to the top of its enclosing function
        // scope — callable from anywhere in the body, including textually-earlier
        // statements. The linearizer would otherwise drop each `function f(){…}`
        // into the switch-case for its textual position, so a call from another
        // case (the common result of branch/loop splitting, or simply the
        // dispatcher visiting cases out of order) would reference an
        // un-initialized binding and throw "<f> is not defined". Pulling the
        // declarations into the function prologue — verbatim, before the dispatch
        // loop — restores exact hoisting semantics. (Function declarations nested
        // inside control-flow constructs have Annex-B / implementation-defined
        // hoisting the CFG cannot model; the gate scan rejects such bodies, so
        // only direct-body declarations reach here.)
        let original = std::mem::take(&mut body.stmts);
        let (fn_decls, original) = hoist_fn_decls(original);

        // Hoist `var` declarations to the top with `undefined` initializers,
        // rewriting their initializers into in-place assignments.
        let (hoisted, rewritten, inscope_idents) = hoist_vars(original);

        // Build the CFG and render the state machine.
        let (blocks, entry, exit) = cfg::build(rewritten);
        // Decouple block id from emitted state value via a seeded permutation.
        let num_live = blocks.len();
        // Dead-state injection: add `round(dead_state_rate * num_live)` unreachable
        // states. The label permutation covers `0..total`.
        let num_dead = (self.dead_state_rate * num_live as f32).round() as usize;
        let total = num_live + num_dead;
        let labels = self.rng.random_perm(total);

        // Choose the dispatcher representation.
        let dispatch = if self.state_vars >= 2 && total >= 1 {
            let k = isqrt_ceil(total);
            let name1 = self.cfg.fresh_name();
            let name2 = self.cfg.fresh_name();
            emit::Dispatch::Two { name1, name2, k }
        } else {
            emit::Dispatch::Single { name: self.cfg.fresh_name() }
        };
        // `None` while inside the anchor's own initializer (bare-literal fallback),
        // else `Some`. `self.anchor` is a `&'a OpaqueAnchor`, so copying the ref out
        // does not borrow `self` for the renderer call below.
        let anchor: Option<&OpaqueAnchor> = if self.inside_core_init {
            None
        } else {
            Some(self.anchor)
        };
        let opts = emit::RenderOpts {
            labels: &labels,
            dispatch,
            anchor,
            inscope_vars: &inscope_idents,
            data_dep_rate: self.data_dep_rate,
        };
        let machine = emit::render(self.rng, blocks, entry, exit, &opts);

        // New body: hoisted function declarations (matching JS hoisting, before
        // any other statement runs), then hoisted var decls, then the rendered
        // machine's statements.
        let mut new_stmts = fn_decls;
        new_stmts.extend(hoisted);
        new_stmts.extend(machine.stmts);
        body.stmts = new_stmts;
        true
    }
}

impl VisitMut for Flattener<'_> {
    fn visit_mut_function(&mut self, n: &mut Function) {
        // Descend first so nested functions are flattened independently.
        n.visit_mut_children_with(self);
        let simple = !n.is_generator && !n.is_async;
        if let Some(body) = &mut n.body {
            self.try_flatten_body(body, simple);
        }
    }

    fn visit_mut_arrow_expr(&mut self, n: &mut ArrowExpr) {
        n.visit_mut_children_with(self);
        let simple = !n.is_async;
        // Only block-bodied arrows (expression bodies have nothing to flatten).
        if let BlockStmtOrExpr::BlockStmt(body) = &mut *n.body {
            self.try_flatten_body(body, simple);
        }
    }

    fn visit_mut_var_declarator(&mut self, n: &mut VarDeclarator) {
        // Detect the anchor's own declarator (`var <core> = ...`). The anchor is not
        // yet assigned while its initializer evaluates, so any function flattened
        // within the initializer must not reference it.
        let is_core_decl =
            matches!(&n.name, Pat::Ident(bi) if bi.id.sym.as_ref() == self.protect_name);
        if is_core_decl {
            let prev = self.inside_core_init;
            self.inside_core_init = true;
            n.visit_mut_children_with(self);
            self.inside_core_init = prev;
        } else {
            n.visit_mut_children_with(self);
        }
    }
}

/// Smallest `k` such that `k * k >= n`, i.e. `ceil(sqrt(n))`. For `n == 0` returns
/// 1 so the two-var split divisor is never zero.
fn isqrt_ceil(n: usize) -> usize {
    if n == 0 {
        return 1;
    }
    let mut k = (n as f64).sqrt() as usize;
    while k * k < n {
        k += 1;
    }
    while k > 1 && (k - 1) * (k - 1) >= n {
        k -= 1;
    }
    k.max(1)
}

/// Splits the body's DIRECT (top-level) function declarations off the front of
/// the statement list, preserving their relative order.
///
/// Returns `(fn_decls, remaining_stmts)`. A `function f(){…}` declaration is
/// hoisted (in JS) to the top of its enclosing function scope with its full
/// definition, so it is callable before its textual position. The flattener
/// emits these declarations into the function prologue (ahead of the dispatch
/// loop) so every emitted `case` sees the binding, exactly as un-flattened code
/// would. Only the body's direct declarations are pulled out — declarations
/// nested inside control-flow are rejected by the gate scan and never reach
/// here.
fn hoist_fn_decls(stmts: Vec<Stmt>) -> (Vec<Stmt>, Vec<Stmt>) {
    let mut fn_decls: Vec<Stmt> = Vec::new();
    let mut rest: Vec<Stmt> = Vec::with_capacity(stmts.len());
    for s in stmts {
        if matches!(&s, Stmt::Decl(Decl::Fn(_))) {
            fn_decls.push(s);
        } else {
            rest.push(s);
        }
    }
    (fn_decls, rest)
}

/// Splits each top-level `var` declaration into a hoisted `var X;` (no init) and an
/// in-place assignment `X = init;` where it originally appeared. Only the
/// declarations are hoisted (matching JS hoisting); initializers keep their
/// original position and order.
///
/// Returns `(hoisted_decls, rewritten_body, hoisted_idents)`. `var` declarations
/// nested inside control-flow constructs are left in place, so only the body's
/// direct `var` statements are hoisted.
///
/// `hoisted_idents` are the (resolver-context-carrying) `Ident`s of every
/// function-hoisted `var` binding. They are live in EVERY emitted `case`, which
/// makes them the safe source for the data-dependent transition trick. They keep
/// their original `SyntaxContext`, so the post-flatten renamer rewrites these
/// injected references in lockstep with the binding.
fn hoist_vars(stmts: Vec<Stmt>) -> (Vec<Stmt>, Vec<Stmt>, Vec<Ident>) {
    use swc_core::common::DUMMY_SP;
    let mut out: Vec<Stmt> = Vec::with_capacity(stmts.len());

    // Collect every `var`-declared simple binding anywhere in this body (not in
    // nested functions) so all names are hoisted to a single declaration.
    struct Collect {
        names: Vec<Ident>,
    }
    impl Visit for Collect {
        fn visit_var_decl(&mut self, n: &VarDecl) {
            if n.kind == VarDeclKind::Var {
                for d in &n.decls {
                    if let Pat::Ident(bi) = &d.name {
                        self.names.push(bi.id.clone());
                    }
                }
            }
            n.visit_children_with(self);
        }
        fn visit_function(&mut self, _n: &Function) {}
        fn visit_arrow_expr(&mut self, _n: &ArrowExpr) {}
        fn visit_class(&mut self, _n: &Class) {}
    }

    let mut collect = Collect { names: Vec::new() };
    for s in &stmts {
        s.visit_with(&mut collect);
    }
    let hoisted_names = collect.names;
    let inscope_idents = hoisted_names.clone();

    // Rewrite the statement list: turn top-level `var` decls into assignment
    // expression statements (preserving order).
    for s in stmts {
        match s {
            Stmt::Decl(Decl::Var(var)) if var.kind == VarDeclKind::Var => {
                for d in var.decls.into_iter() {
                    match (&d.name, d.init) {
                        (Pat::Ident(bi), Some(init)) => {
                            out.push(Stmt::Expr(ExprStmt {
                                span: DUMMY_SP,
                                expr: Box::new(Expr::Assign(AssignExpr {
                                    span: DUMMY_SP,
                                    op: AssignOp::Assign,
                                    left: AssignTarget::Simple(SimpleAssignTarget::Ident(
                                        BindingIdent { id: bi.id.clone(), type_ann: None },
                                    )),
                                    right: init,
                                })),
                            }));
                        }
                        (Pat::Ident(_), None) => { /* bare decl: hoisted below */ }
                        (_, init) => {
                            // Non-ident (destructuring) pattern: keep as a
                            // single-declarator var at its original position.
                            out.push(Stmt::Decl(Decl::Var(Box::new(VarDecl {
                                span: DUMMY_SP,
                                ctxt: var.ctxt,
                                kind: VarDeclKind::Var,
                                declare: false,
                                decls: vec![VarDeclarator {
                                    span: DUMMY_SP,
                                    name: d.name,
                                    init,
                                    definite: false,
                                }],
                            }))));
                        }
                    }
                }
            }
            other => out.push(other),
        }
    }

    // Build the single hoisting decl: `var a, b, c;` (no initializers).
    let hoisted = if hoisted_names.is_empty() {
        Vec::new()
    } else {
        let decls = hoisted_names
            .into_iter()
            .map(|id| VarDeclarator {
                span: DUMMY_SP,
                name: Pat::Ident(BindingIdent { id, type_ann: None }),
                init: None,
                definite: false,
            })
            .collect();
        vec![Stmt::Decl(Decl::Var(Box::new(VarDecl {
            span: DUMMY_SP,
            ctxt: SyntaxContext::empty(),
            kind: VarDeclKind::Var,
            declare: false,
            decls,
        })))]
    };

    (hoisted, out, inscope_idents)
}

#[cfg(test)]
mod tests;
