//! Integration tests for the control-flow-flattening pass.
//!
//! Each builds a minimal pipeline directly (NOT via `register_passes`), INCLUDING
//! the resolver, so the TDZ rewrite's `(name, SyntaxContext)` keys are meaningful:
//!
//! parse → `Js::with_globals`( `Js::resolve` → put `ResolvedScopesArtifact` →
//! optionally put a `DecoderAnchorArtifact` → run the pass → print ) →
//! `assert_behaviorally_equal`.

use super::{isqrt_ceil, CfFlattenPass};
use crate::artifacts::{DecoderAnchorArtifact, ResolvedScopesArtifact};
use crate::config::FileConfig;
use mangler_config::{ConfigFlags, Intensity, ResolvedConfig};
use mangler_core::{Language, Notes, PassConfig, Rng};
use mangler_jsast::{Js, ParseOpts};
use mangler_passgraph::{ArtifactBus, Pass, Resource};
use mangler_testkit::eval::assert_behaviorally_equal;
use std::collections::HashSet;

/// Resolve a config at `level` with `seed`, overriding `cf_flatten.enabled = true`.
fn config(level: Intensity, seed: u64) -> FileConfig {
    let flags = ConfigFlags {
        preset: Some(level),
        seed: Some(seed),
        ..Default::default()
    };
    let mut r = ResolvedConfig::try_from(flags).expect("valid preset config");
    r.passes.cf_flatten.enabled = true;
    FileConfig::new(r, seed, HashSet::new())
}

/// Run the cfflatten pass over `src` at `seed`/`level`, optionally seeding a
/// decoder anchor (`core` must be DEFINED in `src`). Returns the printed output.
fn run_pass(src: &str, level: Intensity, seed: u64, decoder: Option<&str>) -> String {
    let cfg = config(level, seed);
    Js::with_globals(|| {
        let mut ast = Js.parse(src, &ParseOpts::default()).unwrap();
        let marks @ (unresolved_mark, top_level_mark) = Js::resolve(&mut ast);

        let mut bus = ArtifactBus::new();
        // Resolver pseudo-pass: publish the marks.
        bus.enter_pass("resolver", &[], &[Resource::resolved_scopes()]);
        bus.put(ResolvedScopesArtifact {
            unresolved_mark,
            top_level_mark,
        })
        .unwrap();

        // Optional strings pseudo-pass: publish the decoder anchor.
        if let Some(core) = decoder {
            bus.enter_pass("strings", &[], &[Resource::decoder_anchor()]);
            bus.put(DecoderAnchorArtifact {
                core_name: core.into(),
            })
            .unwrap();
        }

        let mut rng = Rng::for_pass(cfg.seed(), "cfflatten");
        let mut notes = Notes::default();
        bus.enter_pass(
            "cfflatten",
            &[Resource::decoder_anchor(), Resource::resolved_scopes()],
            &[],
        );
        CfFlattenPass
            .run(&mut ast, &cfg, &mut rng, &mut bus, &mut notes)
            .unwrap();
        // Print via the PRODUCTION terminal codegen (`optimize` + `fixer` + emit).
        // The `fixer` inserts the precedence-required parentheses around the TDZ
        // guard ternaries — exactly the path the real runner uses (`Js.print`, the
        // bare emit, intentionally skips the fixer). `mangle = false`: cfflatten does
        // not rename, and keeping globals (`_core`, `globalThis`) intact is correct.
        Js::print_optimized(ast, marks, false, &[])
    })
}

/// A program defining a decoder stub `core` returning "abc", then the body.
fn with_decoder(core: &str, body: &str) -> String {
    format!("var {core}=function(i){{return \"abc\";}};{body}")
}

// ---------------------------------------------------------------------------
// Contract
// ---------------------------------------------------------------------------

#[test]
fn pass_contract() {
    let pass = CfFlattenPass;
    assert_eq!(pass.id(), "cfflatten");
    assert_eq!(
        pass.reads(),
        &[Resource::decoder_anchor(), Resource::resolved_scopes()]
    );
    assert!(pass.writes().is_empty());
    assert!(pass.enabled(&config(Intensity::High, 1)));
    // Disabled when the knob is off (Minify preset leaves cf_flatten off).
    let flags = ConfigFlags {
        preset: Some(Intensity::Minify),
        seed: Some(1),
        ..Default::default()
    };
    let r = ResolvedConfig::try_from(flags).unwrap();
    let off = FileConfig::new(r, 1, HashSet::new());
    assert!(!pass.enabled(&off));
}

#[test]
fn isqrt_ceil_is_smallest_k_with_k_squared_ge_n() {
    assert_eq!(isqrt_ceil(0), 1);
    assert_eq!(isqrt_ceil(1), 1);
    assert_eq!(isqrt_ceil(2), 2);
    assert_eq!(isqrt_ceil(4), 2);
    assert_eq!(isqrt_ceil(5), 3);
    assert_eq!(isqrt_ceil(9), 3);
    assert_eq!(isqrt_ceil(10), 4);
    for n in 1..2000usize {
        let k = isqrt_ceil(n);
        assert!(k * k >= n, "k*k must cover n: n={n} k={k}");
        assert!((k - 1) * (k - 1) < n || k == 1, "k must be minimal: n={n} k={k}");
        for v in 0..n {
            assert_eq!((v / k) * k + (v % k), v, "split must reconstruct: n={n} k={k} v={v}");
        }
    }
}

// ---------------------------------------------------------------------------
// Behavioral equivalence — with and without a decoder anchor
// ---------------------------------------------------------------------------

/// Straight-line + branch, WITH a decoder anchor present.
#[test]
fn preserves_branch_behavior_with_decoder() {
    let core = "_core";
    let body = "globalThis.__out=String((function(x){var a=x+1;if(a>3){a=a*2;}else{a=a-1;}var b=a+5;return b;})(4));";
    let src = with_decoder(core, body);
    let reference = body.to_string();
    for seed in 0..10u64 {
        let out = run_pass(&src, Intensity::High, seed, Some(core));
        assert!(out.contains("switch"), "body must be flattened: {out}");
        assert_behaviorally_equal(&reference, &out);
    }
}

/// Straight-line + branch, WITHOUT a decoder anchor (the pass injects its own
/// fallback opaque anchor) — proving the decoupling.
#[test]
fn preserves_branch_behavior_without_decoder() {
    let src = "globalThis.__out=String((function(x){var a=x+1;if(a>3){a=a*2;}else{a=a-1;}var b=a+5;return b;})(4));";
    for seed in 0..10u64 {
        let out = run_pass(src, Intensity::High, seed, None);
        assert!(out.contains("switch"), "body must be flattened: {out}");
        assert_behaviorally_equal(src, &out);
    }
}

/// Loops (`while` and C-`for`) with running totals.
#[test]
fn preserves_loop_behavior() {
    let core = "_core";
    let body = "globalThis.__out=String((function(n){var s=0;var i=0;while(i<n){s=s+i;i=i+1;}for(var j=0;j<n;j=j+1){s=s+j*2;}return s;})(6));";
    let src = with_decoder(core, body);
    let reference = body.to_string();
    for seed in 0..8u64 {
        let out = run_pass(&src, Intensity::High, seed, Some(core));
        assert!(out.contains("switch"), "must flatten: {out}");
        assert_behaviorally_equal(&reference, &out);
    }
}

/// Nested scopes / nested functions are flattened independently and preserve
/// behavior (closures over outer `var`s).
#[test]
fn preserves_nested_function_behavior() {
    let core = "_core";
    let body = "globalThis.__out=String((function(x){var acc=x;function step(d){var t=acc+d;acc=t*2;return acc;}var r1=step(3);var r2=step(1);return r1+r2+acc;})(2));";
    let src = with_decoder(core, body);
    let reference = body.to_string();
    for seed in 0..8u64 {
        let out = run_pass(&src, Intensity::High, seed, Some(core));
        assert_behaviorally_equal(&reference, &out);
    }
}

/// `throw` edges propagate the original exception value (caught at an outer,
/// un-flattened try/catch).
#[test]
fn preserves_throw_behavior() {
    let core = "_core";
    let body = "globalThis.__out=String((function(){try{return (function(x){var a=x+1;if(a>2){throw a;}var b=a+1;return b;})(5);}catch(e){return e;}})());";
    let src = with_decoder(core, body);
    let reference = body.to_string();
    for seed in 0..8u64 {
        let out = run_pass(&src, Intensity::High, seed, Some(core));
        assert_behaviorally_equal(&reference, &out);
    }
}

/// Two-state-var dispatch (state_vars >= 2 at High) preserves behavior.
#[test]
fn preserves_behavior_two_state_vars() {
    let core = "_core";
    let body = "globalThis.__out=String((function(n){var s=1;var i=1;while(i<=n){s=s*i;i=i+1;}return s;})(5));";
    let src = with_decoder(core, body);
    let reference = body.to_string();
    for seed in 0..8u64 {
        let out = run_pass(&src, Intensity::High, seed, Some(core));
        assert_behaviorally_equal(&reference, &out);
    }
}

// ---------------------------------------------------------------------------
// TDZ: same-name `let`/`const` across scopes + the mangled-name regression
// ---------------------------------------------------------------------------

/// `let`/`const` bodies are lowered through the TDZ guard and preserve behavior,
/// including a same-named `let g` in two distinct nested scopes (disambiguated by
/// the resolver marks → distinct hoisted vars).
#[test]
fn tdz_same_name_across_scopes_preserves_behavior() {
    let core = "_core";
    let body = "globalThis.__out=String((function(){let g=10;let total=g;{let g=20;total=total+g;}const h=total+1;return h;})());";
    let src = with_decoder(core, body);
    let reference = body.to_string();
    for seed in 0..10u64 {
        let out = run_pass(&src, Intensity::High, seed, Some(core));
        assert_behaviorally_equal(&reference, &out);
    }
}

/// Accessing a `let` before its declaration must throw a `ReferenceError`
/// (Temporal Dead Zone) — preserved by the TDZ guard. The throwing body (the
/// inner IIFE, which is FLATTENED) is observed at an OUTER, un-flattened try/catch
/// (the outer fn is ineligible because it contains the `try`).
#[test]
fn tdz_throws_on_use_before_init() {
    let core = "_core";
    // Inner fn: reads `g` before its `let g` declaration → TDZ ReferenceError. It
    // is eligible (no try/break) so it IS flattened, exercising the guard on the
    // throwing path. The outer fn catches and classifies the error.
    let body = "globalThis.__out=(function(){try{return (function(){var probe=g+1;var more=probe+1;let g=5;return String(more);})();}catch(e){return e instanceof ReferenceError?\"tdz\":\"other\";}})();";
    let src = with_decoder(core, body);
    let reference = body.to_string();
    for seed in 0..8u64 {
        let out = run_pass(&src, Intensity::High, seed, Some(core));
        assert_behaviorally_equal(&reference, &out);
    }
}

/// Assigning to a `const` must throw a `TypeError` — preserved by the const guard.
/// Same structure: the const-violating inner fn is flattened; the outer try/catch
/// observes the throw.
#[test]
fn const_reassignment_throws_typeerror() {
    let core = "_core";
    let body = "globalThis.__out=(function(){try{return (function(){const c=3;var sum=c+1;var more=sum+1;c=99;return \"no-throw\"+more;})();}catch(e){return e instanceof TypeError?\"te\":\"other\";}})();";
    let src = with_decoder(core, body);
    let reference = body.to_string();
    for seed in 0..8u64 {
        let out = run_pass(&src, Intensity::High, seed, Some(core));
        assert_behaviorally_equal(&reference, &out);
    }
}

/// REGRESSION: the TDZ guard must emit the MANGLED hoisted var name, NOT the
/// original source identifier as cleartext. A prior bug emitted `throwTdz("g")`
/// leaking the source name `g`. The guard's argument is the mangled `_0x…` var, so
/// the original source name `g` must NOT appear as a string-literal argument.
#[test]
fn tdz_guard_emits_mangled_name_not_source_name() {
    let core = "_core";
    let body = "globalThis.__out=String((function(){let g=7;var a=g+1;var b=a+g;return b;})());";
    let src = with_decoder(core, body);
    let out = run_pass(&src, Intensity::High, 1, Some(core));
    assert!(out.contains("switch"), "body must be flattened: {out}");
    assert!(
        !out.contains("\"g\""),
        "TDZ guard must not leak the source name as a cleartext literal: {out}"
    );
    assert_behaviorally_equal(body, &out);
}

// ---------------------------------------------------------------------------
// Bail-to-safe: ineligible bodies are left intact, never miscompiled
// ---------------------------------------------------------------------------

/// A body containing `try`/`catch` is INELIGIBLE; it must be left un-flattened and
/// behavior preserved.
#[test]
fn ineligible_try_body_is_left_intact() {
    let core = "_core";
    let body = "globalThis.__out=String((function(x){var a=x+1;try{a=a*2;}catch(e){a=0;}var b=a+3;return b;})(4));";
    let src = with_decoder(core, body);
    let reference = body.to_string();
    let out = run_pass(&src, Intensity::High, 1, Some(core));
    assert!(out.contains("try"), "ineligible try body must remain: {out}");
    assert_behaviorally_equal(&reference, &out);
}

/// A `for-of` body is ineligible — left intact, behavior preserved.
#[test]
fn ineligible_for_of_body_is_left_intact() {
    let core = "_core";
    let body = "globalThis.__out=String((function(arr){var s=0;for(var x of arr){s=s+x;}var r=s+1;return r;})([1,2,3]));";
    let src = with_decoder(core, body);
    let reference = body.to_string();
    let out = run_pass(&src, Intensity::High, 1, Some(core));
    assert!(out.contains(" of "), "for-of must remain: {out}");
    assert_behaviorally_equal(&reference, &out);
}

/// A body with `break`/`continue` is ineligible — left intact, behavior preserved.
#[test]
fn ineligible_break_continue_body_is_left_intact() {
    let core = "_core";
    let body = "globalThis.__out=String((function(n){var s=0;for(var i=0;i<n;i=i+1){if(i===2){continue;}if(i===5){break;}s=s+i;}return s;})(8));";
    let src = with_decoder(core, body);
    let reference = body.to_string();
    let out = run_pass(&src, Intensity::High, 1, Some(core));
    assert_behaviorally_equal(&reference, &out);
}

// ---------------------------------------------------------------------------
// Determinism
// ---------------------------------------------------------------------------

#[test]
fn deterministic_for_same_seed() {
    let core = "_core";
    let body = "globalThis.__out=String((function(x){var a=x+1;if(a>3){a=a*2;}else{a=a-1;}var b=a+5;return b;})(4));";
    let src = with_decoder(core, body);
    let a = run_pass(&src, Intensity::High, 7, Some(core));
    let b = run_pass(&src, Intensity::High, 7, Some(core));
    assert_eq!(a, b, "same seed must yield byte-identical output");
}

#[test]
fn deterministic_without_decoder() {
    let src = "globalThis.__out=String((function(x){var a=x+1;while(a<10){a=a+2;}return a;})(1));";
    let a = run_pass(src, Intensity::High, 3, None);
    let b = run_pass(src, Intensity::High, 3, None);
    assert_eq!(a, b);
}

// ---------------------------------------------------------------------------
// Corpus spot-check (full register_passes pipeline at High)
// ---------------------------------------------------------------------------

/// Spot-check the behavioral corpus through the FULL `crate::runner::process`
/// pipeline (which now includes cfflatten) at multiple presets and seeds,
/// asserting behavioral equivalence on every corpus file. cfflatten is wired into
/// `register_passes`, so this genuinely exercises it (and would have caught the
/// nested-fn-decl + object-shorthand TDZ miscompiles).
#[test]
fn corpus_spot_check_behavior_preserved() {
    let mut total = 0usize;
    for level in [Intensity::Medium, Intensity::High] {
        for seed in [1u64, 7, 42] {
            let checked = mangler_testkit::corpus::assert_all(|src| {
                crate::test_support::process_with(src, level, seed)
            });
            assert!(checked > 0, "corpus must contain at least one snippet");
            total += checked;
        }
    }
    assert!(total > 0);
}

// ---------------------------------------------------------------------------
// Regression: nested function declarations + const (the corpus miscompile)
// ---------------------------------------------------------------------------

/// REGRESSION (root cause #1): a nested `function` declaration is hoisted to the
/// top of its enclosing function scope and is callable from any later statement.
/// The linearizer used to drop the declaration into a single switch-case, so a
/// call from another case threw "<f> is not defined". `hoist_fn_decls` now lifts
/// direct-body declarations into the function prologue. The minimal repro from the
/// bug report, run through cfflatten directly.
#[test]
fn regression_nested_fn_decl_with_const_hoisted() {
    let core = "_core";
    let body = "globalThis.__out=String((function(){ const a = 1; function f(x){ return x+1; } const b = f(a); const c = f(b); return c; })());";
    let src = with_decoder(core, body);
    for seed in 0..8u64 {
        let out = run_pass(&src, Intensity::High, seed, Some(core));
        assert!(out.contains("switch"), "body must be flattened: {out}");
        assert_behaviorally_equal(body, &out);
    }
}

/// REGRESSION: a function declaration whose body is NOT inlinable (recursive) and
/// is called from a later statement — the declaration must still be hoisted ahead
/// of the dispatch loop so the call resolves it.
#[test]
fn regression_recursive_nested_fn_decl_hoisted() {
    let core = "_core";
    let body = "globalThis.__out=String((function(){ const z = 10; function f(x){ return x<=0 ? z : f(x-1)+x; } const b = f(3); const c = f(b); return c; })());";
    let src = with_decoder(core, body);
    for seed in 0..8u64 {
        let out = run_pass(&src, Intensity::High, seed, Some(core));
        assert_behaviorally_equal(body, &out);
    }
}

/// REGRESSION (root cause #2): an object-literal SHORTHAND property `{ scaled }`
/// is a value-reference to a `const`/`let` binding, but is NOT an `Expr::Ident`
/// node, so the TDZ rewrite missed it — leaving the raw source name (`scaled`,
/// never declared) in the output and throwing "scaled is not defined". The
/// rewrite now expands a shorthand on a mapped binding to `{ scaled: <guarded
/// read of V> }`.
#[test]
fn regression_object_shorthand_const_read_rewritten() {
    let core = "_core";
    let body = "globalThis.__out=String((function(){ const x = 5; const y = x * 2; const z = y + 1; return JSON.stringify({x, y, z}); })());";
    let src = with_decoder(core, body);
    for seed in 0..8u64 {
        let out = run_pass(&src, Intensity::High, seed, Some(core));
        assert!(out.contains("switch"), "body must be flattened: {out}");
        assert_behaviorally_equal(body, &out);
    }
}

/// The exact corpus shape that failed: object with shorthand methods + arrow
/// `this`, nested `function` declarations (called later) and interspersed
/// `const`s, with a shorthand-property result object.
#[test]
fn regression_corpus_arrow_this_lexical_shape() {
    let core = "_core";
    let body = "globalThis.__out=String((function(){ const obj={factor:3,scaleAll(...nums){return nums.map((n)=>n*this.factor);}}; const scaled=obj.scaleAll(1,2,3); function withDefaults(a,b){return [a,b];} const d1=withDefaults(5,10); function restSum(first,...rest){return first+rest.reduce((s,x)=>s+x,0);} const rs=restSum(1,2,3,4); return JSON.stringify({scaled,d1,rs}); })());";
    let src = with_decoder(core, body);
    for seed in 0..8u64 {
        let out = run_pass(&src, Intensity::High, seed, Some(core));
        assert_behaviorally_equal(body, &out);
    }
}

/// BAIL-TO-SAFE: a `function` declaration nested inside control flow has Annex-B
/// hoisting the CFG cannot model. cfflatten must bail (leave the body
/// un-flattened, no dispatch loop) rather than risk a miscompile. (The body is
/// kept inside a nested block so it is not in single-statement context; we assert
/// the bail by the absence of a `switch`.)
#[test]
fn nested_fn_decl_in_control_flow_is_left_intact() {
    let core = "_core";
    let body = "globalThis.__out=String((function(cond){ var pre=1; if(cond){ var t=2; function g(){ return 7; } pre=pre+t; } var out = (typeof g==='function')?g():0; return pre+out; })(true));";
    let src = with_decoder(core, body);
    let out = run_pass(&src, Intensity::High, 1, Some(core));
    assert!(
        !out.contains("switch"),
        "body with control-flow-nested fn decl must NOT be flattened: {out}"
    );
}

/// Fuzz cfflatten DIRECTLY (not via the full `process`, which would not isolate
/// it): generate random function bodies and assert each round-trips equivalently
/// through the pass alone. `None` decoder → the injected fallback anchor path.
#[test]
fn fuzz_cfflatten_direct() {
    mangler_testkit::fuzz::assert_fuzz_transform(400, 0xCF1A_7711, |src| {
        run_pass(src, Intensity::High, 1, None)
    });
}

/// The `has_nested_fn_decl` gate fires for a fn decl inside a loop body too, while
/// a direct-body fn decl alongside loops does not (it is hoistable).
#[test]
fn gate_nested_fn_decl_detection() {
    use crate::passes::cfflatten::eligibility::scan_gates;
    use crate::passes::cfflatten::test_support::parse_body;
    // Direct-body fn decl: hoistable, gate stays false.
    assert!(!scan_gates(&parse_body("var a=1; function f(){return a;} var b=f(); return b;")).has_nested_fn_decl);
    // fn decl inside an `if`: not modellable, gate fires.
    assert!(scan_gates(&parse_body("var a=1; if(a){ function f(){return a;} } var b=2; return b;")).has_nested_fn_decl);
    // fn decl inside a loop: gate fires.
    assert!(scan_gates(&parse_body("var a=1; while(a>0){ function f(){return a;} a=0; } var b=2; return b;")).has_nested_fn_decl);
    // fn decl inside a bare block: gate fires.
    assert!(scan_gates(&parse_body("var a=1; { function f(){return a;} } var b=2; return b;")).has_nested_fn_decl);
    // fn decl inside a NESTED function (independent body): gate does NOT fire.
    assert!(!scan_gates(&parse_body("var a=1; function outer(){ if(a){ function f(){return a;} } } var b=2; return b;")).has_nested_fn_decl);
}


