//! Member-access → computed conversion pass.
//!
//! Rewrites every static-ident member access `obj.prop` → `obj["prop"]` (and the
//! optional-chain form `a?.b` → `a?.["b"]`). The property NAME is preserved as a
//! string literal, so the transform is sound: `obj.x` and `obj["x"]` access the
//! same key, and method-call `this`-binding is unchanged. Only the access SYNTAX
//! changes; no property is renamed.
//!
//! The generated `"prop"` string literals are picked up by the strings pass (which
//! reads [`Resource::property_literals`](mangler_passgraph::Resource::property_literals),
//! so the scheduler orders it after this pass) and encoded — the mechanism that
//! makes API surface (`document.getElementById`, …) un-greppable.
//!
//! ## Not converted
//!
//! * `MemberProp::PrivateName` (`obj.#x`): cannot be expressed as a computed key.
//! * `MemberProp::Computed` (`obj["x"]`): already computed.
//! * `super.x` is a `SuperPropExpr`, not a `MemberExpr` — left untouched (children
//!   still descended, so a computed super key is handled).
//!
//! ## Pass shape
//!
//! The canonical [`Pass<Js, FileConfig>`] shape every JS pass follows:
//! `id` (load-bearing — keys the RNG + tie-break), `enabled` (reads its config
//! knob off `cfg.resolved()`), `writes` (declares the resource it produces so the
//! scheduler can order readers after it), and `run` (the transform, here via the
//! shared [`rewrite::Walk`] combinator — no hand-rolled `VisitMut`).

use crate::artifacts::PropertyLiteralsArtifact;
use crate::config::FileConfig;
use mangler_core::{Notes, Result, Rng};
use mangler_jsast::rewrite::Walk;
use mangler_jsast::{build, Js};
use mangler_passgraph::{ArtifactBus, Pass, Resource};
use swc_core::ecma::ast::{ComputedPropName, Expr, MemberProp, OptChainBase};

/// Rewrites static member access `obj.prop` → `obj["prop"]` (computed).
pub struct MemberAccessPass;

impl Pass<Js, FileConfig> for MemberAccessPass {
    fn id(&self) -> &'static str {
        "memberaccess"
    }

    /// Declares it produces property-name string literals. The strings pass reads
    /// this resource, so the scheduler runs strings *after* this pass.
    fn writes(&self) -> &[Resource] {
        const W: &[Resource] = &[Resource::property_literals()];
        W
    }

    /// Gated by the dedicated `member_access` knob (split off from
    /// `expr_obfuscation` so member-access can run at lower intensities).
    fn enabled(&self, cfg: &FileConfig) -> bool {
        cfg.resolved().passes.expr.member_access
    }

    /// Safe on a fragment: a pure local syntax rewrite, no module-level state.
    fn fragment_safe(&self) -> bool {
        true
    }

    fn run(
        &self,
        ast: &mut <Js as mangler_core::Language>::Ast,
        _cfg: &FileConfig,
        _rng: &mut Rng,
        bus: &mut ArtifactBus,
        _notes: &mut Notes,
    ) -> Result<()> {
        rewrite_member_access(ast.program_mut());
        // Announce that property-name literals now exist (the scheduler edge to the
        // strings pass is what matters; the payload is empty).
        bus.put(PropertyLiteralsArtifact)
            .map_err(|e| mangler_core::Error::transform(self.id(), e.to_string()))?;
        Ok(())
    }
}

/// The rewrite itself, expressed via the shared combinator. Post-order so nested
/// members `a.b.c` are converted bottom-up; a freshly built `obj["prop"]` has a
/// computed prop, so it never re-matches.
fn rewrite_member_access(program: &mut swc_core::ecma::ast::Program) {
    Walk::new()
        .on_expr(|e: &mut Expr| {
            // obj.prop → obj["prop"]
            if let Expr::Member(m) = e
                && let MemberProp::Ident(id) = &m.prop
            {
                let key = build::str_lit(id.sym.as_ref());
                m.prop = MemberProp::Computed(ComputedPropName {
                    span: m.span,
                    expr: Box::new(key),
                });
            }
            // a?.b → a?.["b"]  (optional-chain member; `optional` lives on the
            // OptChainExpr, not on `prop`, so short-circuit semantics are preserved)
            if let Expr::OptChain(o) = e
                && let OptChainBase::Member(m) = &mut *o.base
                && let MemberProp::Ident(id) = &m.prop
            {
                let key = build::str_lit(id.sym.as_ref());
                m.prop = MemberProp::Computed(ComputedPropName {
                    span: m.span,
                    expr: Box::new(key),
                });
            }
        })
        .run(program);
}

#[cfg(test)]
mod tests {
    use super::rewrite_member_access;
    use mangler_core::Language;
    use mangler_jsast::{Js, ParseOpts};

    /// Run ONLY the member-access rewrite and print the (un-minified) AST. We assert
    /// on the rewritten AST directly because the downstream minifier legitimately
    /// re-collapses `obj["validIdent"]` back to `obj.validIdent` — in the full
    /// pipeline the strings pass encodes those literals into decoder calls before
    /// minify, which is what keeps them computed. Here the unit under test is just
    /// the rewrite, so we observe its output pre-minify.
    fn rewrite_only(src: &str) -> String {
        let mut ast = Js.parse(src, &ParseOpts::default()).unwrap();
        rewrite_member_access(ast.program_mut());
        Js.print(&ast)
    }

    #[test]
    fn dotted_access_becomes_computed() {
        // Use a READ position: an assignment LHS is a `SimpleAssignTarget::Member`,
        // not an `Expr::Member`, so an expr walk does not reach it (a known scope
        // limit — the follow-up strings pass and the full member-access port cover
        // assignment targets separately).
        let out = rewrite_only("var x = o.longPropName.nested;");
        assert!(
            out.contains("[\"longPropName\"]") && out.contains("[\"nested\"]"),
            "dotted reads must become computed: {out}"
        );
        assert!(!out.contains(".longPropName"), "no dotted form remains: {out}");
    }

    #[test]
    fn optional_chain_becomes_computed() {
        let out = rewrite_only("var r = a?.longChainProp;");
        assert!(
            out.contains("[\"longChainProp\"]"),
            "optional-chain member must become computed: {out}"
        );
    }

    #[test]
    fn private_field_is_not_converted() {
        let out = rewrite_only("class C { #v = 1; get(){ return this.#v; } }");
        assert!(out.contains("#v"), "private field must remain: {out}");
        assert!(!out.contains("[\"#"), "private field must not be computed: {out}");
    }

    #[test]
    fn pass_is_gated_by_member_access_knob() {
        use crate::config::FileConfig;
        use crate::passes::memberaccess::MemberAccessPass;
        use mangler_config::Intensity;
        use mangler_passgraph::Pass;
        use std::collections::HashSet;

        let minify = FileConfig::new(crate::test_support::resolved(Intensity::Minify, 1), 1, HashSet::new());
        let medium = FileConfig::new(crate::test_support::resolved(Intensity::Medium, 1), 1, HashSet::new());
        assert!(!MemberAccessPass.enabled(&minify), "off at Minify");
        assert!(MemberAccessPass.enabled(&medium), "on at Medium");
    }
}
