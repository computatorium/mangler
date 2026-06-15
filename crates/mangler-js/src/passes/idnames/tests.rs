//! Unit + differential tests for the confusing local-identifier renamer.
//!
//! Each test builds a MINIMAL pipeline directly (NOT via `register_passes`):
//! parse → `Js::resolve` (so the marks exist) → put a `ResolvedScopesArtifact`
//! → run `IdNamesPass::run` → print. The whole thing runs inside one
//! `Js::with_globals` scope so every mark shares an interner. Behavioral
//! equivalence is checked with `assert_behaviorally_equal`; soundness facts
//! (locals renamed, globals/top-level/labels preserved, keep-names respected,
//! the correct `MangleControlArtifact`, determinism) are asserted on the printed
//! output / the bus.

use super::IdNamesPass;
use crate::artifacts::{MangleControlArtifact, ResolvedScopesArtifact};
use crate::config::FileConfig;
use mangler_config::{IdNaming, Intensity, ResolvedConfig};
use mangler_core::{Language, Notes, Rng};
use mangler_jsast::{Js, ParseOpts};
use mangler_passgraph::{ArtifactBus, Pass, Resource};
use std::collections::HashSet;

/// Build a resolved config at `Intensity::High` (Hex default) and override the
/// naming scheme + keep-names.
fn config(naming: IdNaming, keep: &[&str]) -> ResolvedConfig {
    let mut r = crate::test_support::resolved(Intensity::High, 7);
    r.passes.mangle.enabled = true;
    r.passes.mangle.naming = naming;
    r.passes.mangle.keep_names = keep.iter().map(|s| s.to_string()).collect();
    r
}

/// Outcome of running ONLY the idnames pass over `src`: the printed (un-minified)
/// program plus the `MangleControlArtifact` the pass wrote.
struct Run {
    out: String,
    control: MangleControlArtifact,
}

/// Run the minimal idnames-only pipeline at `seed` with `resolved`.
fn run_idnames(src: &str, resolved: ResolvedConfig, seed: u64) -> Run {
    Js::with_globals(|| {
        let mut ast = Js.parse(src, &ParseOpts::default()).expect("parse");
        let (unresolved_mark, top_level_mark) = Js::resolve(&mut ast);

        let cfg = FileConfig::new(resolved, seed, HashSet::new());
        let mut bus = ArtifactBus::new();

        // Seed the bus with the resolver marks (the resolver pseudo-pass).
        bus.enter_pass("resolver", &[], &[Resource::resolved_scopes()]);
        bus.put(ResolvedScopesArtifact {
            unresolved_mark,
            top_level_mark,
        })
        .expect("put marks");

        // Run the pass under test, scoped to its declared reads/writes.
        let pass = IdNamesPass;
        let mut rng = Rng::for_pass(seed, pass.id());
        let mut notes = Notes::default();
        bus.enter_pass(pass.id(), pass.reads(), pass.writes());
        pass.run(&mut ast, &cfg, &mut rng, &mut bus, &mut notes)
            .expect("idnames run");

        // Read the artifact back under a reader scope (as the terminal codegen
        // does), so the bus contract is satisfied.
        bus.enter_pass("minify", &[Resource::mangle_control()], &[]);
        let control = bus
            .get::<MangleControlArtifact>()
            .expect("get control")
            .cloned()
            .unwrap_or_default();

        Run {
            out: Js.print(&ast),
            control,
        }
    })
}

// ── pass contract ───────────────────────────────────────────────────────────

#[test]
fn pass_id_reads_writes() {
    let p = IdNamesPass;
    assert_eq!(p.id(), "idnames");
    assert_eq!(p.reads(), &[Resource::resolved_scopes()]);
    assert_eq!(p.writes(), &[Resource::mangle_control()]);
}

#[test]
fn enabled_tracks_mangle_knob() {
    let on = FileConfig::new(config(IdNaming::Hex, &[]), 1, HashSet::new());
    let mut off_cfg = config(IdNaming::Hex, &[]);
    off_cfg.passes.mangle.enabled = false;
    let off = FileConfig::new(off_cfg, 1, HashSet::new());
    assert!(IdNamesPass.enabled(&on));
    assert!(!IdNamesPass.enabled(&off));
}

// ── Renaming + preservation ─────────────────────────────────────────────────

#[test]
fn hex_renames_locals_and_emits_hex_names() {
    let src = "window.handler = function(incomingValue){ var doubledLocal = incomingValue + incomingValue; return doubledLocal + 1; };";
    let r = run_idnames(src, config(IdNaming::Hex, &[]), 7);
    assert!(
        !r.out.contains("incomingValue"),
        "local param must be renamed: {}",
        r.out
    );
    assert!(
        !r.out.contains("doubledLocal"),
        "local var must be renamed: {}",
        r.out
    );
    assert!(r.out.contains("_0x"), "Hex scheme must emit _0x names: {}", r.out);
    assert!(
        r.control.suppress_builtin_mangle,
        "Hex must suppress swc mangle"
    );
    assert_behaviorally_equal_runtime(src, &r.out);
}

#[test]
fn globals_and_globalthis_are_never_renamed() {
    // `window`/`GLOBAL` are free globals (unresolved mark) — never local — so
    // they must survive verbatim even though the local `localPi` is renamed.
    let src = "window.GLOBAL = function(){ var localPi = 3.14; return localPi; };";
    let r = run_idnames(src, config(IdNaming::Hex, &[]), 7);
    assert!(r.out.contains("window"), "free global window preserved: {}", r.out);
    assert!(r.out.contains("GLOBAL"), "top-level prop GLOBAL preserved: {}", r.out);
    assert!(!r.out.contains("localPi"), "local renamed: {}", r.out);
}

#[test]
fn top_level_bindings_are_never_renamed() {
    // A top-level `var topThing` carries top_level_mark → never renamed; the
    // nested local `innerLocal` is renamed.
    let src = "var topThing = 1; function f(){ var innerLocal = topThing + 1; return innerLocal; }";
    let r = run_idnames(src, config(IdNaming::Hex, &[]), 7);
    assert!(r.out.contains("topThing"), "top-level binding preserved: {}", r.out);
    assert!(!r.out.contains("innerLocal"), "nested local renamed: {}", r.out);
}

#[test]
fn labels_are_not_renamed() {
    let src = "function r(n){ var acc = 0; myLabel: for (var i = 0; i < n; i++) { for (var j = 0; j < n; j++) { if (j > i) continue myLabel; acc += 1; } } return acc; } r(2);";
    let r = run_idnames(src, config(IdNaming::Hex, &[]), 7);
    assert!(r.out.contains("myLabel"), "label preserved verbatim: {}", r.out);
    assert!(!r.out.contains("continue _0x"), "label ref not renamed: {}", r.out);
}

// ── keep-names ──────────────────────────────────────────────────────────────

#[test]
fn keep_names_preserves_matching_local() {
    let src = "function f(){ var keepMe = 1, dropMe = 2; return keepMe + dropMe; } f();";
    let r = run_idnames(src, config(IdNaming::Hex, &["keepMe"]), 7);
    assert!(r.out.contains("keepMe"), "kept local preserved: {}", r.out);
    assert!(!r.out.contains("dropMe"), "non-kept local renamed: {}", r.out);
    assert_eq!(
        r.control.reserved,
        vec!["keepMe".to_string()],
        "keep-names land in MangleControl.reserved"
    );
}

#[test]
fn keep_names_glob_preserves_matching_locals() {
    let src = "function f(){ var initThing = 1, initOther = 2, dropMe = 3; return initThing + initOther + dropMe; } f();";
    let r = run_idnames(src, config(IdNaming::Hex, &["init*"]), 7);
    assert!(r.out.contains("initThing"), "init* preserved initThing: {}", r.out);
    assert!(r.out.contains("initOther"), "init* preserved initOther: {}", r.out);
    assert!(!r.out.contains("dropMe"), "non-matching local renamed: {}", r.out);
}

// ── eval / with bail (no confusing names, swc mangle fallback) ───────────────

#[test]
fn eval_disables_confusing_scheme() {
    let src = "function f(paramLocal){ eval(\"0\"); return paramLocal + paramLocal; } f(2);";
    let r = run_idnames(src, config(IdNaming::Hex, &[]), 7);
    assert!(!r.out.contains("_0x"), "eval disables Hex (no _0x): {}", r.out);
    assert!(r.out.contains("paramLocal"), "local left as-is on bail: {}", r.out);
    assert!(
        !r.control.suppress_builtin_mangle,
        "eval bail must leave swc mangle on"
    );
}

#[test]
fn with_disables_confusing_scheme() {
    let src = "function f(o){ with(o){ return x + x; } } f({x:1});";
    let r = run_idnames(src, config(IdNaming::Hex, &[]), 7);
    assert!(!r.out.contains("_0x"), "with disables Hex (no _0x): {}", r.out);
    assert!(
        !r.control.suppress_builtin_mangle,
        "with bail must leave swc mangle on"
    );
}

// ── scheme gate ─────────────────────────────────────────────────────────────

#[test]
fn short_scheme_renames_nothing_and_defers_to_swc() {
    let src = "window.h = function(incomingValue){ return incomingValue + incomingValue + 1; };";
    let r = run_idnames(src, config(IdNaming::Short, &[]), 7);
    assert!(!r.out.contains("_0x"), "Short emits no _0x: {}", r.out);
    assert!(
        r.out.contains("incomingValue"),
        "Short leaves the local for swc mangle: {}",
        r.out
    );
    assert!(
        !r.control.suppress_builtin_mangle,
        "Short must NOT suppress swc mangle"
    );
}

#[test]
fn short_scheme_still_carries_keep_names_to_reserved() {
    let src = "function f(){ var keepMe = 1; return keepMe; } f();";
    let r = run_idnames(src, config(IdNaming::Short, &["keepMe"]), 7);
    assert_eq!(
        r.control.reserved,
        vec!["keepMe".to_string()],
        "keep-names still reserved on the Short fallback path"
    );
}

#[test]
fn soup_scheme_emits_homoglyph_names_not_hex() {
    let src = "function f(){ var someLongLocalName = 5; return someLongLocalName * 2; } f();";
    let r = run_idnames(src, config(IdNaming::Soup, &[]), 7);
    assert!(
        !r.out.contains("someLongLocalName"),
        "soup renamed the local: {}",
        r.out
    );
    assert!(!r.out.contains("_0x"), "soup must not emit _0x names: {}", r.out);
    assert!(
        r.control.suppress_builtin_mangle,
        "Soup must suppress swc mangle"
    );
    assert_behaviorally_equal_runtime(src, &r.out);
}

// ── collision avoidance ─────────────────────────────────────────────────────

#[test]
fn generated_name_never_captures_a_hexlike_global() {
    // A top-level `_0x1` must never be shadowed by a generated local name: if a
    // local renamed to `_0x1` it would shadow the global inside `f`, reading the
    // param instead of `7` and changing the result.
    let src = "var _0x1 = 7; function f(p) { var q = p + _0x1; return q; } String(f(3));";
    let r = run_idnames(src, config(IdNaming::Hex, &[]), 7);
    assert!(r.out.contains("_0x1"), "the global _0x1 survives: {}", r.out);
    assert_behaviorally_equal_runtime(src, &r.out);
}

// ── determinism ─────────────────────────────────────────────────────────────

const REPRO_SRC: &str =
    "function f(longLocalName){ var t = longLocalName * 2; return t + longLocalName; } f(3);";

#[test]
fn hex_same_seed_identical() {
    let a = run_idnames(REPRO_SRC, config(IdNaming::Hex, &[]), 4242);
    let b = run_idnames(REPRO_SRC, config(IdNaming::Hex, &[]), 4242);
    assert_eq!(a.out, b.out, "Hex output must be reproducible");
}

#[test]
fn soup_same_seed_identical() {
    let a = run_idnames(REPRO_SRC, config(IdNaming::Soup, &[]), 4242);
    let b = run_idnames(REPRO_SRC, config(IdNaming::Soup, &[]), 4242);
    assert_eq!(a.out, b.out, "Soup output must be reproducible");
}

#[test]
fn soup_different_seed_differs() {
    let a = run_idnames(REPRO_SRC, config(IdNaming::Soup, &[]), 1);
    let b = run_idnames(REPRO_SRC, config(IdNaming::Soup, &[]), 2);
    assert_ne!(a.out, b.out, "different seeds must produce different soup names");
}

// ── rich behavioral round-trips ─────────────────────────────────────────────

/// A rich program exercising every local-binding form the renamer must handle.
const RICH: &str = r#"
    function run() {
        var acc = 0;
        for (var i = 0; i < 6; i++) { acc += i; }

        var cfg = { width: 4, height: 3 };
        var { width, height } = cfg;
        var { depth = 2, width: w2 } = cfg;

        var list = [10, 20, 30, 40];
        var [first, second, ...others] = list;

        function scale(width) { return width * 100; }
        var scaled = scale(width);

        var counter = 0;
        function bump() { counter = counter + 1; return counter; }
        bump();
        bump();

        var hits = 0;
        outer: for (var a = 0; a < 4; a++) {
            for (var b = 0; b < 4; b++) {
                if (b === 1) continue outer;
                if (a === 2) break outer;
                hits++;
            }
        }

        var box = {
            v: width, w: height, total: width + height,
            area: function () { return this.total * 2; },
            label(n) { return this.total + n; }
        };

        var payload = {
            acc: acc, width: width, height: height, depth: depth, w2: w2,
            first: first, second: second, others: others.length,
            scaled: scaled, counter: counter, hits: hits,
            boxArea: box.area(), boxLabel: box.label(3),
            m: Math.max(acc, scaled), n: parseInt("21", 10)
        };
        return JSON.stringify(payload);
    }
    String(run());
"#;

#[test]
fn rich_round_trips_hex() {
    let r = run_idnames(RICH, config(IdNaming::Hex, &[]), 31337);
    assert_behaviorally_equal_runtime(RICH, &r.out);
}

#[test]
fn rich_round_trips_soup() {
    let r = run_idnames(RICH, config(IdNaming::Soup, &[]), 99);
    assert_behaviorally_equal_runtime(RICH, &r.out);
}

#[test]
fn tricky_binding_forms_round_trip() {
    let src = r#"
        function run() {
            var fact = function factSelf(n) {
                return n <= 1 ? 1 : n * factSelf(n - 1);
            };
            function withDefaults(base, dbl = base * 2, sum = base + dbl) {
                return base + dbl + sum;
            }
            var caught = "none";
            try {
                throw new Error("boom");
            } catch (errLocal) {
                caught = errLocal.message;
            }
            return JSON.stringify({
                f: fact(5),
                d: withDefaults(3),
                caught: caught
            });
        }
        String(run());
    "#;
    for naming in [IdNaming::Hex, IdNaming::Soup] {
        let r = run_idnames(src, config(naming, &[]), 7);
        assert_behaviorally_equal_runtime(src, &r.out);
    }
}

#[test]
fn object_literal_shorthand_round_trips() {
    let src = r#"
        function f() {
            var v = 5, w = 9;
            var o = { v, w, total: v + w };
            return String(o.v + o.w + o.total);
        }
        f();
    "#;
    let r = run_idnames(src, config(IdNaming::Hex, &[]), 7);
    assert_behaviorally_equal_runtime(src, &r.out);
}

/// `assert_behaviorally_equal` evaluates both programs and compares their
/// completion values; our snippets end in a completion expression (`f()` /
/// `String(run())`), so this proves the rename preserved semantics.
fn assert_behaviorally_equal_runtime(original: &str, transformed: &str) {
    mangler_testkit::eval::assert_behaviorally_equal(original, transformed);
}
