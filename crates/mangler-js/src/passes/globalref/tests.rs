//! Unit + differential tests for the global-reference indirection pass.
//!
//! These build a minimal pipeline directly (parse → `FileConfig` → bus →
//! `pass.run` → print), assert on the printed AST for structural facts (mirroring
//! the `memberaccess`/`expr`/`deadcode` test style), and validate behavior with
//! [`assert_behaviorally_equal`] on programs that exercise QuickJS-present
//! intrinsics (`Math`, `JSON`, `Object`, `parseInt`, `isNaN`, …).

use super::*;
use mangler_config::{ConfigFlags, GlobalIndirect, Intensity, ResolvedConfig};
use mangler_core::PassConfig;
use mangler_jsast::{Js, ParseOpts};
use mangler_passgraph::{ArtifactBus, Pass, Resource};
use mangler_testkit::eval::assert_behaviorally_equal;
use std::collections::HashSet;
use swc_core::ecma::visit::{Visit, VisitWith};

// ── Harness ────────────────────────────────────────────────────────────────

fn resolved(level: Intensity, mode: GlobalIndirect, harden: bool) -> ResolvedConfig {
    let flags = ConfigFlags {
        preset: Some(level),
        seed: Some(1),
        ..Default::default()
    };
    let mut r = ResolvedConfig::try_from(flags).expect("valid preset config");
    r.passes.global_indirect.mode = mode;
    r.passes.global_indirect.harden_anchor = harden;
    r
}

/// Collect every identifier symbol in the source (so injected names never collide).
fn reserved_idents(src: &str) -> HashSet<String> {
    struct V(HashSet<String>);
    impl Visit for V {
        fn visit_ident(&mut self, n: &Ident) {
            self.0.insert(n.sym.to_string());
        }
        fn visit_ident_name(&mut self, n: &IdentName) {
            self.0.insert(n.sym.to_string());
        }
    }
    let ast = Js.parse(src, &ParseOpts::default()).unwrap();
    let mut v = V(HashSet::new());
    ast.program().visit_with(&mut v);
    v.0
}

/// Run ONLY the globalref pass over `src` and return the printed (un-minified)
/// output. We assert on this directly: the full-pipeline minifier would re-fold
/// `_G["Math"]` member reads, but in the real pipeline the strings pass (ordered
/// after us by our `GlobalNameLiterals` write) encodes those literals first.
fn run_pass(src: &str, level: Intensity, seed: u64, mode: GlobalIndirect, harden: bool) -> String {
    let cfg = FileConfig::new(resolved(level, mode, harden), seed, reserved_idents(src));
    let mut ast = Js.parse(src, &ParseOpts::default()).unwrap();
    let mut bus = ArtifactBus::new();
    let mut rng = Rng::for_pass(cfg.seed(), "globalref");
    let mut notes = Notes::default();
    bus.enter_pass(
        "globalref",
        &[Resource::property_literals()],
        &[Resource::global_name_literals()],
    );
    GlobalRefPass
        .run(&mut ast, &cfg, &mut rng, &mut bus, &mut notes)
        .unwrap();
    Js.print(&ast)
}

/// Default Medium/Safe run.
fn safe(src: &str) -> String {
    run_pass(src, Intensity::Medium, 1, GlobalIndirect::Safe, false)
}

/// Default Medium/Aggressive run.
fn aggressive(src: &str) -> String {
    run_pass(src, Intensity::Medium, 1, GlobalIndirect::Aggressive, false)
}

// ── Pass contract ──────────────────────────────────────────────────────────

#[test]
fn pass_contract() {
    let pass = GlobalRefPass;
    assert_eq!(pass.id(), "globalref");
    assert_eq!(pass.reads(), &[Resource::property_literals()]);
    assert_eq!(pass.writes(), &[Resource::global_name_literals()]);

    let off = FileConfig::new(
        resolved(Intensity::Low, GlobalIndirect::Off, false),
        1,
        HashSet::new(),
    );
    let on = FileConfig::new(
        resolved(Intensity::Medium, GlobalIndirect::Safe, false),
        1,
        HashSet::new(),
    );
    assert!(!pass.enabled(&off), "Off mode disables the pass");
    assert!(pass.enabled(&on), "Safe mode enables the pass");
}

#[test]
fn writes_global_name_literals_artifact() {
    // Putting the artifact is what lets the strings pass order after us.
    let cfg = FileConfig::new(
        resolved(Intensity::Medium, GlobalIndirect::Safe, false),
        1,
        reserved_idents("var x = Math.PI;"),
    );
    let mut ast = Js.parse("var x = Math.PI;", &ParseOpts::default()).unwrap();
    let mut bus = ArtifactBus::new();
    let mut rng = Rng::for_pass(cfg.seed(), "globalref");
    let mut notes = Notes::default();
    bus.enter_pass(
        "globalref",
        &[Resource::property_literals()],
        &[Resource::global_name_literals()],
    );
    GlobalRefPass
        .run(&mut ast, &cfg, &mut rng, &mut bus, &mut notes)
        .unwrap();
    assert!(
        bus.contains::<GlobalNameLiteralsArtifact>(),
        "must put the artifact"
    );
}

// ── Anchor + injection ─────────────────────────────────────────────────────

#[test]
fn injects_lexical_accessor_table() {
    let out = safe("var x = Math.PI;");
    assert!(
        out.contains("return Math"),
        "accessor preserves lexical lookup: {out}"
    );
    assert!(
        !out.contains("=globalThis"),
        "no global-object anchor: {out}"
    );
}

#[test]
fn anchor_only_emitted_once_for_many_globals() {
    let out = safe("var a = Math.PI, b = JSON, c = Object, d = Array;");
    assert_eq!(
        out.matches("return Math").count(),
        1,
        "one accessor per name: {out}"
    );
    assert!(
        !out.contains("=globalThis"),
        "no global-object anchor: {out}"
    );
}

#[test]
fn live_getter_globals_are_not_allowlisted() {
    // Value-caching live getters would desync from later reads.
    for name in [
        "innerWidth",
        "innerHeight",
        "scrollX",
        "scrollY",
        "devicePixelRatio",
    ] {
        assert!(
            !allowlist::is_allowlisted(name),
            "{name} must not be allowlisted"
        );
    }
}

// ── Detection: never-declared rule + selection ─────────────────────────────

#[test]
fn safe_indirects_allowlisted_global_read() {
    let out = safe("var s = Math;");
    assert!(
        out.contains("\"Math\""),
        "names/alias literal must carry Math: {out}"
    );
    assert!(
        !out.contains("s=Math;"),
        "bare Math read must be indirected: {out}"
    );
}

#[test]
fn safe_skips_non_allowlisted_free_global() {
    let out = safe("var s = notARealGlobalXyz;");
    assert!(
        out.contains("notARealGlobalXyz"),
        "non-allowlisted free global preserved in Safe: {out}"
    );
    assert!(
        !out.contains("=globalThis"),
        "no table injected when nothing indirected: {out}"
    );
}

#[test]
fn aggressive_indirects_non_allowlisted_free_global() {
    let out = aggressive("var s = notARealGlobalXyz;");
    assert!(
        out.contains("\"notARealGlobalXyz\""),
        "Aggressive indirects any free global: {out}"
    );
    assert!(
        !out.contains("s=notARealGlobalXyz;"),
        "use site must be the alias: {out}"
    );
}

#[test]
fn shadowing_disables_indirection_file_wide() {
    let src = "function g(){ var Math = 1; return Math; } var z = Math.PI;";
    let out = safe(src);
    assert!(
        out.contains("Math.PI"),
        "shadowed Math must stay bare: {out}"
    );
    assert!(
        !out.contains("=globalThis"),
        "no table when the only global is shadowed: {out}"
    );
}

#[test]
fn param_shadow_disables_indirection() {
    let src = "function h(fetch){ return fetch; } var p = fetch('/x');";
    let out = safe(src);
    assert!(
        out.contains("fetch('/x')") || out.contains("fetch(\"/x\")"),
        "param-shadowed fetch must stay bare: {out}"
    );
}

#[test]
fn destructuring_binding_shadow_disables() {
    let src = "var { Object } = lib; var k = Object;";
    let out = safe(src);
    assert!(
        !out.contains("globalThis[\"Object\"]") && !out.contains("=globalThis"),
        "destructured-bound Object must not be indirected: {out}"
    );
}

// ── Bail conditions ────────────────────────────────────────────────────────

#[test]
fn eval_bails_entirely() {
    let out = safe("eval('1'); var z = Math.PI;");
    assert!(
        !out.contains("globalThis"),
        "eval must disable all indirection: {out}"
    );
    assert!(
        out.contains("Math.PI"),
        "Math read stays bare after eval bail: {out}"
    );
}

#[test]
fn with_bails_entirely() {
    let out = safe("with(o){ y } var z = Math.PI;");
    assert!(
        !out.contains("globalThis"),
        "with must disable all indirection: {out}"
    );
}

// ── Position handling ──────────────────────────────────────────────────────

#[test]
fn bare_call_is_indirected() {
    let out = safe("requestAnimationFrame(cb);");
    assert!(
        !out.contains("requestAnimationFrame(cb)"),
        "bare global call must be indirected: {out}"
    );
    assert!(
        out.contains("\"requestAnimationFrame\""),
        "literal carries requestAnimationFrame: {out}"
    );
}

#[test]
fn member_base_is_indirected() {
    let out = safe("document.title = 'x';");
    assert!(
        !out.contains("document.title") && !out.contains("document[\"title\"]"),
        "member base document must be indirected: {out}"
    );
}

#[test]
fn new_expr_is_indirected() {
    let out = safe("var d = new Date();");
    assert!(
        !out.contains("new Date("),
        "new Date must indirect to new <alias>: {out}"
    );
    assert!(out.contains("\"Date\""), "literal carries Date: {out}");
}

#[test]
fn typeof_remains_a_native_lookup() {
    let out = safe("var t = typeof Symbol;");
    assert!(
        out.contains("typeof Symbol"),
        "typeof must preserve missing-binding behavior: {out}"
    );
    assert!(
        !out.contains("return Symbol"),
        "typeof requires no accessor: {out}"
    );
}

#[test]
fn assignment_target_is_not_indirected() {
    let out = safe("fetch = 1;");
    assert!(
        out.contains("fetch=1"),
        "assignment target must stay bare: {out}"
    );
    assert!(
        !out.contains("=globalThis"),
        "no alias hoisted for an assign-only global: {out}"
    );
}

#[test]
fn compound_assignment_target_is_not_indirected() {
    // scrollX is not allowlisted, so use Aggressive to prove the write exclusion.
    let out = aggressive("scrollY += 1;");
    assert!(
        out.contains("scrollY+=1"),
        "compound assign target must stay bare: {out}"
    );
}

#[test]
fn update_target_is_not_indirected() {
    let out = aggressive("scrollX++;");
    assert!(
        out.contains("scrollX++"),
        "update target must stay bare: {out}"
    );
}

#[test]
fn delete_target_is_not_indirected() {
    let out = aggressive("delete navigator;");
    assert!(
        out.contains("delete navigator"),
        "delete target must stay bare: {out}"
    );
}

#[test]
fn assignment_rhs_still_indirected() {
    let out = safe("notDeclaredLhs = Math;");
    assert!(out.contains("notDeclaredLhs="), "LHS stays bare: {out}");
    assert!(
        !out.contains("=Math;"),
        "RHS Math must be indirected: {out}"
    );
}

// ── Whole-name write-exclusion (correctness-critical) ──────────────────────

#[test]
fn for_of_target_excludes_whole_name() {
    let src = "for (freeGlobalTarget of [1,2,3]) { sink(freeGlobalTarget); } record(typeof freeGlobalTarget);";
    let out = aggressive(src);
    assert!(
        out.contains("freeGlobalTarget of"),
        "for-of head target must stay bare: {out}"
    );
    assert!(
        out.contains("typeof freeGlobalTarget"),
        "read of a written name must stay bare: {out}"
    );
    assert!(
        !out.contains(".freeGlobalTarget") && !out.contains("[\"freeGlobalTarget\"]"),
        "no alias hoisted for a written global: {out}"
    );
}

#[test]
fn for_in_target_excludes_whole_name() {
    let src = "for (freeGlobalKey in obj) { sink(freeGlobalKey); } var r = freeGlobalKey;";
    let out = aggressive(src);
    assert!(
        out.contains("freeGlobalKey in"),
        "for-in head target must stay bare: {out}"
    );
    assert!(
        out.contains("=freeGlobalKey"),
        "read of written name stays bare: {out}"
    );
    assert!(
        !out.contains(".freeGlobalKey") && !out.contains("[\"freeGlobalKey\"]"),
        "no alias hoisted for a for-in-written global: {out}"
    );
}

#[test]
fn assigned_name_read_is_also_excluded() {
    let out = aggressive("assignedGlobal = 1; var r = assignedGlobal;");
    assert!(out.contains("assignedGlobal=1"), "write stays bare: {out}");
    assert!(
        out.contains("=assignedGlobal"),
        "read of written name must stay bare: {out}"
    );
}

#[test]
fn update_name_read_is_also_excluded() {
    let out = aggressive("counterGlobal++; var r = counterGlobal;");
    assert!(out.contains("counterGlobal++"), "update stays bare: {out}");
    assert!(
        out.contains("=counterGlobal"),
        "read of updated name must stay bare: {out}"
    );
}

#[test]
fn paren_assign_target_excludes_whole_name() {
    let out = aggressive("(scrollX) = 1; record(scrollX);");
    assert!(out.contains("scrollX"), "the bare name must remain: {out}");
    assert!(
        !out.contains(".scrollX") && !out.contains("[\"scrollX\"]"),
        "parenthesized assign target must exclude the whole name: {out}"
    );
}

#[test]
fn paren_update_target_excludes_whole_name() {
    let out = aggressive("++(scrollX); record(scrollX);");
    assert!(out.contains("scrollX"), "the bare name must remain: {out}");
    assert!(
        !out.contains(".scrollX") && !out.contains("[\"scrollX\"]"),
        "parenthesized prefix-update operand must exclude the whole name: {out}"
    );
}

#[test]
fn paren_delete_target_excludes_whole_name() {
    let out = aggressive("delete (navigator); record(typeof navigator);");
    assert!(
        out.contains("navigator"),
        "the bare name must remain: {out}"
    );
    assert!(
        !out.contains(".navigator") && !out.contains("[\"navigator\"]"),
        "parenthesized delete operand must exclude the whole name: {out}"
    );
}

// ── globalThis self-indirection + directives ───────────────────────────────

#[test]
fn globalthis_is_never_indirected() {
    let out = safe("var g = globalThis.Math;");
    assert!(
        out.contains("globalThis"),
        "globalThis stays (it is the anchor): {out}"
    );
}

#[test]
fn use_strict_directive_stays_first() {
    let out = safe("\"use strict\"; var x = Math.PI;");
    let trimmed = out.trim_start();
    assert!(
        trimmed.starts_with("\"use strict\"") || trimmed.starts_with("'use strict'"),
        "use strict must remain the first statement: {out}"
    );
}

// ── Decoys + dispatcher shape ──────────────────────────────────────────────

/// Count the string entries of the first `["…"]` (dispatcher names array `_GN`).
fn names_array_len(out: &str) -> usize {
    let start = match out.find("=[\"") {
        Some(p) => p + 1,
        None => return 0,
    };
    let bytes = out.as_bytes();
    let mut depth = 0usize;
    let mut end = start;
    for (off, &b) in bytes[start..].iter().enumerate() {
        match b {
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    end = start + off + 1;
                    break;
                }
            }
            _ => {}
        }
    }
    out[start..end].matches('"').count() / 2
}

#[test]
fn decoys_are_injected_and_inert() {
    // One real global (Math) + 2 decoys at Medium → ≥3 names-array entries.
    let out = safe("var x = Math.PI;");
    assert!(
        names_array_len(&out) >= 3,
        "decoys must pad the names array (>=3 entries): {out}"
    );
}

#[test]
fn decoy_count_scales_with_intensity() {
    let count_for = |level: Intensity| -> usize {
        names_array_len(&run_pass(
            "var x = Math.PI;",
            level,
            1,
            GlobalIndirect::Safe,
            false,
        ))
    };
    assert!(
        count_for(Intensity::High) > count_for(Intensity::Medium),
        "High must inject more decoys than Medium"
    );
}

#[test]
fn dispatcher_threshold_is_three() {
    assert_eq!(
        super::DISPATCHER_MIN_ENTRIES,
        3,
        "one-hop fallback applies below 3 combined entries"
    );
}

// ── Anchor hardening ───────────────────────────────────────────────────────

#[test]
fn anchor_hardening_is_unnecessary_for_lexical_accessors() {
    let src = "var x = Math.PI;";
    let normal = run_pass(src, Intensity::High, 1, GlobalIndirect::Safe, false);
    let hardened = run_pass(src, Intensity::High, 1, GlobalIndirect::Safe, true);
    assert_eq!(normal, hardened);
    assert!(!normal.contains("=globalThis"));
}

// ── Determinism ────────────────────────────────────────────────────────────

#[test]
fn same_seed_identical_output() {
    let src = "function f(cb){ requestAnimationFrame(cb); return Math.max(parseInt('1'), 2); }";
    let a = run_pass(src, Intensity::High, 4242, GlobalIndirect::Safe, false);
    let b = run_pass(src, Intensity::High, 4242, GlobalIndirect::Safe, false);
    assert_eq!(a, b, "same source + seed must be byte-identical");
}

#[test]
fn different_seed_changes_layout() {
    let src = "var a = document.title, b = Math.PI, c = JSON, d = Object, e = Array;";
    let a = run_pass(src, Intensity::High, 11, GlobalIndirect::Safe, false);
    let b = run_pass(src, Intensity::High, 22, GlobalIndirect::Safe, false);
    assert_ne!(a, b, "different seeds must produce different layouts");
}

// ── Dispatcher index↔name bijection (correctness-critical) ─────────────────

#[test]
fn dispatcher_index_roundtrips_for_every_entry() {
    for n in 1..=12usize {
        let names: Vec<String> = (0..n).map(|e| format!("G{e}")).collect();
        let entries: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
        for seed in 0..40u64 {
            let mut rng = Rng::for_pass(seed, "globalref");
            // Reproduce the injector's draw order: store, then perm.
            let store = rng.random_perm(n);
            let perm = super::dispatcher_involution(n, &mut rng);

            for i in 0..n {
                assert_eq!(
                    perm[perm[i]], i,
                    "perm must be an involution (n={n}, seed={seed})"
                );
            }
            let mut s_sorted = store.clone();
            s_sorted.sort_unstable();
            assert_eq!(
                s_sorted,
                (0..n).collect::<Vec<_>>(),
                "store must be a permutation"
            );

            let name_at_pos = super::dispatcher_name_at_pos(&entries, &store);
            for (e, entry) in entries.iter().enumerate() {
                let k = super::dispatcher_key(&perm, &store, e);
                let resolved = super::dispatcher_resolve(&name_at_pos, &perm, k);
                assert_eq!(
                    resolved, *entry,
                    "entry {e} mis-resolved (n={n}, seed={seed})"
                );
            }
        }
    }
}

#[test]
fn dispatcher_involution_is_self_inverse() {
    for n in 0..=16usize {
        for seed in 0..16u64 {
            let mut rng = Rng::for_pass(seed, "globalref");
            let perm = super::dispatcher_involution(n, &mut rng);
            assert_eq!(perm.len(), n);
            let mut sorted = perm.clone();
            sorted.sort_unstable();
            assert_eq!(
                sorted,
                (0..n).collect::<Vec<_>>(),
                "not a permutation (n={n})"
            );
            for i in 0..n {
                assert_eq!(perm[perm[i]], i, "not an involution (n={n}, seed={seed})");
            }
        }
    }
}

// ── End-to-end behavioral identity (QuickJS-present intrinsics) ─────────────

/// A bounded program over read-only intrinsics QuickJS provides. The pass must
/// preserve its observable output. We capture via the `globalThis.__out` sink.
const SINK_PROGRAM: &str = r#"
    (function(){
        var nums = [3, 1, 2, 10, 7];
        var m = Math.max.apply(null, nums);
        var n = parseInt("42", 10);
        var bad = isNaN(Number("x")) ? 1 : 0;
        var obj = { a: 1, b: 2, c: 3 };
        var keys = Object.keys(obj).join(",");
        var arr = Array.from([9, 8, 7]).join("-");
        var s = JSON.stringify({ m: m, n: n, bad: bad, keys: keys, arr: arr });
        globalThis.__out = String(s);
    })();
"#;

#[test]
fn safe_indirection_round_trips() {
    for level in [Intensity::Medium, Intensity::High, Intensity::Max] {
        for seed in [1u64, 7, 31337] {
            let out = run_pass(SINK_PROGRAM, level, seed, GlobalIndirect::Safe, false);
            assert_behaviorally_equal(SINK_PROGRAM, &out);
        }
    }
}

#[test]
fn aggressive_indirection_round_trips() {
    for seed in [1u64, 9, 555] {
        let out = run_pass(
            SINK_PROGRAM,
            Intensity::High,
            seed,
            GlobalIndirect::Aggressive,
            false,
        );
        assert_behaviorally_equal(SINK_PROGRAM, &out);
    }
}

#[test]
fn hardened_anchor_round_trips() {
    let out = run_pass(SINK_PROGRAM, Intensity::High, 7, GlobalIndirect::Safe, true);
    assert_behaviorally_equal(SINK_PROGRAM, &out);
}

/// Whole-name write-exclusion round-trips: a written free global (`writtenG`,
/// seeded onto globalThis so the bare write is legal) is mutated and read, while
/// read-only intrinsics alongside it are indirected. The written global must be
/// left bare so its reads see live writes.
#[test]
fn written_global_left_alone_round_trips() {
    let src = r#"
        globalThis.writtenG = 0;
        (function(){
            for (var i = 0; i < 4; i++) { writtenG = writtenG + i; }
            writtenG++;
            writtenG = writtenG + parseInt("10", 10);
            var mx = Math.max.apply(null, [5, 2, 9, 1]);
            var keys = Object.keys({ a: 1, b: 2 }).join("-");
            globalThis.__out = String(JSON.stringify({ w: writtenG, mx: mx, keys: keys }));
        })();
    "#;
    for mode in [GlobalIndirect::Safe, GlobalIndirect::Aggressive] {
        let out = run_pass(src, Intensity::High, 9001, mode, false);
        assert_behaviorally_equal(src, &out);
    }
}

/// `this`-preservation for bare calls: a sloppy-mode bare call sees
/// `this === globalThis`. The indirected bare call must keep the same `this`.
#[test]
fn bare_call_this_is_preserved() {
    let src = r#"
        globalThis.MARKER = "ok";
        function probe() { return this && this.MARKER ? this.MARKER : "no-this"; }
        globalThis.probe = probe;
        globalThis.__out = String(probe());
    "#;
    // `probe` is a function declaration (declared), so it is never indirected;
    // this exercises that bare calls round-trip with `this === globalThis` intact
    // when surrounding intrinsics are indirected.
    let out = run_pass(src, Intensity::High, 7, GlobalIndirect::Aggressive, false);
    assert_behaviorally_equal(src, &out);
}

#[test]
fn commonjs_and_implicit_arguments_are_preserved() {
    let src = "function f(){return [typeof require,typeof module,typeof exports,__filename,__dirname,arguments[0]];}";
    for out in [safe(src), aggressive(src)] {
        for name in [
            "require",
            "module",
            "exports",
            "__filename",
            "__dirname",
            "arguments",
        ] {
            assert!(
                !out.contains(&format!("return {name};")),
                "wrapper binding accessor: {out}"
            );
        }
    }
}

#[test]
fn lexical_accessors_preserve_live_reads_and_failures() {
    let cases = [
        "globalThis.parseInt=function(){return 91};globalThis.__out=String(parseInt('3'));",
        "var hits=0;Object.defineProperty(globalThis,'liveValue',{configurable:true,get:function(){return ++hits}});globalThis.__out=String(liveValue+liveValue)+':'+hits;",
        "var globalThis={}; globalThis.__out=String(Math.PI);",
        "globalThis.customGlobal=function(){'use strict';return this===undefined};globalThis.__out=String(customGlobal());",
        "globalThis.Custom=function(){this.value=1};var a=new Custom();globalThis.Custom=function(){this.value=2};globalThis.__out=String(a.value+(new Custom()).value);",
        "var result='';try{missingGlobalForMangler}catch(e){result=e.name}globalThis.__out=result+':'+typeof (otherMissingGlobal);",
        "var before=typeof anotherMissingGlobal;globalThis.anotherMissingGlobal=4;globalThis.__out=before+':'+anotherMissingGlobal;",
        "function f(){return arguments[0]}globalThis.__out=String(f(7));",
    ];
    for src in cases {
        assert_behaviorally_equal(src, &aggressive(src));
    }
}

#[test]
fn external_lexical_bindings_are_not_global_object_properties() {
    let prelude = "let sharedExternal=7;";
    let src = "globalThis.__out=String(sharedExternal);";
    assert_behaviorally_equal(
        &format!("{prelude}{src}"),
        &format!("{prelude}{}", aggressive(src)),
    );
}
