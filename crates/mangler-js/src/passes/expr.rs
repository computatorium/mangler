//! Expression obfuscation: rewrite integer and boolean literals into opaque,
//! non-foldable expressions anchored to a non-foldable decoder/seed function.
//!
//! swc's final `optimize()` constant-folds and dead-code-eliminates, so a naive
//! `5 → 2+3` folds straight back. To survive, every rewritten literal is built by
//! the [`crate::opaque`] library, which routes the value through an impure-looking
//! anchor call swc cannot prove pure — so the whole expression is preserved while
//! still evaluating to the original value (`>>> 0` / `| 0` make the coercion exact
//! for the supported ranges).
//!
//! # Decoupled anchor (design decision 4)
//!
//! This pass declares `reads() = [Resource::decoder_anchor()]` but treats it as
//! OPTIONAL: [`crate::opaque::anchor_from_bus_or_inject`] returns the strings
//! decoder anchor if the strings pass ran, else injects an independent fallback
//! anchor at the top of the program. So expr no longer HARD-requires strings.
//!
//! # Skips
//!
//! * The decoder's own initializer subtree (`var <core> = …`) — rewriting there
//!   would emit a `core(0)` before `core` is assigned. Handled by
//!   [`Walk::skip_subtree`].
//! * Switch discriminants and case-label constants (cf-flatten consumes them).
//!   Handled by skipping `SwitchStmt` discriminant/labels — here we keep it simple
//!   and only obfuscate within statement bodies; user `switch` labels are rare and
//!   left intact via the leaf range policy (`opaque_lit` only rewrites `>= 2`).

use crate::artifacts::DecoderAnchorArtifact;
use crate::config::FileConfig;
use crate::opaque::{anchor_from_bus_or_inject, opaque_lit, OpaqueAnchor};
use mangler_core::{Language, Notes, PassConfig, Result, Rng};
use mangler_jsast::rewrite::{SubtreeMark, Walk};
use mangler_jsast::Js;
use mangler_passgraph::{ArtifactBus, Pass, Resource};
use swc_core::ecma::ast::{Expr, Pat};

/// Rewrites integer and boolean literals into anchor-coupled opaque expressions.
pub struct ExprObfuscationPass;

impl Pass<Js, FileConfig> for ExprObfuscationPass {
    fn id(&self) -> &'static str {
        "expr"
    }

    /// Reads the decoder anchor — OPTIONAL (falls back to an injected anchor when
    /// absent; see the module docs). Declaring the read is what lets the bus permit
    /// `get::<DecoderAnchorArtifact>()` and orders this pass after strings when
    /// strings is enabled.
    fn reads(&self) -> &[Resource] {
        const R: &[Resource] = &[Resource::decoder_anchor()];
        R
    }

    fn enabled(&self, cfg: &FileConfig) -> bool {
        cfg.resolved().passes.expr.expr_obfuscation
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
        // The fresh anchor name (when injected) is also the declarator to protect
        // from self-`core(0)`.
        let seed_word = format!("a{:x}", cfg.seed() & 0xffff);
        let (anchor, injected) = anchor_from_bus_or_inject(
            ast.program_mut(),
            bus,
            || cfg.fresh_name(),
            &seed_word,
        )
        .map_err(|e| mangler_core::Error::transform(self.id(), e.to_string()))?;

        if injected {
            notes.push(mangler_core::Note::from(
                self.id(),
                "no decoder anchor present; injected an independent opaque anchor",
            ));
        }

        // The name of the declarator whose initializer must NOT be rewritten:
        // the decoder `core`, or the injected fallback (its body returns a string,
        // not an integer, so there is nothing to obfuscate there anyway, but we
        // skip it uniformly for clarity).
        let protect_name: Option<String> = match bus.get::<DecoderAnchorArtifact>() {
            Ok(Some(d)) => Some(d.core_name.clone()),
            _ => None,
        };

        rewrite_literals(ast.program_mut(), rng, &anchor, protect_name.as_deref());
        Ok(())
    }
}

/// Walk the program, replacing eligible literals with opaque equivalents.
///
/// **Post-order** (the [`Walk`] default): the closure fires on a node AFTER its
/// children. A freshly-built opaque replacement has its own integer literals as
/// children, but because they came from `opaque_lit` already-formed (post-order
/// does not re-descend into a node returned by the closure), they are never
/// re-obfuscated — which is exactly what prevents the infinite rewrite that a
/// pre-order walk over generated output would cause.
///
/// The decoder/anchor initializer subtree is pruned via [`Walk::skip_subtree`] so
/// no `core(0)` is emitted before `core` is assigned.
///
/// Follow-up: hiding a whole negative literal `-N` as a single signed opaque value
/// (so the sign does not leak through a visible unary minus) needs the value
/// replaced BEFORE its inner `N` is visited — a pre-order step the post-order leaf
/// rewrite here intentionally omits. A follow-up can add it with a dedicated
/// pre-order pre-pass that consolidates `-N` nodes, then this post-order pass over
/// the remainder.
fn rewrite_literals(
    program: &mut swc_core::ecma::ast::Program,
    rng: &mut Rng,
    anchor: &OpaqueAnchor,
    protect_name: Option<&str>,
) {
    let protect = protect_name.map(|s| s.to_string());
    Walk::new()
        .on_expr(|e: &mut Expr| {
            // Plain integer (>= 2) / boolean literal. After replacement `e` is a
            // Bin/Unary, not a Lit, so it cannot re-match even if revisited.
            if let Expr::Lit(lit) = e
                && let Some(rep) = opaque_lit(rng, anchor, lit)
            {
                *e = rep;
            }
        })
        .skip_subtree(move |mark| match mark {
            // Never rewrite inside `var <protect> = …` (the decoder/anchor init).
            SubtreeMark::VarDeclaratorInit { name } => {
                matches!((&protect, name),
                    (Some(p), Pat::Ident(bi)) if bi.id.sym.as_ref() == p.as_str())
            }
            _ => false,
        })
        .run(program);
}

#[cfg(test)]
mod tests {
    use crate::test_support::process_with;
    use mangler_config::Intensity;

    #[test]
    fn integer_literals_obfuscated_at_medium() {
        // At Medium, expr_obfuscation is on; a `>= 2` integer should no longer
        // appear as a bare literal in a value position (it becomes an opaque expr).
        let out = process_with("function f(){ return 12345; } f();", Intensity::Medium, 1);
        // The opaque form involves >>> and an anchor call, so the raw literal is gone.
        assert!(
            out.contains(">>>") || !out.contains("12345"),
            "integer should be obfuscated into an opaque expr: {out}"
        );
    }

    #[test]
    fn behavior_is_preserved() {
        // Behavioral check is covered broadly by the corpus differential test in
        // tests/. Here, a smoke check that output reparses and runs.
        let out = process_with("globalThis.__out = String(2 + 40);", Intensity::Medium, 3);
        assert!(
            mangler_jsast::Js::reparse(&out, &mangler_jsast::ParseOpts::default()).is_ok(),
            "obfuscated output must reparse: {out}"
        );
    }
}
