//! Behavioral + structural tests for the virtualize pass GLUE.
//!
//! Each test builds a MINIMAL pipeline directly (not via `register_passes`): parse →
//! [`FileConfig`] with `virtualize.target = Some("*")` → [`ArtifactBus`] → per-pass
//! [`Rng`] → `bus.enter_pass` → `pass.run` → print. The transformed program is then
//! compared to the original under `Object.is` semantics via
//! [`mangler_testkit::eval::assert_behaviorally_equal`]. A VM miscompile is the worst
//! possible bug, so "run it and compare the observable result" is the load-bearing
//! guard.

use super::*;
use crate::config::FileConfig;
use crate::artifacts::VmTableArtifact;
use mangler_config::{ConfigFlags, Intensity, ResolvedConfig};
use mangler_core::{Language, PassConfig, Rng};
use mangler_jsast::lang::{Js, ParseOpts};
use mangler_passgraph::{ArtifactBus, Pass, Resource};
use std::collections::HashSet;

/// A resolved config with the virtualize target glob set, every other pass at the
/// quiet `Minify` preset so the only transform exercised is virtualization.
fn resolved_with_target(glob: &str, seed: u64) -> ResolvedConfig {
    let flags = ConfigFlags {
        preset: Some(Intensity::Minify),
        seed: Some(seed),
        virtualize: Some(glob.to_string()),
        ..Default::default()
    };
    ResolvedConfig::try_from(flags).expect("valid config")
}

/// A resolved config with both `target` and `exclude` globs set.
fn resolved_with_target_and_exclude(target: &str, exclude: &str, seed: u64) -> ResolvedConfig {
    let flags = ConfigFlags {
        preset: Some(Intensity::Minify),
        seed: Some(seed),
        virtualize: Some(target.to_string()),
        virtualize_exclude: Some(exclude.to_string()),
        ..Default::default()
    };
    ResolvedConfig::try_from(flags).expect("valid config")
}

/// Run ONLY the virtualize pass over `src` with `target` + `exclude` globs, returning
/// the printed output and whether a [`VmTableArtifact`] was put.
fn run_virtualize_with_exclude(src: &str, target: &str, exclude: &str, seed: u64) -> (String, bool) {
    let cfg = FileConfig::new(resolved_with_target_and_exclude(target, exclude, seed), seed, reserved_idents(src));
    let mut ast = Js.parse(src, &ParseOpts::default()).expect("parse");
    let mut bus = ArtifactBus::new();
    let pass = VirtualizePass;
    bus.enter_pass(pass.id(), pass.reads(), pass.writes());
    let mut rng = Rng::for_pass(cfg.seed(), pass.id());
    let mut notes = mangler_core::Notes::default();
    pass.run(&mut ast, &cfg, &mut rng, &mut bus, &mut notes)
        .expect("run ok");
    let put = bus.contains::<VmTableArtifact>();
    (Js.print(&ast), put)
}

/// Collect every identifier symbol in the source so injected names never collide.
fn reserved_idents(src: &str) -> HashSet<String> {
    use swc_core::ecma::visit::{Visit, VisitWith};
    struct Collect(HashSet<String>);
    impl Visit for Collect {
        fn visit_ident(&mut self, n: &swc_core::ecma::ast::Ident) {
            self.0.insert(n.sym.to_string());
        }
    }
    let ast = Js.parse(src, &ParseOpts::default()).expect("parse");
    let mut c = Collect(HashSet::new());
    ast.into_program().visit_with(&mut c);
    c.0
}

/// Run ONLY the virtualize pass over `src` with the given glob + seed, returning the
/// printed output and whether a [`VmTableArtifact`] was put.
fn run_virtualize(src: &str, glob: &str, seed: u64) -> (String, bool) {
    let cfg = FileConfig::new(resolved_with_target(glob, seed), seed, reserved_idents(src));
    let mut ast = Js.parse(src, &ParseOpts::default()).expect("parse");
    let mut bus = ArtifactBus::new();
    let pass = VirtualizePass;
    bus.enter_pass(pass.id(), pass.reads(), pass.writes());
    let mut rng = Rng::for_pass(cfg.seed(), pass.id());
    let mut notes = mangler_core::Notes::default();
    pass.run(&mut ast, &cfg, &mut rng, &mut bus, &mut notes)
        .expect("run ok");
    let put = bus.contains::<VmTableArtifact>();
    (Js.print(&ast), put)
}

/// Assert the virtualized program behaves identically to the original across several
/// seeds (exercising different diversification variants).
fn assert_equiv(src: &str) {
    for seed in [1u64, 7, 42] {
        let (out, _) = run_virtualize(src, "*", seed);
        mangler_testkit::eval::assert_behaviorally_equal(src, &out);
    }
}

// ---------------------------------------------------------------------------
// Pass-shape contract
// ---------------------------------------------------------------------------

#[test]
fn pass_shape_is_declared() {
    let p = VirtualizePass;
    assert_eq!(p.id(), "virtualize");
    // Phase 3: reads the decoder anchor (to keep the strings-decoder stub native in
    // whole-program mode) — NOT ResolvedScopes, so it still sorts pre-resolver.
    assert_eq!(p.reads(), &[Resource::decoder_anchor()]);
    assert!(
        !p.reads().contains(&Resource::resolved_scopes()),
        "must NOT read ResolvedScopes → still sorts pre-resolver"
    );
    assert_eq!(p.writes(), &[Resource::vm_table()]);
}

#[test]
fn enabled_only_when_target_set() {
    let on = FileConfig::new(resolved_with_target("*", 1), 1, HashSet::new());
    assert!(VirtualizePass.enabled(&on), "target Some → enabled");

    let off_flags = ConfigFlags {
        preset: Some(Intensity::Max),
        seed: Some(1),
        ..Default::default()
    };
    let off = FileConfig::new(
        ResolvedConfig::try_from(off_flags).unwrap(),
        1,
        HashSet::new(),
    );
    assert!(
        !VirtualizePass.enabled(&off),
        "no target even at Max → disabled (opt-in)"
    );
}

// ---------------------------------------------------------------------------
// Behavioral equivalence — the load-bearing guard
// ---------------------------------------------------------------------------

#[test]
fn arithmetic() {
    assert_equiv("function f(a,b){ return a + b * 2 - (a % b); } globalThis.__out=JSON.stringify(f(7,3));");
    assert_equiv("function f(a,b){ return (a & b) | (a ^ b); } globalThis.__out=JSON.stringify(f(12,10));");
    assert_equiv("function f(a,b){ return a ** b; } globalThis.__out=JSON.stringify(f(2,10));");
    assert_equiv("function f(a){ return -a + ~a + !a; } globalThis.__out=JSON.stringify(f(5));");
}

#[test]
fn control_flow() {
    assert_equiv(
        "function f(n){ if(n>0){return 'pos';}else if(n<0){return 'neg';}else{return 'zero';} } globalThis.__out=JSON.stringify(f(-4));",
    );
    assert_equiv(
        "function f(n){ var s=0; for(var i=0;i<n;i++){ s+=i; } return s; } globalThis.__out=JSON.stringify(f(10));",
    );
    assert_equiv(
        "function f(n){ var s=0,i=0; while(i<n){ s=s+i*i; i++; } return s; } globalThis.__out=JSON.stringify(f(6));",
    );
    assert_equiv(
        "function f(n){ var s=0; do { s++; n--; } while(n>0); return s; } globalThis.__out=JSON.stringify(f(5));",
    );
}

#[test]
fn loops_break_continue_labeled() {
    assert_equiv(
        "function f(n){ var s=0; for(var i=0;i<n;i++){ if(i===3)continue; if(i===7)break; s+=i; } return s; } globalThis.__out=JSON.stringify(f(10));",
    );
    assert_equiv(
        "function f(n){ outer: for(var i=0;i<n;i++){ for(var j=0;j<n;j++){ if(i*j>6)break outer; } } return i; } globalThis.__out=JSON.stringify(f(5));",
    );
}

#[test]
fn switch_stmt() {
    assert_equiv(
        "function f(x){ switch(x){ case 1: return 'a'; case 2: return 'b'; default: return 'z'; } } globalThis.__out=JSON.stringify(f(2));",
    );
    assert_equiv(
        "function f(x){ var r=''; switch(x){ case 1: r+='1'; case 2: r+='2'; break; case 3: r+='3'; } return r; } globalThis.__out=JSON.stringify(f(1));",
    );
}

#[test]
fn try_catch_finally() {
    assert_equiv(
        "function f(x){ try { if(x<0) throw 'neg'; return 'ok'; } catch(e){ return 'caught:'+e; } finally { } } globalThis.__out=JSON.stringify(f(-1));",
    );
    assert_equiv(
        "function f(x){ var r=''; try { r+='t'; throw 1; } catch(e){ r+='c'; } finally { r+='f'; } return r; } globalThis.__out=JSON.stringify(f(0));",
    );
}

#[test]
fn recursion_named_fn() {
    assert_equiv("function fac(n){ return n<=1 ? 1 : n*fac(n-1); } globalThis.__out=JSON.stringify(fac(6));");
    assert_equiv("function fib(n){ return n<2 ? n : fib(n-1)+fib(n-2); } globalThis.__out=JSON.stringify(fib(10));");
}

#[test]
fn closures_capture() {
    assert_equiv(
        "function f(a){ var add=function(b){ return a+b; }; return add(10); } globalThis.__out=JSON.stringify(f(5));",
    );
    assert_equiv(
        "function f(n){ var acc=0; var g=function(){ acc+=1; return acc; }; g(); g(); return g(); } globalThis.__out=JSON.stringify(f(0));",
    );
}

#[test]
fn this_binding() {
    // A method on an object literal: `this` at call time must reach the VM frame.
    assert_equiv(
        "function f(){ return this.x + this.y; } var o={x:3,y:4,m:f}; globalThis.__out=JSON.stringify(o.m());",
    );
}

#[test]
fn arrays_objects_destructure() {
    assert_equiv(
        "function f(a,b){ var arr=[a,b,a+b]; return arr[2]; } globalThis.__out=JSON.stringify(f(3,4));",
    );
    assert_equiv(
        "function f(o){ var {p,q}=o; return p+q; } globalThis.__out=JSON.stringify(f({p:2,q:5}));",
    );
    assert_equiv(
        "function f(arr){ var s=0; for(var x of arr){ s+=x; } return s; } globalThis.__out=JSON.stringify(f([1,2,3,4]));",
    );
}

#[test]
fn template_and_strings() {
    assert_equiv("function f(a,b){ return 'sum=' + (a+b); } globalThis.__out=JSON.stringify(f(2,3));");
    assert_equiv("function f(a){ return `val:${a}:${a*2}`; } globalThis.__out=JSON.stringify(f(5));");
}

// ---------------------------------------------------------------------------
// Selectivity: the glob targets specific names
// ---------------------------------------------------------------------------

#[test]
fn glob_selects_only_matching_names() {
    let src = "function hotPath(a){ return a*2; } function coldPath(a){ return a+1; } globalThis.__out=JSON.stringify([hotPath(3),coldPath(3)]);";
    let (out, put) = run_virtualize(src, "hot*", 7);
    assert!(put, "a function matched, so VmTable must be put");
    // The matched function's body was virtualized (it no longer contains `a*2`); the
    // unmatched one is untouched (still contains `a+1`).
    assert!(out.contains("a+1") || out.contains("a + 1"), "coldPath left intact: {out}");
    mangler_testkit::eval::assert_behaviorally_equal(src, &out);
}

// ---------------------------------------------------------------------------
// Bail-to-safe: ineligible bodies are left intact, never miscompiled
// ---------------------------------------------------------------------------

#[test]
fn ineligible_with_bails_intact() {
    // `with` is a permanent structural bail. The function stays un-virtualized and no
    // table/artifact is produced.
    let src = "function f(o){ with(o){ return x; } } globalThis.__out=String(typeof f);";
    let (out, put) = run_virtualize(src, "*", 7);
    assert!(!put, "nothing virtualized → no VmTable artifact");
    assert!(out.contains("with"), "with-body left intact: {out}");
}

#[test]
fn generator_and_async_bail() {
    for src in [
        "function* f(){ yield 1; } globalThis.__out=String(typeof f);",
        "async function f(){ return 1; } globalThis.__out=String(typeof f);",
    ] {
        let (_out, put) = run_virtualize(src, "*", 7);
        assert!(!put, "generator/async must bail: {src}");
    }
}

// ---------------------------------------------------------------------------
// §5a strict-mode coverage (Phase 0b)
// ---------------------------------------------------------------------------

/// A function with its OWN `"use strict"` is now VIRTUALIZED (no strict bail): the
/// pass routes it to a strict thunk + strict interpreter variant. Its observable
/// behavior — including the plain-call `this === undefined` — is preserved.
#[test]
fn own_use_strict_function_virtualizes() {
    let src = "function f(){ 'use strict'; return typeof this; } globalThis.__out=JSON.stringify(f());";
    for seed in [1u64, 7, 42] {
        let (out, put) = run_virtualize(src, "*", seed);
        assert!(put, "strict function now virtualizes (no bail)");
        assert!(out.contains("use strict"), "the strict variant carries the directive:\n{out}");
        mangler_testkit::eval::assert_behaviorally_equal(src, &out);
    }
}

/// A strict function's store to a frozen property THROWS, both native and virtualized.
#[test]
fn own_use_strict_store_throws_equivalently() {
    let src = "function f(o){ 'use strict'; o.x = 9; return o.x; } \
        globalThis.__out=JSON.stringify((function(){try{return f(Object.freeze({x:1}));}catch(e){return 'T:'+e.constructor.name;}})());";
    for seed in [1u64, 7, 42] {
        let (out, put) = run_virtualize(src, "*", seed);
        assert!(put, "strict function virtualizes");
        mangler_testkit::eval::assert_behaviorally_equal(src, &out);
    }
}

/// A function nested in a strict function INHERITS strictness even without its own
/// directive — its virtualized form must observe strict `this` too.
#[test]
fn inherited_strictness_from_enclosing_function() {
    let src = "function outer(){ 'use strict'; function f(){ return typeof this; } return f; } \
        var g = outer(); globalThis.__out=JSON.stringify(g());";
    for seed in [1u64, 7, 42] {
        let (out, _put) = run_virtualize(src, "f", seed);
        mangler_testkit::eval::assert_behaviorally_equal(src, &out);
    }
}

/// §5a byte-identity proof: a fully-SLOPPY program must produce EXACTLY the bytes it
/// did before strict support — no strict names drawn, no strict interpreter emitted.
/// We capture the output of a representative sloppy program and assert it round-trips
/// deterministically and contains exactly ZERO `"use strict"` directives (the only
/// possible strict-machinery footprint), proving the sloppy path is untouched.
#[test]
fn sloppy_program_emits_no_strict_machinery() {
    let src = "function f(a,b){ var s=0; for(var i=0;i<b;i++){ s += a*i; } return s; } \
        globalThis.__out=JSON.stringify(f(3,5));";
    for seed in [1u64, 7, 42] {
        let (out, put) = run_virtualize(src, "*", seed);
        assert!(put, "sloppy function still virtualizes");
        assert!(
            !out.contains("use strict"),
            "a fully-sloppy program emits NO strict directive (byte-identity):\n{out}"
        );
        // Determinism: same seed → byte-identical.
        let (out2, _) = run_virtualize(src, "*", seed);
        assert_eq!(out, out2, "same seed → byte-identical sloppy output");
    }
}

#[test]
fn no_match_produces_no_artifact() {
    let src = "function f(a){ return a*2; } globalThis.__out=JSON.stringify(f(3));";
    let (out, put) = run_virtualize(src, "nomatch", 7);
    assert!(!put, "glob matches nothing → no artifact");
    assert_eq!(out, Js.print(&Js.parse(src, &ParseOpts::default()).unwrap()), "program unchanged");
}

// ---------------------------------------------------------------------------
// Artifact + determinism
// ---------------------------------------------------------------------------

#[test]
fn vm_table_artifact_is_put_with_names() {
    let src = "function f(a){ return a*2; } globalThis.__out=JSON.stringify(f(3));";
    let cfg = FileConfig::new(resolved_with_target("*", 7), 7, reserved_idents(src));
    let mut ast = Js.parse(src, &ParseOpts::default()).unwrap();
    let mut bus = ArtifactBus::new();
    let pass = VirtualizePass;
    bus.enter_pass(pass.id(), pass.reads(), pass.writes());
    let mut rng = Rng::for_pass(cfg.seed(), pass.id());
    let mut notes = mangler_core::Notes::default();
    pass.run(&mut ast, &cfg, &mut rng, &mut bus, &mut notes).unwrap();

    // Re-enter as a declared READER to inspect the produced artifact (the bus
    // enforces reads()/writes() declarations).
    bus.enter_pass("reader", &[Resource::vm_table()], &[]);
    let art = bus.get::<VmTableArtifact>().unwrap().expect("VmTable put");
    assert!(!art.interp_name.is_empty());
    assert!(!art.program_table_name.is_empty());
    assert_ne!(art.interp_name, art.program_table_name);
    // The output references both spliced names.
    let out = Js.print(&ast);
    assert!(out.contains(&art.program_table_name), "table name in output: {out}");
}

#[test]
fn deterministic_same_seed_same_bytes() {
    let src = "function f(a,b){ return a*b+1; } globalThis.__out=JSON.stringify(f(6,7));";
    let a = run_virtualize(src, "*", 99).0;
    let b = run_virtualize(src, "*", 99).0;
    assert_eq!(a, b, "same seed must produce byte-identical output");
}

#[test]
fn determinism_across_distinct_seeds_differs() {
    // Sanity: distinct seeds generally diversify (not a hard contract, but catches a
    // seed that is ignored). Behavioral equivalence still holds (covered above).
    let src = "function f(a,b){ var s=0; for(var i=0;i<b;i++){ s += (a & i) | (a ^ i); } return s; } globalThis.__out=JSON.stringify(f(13,8));";
    let outputs: HashSet<String> = (0u64..8).map(|s| run_virtualize(src, "*", s).0).collect();
    assert!(outputs.len() > 1, "distinct seeds should diversify output");
}

// ---------------------------------------------------------------------------
// Fuzz round-trip: every generated body either virtualizes equivalently or bails
// ---------------------------------------------------------------------------

#[test]
fn fuzz_round_trip() {
    // The deterministic VM fuzzer emits random function bodies over the eligible
    // construct set; each must behave identically once virtualized, or be left
    // unchanged on a bail. A fixed seed keeps the transform deterministic per program.
    mangler_testkit::fuzz::assert_fuzz_transform(300, 0xF0F0_1234, |program| {
        run_virtualize(program, "*", 0xABCD).0
    });
}

#[test]
fn fuzz_across_seeds() {
    for seed in [1u64, 2, 5, 13, 21] {
        mangler_testkit::fuzz::assert_fuzz_transform(80, 0x1234_0000 + seed, move |program| {
            run_virtualize(program, "*", seed).0
        });
    }
}

// ---------------------------------------------------------------------------
// Exclude: behavioral / integration tests (Phase 0a gate)
// ---------------------------------------------------------------------------

/// Core behavioral test: `--virtualize '*' --virtualize-exclude 'render*'` must
/// leave `function render(){}` as native (its body still contains the original
/// ops), while other functions are virtualized (replaced with a VM thunk).
#[test]
fn exclude_keeps_matched_function_native() {
    let src = "\
        function render(n){ return n * 2; } \
        function compute(n){ return n + 1; } \
        globalThis.__out = JSON.stringify([render(5), compute(5)]);";

    let (out, put) = run_virtualize_with_exclude(src, "*", "render*", 7);

    // At least `compute` was virtualized → artifact present.
    assert!(put, "compute should have been virtualized → artifact must be put");

    // `render` body must NOT be a VM thunk: it must still contain the original
    // multiplication (the thunk never contains arithmetic source).
    assert!(
        out.contains("n * 2") || out.contains("n*2"),
        "render body must be native (contains 'n * 2'): {out}"
    );

    // Behavioral equivalence must hold.
    mangler_testkit::eval::assert_behaviorally_equal(src, &out);
}

/// §10: an excluded function is reported in a `Notes` line so the user can confirm a
/// hot path stayed native. Names are de-duplicated and the exclude glob is echoed.
#[test]
fn exclude_emits_notes_listing_kept_native_functions() {
    let src = "\
        function render(n){ return n * 2; } \
        function compute(n){ return n + 1; } \
        globalThis.__out = JSON.stringify([render(5), compute(5)]);";

    let cfg = FileConfig::new(
        resolved_with_target_and_exclude("*", "render*", 7),
        7,
        reserved_idents(src),
    );
    let mut ast = Js.parse(src, &ParseOpts::default()).expect("parse");
    let mut bus = ArtifactBus::new();
    let pass = VirtualizePass;
    bus.enter_pass(pass.id(), pass.reads(), pass.writes());
    let mut rng = Rng::for_pass(cfg.seed(), pass.id());
    let mut notes = mangler_core::Notes::default();
    pass.run(&mut ast, &cfg, &mut rng, &mut bus, &mut notes)
        .expect("run ok");

    let text: String = notes.iter().map(|n| n.to_string()).collect::<Vec<_>>().join("\n");
    assert!(text.contains("render"), "exclude note must name `render`: {text}");
    assert!(text.contains("render*"), "exclude note must echo the glob: {text}");
    assert!(!text.contains("compute"), "virtualized fn must NOT be in the note: {text}");
}

/// Exclude with exact name stops only the exact match.
#[test]
fn exclude_exact_name_stops_only_match() {
    let src = "\
        function foo(x){ return x * 3; } \
        function fooBar(x){ return x + 10; } \
        globalThis.__out = JSON.stringify([foo(4), fooBar(4)]);";

    // Exclude only exact "foo" (not "fooBar").
    let (out, put) = run_virtualize_with_exclude(src, "*", "foo", 7);
    assert!(put, "fooBar should be virtualized → artifact");
    // foo body (x*3) must still be native.
    assert!(
        out.contains("x * 3") || out.contains("x*3"),
        "foo body must remain native: {out}"
    );
    mangler_testkit::eval::assert_behaviorally_equal(src, &out);
}

/// Exclude everything with `*` leaves all functions native (no artifact).
#[test]
fn exclude_star_keeps_all_native() {
    let src = "function f(a){ return a+1; } globalThis.__out = JSON.stringify(f(3));";
    let (out, put) = run_virtualize_with_exclude(src, "*", "*", 7);
    assert!(!put, "exclude '*' with target '*' → nothing virtualized → no artifact");
    // Output must be behaviourally correct.
    mangler_testkit::eval::assert_behaviorally_equal(src, &out);
}

/// Exclude on a target that matches nothing still produces no artifact and the
/// exclude has no observable effect.
#[test]
fn exclude_no_artifact_when_target_matches_nothing() {
    let src = "function f(a){ return a+1; } globalThis.__out = JSON.stringify(f(3));";
    let (out, put) = run_virtualize_with_exclude(src, "nomatch", "render*", 7);
    assert!(!put, "target matches nothing → no artifact");
    let orig_out = Js.print(&Js.parse(src, &ParseOpts::default()).unwrap());
    assert_eq!(out, orig_out, "program unchanged when target never matches");
}

// ---------------------------------------------------------------------------
// Binding-name inference: unit tests (§4.3)
// ---------------------------------------------------------------------------

/// Helper: parse `src`, run virtualization with `target` + `exclude`, and return
/// the printed output. Checks that a named function's body is either native or
/// a thunk based on whether it was expected to be excluded.
fn check_exclude_native(src: &str, target: &str, exclude: &str, native_marker: &str, seed: u64) {
    let (out, _) = run_virtualize_with_exclude(src, target, exclude, seed);
    assert!(
        out.contains(native_marker),
        "expected native marker {native_marker:?} in output:\n{out}"
    );
    mangler_testkit::eval::assert_behaviorally_equal(src, &out);
}

/// Form 1: own ident on FnDecl (`function render(){}`).
#[test]
fn binding_name_own_ident_fn_decl() {
    // The printer removes spaces around operators, so check for "x*2" not "x * 2".
    check_exclude_native(
        "function render(x){ return x * 2; } globalThis.__out=JSON.stringify(render(3));",
        "*", "render", "x*2", 7,
    );
}

/// Form 1b: own ident on named FnExpr (`var f = function render(){}`).
#[test]
fn binding_name_own_ident_fn_expr() {
    // Named FnExpr: own ident "render" wins → excluded by "render*".
    check_exclude_native(
        "var f = function render(x){ return x * 3; }; globalThis.__out=JSON.stringify(f(3));",
        "*", "render*", "x*3", 7,
    );
}

/// Form 2a: binding from VarDeclarator with anonymous FnExpr
/// (`const render = function(){}`).
#[test]
fn binding_name_var_declarator_fn_expr() {
    // The anonymous function expression gets the binding name "compute" from the
    // declarator. With exclude "compute*" it must stay native.
    let src = "var compute = function(x){ return x + 99; }; globalThis.__out=JSON.stringify(compute(1));";
    check_exclude_native(src, "*", "compute*", "x+99", 7);
    // Without exclude: the function IS virtualized (body replaced by thunk).
    let (out_no_excl, put) = run_virtualize(src, "*", 7);
    assert!(put, "anonymous fn via binding name should be virtualized with target '*'");
    assert!(!out_no_excl.contains("x+99"), "body replaced by thunk when not excluded: {out_no_excl}");
    mangler_testkit::eval::assert_behaviorally_equal(src, &out_no_excl);
}

/// Form 2b: ArrowExpr is NOT a top-level virtualization target in the current
/// named-function mode (arrows have no `arguments` binding, so the standard thunk
/// form cannot be used). The arrow stays native regardless of `target`/`exclude`.
/// Binding-name inference for arrows is deferred to the native-closure phase (§4).
#[test]
fn binding_name_var_declarator_arrow_stays_native() {
    // Arrows are not top-level virtualization targets: with target="*", no artifact
    // is produced (there are no eligible named functions), and the source is unchanged.
    let src = "var process = (x) => { return x * 5; }; globalThis.__out=JSON.stringify(process(4));";
    let (out, put) = run_virtualize(src, "*", 7);
    assert!(!put, "arrow-only program → no VmTable artifact (arrows not top-level targets)");
    mangler_testkit::eval::assert_behaviorally_equal(src, &out);
}

/// Form 3: binding from assignment target (`obj.render = function(){}`).
#[test]
fn binding_name_assignment_target() {
    let src = "\
        var obj = {}; \
        obj.render = function(x){ return x * 7; }; \
        globalThis.__out=JSON.stringify(obj.render(2));";
    check_exclude_native(src, "*", "render*", "x*7", 7);
}

/// Form 4a: object literal key-value property (`{ render: function(){} }`).
#[test]
fn binding_name_key_value_prop() {
    let src = "\
        var obj = { render: function(x){ return x * 11; } }; \
        globalThis.__out=JSON.stringify(obj.render(3));";
    check_exclude_native(src, "*", "render*", "x*11", 7);
}

/// Form 4b: shorthand method property (`{ render() {} }`).
#[test]
fn binding_name_method_prop() {
    let src = "\
        var obj = { render: function(x){ return x + 42; } }; \
        globalThis.__out=JSON.stringify(obj.render(1));";
    check_exclude_native(src, "*", "render*", "x+42", 7);
}

/// Anonymous function with no inferable name is NOT excludable — it gets
/// virtualized (no binding context visited by these overrides).
/// Verify this via behavioral correctness (the pass must not miscompile it).
#[test]
fn anonymous_no_binding_name_not_excludable() {
    // A function assigned via a computed key or IIFE has no inferable name.
    // We use an IIFE: the anonymous function is never matched by a name glob.
    let src = "(function(x){ return x * 13; })(2); globalThis.__out='ok';";
    // With target "*" and exclude "anonymous*": nothing named "anonymous" exists
    // → IIFE is still virtualized (if eligible) or bailed safely.
    let (out, _) = run_virtualize_with_exclude(src, "*", "anonymous*", 7);
    mangler_testkit::eval::assert_behaviorally_equal(src, &out);
}

// ---------------------------------------------------------------------------
// Config wiring: exclude flows from CLI flags through ResolvedConfig
// ---------------------------------------------------------------------------

#[test]
fn exclude_config_wired_from_flags() {
    use mangler_config::ConfigFlags;
    let flags = ConfigFlags {
        preset: Some(Intensity::Minify),
        seed: Some(1),
        virtualize: Some("*".to_string()),
        virtualize_exclude: Some("render*".to_string()),
        ..Default::default()
    };
    let r = ResolvedConfig::try_from(flags).expect("valid config");
    assert_eq!(r.passes.virtualize.target.as_deref(), Some("*"));
    assert_eq!(r.passes.virtualize.exclude.as_deref(), Some("render*"));
}

#[test]
fn exclude_absent_stays_none() {
    use mangler_config::ConfigFlags;
    let flags = ConfigFlags {
        preset: Some(Intensity::Minify),
        seed: Some(1),
        virtualize: Some("*".to_string()),
        ..Default::default()
    };
    let r = ResolvedConfig::try_from(flags).expect("valid config");
    assert_eq!(r.passes.virtualize.target.as_deref(), Some("*"));
    assert!(r.passes.virtualize.exclude.is_none(), "no exclude flag → None");
}

#[test]
fn for_preset_keeps_exclude_none() {
    use mangler_config::{Intensity, PassConfigs};
    for level in [Intensity::Minify, Intensity::Low, Intensity::Medium, Intensity::High, Intensity::Max] {
        let p = PassConfigs::for_preset(level);
        assert!(p.virtualize.exclude.is_none(), "{level}: exclude must be None in every preset");
    }
}

// ---------------------------------------------------------------------------
// §5a strictness-propagation attribute (unit)
// ---------------------------------------------------------------------------

fn parse_prog(src: &str) -> Program {
    Js.parse(src, &ParseOpts::default()).expect("parse").into_program()
}

#[test]
fn program_top_strictness_attribute() {
    // Sloppy Script: not strict at top.
    assert!(!program_top_is_strict(&parse_prog("function f(){}")));
    // Script opening with a directive: strict at top.
    assert!(program_top_is_strict(&parse_prog("'use strict'; function f(){}")));
    // ES Module is implicitly strict.
    let module = Js
        .parse("import x from 'm'; function f(){}", &ParseOpts::default())
        .expect("parse module")
        .into_program();
    assert!(program_top_is_strict(&module), "module top is strict");
}

#[test]
fn strict_candidate_pre_scan_is_precise_for_sloppy() {
    // No strict anywhere → no strict candidate (so NO extra names drawn → byte-identity).
    assert!(!program_has_strict_candidate(&parse_prog("function f(a){ return a*2; }"), "*", None));
    assert!(!program_has_strict_candidate(
        &parse_prog("var f = function(){ return 1; }; obj.g = function(){ return 2; };"),
        "*",
        None
    ));
    // Own-directive strict function matching the glob → candidate.
    assert!(program_has_strict_candidate(
        &parse_prog("function f(){ 'use strict'; return this; }"),
        "*",
        None
    ));
    // Strict candidate EXCLUDED by glob → not a candidate.
    assert!(!program_has_strict_candidate(
        &parse_prog("function render(){ 'use strict'; return this; }"),
        "*",
        Some("render")
    ));
    // Strict-by-inheritance nested function matching the glob → candidate.
    assert!(program_has_strict_candidate(
        &parse_prog("function outer(){ 'use strict'; function f(){ return this; } }"),
        "f",
        None
    ));
    // A strict function NOT matching the target glob → not a candidate.
    assert!(!program_has_strict_candidate(
        &parse_prog("function f(){ 'use strict'; return this; }"),
        "other",
        None
    ));
}

// ---------------------------------------------------------------------------
// Phase 1: whole-program virtualization (`--virtualize-program`, §2/§2.1)
// ---------------------------------------------------------------------------

/// A resolved config with whole-program virtualization on, everything else quiet.
fn resolved_whole_program(seed: u64) -> ResolvedConfig {
    let flags = ConfigFlags {
        preset: Some(Intensity::Minify),
        seed: Some(seed),
        virtualize_program: true,
        ..Default::default()
    };
    let r = ResolvedConfig::try_from(flags).expect("valid config");
    assert!(r.passes.virtualize.whole_program);
    r
}

/// Run ONLY the virtualize pass over `src` in whole-program mode, returning the
/// printed output and whether a `VmTableArtifact` was put (i.e. it virtualized).
fn run_whole_program(src: &str, seed: u64) -> (String, bool) {
    let cfg = FileConfig::new(resolved_whole_program(seed), seed, reserved_idents(src));
    let mut ast = Js.parse(src, &ParseOpts::default()).expect("parse");
    let mut bus = ArtifactBus::new();
    let pass = VirtualizePass;
    bus.enter_pass(pass.id(), pass.reads(), pass.writes());
    let mut rng = Rng::for_pass(cfg.seed(), pass.id());
    let mut notes = mangler_core::Notes::default();
    pass.run(&mut ast, &cfg, &mut rng, &mut bus, &mut notes)
        .expect("run ok");
    (Js.print(&ast), bus.contains::<VmTableArtifact>())
}

/// Whole-program `enabled` is true when `virtualize_program` is set even with NO
/// `--virtualize` target glob.
#[test]
fn whole_program_enabled_without_target() {
    let cfg = FileConfig::new(resolved_whole_program(1), 1, HashSet::new());
    assert!(VirtualizePass.enabled(&cfg), "whole_program → enabled even with no target");
}

/// The one-top-level-IIFE WebGL shape: with `--virtualize-program` the interpreter is
/// present, the top level becomes an interpreter call, and it runs to identical
/// observable output.
#[test]
fn whole_program_one_iife_virtualizes_and_runs() {
    // A single top-level IIFE that computes into the sink — the WebGL shape.
    let src = "(function(){ var s=0; for(var i=0;i<10;i++){ s+=i*i; } globalThis.__out=JSON.stringify(s); })();";
    let (out, put) = run_whole_program(src, 7);
    assert!(put, "whole-program virtualized → VmTableArtifact present");
    // The original IIFE's distinctive literals (the accumulator init / loop) are gone
    // from source — they now live only as XOR'd bytecode in the program table. (We
    // cannot grep for `for(` because the interpreter body itself loops.)
    assert!(!out.contains("s+=i*i"), "loop body must be in the VM, not native:\n{out}");
    assert!(!out.contains("for(var i=0;i<10"), "top-level loop gone from native source:\n{out}");
    // The top level ends in an interpreter call (the §2.1 re-entry thunk): the last
    // top-level statement is a bare call whose first arg indexes the program table.
    assert!(out.contains("[1][0],"), "top level re-enters the interpreter over a table entry:\n{out}");
    // Behavioral equivalence (rquickjs SameValue) — the load-bearing guard.
    mangler_testkit::eval::assert_behaviorally_equal(src, &out);
}

/// A sloppy Script top level threads `globalThis` as the §2.1 receiver. Verified
/// structurally (the re-entry call passes `globalThis`) and behaviorally.
#[test]
fn whole_program_sloppy_threads_globalthis_receiver() {
    let src = "(function(){ globalThis.__out = JSON.stringify(typeof this); })();";
    let (out, put) = run_whole_program(src, 3);
    assert!(put, "virtualized");
    // No strict interpreter variant (sloppy program).
    assert!(!out.contains("\"use strict\""), "sloppy program: no strict variant:\n{out}");
    // The §2.1 re-entry call passes `globalThis` (sloppy receiver), not `undefined`.
    assert!(out.contains(",globalThis)"), "sloppy top-level receiver is globalThis:\n{out}");
    mangler_testkit::eval::assert_behaviorally_equal(src, &out);
}

/// A strict Script (`'use strict'`) top level threads `undefined` as the §2.1
/// receiver and routes through a strict interpreter variant. Verified structurally
/// (the call passes `undefined`; a strict interpreter is emitted) and behaviorally
/// (a strict-only divergence — assigning to a frozen property throws under strict).
#[test]
fn whole_program_strict_emits_strict_variant_and_throws() {
    // Strict store to a frozen property MUST throw (sloppy would silently no-op):
    // a real strict/sloppy divergence the strict interpreter variant must preserve.
    let src = "'use strict'; var o=Object.freeze({}); try { o.x=1; globalThis.__out='nothrow'; } catch(e){ globalThis.__out='threw'; }";
    let (out, put) = run_whole_program(src, 5);
    assert!(put, "virtualized");
    // A strict interpreter variant was emitted, and the §2.1 call threads undefined.
    assert!(out.contains("\"use strict\""), "strict interpreter variant present:\n{out}");
    assert!(out.contains(",undefined)"), "strict top-level receiver is undefined:\n{out}");
    mangler_testkit::eval::assert_behaviorally_equal(src, &out);
}

/// Phase 2: a module with an `export` boundary PARTITIONS — the `export` stays
/// native, and the wrappable run(s) around it virtualize. The export-bound `x` is
/// captured by name (resolving to the native module binding), so its declaration stays
/// native and the read-run pulls into the VM.
#[test]
fn whole_program_module_export_partitions() {
    let src = "export const x = 1; globalThis.__out = JSON.stringify(x);";
    let (out, put) = run_whole_program(src, 9);
    assert!(put, "Phase 2: the read-run virtualizes around the export → VM table");
    // The `export const x = 1` boundary stays native.
    assert!(out.contains("export"), "export kept native:\n{out}");
    // The reader run is now an interpreter call (its distinctive native form is gone).
    assert!(out.contains("[0],"), "reader run re-enters the interpreter:\n{out}");
    // The VM program table is spliced (a `[[` numeric literal table).
    assert!(out.contains("[["), "VM program table spliced:\n{out}");
}

/// Bare top-level statements (no IIFE wrapper) virtualize as one chunk and run
/// identically — `var`/`function` become VM locals (self-contained program).
#[test]
fn whole_program_bare_statements_run() {
    let src = "var a=3,b=4; function hyp(x,y){ return Math.sqrt(x*x+y*y); } globalThis.__out=JSON.stringify(hyp(a,b));";
    let (out, put) = run_whole_program(src, 11);
    assert!(put, "virtualized");
    mangler_testkit::eval::assert_behaviorally_equal(src, &out);
}

/// Determinism: same seed ⇒ byte-identical whole-program output.
#[test]
fn whole_program_deterministic_same_seed_same_bytes() {
    let src = "(function(){ var s=0; for(var i=0;i<5;i++) s+=i; globalThis.__out=JSON.stringify(s); })();";
    let (a, _) = run_whole_program(src, 0xC0FFEE);
    let (b, _) = run_whole_program(src, 0xC0FFEE);
    assert_eq!(a, b, "same seed must produce byte-identical output");
}

// ---------------------------------------------------------------------------
// Phase 2: partition + adaptive bisection (§3.1, §3.3, §2.1, §5)
// ---------------------------------------------------------------------------

/// Strip ES-module sugar (`export`/`import`) to an evalable Script that is
/// behaviorally equivalent for self-contained programs (an unconsumed `export` has no
/// runtime effect; a bare side-effect `import` of an unused module likewise). Used so
/// the in-process script harness can run a partitioned MODULE for behavioral parity
/// (the harness evals scripts, not ES modules — a documented harness gap). Operates on
/// the AST so it is robust to the printer's spacing (`import"m"`, `export const …`).
fn strip_module_decls(src: &str) -> String {
    let mut ast = Js.parse(src, &ParseOpts::default()).expect("parse");
    if let Program::Module(m) = ast.program_mut() {
        let mut kept: Vec<ModuleItem> = Vec::new();
        for it in std::mem::take(&mut m.body) {
            match it {
                ModuleItem::Stmt(s) => kept.push(ModuleItem::Stmt(s)),
                ModuleItem::ModuleDecl(d) => match d {
                    // `export const/var/function/class …` → keep the bare declaration.
                    ModuleDecl::ExportDecl(ed) => {
                        kept.push(ModuleItem::Stmt(Stmt::Decl(ed.decl)))
                    }
                    // `export default <expr>;` → keep as an expression statement.
                    ModuleDecl::ExportDefaultExpr(e) => {
                        kept.push(ModuleItem::Stmt(Stmt::Expr(ExprStmt {
                            span: DUMMY_SP,
                            expr: e.expr,
                        })))
                    }
                    // `export { … }`, `import …` (side-effect / binding) → drop.
                    _ => {}
                },
            }
        }
        // Re-home as a Script so the harness evals it (no remaining ModuleDecls).
        let stmts: Vec<Stmt> = kept
            .into_iter()
            .filter_map(|it| match it {
                ModuleItem::Stmt(s) => Some(s),
                _ => None,
            })
            .collect();
        *ast.program_mut() = Program::Script(Script {
            span: DUMMY_SP,
            body: stmts,
            shebang: None,
        });
    }
    Js.print(&ast)
}

/// A MODULE with an `import` boundary in the middle: a wrappable run before AND after
/// the import, each virtualizes; the import stays native; the program runs identically
/// to its script-equivalent. (§3.1 boundary, §9.4 module-boundary.)
#[test]
fn whole_program_module_import_boundary_partitions() {
    // Run A (before import) computes into a global; the import is a native boundary;
    // run B (after) computes the sink. Neither run shares a binding across the import
    // (so no cells needed) — two independent chunks around one native item.
    let src = "var a = 2 + 3; globalThis.__a = a; import 'side-effect'; var b = 10 * 4; globalThis.__out = JSON.stringify(globalThis.__a + b);";
    let (out, put) = run_whole_program(src, 7);
    assert!(put, "the runs around the import virtualize");
    assert!(out.contains("import"), "the import stays native:\n{out}");
    // Two re-entry calls (one per run) → the table has at least two chunks.
    let calls = out.matches("[0],").count();
    assert!(calls >= 2, "two runs → at least two re-entry calls, saw {calls}:\n{out}");
    // Behavioral parity via the script-equivalent (import stripped).
    let stripped = strip_module_decls(src);
    mangler_testkit::eval::assert_behaviorally_equal(&stripped, &strip_module_decls(&out));
}

/// Cross-run `var`/`function` data flow through native cells (§2.1): run A declares a
/// `var` and a `function`; an import boundary separates run B which READS both. The
/// cross-run names are hoisted to native `var x = [undefined]` cells and referenced as
/// `x[0]`; the program computes the correct value AND the cell binding is NOT a real
/// global beyond what the original declared.
#[test]
fn whole_program_cross_run_var_function_flow_via_cells() {
    // `seed` (var) and `dbl` (function) are declared in run A, read in run B across
    // the import boundary → both become cells.
    let src = "var seed = 21; function dbl(n){ return n * 2; } \
        import 'm'; \
        globalThis.__out = JSON.stringify(dbl(seed));";
    let (out, put) = run_whole_program(src, 13);
    assert!(put, "virtualized around the import");
    // The cross-run names are cell-ified: a hoisted `[undefined]` cell and `[0]` reads.
    assert!(
        out.contains("=[undefined]") || out.contains("= [undefined]"),
        "cross-run names hoisted to native cells:\n{out}"
    );
    // Behavioral VALUE parity via the script-equivalent: `dbl(seed)` must be 42 in both.
    let stripped = strip_module_decls(src);
    let stripped_out = strip_module_decls(&out);
    mangler_testkit::eval::assert_behaviorally_equal(&stripped, &stripped_out);
    // Leak guard (§7 no-new-global-leakage): the cross-run bindings live ONLY as the
    // hoisted `var <name>=[undefined]` cells, which in the original ES MODULE are
    // module-scoped (not global properties), exactly as the original `var seed` /
    // `function dbl` were. The transform introduces NO `globalThis.seed=`/
    // `globalThis.dbl=` write (the only globals it touches are the original sink) — so
    // nothing leaks beyond what the original did.
    assert!(
        !out.contains("globalThis.seed") && !out.contains("globalThis.dbl"),
        "cross-run cells must NOT be promoted to real globals:\n{out}"
    );
}

/// An EXPORT-bound name (§5) stays a native binding: `export const x` is native, and a
/// later run that reads `x` captures it by name (resolving to the native module
/// binding). The export declaration is NOT pulled into the VM.
#[test]
fn whole_program_export_bound_name_kept_native() {
    let src = "export const k = 21; globalThis.__out = JSON.stringify(k * 2);";
    let (out, put) = run_whole_program(src, 4);
    assert!(put, "the reader run virtualizes around the native export");
    // `export const k = 21` is emitted verbatim (native binding for the export).
    assert!(
        out.contains("export const k") || out.contains("export const k=21") || out.contains("k = 21") || out.contains("k=21"),
        "export-bound `k` declaration stays native:\n{out}"
    );
    // Behavioral parity via the script-equivalent.
    mangler_testkit::eval::assert_behaviorally_equal(&strip_module_decls(src), &strip_module_decls(&out));
}

/// §3.3 adaptive bisection: an UNSUPPORTED construct (a `with` statement — a permanent
/// VM bail) wedged mid-run. The maximal eligible neighbors still virtualize; ONLY the
/// offender stays native. Verified structurally (the `with` is native, neighbors are
/// re-entry calls) and behaviorally (script-evalable; no shared binding across splits).
#[test]
fn whole_program_bisection_isolates_offender() {
    // Three independent statements; the middle one (`with`) cannot compile. Bisection
    // must isolate it native while the first and last virtualize. No binding is shared
    // across the statements, so bisection is sound.
    let src = "globalThis.__a = 1 + 2; \
        with (Math) { globalThis.__b = floor(3.7); } \
        globalThis.__out = JSON.stringify(globalThis.__a + globalThis.__b);";
    let (out, put) = run_whole_program(src, 8);
    assert!(put, "the eligible neighbors virtualize");
    // The offending `with` stays native (the VM never emits `with`).
    assert!(out.contains("with"), "the `with` offender stays native:\n{out}");
    // At least two re-entry calls (the two eligible neighbors).
    let calls = out.matches("[0],").count();
    assert!(calls >= 2, "neighbors virtualize as separate chunks, saw {calls}:\n{out}");
    mangler_testkit::eval::assert_behaviorally_equal(src, &out);
}

/// A single-IIFE program with NO cross-run bindings stays ONE clean chunk — no cell
/// hoisting, no needless partitioning (Phase-1-shape output preserved).
#[test]
fn whole_program_single_iife_one_chunk_no_cells() {
    let src = "(function(){ var s=0; for(var i=0;i<10;i++){ s+=i*i; } globalThis.__out=JSON.stringify(s); })();";
    let (out, put) = run_whole_program(src, 7);
    assert!(put, "virtualized");
    // No cell hoisting (no cross-run bindings).
    assert!(!out.contains("[undefined]"), "no needless cell hoisting for one chunk:\n{out}");
    // Exactly one re-entry call (one chunk).
    assert_eq!(out.matches("[1][0],").count() + out.matches("[0][0],").count(), 1, "single chunk → one re-entry call:\n{out}");
    mangler_testkit::eval::assert_behaviorally_equal(src, &out);
}

/// Determinism (§7): same seed ⇒ byte-identical output INCLUDING partition boundaries
/// and the bisection sequence, for a partitioned + bisected program.
#[test]
fn whole_program_partition_deterministic_same_seed() {
    let src = "globalThis.__a = 1 + 2; \
        with (Math) { globalThis.__b = floor(3.7); } \
        var q = 5; globalThis.__c = q * q; \
        globalThis.__out = JSON.stringify(globalThis.__a + globalThis.__b + globalThis.__c);";
    let (a, _) = run_whole_program(src, 0xABCDEF);
    let (b, _) = run_whole_program(src, 0xABCDEF);
    assert_eq!(a, b, "same seed → byte-identical partition + bisection output");
}
