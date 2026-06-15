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
    assert!(p.reads().is_empty(), "no reads → sorts pre-resolver");
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

#[test]
fn use_strict_directive_bails() {
    let src = "function f(){ 'use strict'; return this; } globalThis.__out=String(typeof f);";
    let (_out, put) = run_virtualize(src, "*", 7);
    assert!(!put, "own use-strict directive must bail");
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
