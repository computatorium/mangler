//! The per-file pipeline runner — reproduces the legacy `lang/js/mod.rs` flow via
//! the pass-graph model.
//!
//! [`process`] is the single entry point: parse → derive the effective seed →
//! build [`FileConfig`] → collect enabled passes (including the resolver and minify
//! pseudo-passes) → [`schedule_nodes`] → run loop → codegen → finalizers.
//!
//! # Why the whole flow runs inside ONE `GLOBALS` scope
//!
//! swc `Mark`s (the resolver output, consumed by the minifier) are only valid
//! within the `GLOBALS` scope they were allocated in. So the runner wraps the
//! resolve + all passes + codegen in a single [`Js::with_globals`]. The resolver is
//! modeled as a pass (`id() == "resolver"`) that calls [`Js::resolve`] and stores
//! the marks on the bus as a [`ResolvedScopesArtifact`]; later passes read them.
//!
//! # The resolver and minify pseudo-passes
//!
//! Two scheduler nodes are not ordinary AST passes:
//!
//! * **resolver** — runs `Js::resolve(ast)` and `bus.put(ResolvedScopesArtifact)`.
//!   It writes `ResolvedScopes`, so any pass reading it lands after the resolver —
//!   this is what produces the Pre/PostResolver split from the topological sort.
//! * **minify** — the terminal codegen. It *consumes* the `Ast` by value into
//!   [`Js::print_optimized`], so it can't be a `&mut Ast` pass; the runner detects
//!   it in the run loop and performs codegen as the final step, reading the
//!   [`MangleControlArtifact`] the `idnames` pass wrote.
//!
//! # Finalizers
//!
//! After codegen the runner patches self-coupled keys, applies the anti-tamper
//! [`wrap`](crate::selfdefend::wrap), then runs `--verify` against the exact final
//! artifact. These are not
//! scheduler passes because they operate on the emitted string, not the AST.

use crate::artifacts::{MangleControlArtifact, ResolvedScopesArtifact, SelfCoupledKeyArtifact};
use crate::config::FileConfig;
use crate::passes::{
    cfflatten::CfFlattenPass, deadcode::DeadCodePass, expr::ExprObfuscationPass,
    globalref::GlobalRefPass, idnames::IdNamesPass, memberaccess::MemberAccessPass,
    strings::StringsPass, virtualize::VirtualizePass,
};
use crate::{seed, selfdefend};
use mangler_config::ResolvedConfig;
use mangler_core::{Error, Language, Notes, PassConfig, Result, Rng};
use mangler_jsast::{Js, ParseOpts};
use mangler_passgraph::{ArtifactBus, Pass, PassNode, schedule_nodes};

/// The id of the resolver pseudo-pass.
const RESOLVER_ID: &str = "resolver";
/// The id of the terminal minify/codegen pseudo-pass.
const MINIFY_ID: &str = "minify";

/// Build the full set of AST passes (in registration order; the scheduler reorders
/// them by declared reads/writes). **Follow-up agents append their pass here.**
///
/// Does NOT include the resolver or minify pseudo-passes — those are injected into
/// the schedule by the runner (they need special handling: marks / by-value Ast).
pub fn register_passes() -> Vec<Box<dyn Pass<Js, FileConfig>>> {
    // Registration order is NOT execution order — the scheduler derives execution
    // order from each pass's declared reads/writes (the resolver is a pseudo-pass).
    // Listed here roughly in pipeline order for readability only.
    vec![
        Box::new(MemberAccessPass),
        Box::new(GlobalRefPass),
        Box::new(StringsPass),
        Box::new(VirtualizePass),
        Box::new(ExprObfuscationPass),
        Box::new(CfFlattenPass),
        Box::new(DeadCodePass),
        Box::new(IdNamesPass),
    ]
}

/// Process JS/TS `src` into minified, obfuscated output, returning the output and
/// any non-fatal [`Notes`] the passes produced.
///
/// `opts` selects the dialect (TS/JSX/module); `cfg` is the validated obfuscation
/// configuration. Determinism: same `src` + same `cfg.engine.seed` →
/// byte-identical output.
pub fn process(src: &str, opts: &ParseOpts, cfg: &ResolvedConfig) -> Result<(String, Notes)> {
    // Parse outside GLOBALS (a pure parse needs none) so the fingerprint reads the
    // unmutated tree.
    let mut ast = Js.parse(src, opts)?;
    let (eff_seed, reserved_idents) =
        seed::effective_seed_and_idents(ast.program(), cfg.engine.seed);

    let compiler_sites =
        if cfg.passes.virtualize.whole_program || cfg.passes.virtualize.target.is_some() {
            crate::passes::virtualize::source_compiler_sites(ast.program_mut())
        } else {
            Default::default()
        };
    let file_cfg = FileConfig::new(cfg.clone(), eff_seed, reserved_idents)
        .with_source_functions(crate::passes::virtualize::source_functions(ast.program()))
        .with_source_compiler_sites(compiler_sites);
    let class_contexts =
        if cfg.passes.virtualize.whole_program || cfg.passes.virtualize.target.is_some() {
            crate::passes::virtualize::eval_class_contexts(ast.program(), &file_cfg, None)
        } else {
            Default::default()
        };
    let file_cfg = file_cfg.with_eval_class_contexts(class_contexts);

    // Collect the enabled AST passes, plus the resolver + minify pseudo-pass nodes.
    let passes = register_passes();
    let mut nodes: Vec<PassNode> = passes
        .iter()
        .filter(|p| p.enabled(&file_cfg))
        .map(|p| {
            // Augment ONLY the scheduling edges with ordering-only resources (the
            // pass's real reads()/writes() — and thus the bus contract and the
            // pass's own tests — are untouched; these custom resources are never
            // get()/put() at runtime). This pins the post-resolver sequence the
            // scheduler cannot otherwise derive (no data dependency carries it).
            let (reads, writes) = augment_order(p.id(), p.reads(), p.writes());
            PassNode::new(p.id(), &reads, &writes)
        })
        .collect();
    nodes.push(resolver_node());
    // NOTE: minify/codegen is deliberately NOT a scheduled node. It is the terminal
    // step that must run after EVERY pass (it also consumes the `Ast` by value into
    // the swc emitter). A schedulable "minify pass" can only declare reads of what
    // *other* passes write — but expr/deadcode/cfflatten write nothing, so the
    // topological tie-break could legally place minify before them and they'd be
    // skipped. So the runner runs codegen unconditionally once the schedule drains.

    let order = schedule_nodes(&nodes).map_err(|e| Error::config(e.to_string()))?;

    let mut notes = Notes::new();

    // The whole resolve + pass + codegen flow shares one GLOBALS scope so all marks
    // interoperate. The closure also surfaces the optional self-coupled-key
    // interpreter name (the strings pass `put`s it on the bus when `--self-coupled-key`
    // is active) so the POST-codegen finalizer below can patch the source-hash sentinel.
    let (output, self_coupled_interp) =
        Js::with_globals(|| -> Result<(String, Option<String>)> {
            let mut ast = ast;
            let mut bus = ArtifactBus::new();
            let mut marks: Option<(swc_core::common::Mark, swc_core::common::Mark)> = None;

            for node in &order {
                match node.id {
                    RESOLVER_ID => {
                        bus.enter_pass(RESOLVER_ID, &node.reads, &node.writes);
                        let (unresolved, top_level) = Js::resolve(&mut ast);
                        marks = Some((unresolved, top_level));
                        bus.put(ResolvedScopesArtifact {
                            unresolved_mark: unresolved,
                            top_level_mark: top_level,
                        })
                        .map_err(|e| Error::transform(RESOLVER_ID, e.to_string()))?;
                    }
                    _ => {
                        let pass = passes
                            .iter()
                            .find(|p| p.id() == node.id)
                            .expect("scheduled node has a registered pass");
                        let mut rng = Rng::for_pass(file_cfg.seed(), pass.id());
                        bus.enter_pass(pass.id(), pass.reads(), pass.writes());
                        pass.run(&mut ast, &file_cfg, &mut rng, &mut bus, &mut notes)?;
                    }
                }
            }

            // Surface the self-coupled-key interpreter name (if the strings pass put one)
            // BEFORE codegen consumes the AST, under a reader scope that declares the
            // resource the artifact rides on (`decoder_anchor`).
            bus.enter_pass(
                "self-coupled-key-reader",
                &[mangler_passgraph::Resource::decoder_anchor()],
                &[],
            );
            let self_coupled_interp = bus
                .get::<SelfCoupledKeyArtifact>()
                .map_err(|e| Error::transform("strings", e.to_string()))?
                .map(|a| a.interp_name.clone());

            // -- Terminal minify/codegen (always runs last, after every pass) --
            // Scope the bus to the codegen step so its reads are contract-valid.
            bus.enter_pass(
                MINIFY_ID,
                &[
                    mangler_passgraph::Resource::mangle_control(),
                    mangler_passgraph::Resource::resolved_scopes(),
                ],
                &[],
            );
            let (unresolved, top_level) =
                marks.expect("the resolver pseudo-pass is always scheduled, so marks are set");
            let control = bus
                .get::<MangleControlArtifact>()
                .map_err(|e| Error::transform(MINIFY_ID, e.to_string()))?
                .cloned()
                .unwrap_or_default();
            // swc mangle runs unless the idnames pass suppressed it.
            let mangle = cfg.passes.mangle.enabled && !control.suppress_builtin_mangle;
            let reserved = if control.reserved.is_empty() {
                cfg.engine.keep_names.clone()
            } else {
                control.reserved.clone()
            };
            Ok((
                Js::print_optimized(ast, (unresolved, top_level), mangle, &reserved),
                self_coupled_interp,
            ))
        })?;

    // -- Post-codegen finalizers (operate on the STRING, not the AST) --

    let eff_seed = file_cfg.seed();
    let anti = &cfg.passes.anti_tamper;
    // Patch interpreter hashes before anti-tamper computes final function-source
    // checksums. Verification never changes either finalizer's behavior.
    let output = match self_coupled_interp {
        Some(interp_name) => {
            crate::passes::strings::stub::patch_self_coupled_expected(output, &interp_name)?
        }
        None => output,
    };

    let output = selfdefend::wrap(output, anti, eff_seed, opts)?;

    if cfg.engine.verify {
        // Re-parse the certified output to catch a pass that emitted malformed code.
        Js::reparse(&output, opts)
            .map_err(|e| Error::verify(format!("mangled output failed to re-parse: {e}")))?;
    }

    Ok((output, notes))
}

/// Ordering-only resources that pin the POST-resolver pass sequence
/// `expr → cfflatten → deadcode → idnames`. This order carries NO data dependency
/// the scheduler could derive, yet it is load-bearing (legacy semantics):
/// * **expr before cfflatten** — else cfflatten would re-wrap expr's already-opaque
///   constants, multiplying output size on flatten-heavy input.
/// * **deadcode after cfflatten** — deadcode prepends opaque-guarded dead branches;
///   running it before cfflatten would feed those branches into cfflatten's CFG
///   construction and corrupt the flattened body (an injected `var` can get
///   separated from a live use → a runtime `ReferenceError`).
/// * **idnames last** — it renames every local, so all injected locals must exist.
///
/// These are appended only to the SCHEDULING view (the [`PassNode`]); the passes
/// never `get`/`put` them, so the bus contract and the passes' own declared
/// reads/writes are unchanged. A missing writer (a disabled pass) simply drops the
/// edge — the sequence degrades gracefully to whatever subset is enabled.
fn augment_order(
    id: &str,
    reads: &[mangler_passgraph::Resource],
    writes: &[mangler_passgraph::Resource],
) -> (
    Vec<mangler_passgraph::Resource>,
    Vec<mangler_passgraph::Resource>,
) {
    use mangler_passgraph::Resource;
    const EXPR_DONE: Resource = Resource::Custom("js::ord/expr-obfuscated");
    const CF_DONE: Resource = Resource::Custom("js::ord/cf-flattened");
    const DC_DONE: Resource = Resource::Custom("js::ord/dead-injected");
    let mut r = reads.to_vec();
    let mut w = writes.to_vec();
    match id {
        "expr" => {
            r.push(Resource::resolved_scopes());
            w.push(EXPR_DONE);
        }
        "cfflatten" => {
            r.push(EXPR_DONE);
            w.push(CF_DONE);
        }
        "deadcode" => {
            r.push(Resource::resolved_scopes());
            r.push(CF_DONE);
            w.push(DC_DONE);
        }
        "idnames" => {
            r.push(DC_DONE);
        }
        _ => {}
    }
    (r, w)
}

/// The resolver pseudo-pass node. It READS every resource the PRE-resolver
/// (node-injecting) passes write — `PropertyLiterals` (member-access),
/// `GlobalNameLiterals` (globalref), `DecoderAnchor` (strings), `VmTable`
/// (virtualize) — so the topological sort places the resolver AFTER them; their
/// spliced nodes then receive fresh marks from this single resolver run. It WRITES
/// `ResolvedScopes`, so POST-resolver passes (which read it — cfflatten, idnames)
/// land after. This read/write pair IS the Pre/PostResolver split, derived rather
/// than hardcoded. (Declaring a read with no writer at a given level is a no-op —
/// it simply adds no incoming edge.)
fn resolver_node() -> PassNode {
    use mangler_passgraph::Resource;
    const READS: &[Resource] = &[
        Resource::property_literals(),
        Resource::global_name_literals(),
        Resource::decoder_anchor(),
        Resource::vm_table(),
    ];
    const WRITES: &[Resource] = &[Resource::resolved_scopes()];
    PassNode::new(RESOLVER_ID, READS, WRITES)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mangler_config::{ConfigFlags, Intensity};

    fn cfg(level: Intensity, seed: u64) -> ResolvedConfig {
        let flags = ConfigFlags {
            preset: Some(level),
            seed: Some(seed),
            ..Default::default()
        };
        ResolvedConfig::try_from(flags).expect("valid config")
    }

    fn run(src: &str, level: Intensity, seed: u64) -> String {
        process(src, &ParseOpts::default(), &cfg(level, seed))
            .expect("process failed")
            .0
    }

    #[test]
    fn strips_comments_and_minifies() {
        let out = run("// hi\nconst x = 1 + 2;\n", Intensity::Minify, 1);
        assert!(!out.contains("hi"));
        assert!(!out.contains('\n') || out.trim() == out);
    }

    #[test]
    fn renames_locals_but_not_globals() {
        let src =
            "function f(){ var localVariable = 5; return localVariable + window.GLOBAL_THING; }";
        let out = run(src, Intensity::Minify, 1);
        assert!(!out.contains("localVariable"), "local renamed: {out}");
        assert!(out.contains("GLOBAL_THING"), "global preserved: {out}");
    }

    #[test]
    fn determinism_same_seed_byte_identical() {
        let src = "function f(a){ return a * 3 + 7; } f(2);";
        let a = run(src, Intensity::Medium, 42);
        let b = run(src, Intensity::Medium, 42);
        assert_eq!(a, b, "same source+seed must be byte-identical");
    }

    #[test]
    fn different_seeds_diverge() {
        let src = "function f(a){ return a * 3 + 7; } f(2);";
        let a = run(src, Intensity::Medium, 1);
        let b = run(src, Intensity::Medium, 2);
        // Effective seed is fingerprint-mixed, so different user seeds → different
        // output (overwhelmingly; a collision would be astronomically unlikely).
        assert_ne!(a, b, "different seeds should diverge");
    }

    #[test]
    fn parse_error_is_an_error_not_panic() {
        let err = process(
            "function (",
            &ParseOpts::default(),
            &cfg(Intensity::Minify, 1),
        )
        .unwrap_err();
        assert!(matches!(err, Error::Parse { .. }));
    }

    #[test]
    fn verify_accepts_valid_output() {
        let mut c = cfg(Intensity::High, 7);
        c.engine.verify = true;
        let out = process(
            "function f(){ var a = 1; return a + 2; } f();",
            &ParseOpts::default(),
            &c,
        );
        assert!(out.is_ok(), "verify must accept valid output: {out:?}");
    }

    #[test]
    fn verify_preserves_the_exact_hardened_artifact() {
        let src = "function pay(x){return 'paid:'+x;}globalThis.__out=pay(42);";
        let mut config = cfg(Intensity::Max, 42);
        config.passes.strings.exec_trace_key = true;
        config.passes.strings.self_coupled_key = true;
        config.engine.verify = false;
        let original = process(src, &ParseOpts::default(), &config).unwrap().0;
        config.engine.verify = true;
        let verified = process(src, &ParseOpts::default(), &config).unwrap().0;
        assert_eq!(original, verified);
        mangler_testkit::assert_behaviorally_equal(src, &verified);
    }

    #[test]
    fn minification_preserves_function_strict_receivers_and_stores() {
        for src in [
            "function pay(){'use strict';return this===undefined;}globalThis.__out=pay();",
            "function pay(){'use strict';var obj=Object.freeze({x:1});try{obj.x=2;return false;}catch(e){return e instanceof TypeError;}}globalThis.__out=pay();",
        ] {
            let out = run(src, Intensity::Minify, 1);
            assert!(
                out.contains("use strict"),
                "strict directive disappeared: {out}"
            );
            mangler_testkit::assert_behaviorally_equal(src, &out);
        }
    }

    #[test]
    fn short_names_respect_keep_globs() {
        let src = "function f(x){var publicTotal=x+1,privateTotal=x+2;sink(publicTotal,privateTotal);return publicTotal;}f(window.q);";
        let mut config = cfg(Intensity::Minify, 7);
        config.engine.keep_names = vec!["public*".into()];
        config.passes.mangle.keep_names = config.engine.keep_names.clone();
        let out = process(src, &ParseOpts::default(), &config).unwrap().0;
        assert!(out.contains("publicTotal"), "{out}");
        assert!(!out.contains("privateTotal"), "{out}");
    }
    #[test]
    fn all_presets_preserve_observable_initializers_and_strict_receivers() {
        let sources = [
            "function f(){'use strict';return this===undefined};console.log(f())",
            "function pay(){let s=[];let k={[Symbol.toPrimitive](){s.push('key');return 'x'}};let o={[k]:(s.push('value'),1)};return s};console.log(JSON.stringify(pay()))",
            "let C='outer';try{let X=class C extends C{}}catch(e){console.log(e.name)}",
        ];
        for level in [
            Intensity::Minify,
            Intensity::Low,
            Intensity::Medium,
            Intensity::High,
            Intensity::Max,
        ] {
            for src in sources {
                let out = run(src, level, 1);
                mangler_testkit::assert_behaviorally_equal(src, &out);
            }
        }
    }
}
