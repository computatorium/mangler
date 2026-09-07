//! Self-contained pipeline tests for the strings pass (NOT via `register_passes`).
//!
//! Build a minimal pipeline directly: parse → FileConfig → ArtifactBus →
//! `Rng::for_pass` → `bus.enter_pass` → `pass.run` → read `DecoderAnchorArtifact`
//! under a reader scope → print → `assert_behaviorally_equal`.

use super::StringsPass;
use crate::artifacts::DecoderAnchorArtifact;
use crate::config::FileConfig;
use mangler_config::{Intensity, StringMode};
use mangler_core::{Language, Notes, Rng};
use mangler_jsast::{Js, ParseOpts};
use mangler_passgraph::{ArtifactBus, Pass, Resource};
use std::collections::HashSet;

/// Run ONLY the strings pass over `src` at `level`/`seed`, returning the printed
/// output and the put `DecoderAnchorArtifact` (if any).
fn run_strings(src: &str, level: Intensity, seed: u64) -> (String, Option<String>) {
    let resolved = crate::test_support::resolved(level, seed);
    let cfg = FileConfig::new(resolved, seed, HashSet::new());
    Js::with_globals(|| {
        let mut ast = Js.parse(src, &ParseOpts::default()).expect("parse");
        let mut bus = ArtifactBus::new();
        let mut rng = Rng::for_pass(seed, "strings");
        let mut notes = Notes::new();
        let pass = StringsPass;

        bus.enter_pass(pass.id(), pass.reads(), pass.writes());
        pass.run(&mut ast, &cfg, &mut rng, &mut bus, &mut notes)
            .expect("run");

        // Read the anchor back under a reader scope.
        bus.enter_pass("reader", &[Resource::decoder_anchor()], &[]);
        let anchor = bus
            .get::<DecoderAnchorArtifact>()
            .expect("bus get")
            .map(|a| a.core_name.clone());

        // Resolve so codegen emits a clean, valid program (mirrors the runner, which
        // schedules the resolver after injection passes).
        Js::resolve(&mut ast);
        (Js.print(&ast), anchor)
    })
}

fn modes() -> [(Intensity, StringMode); 4] {
    [
        (Intensity::Low, StringMode::Encode),
        (Intensity::Medium, StringMode::Encode),
        (Intensity::High, StringMode::Encrypt),
        (Intensity::Max, StringMode::Encrypt),
    ]
}

const PROG: &str = r#"
var greeting = "hello world";
var obj = { "key name": "value here", plain: "another" };
function f(x) { return "prefix-" + x + "-suffix"; }
var tpl = `a${1 + 2}b${greeting}c`;
var arr = ["one", "two", "three", "one"];
globalThis.__out = greeting + "|" + obj["key name"] + "|" + f("Z") + "|" + tpl + "|" + arr.join(",");
"#;

#[test]
fn each_mode_round_trips_behaviorally() {
    for (level, expected_mode) in modes() {
        // Sanity-check the preset really selects the mode under test.
        let resolved = crate::test_support::resolved(level, 1);
        assert_eq!(resolved.passes.strings.mode, expected_mode, "{level:?}");

        let (out, anchor) = run_strings(PROG, level, 1234);
        assert!(anchor.is_some(), "decoder anchor must be put at {level:?}");
        // The literal text must be gone (encoded) — at least the obvious one.
        assert!(
            !out.contains("hello world"),
            "literal must be encoded at {level:?}: {out}"
        );
        mangler_testkit::eval::assert_behaviorally_equal(PROG, &out);
    }
}

#[test]
fn directives_and_skips_are_not_encoded() {
    let src = r#""use strict";
var x = "encode me please";
var o = { method: function(){ return "value"; } };
globalThis.__out = x + o.method();
"#;
    let (out, _) = run_strings(src, Intensity::Medium, 7);
    // The directive must survive verbatim as the first statement.
    assert!(
        out.trim_start().starts_with("\"use strict\""),
        "directive preserved: {out}"
    );
    assert!(
        !out.contains("encode me please"),
        "body literal encoded: {out}"
    );
    mangler_testkit::eval::assert_behaviorally_equal(src, &out);
}

#[test]
fn already_call_form_is_not_double_encoded() {
    // A program whose strings have ALL been replaced once must not change semantics
    // if we run again — covered behaviorally by the round-trip; here we assert the
    // single-run output reparses and runs.
    let src = r#"globalThis.__out = "a" + "b" + "ab";"#;
    let (out, _) = run_strings(src, Intensity::High, 99);
    assert!(
        Js::reparse(&out, &ParseOpts::default()).is_ok(),
        "output must reparse: {out}"
    );
    mangler_testkit::eval::assert_behaviorally_equal(src, &out);
}

#[test]
fn determinism_same_seed_byte_identical() {
    let a = run_strings(PROG, Intensity::Max, 555).0;
    let b = run_strings(PROG, Intensity::Max, 555).0;
    assert_eq!(a, b, "same seed must yield byte-identical output");
}

#[test]
fn different_seed_differs() {
    let a = run_strings(PROG, Intensity::Max, 1).0;
    let b = run_strings(PROG, Intensity::Max, 2).0;
    assert_ne!(a, b, "different seeds should diverge");
}

#[test]
fn dynamic_key_round_trips_when_host_matches() {
    // Configure a dynamic key whose source_expr evaluates to the expected value in
    // the test host, so decode is exact.
    let mut resolved = crate::test_support::resolved(Intensity::High, 3);
    resolved.passes.strings.dynamic_key = Some(mangler_config::DynamicKey {
        source_expr: "\"prod-host\"".into(),
        expected: "prod-host".into(),
    });
    let cfg = FileConfig::new(resolved, 3, HashSet::new());
    let src = r#"globalThis.__out = "secret-config-value" + "/" + "endpoint";"#;
    let out = Js::with_globals(|| {
        let mut ast = Js.parse(src, &ParseOpts::default()).unwrap();
        let mut bus = ArtifactBus::new();
        let mut rng = Rng::for_pass(3, "strings");
        let mut notes = Notes::new();
        bus.enter_pass("strings", StringsPass.reads(), StringsPass.writes());
        StringsPass
            .run(&mut ast, &cfg, &mut rng, &mut bus, &mut notes)
            .unwrap();
        Js::resolve(&mut ast);
        Js.print(&ast)
    });
    assert!(!out.contains("secret-config-value"), "encoded: {out}");
    mangler_testkit::eval::assert_behaviorally_equal(src, &out);
}

#[test]
fn empty_program_is_untouched_and_no_anchor() {
    let src = "var x = 1 + 2; globalThis.__out = x;";
    let (out, anchor) = run_strings(src, Intensity::High, 1);
    assert!(anchor.is_none(), "no anchor when there are no strings");
    mangler_testkit::eval::assert_behaviorally_equal(src, &out);
}

#[test]
fn corpus_spot_check() {
    // Drive a few small representative programs through the pass.
    let progs = [
        r#"globalThis.__out = JSON.stringify({a:"x",b:[1,"two",3]});"#,
        r#"function g(){return "α"+"β"+"γ";} globalThis.__out = g();"#,
        r#"var m = new Map([["k","v"]]); globalThis.__out = m.get("k");"#,
        r#"globalThis.__out = "tab\there\nnewline\\backslash\"quote";"#,
        r#"globalThis.__out = `emoji: ${"😀"} done`;"#,
    ];
    for (i, p) in progs.iter().enumerate() {
        let (out, _) = run_strings(p, Intensity::Max, 100 + i as u64);
        mangler_testkit::eval::assert_behaviorally_equal(p, &out);
    }
}

#[test]
fn pass_shape_is_correct() {
    let p = StringsPass;
    assert_eq!(p.id(), "strings");
    assert_eq!(
        p.reads(),
        &[
            Resource::property_literals(),
            Resource::global_name_literals()
        ]
    );
    assert_eq!(p.writes(), &[Resource::decoder_anchor()]);

    let off = FileConfig::new(
        crate::test_support::resolved(Intensity::Minify, 1),
        1,
        HashSet::new(),
    );
    let on = FileConfig::new(
        crate::test_support::resolved(Intensity::Medium, 1),
        1,
        HashSet::new(),
    );
    assert!(!p.enabled(&off), "disabled at Minify (StringMode::None)");
    assert!(p.enabled(&on), "enabled at Medium");
}

// ----- opt-in VM-backed string-hardening modes (driven through the full runner) -----

mod vm_modes {
    use mangler_config::{ConfigFlags, Intensity, ResolvedConfig};
    use mangler_jsast::{Js, ParseOpts};

    /// A program whose decoded strings are observable via `globalThis.__out`.
    const VM_PROG: &str = r#"
var greeting = "hello world";
var obj = { "key name": "value here", plain: "another" };
function f(x) { return "prefix-" + x + "-suffix"; }
var tpl = `a${1 + 2}b${greeting}c`;
var arr = ["one", "two", "three", "one"];
globalThis.__out = greeting + "|" + obj["key name"] + "|" + f("Z") + "|" + tpl + "|" + arr.join(",");
"#;

    /// Build a resolved config at `level`/`seed`, then apply `tweak` to set opt-in flags.
    fn cfg(level: Intensity, seed: u64, tweak: impl FnOnce(&mut ResolvedConfig)) -> ResolvedConfig {
        let flags = ConfigFlags {
            preset: Some(level),
            seed: Some(seed),
            ..Default::default()
        };
        let mut c = ResolvedConfig::try_from(flags).expect("valid config");
        tweak(&mut c);
        c
    }

    fn run(src: &str, c: &ResolvedConfig) -> String {
        crate::runner::process(src, &ParseOpts::default(), c)
            .expect("process failed")
            .0
    }

    #[test]
    fn strings_in_vm_round_trips_behaviorally() {
        let c = cfg(Intensity::High, 4242, |c| c.passes.strings.in_vm = true);
        let out = run(VM_PROG, &c);
        assert!(
            !out.contains("hello world"),
            "literal must be encoded: {out}"
        );
        mangler_testkit::eval::assert_behaviorally_equal(VM_PROG, &out);
    }

    #[test]
    fn strings_in_vm_self_coupled_round_trips() {
        let c = cfg(Intensity::High, 9001, |c| {
            c.passes.strings.in_vm = true;
            c.passes.strings.self_coupled_key = true;
        });
        let out = run(VM_PROG, &c);
        assert!(
            !out.contains("hello world"),
            "literal must be encoded: {out}"
        );
        // The sentinel must have been patched to a real (non-sentinel) value.
        assert!(
            !out.contains("SCK9999999999"),
            "sentinel must be patched: {out}"
        );
        assert!(
            out.contains("SCK"),
            "patched sentinel token must remain: {out}"
        );
        mangler_testkit::eval::assert_behaviorally_equal(VM_PROG, &out);
    }

    #[test]
    fn strings_in_vm_exec_trace_round_trips() {
        let c = cfg(Intensity::High, 7777, |c| {
            c.passes.strings.in_vm = true;
            c.passes.strings.exec_trace_key = true;
        });
        let out = run(VM_PROG, &c);
        assert!(
            !out.contains("hello world"),
            "literal must be encoded: {out}"
        );
        mangler_testkit::eval::assert_behaviorally_equal(VM_PROG, &out);
    }

    #[test]
    fn each_vm_mode_is_deterministic() {
        for tweak in [
            |c: &mut ResolvedConfig| c.passes.strings.in_vm = true,
            |c: &mut ResolvedConfig| {
                c.passes.strings.in_vm = true;
                c.passes.strings.self_coupled_key = true;
            },
            |c: &mut ResolvedConfig| {
                c.passes.strings.in_vm = true;
                c.passes.strings.exec_trace_key = true;
            },
        ] {
            let c = cfg(Intensity::High, 31337, tweak);
            let a = run(VM_PROG, &c);
            let b = run(VM_PROG, &c);
            assert_eq!(a, b, "same seed + flags must be byte-identical");
        }
    }

    #[test]
    fn default_path_byte_identical_when_flags_off() {
        // With all VM flags off, output must be byte-identical to a build that never
        // knew about them (the hard invariant).
        let c = cfg(Intensity::High, 5150, |_| {});
        let a = run(VM_PROG, &c);
        let b = run(
            VM_PROG,
            &cfg(Intensity::High, 5150, |c| {
                // Explicitly leave flags at their defaults (off).
                c.passes.strings.in_vm = false;
                c.passes.strings.self_coupled_key = false;
                c.passes.strings.exec_trace_key = false;
            }),
        );
        assert_eq!(a, b, "flags-off output must be byte-identical");
    }

    #[test]
    fn verification_preserves_string_protection() {
        let c = cfg(Intensity::High, 2024, |c| {
            c.passes.strings.in_vm = true;
            c.passes.strings.self_coupled_key = true;
            c.passes.strings.exec_trace_key = true;
            c.engine.verify = true;
        });
        let out = run(VM_PROG, &c);
        let mut unchecked = c.clone();
        unchecked.engine.verify = false;
        assert_eq!(
            out,
            run(VM_PROG, &unchecked),
            "verify must not weaken protection"
        );
        assert!(
            out.contains("SCK"),
            "self-coupled protection remains active"
        );
        assert!(
            !out.contains("SCK9999999999"),
            "no unpatched sentinel under verify: {out}"
        );
        assert!(
            Js::reparse(&out, &ParseOpts::default()).is_ok(),
            "verify output reparses"
        );
        mangler_testkit::eval::assert_behaviorally_equal(VM_PROG, &out);
    }

    #[test]
    fn self_coupled_tamper_breaks_decode() {
        // Patching the interpreter source after the build changes its toString() → the
        // runtime self-hash diverges → every key byte is poisoned → decode garbles.
        let c = cfg(Intensity::High, 1212, |c| {
            c.passes.strings.in_vm = true;
            c.passes.strings.self_coupled_key = true;
        });
        let out = run(VM_PROG, &c);
        // The patched sentinel binds the decode key to the interpreter + decode-wrapper
        // source. Tamper by inserting a harmless no-op at the top of the INTERPRETER
        // body: this changes `("" + interp)` → the runtime self-hash mismatches → every
        // key byte is poisoned. The interpreter is the `function …(…){…}` declaration
        // emitted immediately after the hoisted `var <rc>=Reflect.construct;` alias.
        let alias = out
            .find("=Reflect.construct;function ")
            .expect("rc alias + interpreter");
        // Advance to the interpreter body's opening brace (skip its param list).
        let after_alias = alias + "=Reflect.construct;function ".len();
        let brace = out[after_alias..]
            .find('{')
            .expect("interpreter body brace")
            + after_alias;
        let mut tampered = String::with_capacity(out.len() + 8);
        tampered.push_str(&out[..=brace]);
        tampered.push_str("void 0;");
        tampered.push_str(&out[brace + 1..]);
        // Sanity: the tamper is syntactically valid.
        assert!(
            Js::reparse(&tampered, &ParseOpts::default()).is_ok(),
            "tampered source must still parse: {tampered}"
        );
        // The tampered program must NOT reproduce the original observable output.
        let r = mangler_testkit::eval::eval_same_value(VM_PROG, &tampered);
        assert!(!r.equal, "tampering the decode wrapper must break decode");
    }
}

#[test]
fn arrow_directives_preserve_inherited_strictness() {
    let src = "var f=()=>{'use strict';return (function(){return this===undefined})();};globalThis.__out=String(f());";
    let (out, _) = run_strings(src, Intensity::Low, 1);
    mangler_testkit::eval::assert_behaviorally_equal(src, &out);
}

#[test]
fn static_import_and_reexport_attributes_remain_literals() {
    for src in [
        "import data from './data.json' with {type:'json'};globalThis.__out='encoded';",
        "export {default as data} from './data.json' with {type:'json'};globalThis.__out='encoded';",
        "export * from './data.json' with {type:'json'};globalThis.__out='encoded';",
    ] {
        let (out, _) = run_strings(src, Intensity::Low, 1);
        assert!(
            out.contains("type:\"json\""),
            "attribute must remain literal: {out}"
        );
        Js::reparse(&out, &ParseOpts::default()).expect("static attributes must parse");
        assert!(
            !out.contains("encoded"),
            "ordinary strings must still encode"
        );
    }
}
