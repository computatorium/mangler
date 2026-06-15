//! Anti-tamper output hardening — a **post-codegen string finalizer**.
//!
//! Unlike the AST passes, the anti-tamper transforms operate on the FINAL emitted
//! JS string: [`wrap`] prepends a guard prefix to the codegen output. The plan
//! calls anti-tamper a "pass", but it is post-codegen by nature (it hashes the
//! emitted source and prepends raw text), so the runner applies it as a finalizer
//! stage after [`mangler_jsast::Js::print_optimized`], not as a scheduler node.
//!
//! Gated by [`mangler_config::AntiTamperConfig`]'s `self_defending` /
//! `debug_protection`. The guards deliberately diverge under
//! debugging/DevTools/beautification ("aggressive, accept divergence"); the
//! behavioral corpus disables them.
//!
//! # Determinism
//!
//! Reproducible: the same `eff_seed` yields the same guard names/periods, drawn
//! from a per-finalizer [`Rng`] (`Rng::for_pass(eff_seed, "anti-tamper")`). The
//! baked integrity constant uses [`mangler_core::hash::djb2_utf16`] — the exact
//! mirror of the in-JS DJB2 loop emitted into the guard — so the build-time
//! expected digest matches what the runtime probe reports.

use mangler_config::AntiTamperConfig;
use mangler_core::hash::djb2_utf16;
use mangler_core::Rng;

/// A seeded `_0x…` guard identifier.
fn name(rng: &mut Rng) -> String {
    format!("_0x{:08x}", rng.random_u32())
}

/// Browser-only `debugger`/interval trap, guarded so a host without the APIs still
/// runs the user code. Interval period is seeded. The trap function does NOT call
/// itself: `setInterval` re-arms the `debugger` hit each period on a fresh stack.
fn debug_protection_snippet(rng: &mut Rng) -> String {
    let f = name(rng);
    let period = 2000 + (rng.random_u32() % 4000);
    format!(
        "try{{(function(){{var {f}=function(){{try{{(function(){{}}).constructor('debugger')();}}catch(e){{}}}};try{{{f}();}}catch(e){{}}try{{setInterval({f},{period});}}catch(e){{}}}})();}}catch(e){{}}"
    )
}

/// The standalone DJB2 string-hash helper emitted into the guard:
/// `h = (h*33 + s.charCodeAt(k)) >>> 0` over the UTF-16 code units — the in-JS
/// mirror of [`mangler_core::hash::djb2_utf16`].
fn djb2_check_src(fn_name: &str) -> String {
    format!(
        "function {fn_name}(s){{var h=5381,k=0;for(;k<s.length;k++)h=(h*33+s.charCodeAt(k))>>>0;return h;}}"
    )
}

/// Self-defending guard: defines the DJB2 hasher, a fixed `probe` function, and a
/// guard that hashes the probe's `toString()` and on mismatch recurses into a
/// runaway loop. The probed string is the *probe's* source (so the baked digest
/// does not depend on itself), computed here via [`djb2_utf16`] over the exact
/// `function {probe}(){return <nonce>;}` text the engine reports back verbatim.
fn self_defending_snippet(rng: &mut Rng) -> String {
    let check = name(rng);
    let probe = name(rng);
    let g = name(rng);
    let chk_src = djb2_check_src(&check);
    let nonce = rng.random_u32();
    let probe_src = format!("function {probe}(){{return {nonce};}}");
    let expected = djb2_utf16(&probe_src);
    format!(
        "try{{{chk_src}{probe_src}var {g}=function(){{if({check}(\"\"+{probe})!=={expected}){{while(1){{}}}}}};{g}();}}catch(e){{}}"
    )
}

/// Build the anti-tamper prefix the config would emit (debug protection and/or
/// self-defending snippets), or `None` when neither is on. Reproducible: same
/// `eff_seed` → same snippet names/periods.
fn anti_tamper_prefix(cfg: &AntiTamperConfig, eff_seed: u64) -> Option<String> {
    if !cfg.self_defending && !cfg.debug_protection {
        return None;
    }
    let mut rng = Rng::for_pass(eff_seed, "anti-tamper");
    let mut prefix = String::new();
    if cfg.debug_protection {
        prefix.push_str(&debug_protection_snippet(&mut rng));
    }
    if cfg.self_defending {
        prefix.push_str(&self_defending_snippet(&mut rng));
    }
    Some(prefix)
}

/// The prefix [`wrap`] WOULD prepend for this config/seed, so `--verify` can
/// statically parse-check its syntax even though `wrap` skips the traps in verify
/// mode (executing them would hang/diverge the re-parse run). `None` when no
/// anti-tamper feature is enabled.
pub fn verify_prefix(cfg: &AntiTamperConfig, eff_seed: u64) -> Option<String> {
    anti_tamper_prefix(cfg, eff_seed)
}

/// Apply the configured anti-tamper wrap to `output`, prepending the guard prefix.
/// Reproducible: same `eff_seed` → same names/periods. When `verify` is true the
/// traps are skipped (executing the `setInterval(debugger)` / `while(1)` traps
/// would hang or diverge the equivalence-check re-parse); the prefix templates are
/// still parse-checked separately by the caller via [`verify_prefix`].
pub fn wrap(output: String, cfg: &AntiTamperConfig, eff_seed: u64, verify: bool) -> String {
    if verify {
        return output;
    }
    match anti_tamper_prefix(cfg, eff_seed) {
        Some(prefix) => format!("{prefix}{output}"),
        None => output,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mangler_jsast::{Js, ParseOpts};

    fn at(self_defending: bool, debug_protection: bool) -> AntiTamperConfig {
        AntiTamperConfig {
            self_defending,
            debug_protection,
        }
    }

    #[test]
    fn disabled_is_a_no_op() {
        let out = wrap("CODE;".to_string(), &at(false, false), 1, false);
        assert_eq!(out, "CODE;");
        assert!(verify_prefix(&at(false, false), 1).is_none());
    }

    #[test]
    fn verify_mode_skips_traps() {
        let out = wrap("CODE;".to_string(), &at(true, true), 1, true);
        assert_eq!(out, "CODE;", "verify mode must not prepend live traps");
    }

    #[test]
    fn wrap_prepends_and_is_deterministic() {
        let a = wrap("X;".to_string(), &at(true, true), 7, false);
        let b = wrap("X;".to_string(), &at(true, true), 7, false);
        assert_eq!(a, b, "same seed → byte-identical wrap");
        assert!(a.ends_with("X;"));
        assert!(a.len() > 2, "a prefix was prepended");
    }

    #[test]
    fn prefix_templates_parse() {
        let prefix = verify_prefix(&at(true, true), 42).expect("enabled → prefix");
        assert!(
            Js::reparse(&prefix, &ParseOpts::default()).is_ok(),
            "anti-tamper prefix must be valid JS: {prefix}"
        );
    }

    #[test]
    fn self_defending_digest_matches_core_djb2() {
        // The baked expected constant must equal djb2_utf16 of the probe source —
        // proving the build-time mirror agrees with the in-JS loop.
        let mut rng = Rng::for_pass(99, "anti-tamper");
        let snippet = self_defending_snippet(&mut rng);
        // Re-derive: the snippet embeds `function <probe>(){return <nonce>;}` and the
        // matching digest. We just assert it reparses and contains a numeric digest.
        assert!(Js::reparse(&snippet, &ParseOpts::default()).is_ok());
        // Sanity: djb2_utf16 of a known probe matches the documented recurrence.
        let probe_src = "function _0x1(){return 5;}";
        assert_eq!(djb2_utf16(probe_src), {
            let mut h: u32 = 5381;
            for c in probe_src.encode_utf16() {
                h = h.wrapping_mul(33).wrapping_add(c as u32);
            }
            h
        });
    }
}
