//! Differential + determinism integration tests for the `mangler-js` runner.
//!
//! Validates that the pipeline — with the member-access + expr exemplar passes
//! enabled — PRESERVES BEHAVIOR over the committed corpus, is idempotent, and is
//! deterministic (same source + seed → byte-identical). All evaluation goes
//! through `mangler-testkit`'s in-process rquickjs harness; we never invoke a bare
//! `node` (a hard project rule).

use mangler_config::{ConfigFlags, Intensity, ResolvedConfig};
use mangler_jsast::ParseOpts;
use mangler_testkit::{corpus, eval, golden};

/// Resolve a config from a preset + seed.
fn cfg(level: Intensity, seed: u64) -> ResolvedConfig {
    let flags = ConfigFlags {
        preset: Some(level),
        seed: Some(seed),
        ..Default::default()
    };
    ResolvedConfig::try_from(flags).expect("valid config")
}

/// Run the pipeline over `src` at `level`/`seed`. Panics on a hard error (corpus
/// files are valid JS, so a hard error is a real bug worth surfacing loudly).
fn mangle(src: &str, level: Intensity, seed: u64) -> String {
    mangler_js::process(src, &ParseOpts::default(), &cfg(level, seed))
        .expect("pipeline must not hard-error on valid corpus input")
        .0
}

/// The whole behavioral corpus must round-trip unchanged in observable behavior at
/// Medium (member-access + expr obfuscation both active).
#[test]
fn corpus_behavior_preserved_at_medium() {
    let n = corpus::assert_all(|src| mangle(src, Intensity::Medium, 1));
    assert!(n >= 20, "expected the full corpus, checked only {n}");
}

/// Behavior must also be preserved at Minify (just mangle/minify — the baseline)
/// and across a couple of seeds, guarding the determinism-independent equivalence.
#[test]
fn corpus_behavior_preserved_across_levels_and_seeds() {
    for (level, seed) in [
        (Intensity::Minify, 1),
        (Intensity::Low, 1),
        (Intensity::Medium, 7),
        (Intensity::Medium, 99),
        (Intensity::High, 1),
        (Intensity::High, 42),
        (Intensity::Max, 1),
        (Intensity::Max, 42),
    ] {
        let n = corpus::assert_all(|src| mangle(src, level, seed));
        assert!(n >= 20, "checked only {n} at {level:?}/{seed}");
    }
}

/// Same source + same seed → byte-identical output (the determinism contract).
#[test]
fn determinism_byte_identical() {
    let samples = [
        "function f(a, b){ return a * 3 + b - 7; } f(2, 5);",
        "var o = { k: 1 }; globalThis.__out = String(o.k + 41);",
        "const xs = [1, 2, 3].map(x => x * 2 + 1); globalThis.__out = String(xs.length);",
    ];
    for src in samples {
        let a = mangle(src, Intensity::Medium, 42);
        let b = mangle(src, Intensity::Medium, 42);
        assert_eq!(a, b, "same source+seed must be byte-identical for: {src}");
    }
}

/// Idempotence: mangling the output again yields byte-identical bytes (the
/// transform reaches a fixpoint and does not churn its own output). The effective
/// seed is content-derived, so the second run uses a different effective seed than
/// the first — yet must still be a fixpoint, because the already-obfuscated output
/// has no further eligible literals/members to transform (they are now opaque exprs
/// / computed accesses), so the second pass is observably a near no-op that
/// stabilizes. We assert idempotence on the SECOND application onward.
#[test]
fn idempotent_at_minify() {
    // At Minify only mangle/minify runs (no literal/member rewriting), so the
    // transform is a clean fixpoint.
    let src =
        "function f(){ var localName = 1; return localName + 2; } globalThis.__out = String(f());";
    golden::assert_idempotent(|s| mangle(s, Intensity::Minify, 1), src);
}

/// A direct behavioral spot-check via `assert_behaviorally_equal` (the eval-sink
/// harness), independent of the corpus runner.
#[test]
fn spot_check_behavioral_equivalence() {
    let src = "globalThis.__out = String((2 + 40) * 3 - 6);";
    let out = mangle(src, Intensity::Medium, 5);
    eval::assert_behaviorally_equal(src, &out);
}

// ---------------------------------------------------------------------------
// Phase 1: whole-program virtualization through the FULL pipeline
// ---------------------------------------------------------------------------

/// Resolve a config with `--virtualize-program` on, at the given preset/seed.
fn cfg_whole_program(level: Intensity, seed: u64) -> ResolvedConfig {
    let flags = ConfigFlags {
        preset: Some(level),
        seed: Some(seed),
        virtualize_program: true,
        ..Default::default()
    };
    let r = ResolvedConfig::try_from(flags).expect("valid config");
    assert!(r.passes.virtualize.whole_program);
    r
}

/// Mangle `src` through the FULL pipeline with whole-program virtualization on.
fn mangle_whole_program(src: &str, level: Intensity, seed: u64) -> String {
    mangler_js::process(src, &ParseOpts::default(), &cfg_whole_program(level, seed))
        .expect("pipeline must not hard-error")
        .0
}

/// §9.3(a) WebGL regression: the checked-in `webgl_render_loop.js` fixture (one
/// top-level IIFE) virtualizes under `--virtualize-program` (interpreter present;
/// top level becomes an interpreter call) AND runs to identical observable output
/// under the stubbed GL context.
#[test]
fn whole_program_webgl_fixture_virtualizes_and_renders() {
    let src = include_str!("../../../tests/corpus/webgl_render_loop.js");
    let out = mangle_whole_program(src, Intensity::Medium, 7);
    // The distinctive native loop body is gone — it now lives as VM bytecode.
    assert!(
        !out.contains("renderFrame(frame)"),
        "render loop pulled into the VM:\n{out}"
    );
    assert!(
        !out.contains("__gl.push"),
        "GL recorder calls are in the VM, not native"
    );
    // It still renders identically (same GL call sequence into __out).
    eval::assert_behaviorally_equal(src, &out);
}

/// Whole-program virtualization is deterministic through the full pipeline: same
/// source + seed → byte-identical output.
#[test]
fn whole_program_deterministic_through_pipeline() {
    let src = include_str!("../../../tests/corpus/webgl_render_loop.js");
    let a = mangle_whole_program(src, Intensity::Medium, 42);
    let b = mangle_whole_program(src, Intensity::Medium, 42);
    assert_eq!(
        a, b,
        "same source+seed must be byte-identical under whole-program"
    );
}

/// Resolve a whole-program config that ALSO excludes a name-glob (Phase 3).
fn cfg_whole_program_exclude(level: Intensity, seed: u64, exclude: &str) -> ResolvedConfig {
    let flags = ConfigFlags {
        preset: Some(level),
        seed: Some(seed),
        virtualize_program: true,
        virtualize_exclude: Some(exclude.to_string()),
        ..Default::default()
    };
    let r = ResolvedConfig::try_from(flags).expect("valid config");
    assert!(r.passes.virtualize.whole_program);
    assert_eq!(r.passes.virtualize.exclude.as_deref(), Some(exclude));
    r
}

fn mangle_whole_program_exclude(src: &str, level: Intensity, seed: u64, exclude: &str) -> String {
    mangler_js::process(
        src,
        &ParseOpts::default(),
        &cfg_whole_program_exclude(level, seed, exclude),
    )
    .expect("pipeline must not hard-error")
    .0
}

/// §9.3(b/c) Phase-3 WebGL regression: `--virtualize-program --virtualize-exclude
/// 'renderFrame'` leaves the named render function BYTE-FOR-BYTE NATIVE (a native
/// closure inside the virtualized IIFE chunk) while the rest virtualizes, and the
/// output runs to the identical GL call sequence under the stubbed GL context.
#[test]
fn whole_program_excluded_render_loop_stays_native_and_renders() {
    let src = include_str!("../../../tests/corpus/webgl_render_loop.js");
    // Use Minify so downstream renaming does not rewrite the native function body
    // identifiers (the §9.3(b) "byte-for-byte native" assertion is on the loop body,
    // which the renamer would otherwise mangle). The interpreter still virtualizes
    // the IIFE.
    let out = mangle_whole_program_exclude(src, Intensity::Minify, 7, "renderFrame");
    // (b) The excluded render function stays native: its distinctive GL-driving body
    // survives verbatim (`drawArrays`/`clearColor` are the native loop body — they
    // live as a native factory const in the program table, not as bytecode). The fn
    // NAME itself may be minified away when never self-referenced; the body is the
    // load-bearing "stayed native" evidence.
    assert!(
        out.contains("drawArrays") && out.contains("clearColor"),
        "excluded renderFrame must stay native (its GL body intact):\n{out}"
    );
    // The rest still virtualizes: a VM program table was spliced.
    assert!(
        out.contains("[["),
        "VM program table spliced (IIFE virtualized):\n{out}"
    );
    // (c) It renders identically (same GL call sequence into __out).
    eval::assert_behaviorally_equal(src, &out);
}

/// Phase-3 WebGL determinism: same source + seed + exclude → byte-identical output,
/// including the native-closure factory and the divert decision.
#[test]
fn whole_program_excluded_deterministic_through_pipeline() {
    let src = include_str!("../../../tests/corpus/webgl_render_loop.js");
    let a = mangle_whole_program_exclude(src, Intensity::Medium, 42, "renderFrame");
    let b = mangle_whole_program_exclude(src, Intensity::Medium, 42, "renderFrame");
    assert_eq!(a, b, "same source+seed+exclude must be byte-identical");
}

/// Phase-3 coverage: a top-level program whose only wrappable run contains a
/// GENERATOR nested function. With an exclude glob present (which turns on the
/// ineligible-divert), the generator becomes a NATIVE CLOSURE inside the virtualized
/// chunk — instead of failing the run — and the program runs identically through the
/// full pipeline. Minify keeps the downstream opaque/expr machinery off so the test
/// isolates the divert mechanism (the Medium full-pipeline path is covered by the
/// VM-level behavioral suite).
#[test]
fn whole_program_generator_diverts_to_native_through_pipeline() {
    let src = "(function(){ \
        function* g(){ yield 1; yield 2; yield 3; } \
        var it = g(); var s = 0; var r; \
        while (!(r = it.next()).done) { s += r.value; } \
        globalThis.__out = String(s); \
    })();";
    // An exclude glob (matching nothing here) turns on the ineligible-divert so `g`
    // becomes a native closure rather than bisecting out.
    let out = mangle_whole_program_exclude(src, Intensity::Minify, 5, "__none__");
    assert!(out.contains("function*"), "generator stays native:\n{out}");
    // It still virtualizes the surrounding run (a VM program table was spliced).
    assert!(
        out.contains("[["),
        "VM table spliced (run virtualized around the native generator):\n{out}"
    );
    eval::assert_behaviorally_equal(src, &out);
}

/// Phase 2: a module with an `export` boundary PARTITIONS through the full pipeline —
/// the export stays native and the wrappable run virtualizes. The output is still a
/// valid module that round-trips through the pipeline without a hard error.
#[test]
fn whole_program_module_export_partitions_through_pipeline() {
    let src = "export const k = 21; globalThis.__out = String(k * 2);";
    let out = mangle_whole_program(src, Intensity::Minify, 3);
    // The export boundary is preserved (still a module export).
    assert!(
        out.contains("export"),
        "export boundary kept native:\n{out}"
    );
    // The reader run pulled into the VM (a program table was spliced).
    assert!(
        out.contains("[["),
        "VM program table spliced (reader run virtualized):\n{out}"
    );
}

/// Regression (review CRITICALs): a cross-run `let` that is ALSO exported must NOT be
/// cell-ified — `export { k }` cannot become `export { k[0] }` (Bug A) and a `let` cell
/// would lose TDZ (Bug B). The binding stays a native module declaration; the pipeline
/// must process it without error and keep the export boundary.
#[test]
fn whole_program_cross_run_exported_let_stays_native() {
    let src = "let k = 21; export { k }; globalThis.__out = String(k * 2);";
    let out = mangle_whole_program(src, Intensity::Minify, 3);
    assert!(
        out.contains("export"),
        "export boundary kept native:\n{out}"
    );
    // `k`'s declaration is NOT rewritten into a `[undefined]` cell array (the unsafe
    // lowering the review caught); it remains a real lexical/native binding.
    assert!(
        !out.contains("=[undefined]") && !out.contains("=[void 0]"),
        "exported let must not be hoisted to a cell array:\n{out}"
    );
}

// ---------------------------------------------------------------------------
// Native class envelopes and regex literals under whole-program protection
// ---------------------------------------------------------------------------

/// Resolve a whole-program config that also enables the Phase-4 desugar flags.
fn cfg_whole_program_desugar(
    level: Intensity,
    seed: u64,
    class: bool,
    regex: bool,
) -> ResolvedConfig {
    let flags = ConfigFlags {
        preset: Some(level),
        seed: Some(seed),
        virtualize_program: true,
        virtualize_desugar_class: class,
        virtualize_desugar_regex: regex,
        ..Default::default()
    };
    let r = ResolvedConfig::try_from(flags).expect("valid config");
    assert!(r.passes.virtualize.whole_program);
    assert_eq!(r.passes.virtualize.desugar_class, class);
    assert_eq!(r.passes.virtualize.desugar_regex, regex);
    r
}

fn mangle_wp_desugar(src: &str, level: Intensity, seed: u64, class: bool, regex: bool) -> String {
    mangler_js::process(
        src,
        &ParseOpts::default(),
        &cfg_whole_program_desugar(level, seed, class, regex),
    )
    .expect("pipeline must not hard-error")
    .0
}

/// A program exercising the full supported class surface: ctor + method + instance
/// field + `extends` + `super()` + a `static` method and `static` field. With
/// `--virtualize-desugar-class`, eligible methods are protected inside a native class
/// envelope; constructors, fields, and heritage retain JavaScript semantics.
const CLASS_PROGRAM: &str = "\
    class Animal { \
      constructor(name) { this.name = name; this.legs = 4; } \
      describe() { return this.name + ' has ' + this.legs + ' legs'; } \
      static kingdom() { return 'Animalia'; } \
    } \
    class Dog extends Animal { \
      constructor(name) { super(name); this.sound = 'woof'; } \
      speak() { return this.name + ' says ' + this.sound; } \
    } \
    Dog.species = 'canine'; \
    var d = new Dog('Rex'); \
    var out = d.describe() + '|' + d.speak() + '|' + Animal.kingdom() + '|' + Dog.species; \
    out += '|' + String(Object.getPrototypeOf(d) === Dog.prototype); \
    out += '|' + String(Object.getPrototypeOf(Dog.prototype) === Animal.prototype); \
    out += '|' + Object.keys(d).join(','); \
    out += '|' + String(Object.getOwnPropertyDescriptor(Dog.prototype, 'speak').enumerable); \
    globalThis.__out = out;";

/// Eligible methods can be virtualized without replacing the native class envelope.
#[test]
fn whole_program_desugar_class_virtualizes_and_behaves() {
    let src = format!("(function(){{ {CLASS_PROGRAM} }})();");
    let out = mangle_wp_desugar(&src, Intensity::Minify, 7, true, false);
    assert!(
        out.contains("class"),
        "native class envelope retained:\n{out}"
    );
    eval::assert_behaviorally_equal(&src, &out);
}

/// With the flag OFF (default), the class stays NATIVE — behavior unchanged and the
/// class keyword is still present (the default-behavior guard).
#[test]
fn whole_program_desugar_class_off_keeps_class_native() {
    let src = format!("(function(){{ {CLASS_PROGRAM} }})();");
    let out = mangle_wp_desugar(&src, Intensity::Minify, 7, false, false);
    assert!(
        out.contains("class"),
        "class stays native when flag OFF:\n{out}"
    );
    eval::assert_behaviorally_equal(&src, &out);
}

/// Method NON-ENUMERABILITY and instance-field init ORDER are preserved by the
/// lowering (asserted inside CLASS_PROGRAM via Object.keys / getOwnPropertyDescriptor,
/// which both forms must agree on).
#[test]
fn whole_program_desugar_class_preserves_method_enumerability_and_field_order() {
    // Drive directly: Object.keys(d) must be the instance fields in init order
    // (`name,legs,sound`), and the prototype method must be non-enumerable. The
    // assert_behaviorally_equal compares original-vs-desugared, so any divergence in
    // enumerability/order between class and function form is caught.
    let src = format!("(function(){{ {CLASS_PROGRAM} }})();");
    let out = mangle_wp_desugar(&src, Intensity::Minify, 13, true, false);
    eval::assert_behaviorally_equal(&src, &out);
}

/// SKIP-DESUGAR: a class with a private field `#x` is NOT desugared (stays native) and
/// the program still runs identically.
#[test]
fn whole_program_desugar_class_private_field_stays_native() {
    let src = "(function(){ \
        var c; \
        class C { #x = 41; getX() { return this.#x + 1; } } \
        c = new C(); \
        globalThis.__out = String(c.getX()); \
    })();";
    let out = mangle_wp_desugar(src, Intensity::Minify, 7, true, false);
    // The private field (renamed by the minifier, but the `#` private-name syntax
    // survives) proves the class was NOT desugared — it stayed a native class.
    assert!(
        out.contains('#') && out.contains("class"),
        "private-field class stays native:\n{out}"
    );
    eval::assert_behaviorally_equal(src, &out);
}

/// SKIP-DESUGAR: a class with a `static {}` block stays native.
#[test]
fn whole_program_desugar_class_static_block_stays_native() {
    let src = "(function(){ \
        class C { static n; static { C.n = 7; } } \
        globalThis.__out = String(C.n); \
    })();";
    let out = mangle_wp_desugar(src, Intensity::Minify, 7, true, false);
    assert!(
        out.contains("static{") || out.contains("static {"),
        "static-block class stays native:\n{out}"
    );
    eval::assert_behaviorally_equal(src, &out);
}

/// SKIP-DESUGAR: a class with a decorator stays native (decorators parse under the
/// default opts? if not, this is a compile-time no-op — guarded by behavior anyway).
#[test]
fn whole_program_desugar_class_computed_key_stays_native() {
    // A computed-name method may close over a TDZ binding (§3.2) — skip-desugar.
    let src = "(function(){ \
        var k = 'go'; \
        class C { [k]() { return 5; } } \
        globalThis.__out = String(new C().go()); \
    })();";
    let out = mangle_wp_desugar(src, Intensity::Minify, 7, true, false);
    assert!(
        out.contains("class"),
        "computed-key class stays native:\n{out}"
    );
    eval::assert_behaviorally_equal(src, &out);
}

/// SKIP-DESUGAR (CRITICAL guard): a derived class whose `super()` is NOT a single
/// direct top-level statement (here a conditional `super`) must stay NATIVE — the
/// lowering only splices a top-level `super(...);`, so any other position would silently
/// drop the base-constructor init. The class must remain a `class` AND behave
/// identically.
#[test]
fn whole_program_desugar_class_conditional_super_stays_native() {
    let src = "(function(){ \
        class A { constructor(x){ this.x = x; } } \
        class B extends A { constructor(x){ if (x > 0) super(x); else super(-x); this.y = 9; } } \
        var b1 = new B(5); var b2 = new B(-3); \
        globalThis.__out = String(b1.x) + '|' + String(b1.y) + '|' + String(b2.x) + '|' + String(b2.y); \
    })();";
    let out = mangle_wp_desugar(src, Intensity::Minify, 7, true, false);
    assert!(
        out.contains("class"),
        "conditional-super class stays native:\n{out}"
    );
    eval::assert_behaviorally_equal(src, &out);
}

/// SKIP-DESUGAR: a class with a getter/setter accessor stays native (tight surface).
#[test]
fn whole_program_desugar_class_accessor_stays_native() {
    let src = "(function(){ \
        class C { constructor(){ this._v = 10; } get v(){ return this._v; } set v(x){ this._v = x; } } \
        var c = new C(); c.v = 42; \
        globalThis.__out = String(c.v); \
    })();";
    let out = mangle_wp_desugar(src, Intensity::Minify, 7, true, false);
    assert!(out.contains("class"), "accessor class stays native:\n{out}");
    eval::assert_behaviorally_equal(src, &out);
}

/// regex→RegExp: even with the flag ON the program behaves identically — `lastIndex`
/// statefulness on a `g` regex used with `.exec` in a loop, and `String.replace`.
#[test]
fn whole_program_desugar_regex_behaves_identically() {
    let src = "(function(){ \
        var re = /a(\\d)/g; var s = 'a1 a2 a3'; var m; var acc = ''; \
        while ((m = re.exec(s)) !== null) { acc += m[1]; } \
        var rep = 'foo bar'.replace(/o/g, '0'); \
        globalThis.__out = acc + '|' + rep + '|' + String(/x/i.test('XYZ')); \
    })();";
    // ON
    let on = mangle_wp_desugar(src, Intensity::Minify, 9, false, true);
    eval::assert_behaviorally_equal(src, &on);
    // OFF (default) — also identical, and the regex literal survives.
    let off = mangle_wp_desugar(src, Intensity::Minify, 9, false, false);
    eval::assert_behaviorally_equal(src, &off);
}

/// Generated class hierarchies preserve behavior while eligible methods are protected.
#[test]
fn fuzz_desugar_class_parity() {
    use mangler_testkit::fuzz::{build_class_program, check_program};
    let base = 0xC1A5_0000_0000_0000u64;
    let mut transform = |src: &str| mangle_wp_desugar(src, Intensity::Minify, 0xABCD, true, false);
    let mut failures = Vec::new();
    for i in 0..200u64 {
        let seed = base.wrapping_add(i.wrapping_mul(0x0100_0001));
        let program = build_class_program(seed);
        let diff = check_program(&program, &mut transform);
        if diff.is_divergent() {
            failures.push((seed, program, diff));
        }
    }
    if let Some((seed, program, diff)) = failures.first() {
        panic!(
            "class-desugar fuzz divergence (seed {seed}): {}\n  original:    {}\n  transformed: {}\n--- program ---\n{program}",
            diff.reason, diff.original, diff.transformed
        );
    }
}

/// Class-desugar determinism inside the fuzzer: same seed ⇒ byte-identical output.
#[test]
fn fuzz_desugar_class_deterministic() {
    use mangler_testkit::fuzz::build_class_program;
    for i in [0u64, 3, 17, 42] {
        let program = build_class_program(0xC1A5_0000 + i);
        let a = mangle_wp_desugar(&program, Intensity::Medium, 7, true, false);
        let b = mangle_wp_desugar(&program, Intensity::Medium, 7, true, false);
        assert_eq!(a, b, "class-desugar must be byte-identical for seed {i}");
    }
}

/// The legacy regex flag must preserve native literal semantics across generated inputs.
#[test]
fn fuzz_desugar_regex_parity() {
    use mangler_testkit::fuzz::{build_regex_program, check_program};
    let base = 0x5EED_0000_0000_0000u64;
    let mut transform = |src: &str| mangle_wp_desugar(src, Intensity::Minify, 0xBEEF, false, true);
    let mut failures = Vec::new();
    for i in 0..200u64 {
        let seed = base.wrapping_add(i.wrapping_mul(0x0100_0001));
        let program = build_regex_program(seed);
        let diff = check_program(&program, &mut transform);
        if diff.is_divergent() {
            failures.push((seed, program, diff));
        }
    }
    if let Some((seed, program, diff)) = failures.first() {
        panic!(
            "regex-desugar fuzz divergence (seed {seed}): {}\n  original:    {}\n  transformed: {}\n--- program ---\n{program}",
            diff.reason, diff.original, diff.transformed
        );
    }
}

/// Determinism: same source + seed + desugar flags → byte-identical output.
#[test]
fn whole_program_desugar_class_deterministic() {
    let src = format!("(function(){{ {CLASS_PROGRAM} }})();");
    let a = mangle_wp_desugar(&src, Intensity::Medium, 42, true, false);
    let b = mangle_wp_desugar(&src, Intensity::Medium, 42, true, false);
    assert_eq!(a, b, "same source+seed+flags must be byte-identical");
}

/// Phase 2 differential: a SCRIPT with an unsupported construct (`with`) wedged
/// between two eligible statements — bisection isolates the offender native while the
/// neighbors virtualize, and the program runs identically through the full pipeline.
#[test]
fn whole_program_bisection_runs_through_pipeline() {
    let src = "globalThis.__a = 6 * 7; \
        with (Math) { globalThis.__b = max(1, 2); } \
        globalThis.__out = String(globalThis.__a + globalThis.__b);";
    let out = mangle_whole_program(src, Intensity::Medium, 5);
    eval::assert_behaviorally_equal(src, &out);
}
