//! Dead-code injection: prepend always-FALSE opaque-predicate guards wrapping
//! unreachable junk branches to function / arrow bodies.
//!
//! The guard is [`crate::opaque::opaque_bool`]`(rng, &anchor, false)`: it is
//! provably `false` at runtime (so the wrapped junk never executes and behavior
//! is preserved exactly) yet non-foldable by swc (so the compressor keeps both the
//! predicate and the dead branch). The junk inside is pure, side-effect-free
//! filler built from fresh `var`s — it would be inert even if it ran.
//!
//! # Decoupled anchor (design decision 4)
//!
//! Declares `reads() = [Resource::decoder_anchor()]` but treats it as OPTIONAL:
//! [`crate::opaque::anchor_from_bus_or_inject`] returns the strings decoder anchor
//! when the strings pass ran, else injects an independent, non-foldable fallback
//! anchor at the top of the program. So dead-code no longer HARD-requires strings.
//!
//! # Skips
//!
//! The decoder's own initializer subtree (`var <core> = …`) is skipped: injecting
//! a `core(0)`-anchored guard there would emit a call to `core` before it is
//! assigned. Mirrors the legacy `inside_core_init` guard, using
//! [`DecoderAnchorArtifact::is_core_declarator`].

use crate::artifacts::{DecoderAnchorArtifact, VmTableArtifact};
use crate::config::FileConfig;
use crate::opaque::{OpaqueAnchor, anchor_from_bus_or_inject, opaque_bool};
use mangler_core::{Language, Note, Notes, PassConfig, Result, Rng};
use mangler_jsast::Js;
use mangler_jsast::build as b;
use mangler_passgraph::{ArtifactBus, Pass, Resource};
use swc_core::ecma::ast::{
    ArrowExpr, BlockStmt, BlockStmtOrExpr, Function, IfStmt, Pat, Stmt, VarDeclKind,
};
use swc_core::ecma::visit::{VisitMut, VisitMutWith};

/// Prepends always-false-guarded dead branches to function / arrow bodies, at the
/// rate `cf_flatten.dead_code_rate`. Behavior-preserving: the guards are constant
/// `false`, so the wrapped junk is unreachable.
pub struct DeadCodePass;

impl Pass<Js, FileConfig> for DeadCodePass {
    fn id(&self) -> &'static str {
        "deadcode"
    }

    /// Reads the decoder anchor — OPTIONAL (falls back to an injected anchor when
    /// absent; see the module docs). Declaring the read is what lets the bus permit
    /// `get::<DecoderAnchorArtifact>()` and orders this pass after strings when
    /// strings is enabled.
    fn reads(&self) -> &[Resource] {
        const R: &[Resource] = &[Resource::decoder_anchor(), Resource::vm_table()];
        R
    }

    fn enabled(&self, cfg: &FileConfig) -> bool {
        cfg.resolved().passes.cf_flatten.dead_code_rate > 0.0
    }

    fn run(
        &self,
        ast: &mut <Js as Language>::Ast,
        cfg: &FileConfig,
        rng: &mut Rng,
        bus: &mut ArtifactBus,
        notes: &mut Notes,
    ) -> Result<()> {
        // Resolve the anchor: decoder if present, else inject an independent one.
        let seed_word = format!("d{:x}", cfg.seed() & 0xffff);
        let (anchor, injected) =
            anchor_from_bus_or_inject(ast.program_mut(), bus, || cfg.fresh_name(), &seed_word)
                .map_err(|e| mangler_core::Error::transform(self.id(), e.to_string()))?;

        if injected {
            notes.push(Note::from(
                self.id(),
                "no decoder anchor present; injected an independent opaque anchor",
            ));
        }

        // The declarator whose initializer must NOT receive a `core(0)` guard: the
        // decoder `core` (only meaningful when an actual decoder anchor is present;
        // the injected fallback returns a string and is fine to leave unguarded, and
        // we never recurse into it because we only inject into function bodies).
        let protect_name: Option<String> = match bus.get::<DecoderAnchorArtifact>() {
            Ok(Some(d)) => Some(d.core_name.clone()),
            _ => None,
        };

        // The anchor function (decoder `core` OR the injected fallback) must never
        // receive injection: its body is on the value path of every guard, so a
        // `core(0)` guard inside it would recurse forever / read `core` before it is
        // assigned. Skip both forms — a named `function <anchor>(…)` declaration
        // (the fallback shape) and a `var <anchor> = …` initializer (the decoder
        // shape) — by name.
        let skip_fn_name = anchor.name().to_string();
        // Also protect a `var <core> = …` initializer subtree (the decoder shape).
        let protect_name = protect_name.or_else(|| Some(skip_fn_name.clone()));

        let rate = cfg.resolved().passes.cf_flatten.dead_code_rate as f32;
        let mut injector = DeadInjector {
            runtime: bus
                .get::<VmTableArtifact>()
                .map_err(|e| mangler_core::Error::transform(self.id(), e.to_string()))?
                .cloned(),
            cfg,
            rng,
            anchor,
            rate,
            protect_name,
            skip_fn_name,
            inside_core_init: false,
        };
        ast.program_mut().visit_mut_with(&mut injector);
        Ok(())
    }
}

/// swc visitor that prepends always-false dead branches to every function / arrow
/// body (except the decoder-`core` initializer subtree).
struct DeadInjector<'a> {
    runtime: Option<VmTableArtifact>,
    cfg: &'a FileConfig,
    rng: &'a mut Rng,
    anchor: OpaqueAnchor,
    rate: f32,
    protect_name: Option<String>,
    /// Name of the anchor function (decoder `core` or injected fallback). A named
    /// `function <skip_fn_name>(…)` declaration is skipped ENTIRELY — never
    /// injected into nor descended — so the anchor body stays on the value path of
    /// the guards without recursing into itself.
    skip_fn_name: String,
    /// True while descending the decoder `core`'s own initializer — suppresses
    /// injection so no `core(0)` guard is emitted before `core` is assigned.
    inside_core_init: bool,
}

impl DeadInjector<'_> {
    /// Build the body of an unreachable junk branch: side-effect-free `var`s built
    /// from fresh, collision-free names so nothing dangles even if statically
    /// analyzed. Drawn from a few shapes so there is no single junk signature.
    fn junk_body(&mut self) -> Vec<Stmt> {
        match self.rng.pick(3) {
            // var <a> = <n>;
            0 => {
                let n = self.cfg.fresh_name();
                vec![b::var_decl(
                    VarDeclKind::Var,
                    &n,
                    b::num_u32(self.rng.random_u32()),
                )]
            }
            // var <a> = <n>; var <b> = <a> + <m>;  (reads the first fresh var)
            1 => {
                let a = self.cfg.fresh_name();
                let s1 = b::var_decl(VarDeclKind::Var, &a, b::num_u32(self.rng.random_u32()));
                let bn = self.cfg.fresh_name();
                let init = b::bin(
                    swc_core::ecma::ast::BinaryOp::Add,
                    b::ident_expr(&a),
                    b::num_u32(self.rng.random_u32()),
                );
                let s2 = b::var_decl(VarDeclKind::Var, &bn, init);
                vec![s1, s2]
            }
            // var <a> = <opaque(false?…)>; — couples the junk to the anchor too.
            _ => {
                let a = self.cfg.fresh_name();
                let init = opaque_bool(self.rng, &self.anchor, false);
                vec![b::var_decl(VarDeclKind::Var, &a, init)]
            }
        }
    }

    /// `if (<always-false guard>) { <junk> }` — the dead branch. The guard is
    /// `opaque_bool(rng, &anchor, false)`, constant-false at runtime, non-foldable.
    fn dead_branch(&mut self) -> Stmt {
        let test = opaque_bool(self.rng, &self.anchor, false);
        let junk = self.junk_body();
        Stmt::If(IfStmt {
            span: mangler_jsast::span::injected_span(),
            test: Box::new(test),
            cons: Box::new(Stmt::Block(b::block(junk))),
            alt: None,
        })
    }

    /// Prepend dead branches to `body` at the configured rate. Per-body Bernoulli
    /// decision via `random_f32_unit()`: with probability `rate` the body receives
    /// `1 + pick(2)` (1..=3) dead branches. The draw is taken unconditionally per
    /// body so the RNG stream stays a deterministic function of `(seed, "deadcode")`
    /// and body-visit order.
    fn inject(&mut self, body: &mut BlockStmt) {
        if self.inside_core_init {
            return;
        }
        let fire = self.rng.random_f32_unit() < self.rate;
        if !fire {
            return;
        }
        let n = 1 + self.rng.pick(3);
        let mut branches: Vec<Stmt> = Vec::with_capacity(n);
        for _ in 0..n {
            branches.push(self.dead_branch());
        }
        let at = mangler_jsast::directives::leading_directive_count(&body.stmts);
        body.stmts.splice(at..at, branches);
    }
}

impl VisitMut for DeadInjector<'_> {
    fn visit_mut_fn_decl(&mut self, n: &mut swc_core::ecma::ast::FnDecl) {
        // Skip the anchor function entirely (decode-path / self-recursion guard):
        // neither inject into it nor descend into its body.
        if n.ident.sym.as_ref() == self.skip_fn_name
            || self.runtime.as_ref().is_some_and(|vm| {
                vm.interpreter_names
                    .iter()
                    .any(|name| name == n.ident.sym.as_ref())
            })
        {
            return;
        }
        n.visit_mut_children_with(self);
    }

    fn visit_mut_function(&mut self, n: &mut Function) {
        n.visit_mut_children_with(self);
        if let Some(body) = &mut n.body {
            self.inject(body);
        }
    }

    fn visit_mut_arrow_expr(&mut self, n: &mut ArrowExpr) {
        n.visit_mut_children_with(self);
        if let BlockStmtOrExpr::BlockStmt(body) = &mut *n.body {
            self.inject(body);
        }
    }

    fn visit_mut_var_declarator(&mut self, n: &mut swc_core::ecma::ast::VarDeclarator) {
        if self
            .runtime
            .as_ref()
            .is_some_and(|vm| is_named_declarator(&n.name, &vm.program_table_name))
        {
            return;
        }
        // Suppress injection inside the decoder `core`'s own initializer subtree:
        // a `core(0)`-anchored guard there would call `core` before it is assigned.
        let is_core = self
            .protect_name
            .as_deref()
            .map(|p| is_named_declarator(&n.name, p))
            .unwrap_or(false);
        if is_core {
            let prev = self.inside_core_init;
            self.inside_core_init = true;
            n.visit_mut_children_with(self);
            self.inside_core_init = prev;
        } else {
            n.visit_mut_children_with(self);
        }
    }
}

/// Is `pat` the simple binding `name`? (The decoder-core declarator check.)
fn is_named_declarator(pat: &Pat, name: &str) -> bool {
    matches!(pat, Pat::Ident(bi) if bi.id.sym.as_ref() == name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifacts::DecoderAnchorArtifact;
    use mangler_config::{ConfigFlags, Intensity, ResolvedConfig};
    use mangler_jsast::{Js, ParseOpts};
    use mangler_passgraph::ArtifactBus;
    use mangler_testkit::eval::assert_behaviorally_equal;
    use std::collections::HashSet;

    fn resolved(level: Intensity) -> ResolvedConfig {
        let flags = ConfigFlags {
            preset: Some(level),
            seed: Some(1),
            ..Default::default()
        };
        ResolvedConfig::try_from(flags).expect("valid preset config")
    }

    /// Resolve a config, optionally overriding the dead-code rate.
    fn config(level: Intensity, seed: u64, rate: Option<f64>) -> FileConfig {
        let mut r = resolved(level);
        if let Some(rate) = rate {
            r.passes.cf_flatten.dead_code_rate = rate;
        }
        FileConfig::new(r, seed, HashSet::new())
    }

    /// Run the deadcode pass over `src` at `seed`, optionally seeding a decoder
    /// anchor into the bus first. Returns the printed output.
    fn run_pass(src: &str, seed: u64, rate: f64, decoder: Option<&str>) -> String {
        let cfg = config(Intensity::High, seed, Some(rate));
        let mut ast = Js.parse(src, &ParseOpts::default()).unwrap();
        let mut bus = ArtifactBus::new();

        if let Some(core) = decoder {
            bus.enter_pass("strings", &[], &[Resource::decoder_anchor()]);
            bus.put(DecoderAnchorArtifact {
                core_name: core.into(),
            })
            .unwrap();
        }

        let mut rng = Rng::for_pass(cfg.seed(), "deadcode");
        let mut notes = Notes::default();
        bus.enter_pass("deadcode", DeadCodePass.reads(), &[]);
        let pass = DeadCodePass;
        pass.run(&mut ast, &cfg, &mut rng, &mut bus, &mut notes)
            .unwrap();
        Js.print(&ast)
    }

    /// A program that defines a decoder stub `core` returning "abc", then sinks the
    /// result so behavior is observable. The dead branches reference `core(0)`.
    fn with_decoder(core: &str, body: &str) -> String {
        format!("var {core}=function(i){{return \"abc\";}};{body}")
    }

    /// The pass id / reads / writes / enabled contract.
    #[test]
    fn pass_contract() {
        let pass = DeadCodePass;
        assert_eq!(pass.id(), "deadcode");
        assert_eq!(
            pass.reads(),
            &[Resource::decoder_anchor(), Resource::vm_table()]
        );
        assert!(pass.writes().is_empty());
        assert!(pass.enabled(&config(Intensity::High, 1, Some(0.5))));
        assert!(!pass.enabled(&config(Intensity::High, 1, Some(0.0))));
    }

    /// Behavior is preserved WITH a decoder anchor present: a function returning a
    /// value still returns it after injection. Compared via the testkit harness.
    #[test]
    fn behavior_preserved_with_decoder() {
        let core = "_core";
        let src = with_decoder(
            core,
            "globalThis.__out=String((function(x){return x*2+1;})(20));",
        );
        let reference = "globalThis.__out=String((function(x){return x*2+1;})(20));";
        for seed in 0..12u64 {
            let out = run_pass(&src, seed, 1.0, Some(core));
            assert!(out.contains("if("), "a dead branch must be injected: {out}");
            assert_behaviorally_equal(reference, &out);
        }
    }

    /// Behavior is preserved WITHOUT a decoder anchor: the pass falls back to an
    /// injected anchor (design decision 4) and still preserves semantics.
    #[test]
    fn behavior_preserved_without_decoder() {
        let src = "globalThis.__out=String((function(x){return x+100;})(5));";
        for seed in 0..12u64 {
            let out = run_pass(src, seed, 1.0, None);
            assert_behaviorally_equal(src, &out);
        }
    }

    /// Arrow-function bodies are also injected into, and behavior is preserved.
    #[test]
    fn behavior_preserved_arrow_body() {
        let core = "_core";
        let src = with_decoder(
            core,
            "var f=(a,b)=>{return a-b;};globalThis.__out=String(f(9,4));",
        );
        let reference = "var f=(a,b)=>{return a-b;};globalThis.__out=String(f(9,4));";
        for seed in 0..8u64 {
            let out = run_pass(&src, seed, 1.0, Some(core));
            assert_behaviorally_equal(reference, &out);
        }
    }

    /// Determinism: same (seed, source, rate) ⇒ byte-identical output.
    #[test]
    fn deterministic_for_same_seed() {
        let core = "_core";
        let src = with_decoder(
            core,
            "function f(x){return x+1;}globalThis.__out=String(f(3));",
        );
        let a = run_pass(&src, 7, 1.0, Some(core));
        let b = run_pass(&src, 7, 1.0, Some(core));
        assert_eq!(a, b);
    }

    /// The rate gates injection: at rate 0 no `if` guard is added (the pass is
    /// disabled via `enabled`, but even if forced to run, a 0 rate fires nothing).
    #[test]
    fn rate_gates_injection() {
        let core = "_core";
        let src = with_decoder(core, "function f(x){return x;}function g(y){return y;}");
        // Rate ~0: Bernoulli never fires → no dead branch.
        // (Use a tiny positive rate so `enabled` is satisfied and `run` proceeds;
        // random_f32_unit() is in [0,1) so a rate of f32::MIN_POSITIVE never fires.)
        let out_low = run_pass(&src, 3, f32::MIN_POSITIVE as f64, Some(core));
        assert!(
            !out_low.contains("if("),
            "near-zero rate must not inject: {out_low}"
        );
        // Rate 1: every body fires.
        let out_high = run_pass(&src, 3, 1.0, Some(core));
        assert!(out_high.contains("if("), "rate 1.0 must inject: {out_high}");
    }

    /// The decoder `core` initializer subtree is NOT injected into (no `core(0)`
    /// guard emitted before `core` is assigned). The decoder stub here is a plain
    /// function expression initializer; injecting into it would wrap its body in a
    /// `core(0)` guard. We assert behavior stays correct (the regression that the
    /// skip prevents would be a ReferenceError / recursion).
    #[test]
    fn skips_decoder_core_initializer() {
        // A decoder whose initializer contains a nested function body — a juicy
        // injection target the skip must protect.
        let core = "_core";
        let src = format!(
            "var {core}=(function(){{var t=\"abc\";return function(i){{return t;}};}})();\
             globalThis.__out=String({core}(0));"
        );
        let reference = "var _c=(function(){var t=\"abc\";return function(i){return t;};})();\
             globalThis.__out=String(_c(0));"
            .to_string();
        for seed in 0..8u64 {
            let out = run_pass(&src, seed, 1.0, Some(core));
            assert_behaviorally_equal(&reference, &out);
        }
    }
    #[test]
    fn injected_branches_preserve_function_strictness() {
        let src = "function f(a){'use strict';a=9;return String(this===undefined)+':'+arguments[0];}globalThis.__out=f(1);";
        let out = run_pass(src, 1, 1.0, None);
        assert_behaviorally_equal(src, &out);
    }
}
