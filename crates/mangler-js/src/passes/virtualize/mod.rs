//! Compile source function bodies and top-level runs into a shared diversified VM.
//!
//! Native envelopes retain callable reflection and lexical host protocols; source
//! parameters, body control flow and expressions execute as bytecode.
//! Source identities are captured before lowering, so generated helpers never count
//! as protection and required coverage fails when a source function remains native.
//! Suspension state machines and class lexical bridges expose host protocols without
//! treating the original business logic as an opaque native escape.

use crate::artifacts::VmTableArtifact;
use crate::config::FileConfig;
use mangler_core::Language;
use mangler_core::{Notes, Result, Rng};
use mangler_jsast::lang::{Js, ParseOpts};
use mangler_passgraph::{ArtifactBus, Pass, Resource};
use mangler_vm::{
    Chunk, CompileOptions, Eligibility, TableBuilder, VmNames, classify_body,
    compile_body_with_opts,
};
use swc_core::common::{DUMMY_SP, Spanned};
use swc_core::ecma::ast::*;
use swc_core::ecma::visit::{VisitMut, VisitMutWith};

use mangler_jsast::span::GeneratedSpans;

mod async_declaration;
mod classes;
mod coverage;
#[path = "classes/source_contexts.rs"]
pub(crate) mod eval_contexts;
mod eval_runtime;
mod generator_declaration;
mod glob;
mod module_runtime;
mod shells;
mod source_compiler;
mod source_producers;
use super::intrinsics;
pub(crate) use source_compiler::SourceDependencies;
mod partition;

/// Function virtualization: compile eligible named-function bodies to VM bytecode and
/// replace them with thunks that re-enter a spliced interpreter over a shared program
/// table.
pub struct VirtualizePass;

pub(crate) fn eval_class_contexts(
    program: &Program,
    cfg: &FileConfig,
    seed: Option<&mangler_vm::eval::EvalClassContext>,
) -> eval_contexts::SourceClassContexts {
    eval_contexts::collect(program, cfg, seed)
}

pub(crate) fn source_compiler_sites(program: &mut Program) -> SourceDependencies {
    source_compiler::dependencies(program)
}

pub(crate) fn source_functions(program: &Program) -> Vec<(u32, String, bool)> {
    coverage::candidates(program)
        .into_iter()
        .map(|c| (c.span, c.name, c.anonymous))
        .collect()
}

impl Pass<Js, FileConfig> for VirtualizePass {
    fn id(&self) -> &'static str {
        "virtualize"
    }

    /// Reads the strings-decoder anchor (OPTIONAL, soft-degrades when absent): in
    /// whole-program mode the partition must keep the strings-decoder stub NATIVE
    /// (never swallow it into the VM — its `core(idx)` calls inside virtualized code
    /// would then reference a name that no longer exists). Declaring the read orders
    /// virtualize AFTER strings (already the registration order) without reading
    /// `ResolvedScopes`, so it still runs BEFORE the resolver pseudo-pass — the
    /// spliced interpreter gets fresh resolver marks like the rest of the module.
    fn reads(&self) -> &[Resource] {
        const R: &[Resource] = &[Resource::decoder_anchor()];
        R
    }

    /// Produces the shared VM table + interpreter. Downstream passes (expr,
    /// cf-flatten, dead-code) read this to avoid bloating the hoisted bytecode array.
    fn writes(&self) -> &[Resource] {
        const W: &[Resource] = &[Resource::vm_table()];
        W
    }

    /// Opt-in: enabled when a virtualize target glob is configured OR whole-program
    /// virtualization is requested (Phase 1).
    fn enabled(&self, cfg: &FileConfig) -> bool {
        let v = &cfg.resolved().passes.virtualize;
        v.target.is_some() || v.whole_program
    }

    fn run(
        &self,
        ast: &mut <Js as Language>::Ast,
        cfg: &FileConfig,
        rng: &mut Rng,
        bus: &mut ArtifactBus,
        notes: &mut Notes,
    ) -> Result<()> {
        let vcfg = &cfg.resolved().passes.virtualize;
        let whole_program = vcfg.whole_program;
        let exclude: Option<String> = vcfg.exclude.clone();
        // In whole-program mode `target` is ignored (§1 matrix; validation also clears
        // it). Outside whole-program mode it is required (`enabled` gates this).
        let target = match (&vcfg.target, whole_program) {
            (_, true) => String::new(),
            (Some(t), false) => t.clone(),
            // `enabled` already gates this; defensive no-op if somehow reached.
            (None, false) => return Ok(()),
        };

        // ONE diversification per file, drawn once from this pass's RNG. Every chunk in
        // the shared table is serialized under it, so a single interpreter decodes them
        // all.
        let mut tb = TableBuilder::new(rng);

        // Draw the prologue names UP FRONT so the thunks and the spliced interpreter
        // agree (see module docs). File-wide-unique via the shared allocator.
        //
        // Nested bodies and runtime eval can introduce any strict/EH variant.
        // Reserve distinct symbols even when a variant is ultimately unused.
        let lean_interp = cfg.fresh_name();
        let eh_interp = cfg.fresh_name();
        let table = cfg.fresh_name();
        let rc = cfg.fresh_name();
        let sy = cfg.fresh_name();
        let lean_interp_strict = cfg.fresh_name();
        let eh_interp_strict = cfg.fresh_name();
        let names = VmNames {
            lean_interp,
            eh_interp,
            lean_interp_strict,
            eh_interp_strict,
            table,
            rc,
            sy,
        };

        let iterator_alias = classes::IteratorAlias::new(&names.sy);

        // The strings-decoder stub (and its `core` declaration), if strings ran
        // before us, must stay NATIVE in whole-program mode (a top-level run that
        // swallowed it into the VM would leave the virtualized `core(idx)` decode
        // calls referencing a name that no longer exists at module scope).
        let protect_native: Vec<String> = match bus.get::<crate::artifacts::DecoderAnchorArtifact>()
        {
            Ok(Some(d)) => vec![d.core_name.clone()],
            _ => Vec::new(),
        };

        let source_dependencies = cfg
            .source_compiler_dependencies()
            .cloned()
            .unwrap_or_else(|| source_compiler::dependencies(ast.program_mut()));
        let source_compiler_sites = source_dependencies.consumers;
        let eval_class_contexts = cfg
            .eval_class_contexts()
            .cloned()
            .unwrap_or_else(|| eval_contexts::collect(ast.program(), cfg, None));
        let ambient_names = source_ambient_names(ast.program());
        let top_environment = TopEnvironment::new(ast.program());
        let mut original_candidates: Vec<_> = coverage::candidates(ast.program())
            .into_iter()
            .filter(|c| {
                cfg.source_functions()
                    .is_none_or(|source| source.contains(&(c.span, c.name.clone())))
            })
            .collect();
        // Every source identity remains accountable even when an earlier transform
        // removed or renamed its AST node. Missing provenance must fail required coverage.
        if let Some(source) = cfg.source_functions() {
            let existing: std::collections::HashSet<_> = original_candidates
                .iter()
                .map(|c| (c.span, c.name.clone()))
                .collect();
            for (span, name) in source {
                if !existing.contains(&(*span, name.clone())) {
                    original_candidates.push(coverage::Candidate {
                        name: name.clone(),
                        span: *span,
                        end: span.saturating_add(1),
                        reason: Some("transformed_source_missing"),
                        anonymous: cfg
                            .source_anonymous()
                            .is_some_and(|names| names.contains(span)),
                    });
                }
            }
            original_candidates.sort_by(|a, b| (a.span, &a.name).cmp(&(b.span, &b.name)));
        }
        let source_anonymous = cfg.source_anonymous().cloned().unwrap_or_else(|| {
            original_candidates
                .iter()
                .filter_map(|candidate| candidate.anonymous.then_some(candidate.span))
                .collect()
        });
        let source_names: std::collections::HashMap<_, _> = original_candidates
            .iter()
            .map(|c| (c.span, c.name.clone()))
            .collect();
        let excluded_ranges = coverage::excluded_ranges(&original_candidates, exclude.as_deref());
        let native_apply = cfg.fresh_name();
        let mut outcomes: std::collections::HashMap<_, _> = original_candidates
            .iter()
            .filter(|candidate| coverage::in_ranges(candidate.span, &excluded_ranges))
            .map(|candidate| (candidate.span, Some("excluded".to_string())))
            .collect();
        let mut top_level_errors = Vec::new();
        let intrinsic_isolation = intrinsics::Isolation::prepare(ast.program());
        let target_ranges = coverage::excluded_ranges(&original_candidates, Some(&target));
        let mut selected_spans: std::collections::HashSet<u32> = original_candidates
            .iter()
            .filter(|candidate| {
                (whole_program || coverage::in_ranges(candidate.span, &target_ranges))
                    && !coverage::in_ranges(candidate.span, &excluded_ranges)
            })
            .map(|candidate| candidate.span)
            .collect();
        if cfg.runtime_eval_frontend() {
            selected_spans.insert(0);
        }
        let resources =
            super::resources::lower(ast.program_mut(), &selected_spans, whole_program, cfg)?;
        let mut shell_intrinsics = resources.helpers;
        let parameters = classes::SuspendedParameters::prepare(
            ast.program_mut(),
            &selected_spans,
            cfg,
            &native_apply,
            &iterator_alias,
        );
        let lowered = super::suspension::lower_with_lexicals(ast.program_mut(), &selected_spans);
        parameters.restore(ast.program_mut());
        shell_intrinsics.extend(lowered.helpers);
        let suspensions = lowered.suspensions;
        let native_declarations = lowered.native_declarations;
        let suspension_lexicals = lowered.lexicals;
        let suspension_references = lowered.references;
        let mut internal_bindings = lowered.internals;
        internal_bindings.extend(resources.internals);
        cfg.collect_runtime_internals(&internal_bindings);
        let mut class_method_helper = None;
        let mut class_used = false;
        if whole_program {
            let mut v = Virtualizer {
                cfg,
                native_apply: &native_apply,
                iterator_alias: &iterator_alias,
                target: "*",
                exclude: exclude.as_deref(),
                names: &names,
                tb: &mut tb,
                used: false,
                strict_stack: vec![program_top_is_strict(ast.program())],
                excluded: Vec::new(),
                outcomes: &mut outcomes,
                class_methods_only: true,
                source_names: &source_names,
                source_anonymous: &source_anonymous,
                source_compiler_sites: &source_compiler_sites,
                eval_class_contexts: &eval_class_contexts,
                excluded_ranges: &excluded_ranges,
                suspensions: &suspensions,
                native_declarations: &native_declarations,
                suspension_lexicals: &suspension_lexicals,
                suspension_references: &suspension_references,
                internal_bindings: &internal_bindings,
                top_environment: &top_environment,
                whole_envelopes: false,
                top_level_errors: &mut top_level_errors,
                protect_native: &protect_native,
                shell_hoists: Vec::new(),
                function_depth: 0,
                class_initializer_context: false,
                shell_intrinsics: &mut shell_intrinsics,
                class_method_helper: &mut class_method_helper,
                ambient_names: &ambient_names,
            };
            ast.program_mut().visit_mut_with(&mut v);
            class_used = v.used;
        }
        let mut used = if whole_program {
            // Phase 1: all-or-nothing top-level wrapper. Returns true iff the whole
            // top level was virtualized (else the program is left native).
            virtualize_whole_program(
                ast.program_mut(),
                cfg,
                &native_apply,
                &iterator_alias,
                &names,
                &mut tb,
                exclude.as_deref(),
                &protect_native,
                &suspensions,
                &suspension_lexicals,
                &suspension_references,
                &internal_bindings,
                &source_compiler_sites,
                &eval_class_contexts,
                &top_environment,
                &ambient_names,
                &mut top_level_errors,
            )
        } else {
            let mut v = Virtualizer {
                cfg,
                native_apply: &native_apply,
                iterator_alias: &iterator_alias,
                target: &target,
                exclude: exclude.as_deref(),
                names: &names,
                tb: &mut tb,
                used: false,
                strict_stack: vec![program_top_is_strict(ast.program())],
                excluded: Vec::new(),
                outcomes: &mut outcomes,
                class_methods_only: false,
                source_names: &source_names,
                source_anonymous: &source_anonymous,
                source_compiler_sites: &source_compiler_sites,
                eval_class_contexts: &eval_class_contexts,
                excluded_ranges: &excluded_ranges,
                suspensions: &suspensions,
                native_declarations: &native_declarations,
                suspension_lexicals: &suspension_lexicals,
                suspension_references: &suspension_references,
                internal_bindings: &internal_bindings,
                top_environment: &top_environment,
                whole_envelopes: false,
                top_level_errors: &mut top_level_errors,
                protect_native: &protect_native,
                shell_hoists: Vec::new(),
                function_depth: 0,
                class_initializer_context: false,
                shell_intrinsics: &mut shell_intrinsics,
                class_method_helper: &mut class_method_helper,
                ambient_names: &ambient_names,
            };
            ast.program_mut().visit_mut_with(&mut v);
            // §10: surface which functions were kept native so the user can confirm a
            // hot path (e.g. a render loop) was excluded as intended. Deterministic and
            // de-duplicated, in first-seen order.
            if !v.excluded.is_empty() {
                let glob = exclude.as_deref().unwrap_or("");
                notes.push(mangler_core::Note::from(
                    "virtualize",
                    format!(
                        "kept {} function(s) native via --virtualize-exclude '{}': {}",
                        v.excluded.len(),
                        glob,
                        v.excluded.join(", ")
                    ),
                ));
            }
            v.used
        } || class_used;

        if whole_program {
            // Native module boundaries must not strand independently compilable
            // function bodies. Protect remaining source functions in their lexical
            // envelopes after partitioning has handled top-level execution.
            let mut remaining = Virtualizer {
                cfg,
                native_apply: &native_apply,
                iterator_alias: &iterator_alias,
                target: "*",
                exclude: exclude.as_deref(),
                names: &names,
                tb: &mut tb,
                used: false,
                strict_stack: vec![program_top_is_strict(ast.program())],
                excluded: Vec::new(),
                outcomes: &mut outcomes,
                class_methods_only: false,
                source_names: &source_names,
                source_anonymous: &source_anonymous,
                source_compiler_sites: &source_compiler_sites,
                eval_class_contexts: &eval_class_contexts,
                excluded_ranges: &excluded_ranges,
                suspensions: &suspensions,
                native_declarations: &native_declarations,
                suspension_lexicals: &suspension_lexicals,
                suspension_references: &suspension_references,
                internal_bindings: &internal_bindings,
                top_environment: &top_environment,
                whole_envelopes: true,
                top_level_errors: &mut top_level_errors,
                protect_native: &protect_native,
                shell_hoists: Vec::new(),
                function_depth: 0,
                class_initializer_context: false,
                shell_intrinsics: &mut shell_intrinsics,
                class_method_helper: &mut class_method_helper,
                ambient_names: &ambient_names,
            };
            ast.program_mut().visit_mut_with(&mut remaining);
            used |= remaining.used;
            let native: std::collections::HashMap<_, _> = coverage::candidates(ast.program())
                .into_iter()
                .map(|c| ((c.span, c.name.clone()), c))
                .collect();
            for c in &original_candidates {
                if outcomes.contains_key(&c.span) {
                    continue;
                }
                let reason = if coverage::in_ranges(c.span, &excluded_ranges) {
                    Some("excluded".to_string())
                } else if let Some(c) = native.get(&(c.span, c.name.clone())) {
                    Some(c.reason.unwrap_or("native_partition").to_string())
                } else if used {
                    c.reason
                        .filter(|r| *r == "transformed_source_missing")
                        .map(str::to_string)
                } else {
                    Some("native_partition".to_string())
                };
                outcomes.insert(c.span, reason);
            }
        }
        if !top_level_errors.is_empty() {
            return Err(mangler_core::Error::transform(
                "virtualize",
                format!(
                    "top-level source could not be virtualized: {}",
                    top_level_errors.join(", ")
                ),
            ));
        }
        coverage::report(&original_candidates, &outcomes, vcfg, notes)?;

        // §10: in whole-program mode, report that everything was virtualized and which
        // exclude glob (if any) protected hot paths kept native inside the VM frames.
        if whole_program && used {
            let msg = match exclude.as_deref() {
                Some(g) => format!(
                    "whole-program virtualization active; functions matching '{g}' kept native"
                ),
                None => "whole-program virtualization active".to_string(),
            };
            notes.push(mangler_core::Note::from("virtualize", msg));
        }

        used |= cfg.runtime_eval_frontend();
        if !used {
            // Other passes can still protect globals and strings in excluded
            // declarations. Their cyclic module entries need the same capsule,
            // while the absence of a bytecode table remains authoritative.
            let mut prologue = Vec::new();
            module_runtime::install(ast, cfg, &mut prologue);
            splice_prologue(ast.program_mut(), prologue);
            return Ok(());
        }

        // Emit the shared prologue (rc/sy aliases, interpreter(s), table `var`) under
        // the SAME names the thunks reference, and splice it at module top — ABOVE
        // every thunk, the ordering guarantee the table initializer relies on.
        if iterator_alias.required() {
            tb.require_iterator_alias();
        }
        let mut vt = tb.finish(&names)?;
        if vt.has_eval {
            source_producers::mediate(
                ast.program_mut(),
                &source_dependencies.bind_producers,
                &names.table,
            );
        }
        if vt.has_eval && !cfg.runtime_frontend() {
            if matches!(
                top_environment.context,
                mangler_vm::eval::SourceContext::Script
            ) {
                let environment = ambient_environment(
                    &ambient_names,
                    program_top_is_strict(ast.program()),
                    Some(&top_environment),
                );
                if let Some(statement) =
                    parse_one_top_stmt(&format!("{}.globalEnvironment={environment};", names.table))
                {
                    vt.prologue.push(statement);
                }
            }
            vt.prologue.extend(eval_runtime::attach(&names.table, cfg)?);
        }
        vt.prologue.extend(shell_intrinsics);
        if let Some(alias) = parse_one_top_stmt(&format!("var {native_apply}=Reflect.apply;")) {
            vt.prologue.insert(0, alias);
        }
        intrinsic_isolation
            .protect(&mut vt.prologue, &names.table, cfg)
            .map_err(|path| {
                mangler_core::Error::transform(
                    "virtualize",
                    format!("runtime_intrinsic_shadow: unavailable {path}"),
                )
            })?;
        ast.register_runtime(&vt.prologue, Some(&names.table));
        let interpreter_names = vec![
            names.lean_interp.clone(),
            names.eh_interp.clone(),
            names.lean_interp_strict.clone(),
            names.eh_interp_strict.clone(),
        ];
        module_runtime::install(ast, cfg, &mut vt.prologue);
        if cfg.runtime_frontend() {
            cfg.collect_runtime_support(vt.prologue, names.table.clone());
        } else {
            splice_prologue(ast.program_mut(), vt.prologue);
        }

        bus.put(VmTableArtifact {
            interpreter_names,
            program_table_name: names.table,
        })
        .map_err(|e| mangler_core::Error::transform(self.id(), e.to_string()))?;

        Ok(())
    }
}

/// The mutable walk: virtualize each eligible named function in place.
fn prepare_source(
    function: &mut Function,
    cfg: &FileConfig,
    apply: &str,
    iterator_alias: &classes::IteratorAlias<'_>,
    contexts: &eval_contexts::SourceClassContexts,
    external: Option<&mangler_vm::eval::EvalClassContext>,
) -> (Function, Vec<Stmt>, std::collections::HashSet<String>) {
    let (mut bridges, hidden) =
        classes::prepare_nested_object_eval(function, contexts, cfg, apply, iterator_alias);
    let (prepared, lexical) = if let Some(context) = external {
        classes::prepare_external_eval(function, cfg, apply, iterator_alias, context)
    } else {
        classes::prepare(function, cfg, apply, iterator_alias)
    };
    bridges.extend(lexical);
    bridges.extend(eval_contexts::native_bridges(
        &prepared,
        contexts,
        cfg,
        apply,
        iterator_alias,
    ));
    (prepared, bridges, hidden)
}

fn with_hidden<'a>(
    existing: &'a std::collections::HashSet<String>,
    hidden: std::collections::HashSet<String>,
) -> std::borrow::Cow<'a, std::collections::HashSet<String>> {
    let mut names = std::borrow::Cow::Borrowed(existing);
    if !hidden.is_empty() {
        names.to_mut().extend(hidden);
    }
    names
}

struct Virtualizer<'a> {
    cfg: &'a FileConfig,
    native_apply: &'a str,
    iterator_alias: &'a classes::IteratorAlias<'a>,
    /// The glob pattern function names are matched against.
    target: &'a str,
    /// Optional glob pattern: functions whose inferred name matches this are kept
    /// native even if they match `target`. `None` = no exclusions.
    exclude: Option<&'a str>,
    /// The prologue names (drawn up-front) every thunk references.
    names: &'a VmNames,
    tb: &'a mut TableBuilder,
    /// Set true once any function in this file was virtualized.
    used: bool,
    /// §5a strictness propagation: a top-down inherited attribute. The top entry is
    /// the program top-level strictness (true for ES Module / `"use strict"` Script);
    /// entering a function whose body begins with `"use strict"`, OR any function
    /// nested in an already-strict scope, pushes `true`. A function is virtualized
    /// strict iff the top of this stack is `true` when its body is compiled.
    strict_stack: Vec<bool>,
    /// Names of functions kept native because they matched `exclude` (for the §10
    /// `Notes` report). First-seen order, de-duplicated.
    excluded: Vec<String>,
    outcomes: &'a mut std::collections::HashMap<u32, Option<String>>,
    class_methods_only: bool,
    source_names: &'a std::collections::HashMap<u32, String>,
    source_anonymous: &'a std::collections::HashSet<u32>,
    source_compiler_sites: &'a std::collections::HashSet<u32>,
    eval_class_contexts: &'a eval_contexts::SourceClassContexts,
    excluded_ranges: &'a [(u32, u32)],
    suspensions: &'a std::collections::HashMap<u32, mangler_vm::SuspensionKind>,
    native_declarations: &'a std::collections::HashMap<u32, super::suspension::NativeDeclaration>,
    suspension_lexicals: &'a mangler_vm::eval::SuspensionLexicalScopes,
    suspension_references: &'a mangler_vm::eval::SuspensionLexicalReferences,
    internal_bindings: &'a std::collections::HashSet<String>,
    top_environment: &'a TopEnvironment,
    whole_envelopes: bool,
    top_level_errors: &'a mut Vec<String>,
    protect_native: &'a [String],
    shell_hoists: Vec<Vec<Stmt>>,
    function_depth: usize,
    class_initializer_context: bool,
    shell_intrinsics: &'a mut Vec<Stmt>,
    class_method_helper: &'a mut Option<String>,
    ambient_names: &'a [String],
}

impl Virtualizer<'_> {
    /// Try to virtualize `function` (whose inferred name is `name`). Returns true if
    /// its body was replaced with a thunk. **Bail-to-safe**: any reason to skip
    /// returns false and leaves the function untouched (never a miscompile).
    fn try_virtualize(&mut self, name: &str, function: &mut Function) -> bool {
        self.try_virtualize_kind(name, function, false, None)
    }

    fn try_virtualize_arrow(&mut self, name: &str, arrow: &mut ArrowExpr) -> bool {
        let mut function = Function {
            span: arrow.span,
            params: arrow
                .params
                .iter()
                .cloned()
                .map(|pat| Param {
                    span: DUMMY_SP,
                    decorators: vec![],
                    pat,
                })
                .collect(),
            body: Some(match &mut *arrow.body {
                ArrowFunctionBody::FunctionBody(body) => {
                    mangler_jsast::deep::clone_function_body(body)
                }
                ArrowFunctionBody::Expr(expr) => FunctionBody {
                    span: DUMMY_SP,
                    stmts: vec![Stmt::Return(ReturnStmt {
                        span: DUMMY_SP,
                        arg: Some(Box::new(mangler_jsast::deep::clone_expr(expr))),
                    })],
                    ..Default::default()
                },
            }),
            is_async: arrow.is_async,
            ..Default::default()
        };
        if !self.try_virtualize_kind(name, &mut function, true, None) {
            return false;
        }
        arrow.params = function.params.into_iter().map(|param| param.pat).collect();
        *arrow.body = ArrowFunctionBody::FunctionBody(function.body.unwrap());
        true
    }

    fn try_virtualize_kind(
        &mut self,
        name: &str,
        function: &mut Function,
        lexical_arrow: bool,
        self_binding: Option<&str>,
    ) -> bool {
        if self.outcomes.get(&function.span.lo.0) == Some(&None) {
            return true;
        }
        // Hygiene can rename a lowered function; targeting remains in source names.
        let source_name = self.source_names.get(&function.span.lo.0).cloned();
        let name = source_name.as_deref().unwrap_or(name);
        if self.class_methods_only
            || !self.is_source(name, function)
            || !glob::matches(self.target, name)
        {
            return false;
        }
        self.outcomes
            .insert(function.span.lo.0, Some("unsupported".to_string()));
        // Exclude check: if the function's inferred name matches the exclude glob,
        // keep it native. Bail-to-safe: exclude never miscompiles.
        if coverage::in_ranges(function.span.lo.0, self.excluded_ranges) {
            self.outcomes
                .insert(function.span.lo.0, Some("excluded".to_string()));
            if !self.excluded.iter().any(|n| n == name) {
                self.excluded.push(name.to_string());
            }
            return false;
        }
        // Generators/async are not modeled by the flat-slot VM.
        if function.is_generator || function.is_async {
            self.outcomes.insert(
                function.span.lo.0,
                Some(
                    if function.is_async {
                        "async"
                    } else {
                        "generator"
                    }
                    .to_string(),
                ),
            );
            return false;
        }
        let native_parameters = self
            .native_declarations
            .get(&function.span.lo.0)
            .is_some_and(|declaration| declaration.kind != mangler_vm::SuspensionKind::Async);
        if native_parameters {
            self.function_depth += 1;
            let initializers = self.virtualize_native_initializers(function, true);
            self.function_depth -= 1;
            if let Err(reason) = initializers {
                self.outcomes.insert(function.span.lo.0, Some(reason));
                return false;
            }
        }
        // Native parameter expressions keep their own activation environment;
        // body lexical bridges are unavailable while defaults are evaluating.
        let parameters = native_parameters.then(|| std::mem::take(&mut function.params));
        self.protect_nested_classes(function);
        let (mut prepared, bridges, hidden) = prepare_source(
            function,
            self.cfg,
            self.native_apply,
            self.iterator_alias,
            self.eval_class_contexts,
            None,
        );
        if let Some(parameters) = parameters {
            function.params = parameters;
            prepared.params = function.params.clone();
        }
        let internal_bindings = with_hidden(self.internal_bindings, hidden);
        let body = match &prepared.body {
            Some(b) => b,
            None => return false,
        };
        // Structural eligibility (with/eval/await/yield + sloppy arguments-alias bail
        // + §5a `arguments.callee/.caller` bail).
        if let Eligibility::Skip(reason) = classify_body(&function.params, body) {
            self.outcomes
                .insert(function.span.lo.0, Some(format!("{reason:?}")));
            return false;
        }
        // §5a: strictness is a top-down inherited attribute. This function is strict
        // if an enclosing scope is strict OR its own body opens with `"use strict"`.
        // A strict function is virtualized by routing it to a strict thunk (correct
        // un-coerced `this`) + a strict interpreter variant (Store* throws on
        // non-writable/getter-only/frozen targets) — no strict bail anymore.
        let is_strict = self.current_strict() || has_use_strict_directive(body);

        // Compile the body to bytecode. `compile_body` is the final authority: on any
        // residual unsupported shape (incl. a nested closure that cannot compile) it
        // returns Err and we leave the whole function un-virtualized.
        //
        // Phase 3 (§4): thread the exclude glob so a NESTED function (any depth)
        // matching it stays a native closure inside this virtualized body. In named-
        // target mode we do NOT divert ineligible nested fns (an ineligible nested fn
        // still bails the parent, preserving pre-Phase-3 behavior); whole-program mode
        // (the coverage driver) enables that divert separately.
        let opts = CompileOptions {
            source_utf16: self.cfg.source_utf16(),
            source_compiler_sites: Some(self.source_compiler_sites),
            eval_class_contexts: Some(&self.eval_class_contexts.calls),
            exclude: self.exclude,
            divert_ineligible: false,
            live_captures: true,
            native_parameters,
            lexical_arguments: lexical_arrow,
            self_binding,
            strict: is_strict,
            source_context: if lexical_arrow
                && self.function_depth == 0
                && !self.class_initializer_context
            {
                self.top_environment.context
            } else {
                mangler_vm::eval::SourceContext::Function
            },
            external_var_bindings: None,
            suspensions: Some(self.suspensions),
            suspension_lexicals: Some(self.suspension_lexicals),
            suspension_references: Some(self.suspension_references),
            internal_bindings: Some(&internal_bindings),
            ..Default::default()
        };
        // Bytecode initializes source parameters once, including defaults and TDZ.
        // The native signature preserves reflection and arguments-object mapping.
        let params = &prepared.params[..];
        let compiled = match compile_body_with_opts(params, body, opts) {
            Ok(c) => c,
            Err(reason) => {
                self.outcomes
                    .insert(function.span.lo.0, Some(reason.to_string()));
                return false;
            }
        };

        // Register the chunk tree (children flattened, MakeClosure child-indices
        // resolved to table indices) and get the root chunk handle, recording its
        // strictness so `finish` emits the matching interpreter variant.
        let environment = if mangler_vm::eval::requires_environment(&compiled) {
            ambient_environment(self.ambient_names, is_strict, None)
        } else {
            "null".into()
        };
        let chunk = self.tb.add_strict(compiled, is_strict);

        // Build the re-entry thunk and install it as the function's new body. A parse
        // failure here (it never should, the source is machine-generated) is a sound
        // skip — but note the chunk is already in the table; leaving the original body
        // would call the un-thunked function while its table entry sits unused, which
        // is fine (extra dead table entry, never a miscompile).
        let (parameters, actual_arguments) = if native_parameters {
            (prepared.params.clone(), "arguments".into())
        } else {
            invocation_parameters(function, self.cfg, lexical_arrow)
        };
        let parameter_refs = if !lexical_arrow && !chunk.argument_mappings.is_empty() {
            format!(
                "[{}]",
                chunk
                    .argument_mappings
                    .iter()
                    .map(|(index, slot)| {
                        let Pat::Ident(binding) = &parameters[*index as usize].pat else {
                            unreachable!()
                        };
                        let name = &binding.id.sym;
                        format!("[{slot},{{get:()=>{name},set:(_value)=>{name}=_value}}]")
                    })
                    .collect::<Vec<_>>()
                    .join(",")
            )
        } else {
            "null".into()
        };
        let mut stmts = match thunk_stmts(
            self.names,
            &chunk,
            has_use_strict_directive(body),
            &actual_arguments,
            &parameter_refs,
            &environment,
            if lexical_arrow && self.function_depth == 0 {
                "void 0"
            } else {
                "new.target"
            },
            self_binding,
        ) {
            Some(s) => s,
            None => return false,
        };
        let directive_count = stmts
            .iter()
            .take_while(|stmt| mangler_jsast::directives::is_directive(stmt))
            .count();
        stmts.splice(directive_count..directive_count, bridges);
        // A successfully compiled body also protects nested child chunks, except
        // explicit native escapes. Record original spans before installing the thunk.
        let descendants = coverage::function_candidates(function, true);
        for candidate in &descendants {
            if candidate.span == 0 {
                continue;
            }
            let reason = coverage::in_ranges(candidate.span, self.excluded_ranges)
                .then(|| "excluded".to_string());
            self.outcomes.insert(candidate.span, reason);
        }
        self.outcomes.insert(function.span.lo.0, None);
        function.params = parameters;
        function.body = Some(FunctionBody {
            span: DUMMY_SP,
            stmts,
            ..Default::default()
        });
        self.used = true;
        true
    }

    fn virtualize_native_initializers(
        &mut self,
        function: &mut Function,
        new_target: bool,
    ) -> std::result::Result<(), String> {
        struct Initializers<'a, 'b> {
            vm: &'a mut Virtualizer<'b>,
            error: Option<String>,
            new_target: bool,
        }
        impl VisitMut for Initializers<'_, '_> {
            fn visit_mut_assign_pat(&mut self, pattern: &mut AssignPat) {
                pattern.left.visit_mut_with(self);
                let name = match &*pattern.left {
                    Pat::Ident(binding) => Some(binding.id.sym.to_string()),
                    _ => None,
                };
                if self.error.is_none() {
                    self.error = self
                        .vm
                        .virtualize_initializer_with_target(
                            &mut pattern.right,
                            name.as_deref(),
                            self.new_target,
                        )
                        .err();
                }
            }
            fn visit_mut_object_pat_prop(&mut self, property: &mut ObjectPatProp) {
                if let ObjectPatProp::Assign(assign) = property {
                    if let Some(value) = &mut assign.value
                        && self.error.is_none()
                    {
                        self.error = self
                            .vm
                            .virtualize_initializer_with_target(
                                value,
                                Some(assign.key.id.sym.as_ref()),
                                self.new_target,
                            )
                            .err();
                    }
                } else {
                    property.visit_mut_children_with(self);
                }
            }
            fn visit_mut_computed_prop_name(&mut self, key: &mut ComputedPropName) {
                if self.error.is_none() {
                    self.error = self
                        .vm
                        .virtualize_initializer_with_target(&mut key.expr, None, self.new_target)
                        .err();
                }
            }
        }
        let mut initializers = Initializers {
            vm: self,
            error: None,
            new_target,
        };
        function.params.visit_mut_with(&mut initializers);
        initializers.error.map_or(Ok(()), Err)
    }

    /// An expression has a finite number of await sites; native conditional awaits
    /// drive its bytecode iterator without adding a Promise adoption step. Keeping
    /// this driver inside the original pattern preserves native partial TDZ and
    /// IteratorClose. A synthetic iterator closes the suspended bytecode expression
    /// when an awaited rejection exits the native default expression.
    fn virtualize_awaited_initializer(
        &mut self,
        expression: &mut Box<Expr>,
    ) -> std::result::Result<(), String> {
        let await_count = expression_await_count(expression);
        let body = FunctionBody {
            stmts: vec![Stmt::Return(ReturnStmt {
                span: DUMMY_SP,
                arg: Some(Box::new(mangler_jsast::deep::clone_expr(expression))),
            })],
            ..Default::default()
        };
        let (mut helpers, _) = compile_top_level_await(
            body,
            self.cfg,
            self.native_apply,
            self.iterator_alias,
            self.names,
            self.tb,
            &Default::default(),
            self.suspensions,
            self.suspension_lexicals,
            self.suspension_references,
            self.internal_bindings,
            self.source_compiler_sites,
            self.eval_class_contexts,
            self.top_environment,
            self.ambient_names,
        )?;
        if !matches!(helpers.pop(), Some(Stmt::While(_))) {
            return Err("await expression driver shape".into());
        }
        let Some(Stmt::Decl(Decl::Var(mut state))) = helpers.pop() else {
            return Err("await expression state shape".into());
        };
        if state.decls.len() != 2 {
            return Err("await expression state bindings".into());
        }
        let mut step_decl = state.decls.pop().unwrap();
        let mut iterator_decl = state.decls.pop().unwrap();
        let Pat::Ident(iterator_binding) = &iterator_decl.name else {
            return Err("await iterator binding".into());
        };
        let Pat::Ident(step_binding) = &step_decl.name else {
            return Err("await result binding".into());
        };
        let iterator = iterator_binding.id.sym.to_string();
        let step = step_binding.id.sym.to_string();
        let producer = iterator_decl
            .init
            .take()
            .ok_or("await iterator initializer")?;
        step_decl.init = None;
        let result = self.cfg.fresh_name();
        let marker = self.cfg.fresh_name();
        let symbol = self.cfg.fresh_name();
        let mut resume = format!("{step}.value");
        for _ in 0..await_count {
            resume = format!(
                "({step}.done?{step}.value:({step}={iterator}.next(await {step}.value),{resume}))"
            );
        }
        let source = format!(
            "async function _expression(){{let {result};return ({iterator}={marker}(),{step}={iterator}.next(),[{result}={resume}]={{[{symbol}](){{return this}},next(){{return {{done:false,value:void 0}}}},return(){{if(!{step}.done){iterator}.return();return {{done:true}}}}}},{result});}}"
        );
        let mut driver = parse_fn_body_stmts(&source).ok_or("await expression parser")?;
        let Some(Stmt::Return(ReturnStmt {
            arg: Some(mut replacement),
            ..
        })) = driver.pop()
        else {
            return Err("await expression return".into());
        };
        struct Producer {
            marker: String,
            value: Option<Box<Expr>>,
        }
        impl VisitMut for Producer {
            fn visit_mut_expr(&mut self, expression: &mut Expr) {
                if matches!(expression, Expr::Call(CallExpr { callee: Callee::Expr(callee), .. }) if matches!(&**callee, Expr::Ident(id) if id.sym.as_ref() == self.marker.as_str()))
                {
                    *expression = *self.value.take().expect("single producer placeholder");
                } else {
                    expression.visit_mut_children_with(self);
                }
            }
        }
        replacement.visit_mut_with(&mut Producer {
            marker,
            value: Some(producer),
        });
        state.decls = vec![iterator_decl, step_decl];
        helpers.push(Stmt::Decl(Decl::Var(state)));
        helpers.extend(driver);
        helpers.push(
            parse_one_top_stmt(&format!("var {symbol}=Symbol.iterator;"))
                .ok_or("await iterator symbol")?,
        );
        self.shell_intrinsics.extend(helpers);
        if let Expr::Paren(paren) = &mut *replacement {
            paren.span = mangler_jsast::span::runtime_span();
        }
        *expression = replacement;
        self.used = true;
        Ok(())
    }

    fn virtualize_initializer(
        &mut self,
        expression: &mut Box<Expr>,
        inferred: Option<&str>,
    ) -> std::result::Result<(), String> {
        self.virtualize_initializer_with_target(expression, inferred, self.function_depth > 0)
    }

    fn virtualize_initializer_with_target(
        &mut self,
        expression: &mut Box<Expr>,
        inferred: Option<&str>,
        new_target: bool,
    ) -> std::result::Result<(), String> {
        if !new_target && expression_has_await(expression) {
            return self.virtualize_awaited_initializer(expression);
        }
        // Preserve NamedEvaluation without introducing a binding that shadows a
        // parameter captured by a default-created closure.
        let source_expression = Box::new(mangler_jsast::deep::clone_expr(expression));
        let value = if let Some(name) = inferred
            && mangler_jsast::assignment_target::is_anonymous_definition(&source_expression)
        {
            mangler_jsast::assignment_target::named_value(name, source_expression)
        } else {
            source_expression
        };
        let mut original = Function {
            body: Some(FunctionBody {
                span: DUMMY_SP,
                stmts: vec![Stmt::Return(ReturnStmt {
                    span: DUMMY_SP,
                    arg: Some(value),
                })],
                ..Default::default()
            }),
            ..Default::default()
        };
        self.protect_nested_classes(&mut original);
        let (prepared, bridges, hidden) = prepare_source(
            &mut original,
            self.cfg,
            self.native_apply,
            self.iterator_alias,
            self.eval_class_contexts,
            None,
        );
        let internal_bindings = with_hidden(self.internal_bindings, hidden);
        let strict = self.current_strict();
        if let Eligibility::Skip(reason) = classify_body(&[], prepared.body.as_ref().unwrap()) {
            return Err(reason.into());
        }
        let compiled = compile_body_with_opts(
            &[],
            prepared.body.as_ref().unwrap(),
            CompileOptions {
                source_utf16: self.cfg.source_utf16(),
                source_compiler_sites: Some(self.source_compiler_sites),
                eval_class_contexts: Some(&self.eval_class_contexts.calls),
                exclude: self.exclude,
                divert_ineligible: false,
                live_captures: true,
                native_parameters: true,
                lexical_arguments: true,
                lexical_entry: true,
                source_context: if new_target {
                    mangler_vm::eval::SourceContext::Function
                } else {
                    self.top_environment.context
                },
                strict,
                external_var_bindings: None,
                suspensions: Some(self.suspensions),
                suspension_lexicals: Some(self.suspension_lexicals),
                suspension_references: Some(self.suspension_references),
                internal_bindings: Some(&internal_bindings),
                ..Default::default()
            },
        )
        .map_err(str::to_string)?;
        let environment = if mangler_vm::eval::requires_environment(&compiled) {
            ambient_environment(
                self.ambient_names,
                strict,
                (self.function_depth == 0).then_some(self.top_environment),
            )
        } else {
            "null".into()
        };
        let chunk = self.tb.add_strict(compiled, strict);
        let mut stmts = thunk_stmts(
            self.names,
            &chunk,
            false,
            "[]",
            "null",
            &environment,
            if new_target { "new.target" } else { "void 0" },
            None,
        )
        .ok_or_else(|| "initializer_thunk".to_string())?;
        stmts.splice(0..0, bridges);
        let callback = Expr::Arrow(ArrowExpr {
            span: DUMMY_SP,
            body: Box::new(ArrowFunctionBody::FunctionBody(FunctionBody {
                span: DUMMY_SP,
                stmts,
                ..Default::default()
            })),
            ..Default::default()
        });
        **expression = Expr::Call(CallExpr {
            span: DUMMY_SP,
            callee: Callee::Expr(Box::new(Expr::Paren(ParenExpr {
                span: DUMMY_SP,
                expr: Box::new(callback),
            }))),
            ..Default::default()
        });
        for candidate in coverage::function_candidates(&original, true) {
            self.outcomes.insert(
                candidate.span,
                coverage::in_ranges(candidate.span, self.excluded_ranges)
                    .then(|| "excluded".to_string()),
            );
        }
        self.used = true;
        Ok(())
    }

    fn restore_native_declaration(&mut self, function: &mut Function) {
        let declaration = self.native_declarations[&function.span.lo.0];
        match declaration.kind {
            mangler_vm::SuspensionKind::Async => async_declaration::restore(function, self.cfg),
            kind => generator_declaration::restore(
                function,
                declaration.finally_depth,
                kind == mangler_vm::SuspensionKind::AsyncGenerator,
                self.cfg,
                self.shell_intrinsics,
            ),
        }
    }

    fn hoist_shell(&mut self, function: FnDecl) {
        let span = function.function.span.lo.0;
        let kind = self.suspensions[&span];
        let name = self
            .source_names
            .get(&span)
            .cloned()
            .unwrap_or_else(|| function.ident.sym.to_string());
        let (value, intrinsics) = shells::callable(*function.function, None, &name, kind, self.cfg);
        self.shell_intrinsics.extend(intrinsics);
        let declaration = Stmt::Decl(Decl::Var(Box::new(VarDecl {
            span: DUMMY_SP,
            kind: VarDeclKind::Var,
            decls: vec![VarDeclarator {
                span: DUMMY_SP,
                name: Pat::Ident(function.ident.into()),
                init: Some(value),
                definite: false,
            }],
            ..Default::default()
        })));
        self.shell_hoists
            .last_mut()
            .expect("suspension declaration statement list")
            .push(declaration);
    }

    fn protect_nested_classes(&mut self, function: &mut Function) {
        struct Nested<'a, 'b>(&'a mut Virtualizer<'b>);
        impl VisitMut for Nested<'_, '_> {
            fn visit_mut_bin_expr(&mut self, binary: &mut BinExpr) {
                mangler_jsast::deep::walk_binary_mut(binary, self);
            }
            fn visit_mut_class(&mut self, class: &mut swc_core::ecma::ast::Class) {
                self.0.visit_mut_class(class);
            }
        }
        let target = self.target;
        self.target = "*";
        function.visit_mut_children_with(&mut Nested(self));
        self.target = target;
    }

    fn is_source(&self, _name: &str, function: &Function) -> bool {
        self.source_names.contains_key(&function.span.lo.0)
    }

    /// The strictness inherited by the scope currently being walked (the top of the
    /// stack). Used to decide a function's strictness before `try_virtualize`.
    fn current_strict(&self) -> bool {
        *self.strict_stack.last().unwrap_or(&false)
    }

    /// Whether the body of a function/arrow we are about to descend into is strict:
    /// strict if the enclosing scope is strict OR the body opens with `"use strict"`.
    fn body_is_strict(&self, body: Option<&FunctionBody>) -> bool {
        self.current_strict() || body.is_some_and(has_use_strict_directive)
    }
}

impl VisitMut for Virtualizer<'_> {
    fn visit_mut_stmts(&mut self, statements: &mut Vec<Stmt>) {
        self.shell_hoists.push(Vec::new());
        statements.visit_mut_children_with(self);
        let hoists = self.shell_hoists.pop().unwrap();
        let directives = statements
            .iter()
            .take_while(|stmt| mangler_jsast::directives::is_directive(stmt))
            .count();
        statements.splice(directives..directives, hoists);
    }
    fn visit_mut_module_items(&mut self, items: &mut Vec<ModuleItem>) {
        self.shell_hoists.push(Vec::new());
        for item in items.iter_mut() {
            item.visit_mut_with(self);
            if let ModuleItem::ModuleDecl(ModuleDecl::ExportDecl(export)) = item
                && let Decl::Fn(function) = &export.decl
            {
                let span = function.function.span.lo.0;
                if self.outcomes.get(&span) == Some(&None)
                    && self.native_declarations.contains_key(&span)
                {
                    let Decl::Fn(function) = &mut export.decl else {
                        unreachable!()
                    };
                    self.restore_native_declaration(&mut function.function);
                } else if self.outcomes.get(&span) == Some(&None)
                    && self.suspensions.contains_key(&span)
                {
                    let Decl::Fn(function) =
                        std::mem::replace(&mut export.decl, Decl::Var(Box::default()))
                    else {
                        unreachable!()
                    };
                    let id = function.ident.clone();
                    self.hoist_shell(function);
                    *item = ModuleItem::ModuleDecl(ModuleDecl::ExportNamed(NamedExport {
                        span: DUMMY_SP,
                        specifiers: vec![ExportSpecifier::Named(ExportNamedSpecifier {
                            span: DUMMY_SP,
                            orig: ModuleExportName::Ident(id),
                            exported: None,
                            is_type_only: false,
                        })],
                        src: None,
                        type_only: false,
                        with: None,
                    }));
                }
            }
            if let ModuleItem::ModuleDecl(ModuleDecl::ExportDefaultDecl(export)) = item
                && let DefaultDecl::Fn(function) = &export.decl
            {
                let span = function.function.span.lo.0;
                if self.outcomes.get(&span) == Some(&None)
                    && self.native_declarations.contains_key(&span)
                {
                    let DefaultDecl::Fn(function) = &mut export.decl else {
                        unreachable!()
                    };
                    self.restore_native_declaration(&mut function.function);
                } else if self.outcomes.get(&span) == Some(&None)
                    && self.suspensions.contains_key(&span)
                {
                    let DefaultDecl::Fn(function) = std::mem::replace(
                        &mut export.decl,
                        DefaultDecl::Fn(FnExpr {
                            ident: None,
                            function: Box::default(),
                        }),
                    ) else {
                        unreachable!()
                    };
                    let id = function.ident.unwrap_or_else(|| {
                        Ident::new_no_ctxt(self.cfg.fresh_name().into(), DUMMY_SP)
                    });
                    self.hoist_shell(FnDecl {
                        ident: id.clone(),
                        declare: false,
                        function: function.function,
                    });
                    *item = ModuleItem::ModuleDecl(ModuleDecl::ExportNamed(NamedExport {
                        span: DUMMY_SP,
                        specifiers: vec![ExportSpecifier::Named(ExportNamedSpecifier {
                            span: DUMMY_SP,
                            orig: ModuleExportName::Ident(id),
                            exported: Some(ModuleExportName::Ident(Ident::new_no_ctxt(
                                "default".into(),
                                DUMMY_SP,
                            ))),
                            is_type_only: false,
                        })],
                        src: None,
                        type_only: false,
                        with: None,
                    }));
                }
            }
        }
        let hoists = self.shell_hoists.pop().unwrap();
        let directives = items.iter().take_while(|item| matches!(item,ModuleItem::Stmt(stmt) if mangler_jsast::directives::is_directive(stmt))).count();
        items.splice(
            directives..directives,
            hoists.into_iter().map(ModuleItem::Stmt),
        );
    }
    fn visit_mut_stmt(&mut self, statement: &mut Stmt) {
        statement.visit_mut_children_with(self);
        if let Stmt::Decl(Decl::Fn(function)) = statement {
            let span = function.function.span.lo.0;
            if self.outcomes.get(&span) == Some(&None)
                && self.native_declarations.contains_key(&span)
            {
                self.restore_native_declaration(&mut function.function);
            } else if self.outcomes.get(&span) == Some(&None)
                && self.suspensions.contains_key(&span)
            {
                let Stmt::Decl(Decl::Fn(function)) =
                    std::mem::replace(statement, Stmt::Empty(EmptyStmt { span: DUMMY_SP }))
                else {
                    unreachable!()
                };
                self.hoist_shell(function);
            }
        }
    }
    fn visit_mut_expr(&mut self, expression: &mut Expr) {
        let suspended_object = matches!(expression, Expr::Object(object) if object.props.iter().any(|property| {
            let PropOrSpread::Prop(property) = property else { return false; };
            let Prop::Method(method) = &**property else { return false; };
            let span = method.function.span.lo.0;
            self.suspensions.contains_key(&span)
                && self.source_names.get(&span).is_some_and(|name| glob::matches(self.target, name))
                && !coverage::in_ranges(span, self.excluded_ranges)
                && self.outcomes.get(&span) != Some(&None)
        }));
        if suspended_object {
            let mut source = Box::new(std::mem::replace(
                expression,
                Expr::Invalid(Invalid { span: DUMMY_SP }),
            ));
            if let Err(reason) = self.virtualize_initializer(&mut source, None) {
                self.top_level_errors
                    .push(format!("object suspension methods: {reason}"));
            }
            *expression = *source;
            return;
        }
        expression.visit_mut_children_with(self);
        if let Expr::Fn(function) = expression {
            let span = function.function.span.lo.0;
            if self.outcomes.get(&span) == Some(&None) && self.suspensions.contains_key(&span) {
                let kind = self.suspensions[&span];
                let name = if self.source_anonymous.contains(&span) {
                    String::new()
                } else {
                    self.source_names.get(&span).cloned().unwrap_or_default()
                };
                let Expr::Fn(function) =
                    std::mem::replace(expression, Expr::Invalid(Invalid { span: DUMMY_SP }))
                else {
                    unreachable!()
                };
                let (value, intrinsics) =
                    shells::callable(*function.function, function.ident, &name, kind, self.cfg);
                self.shell_intrinsics.extend(intrinsics);
                *expression = *value;
            }
        } else if let Expr::Arrow(arrow) = expression {
            let span = arrow.span.lo.0;
            if self.outcomes.get(&span) == Some(&None)
                && self.suspensions.get(&span) == Some(&mangler_vm::SuspensionKind::Async)
            {
                let name = if self.source_anonymous.contains(&span) {
                    String::new()
                } else {
                    self.source_names.get(&span).cloned().unwrap_or_default()
                };
                let Expr::Arrow(arrow) =
                    std::mem::replace(expression, Expr::Invalid(Invalid { span: DUMMY_SP }))
                else {
                    unreachable!()
                };
                let (value, intrinsics) = shells::arrow(arrow, &name, self.cfg);
                self.shell_intrinsics.extend(intrinsics);
                *expression = *value;
            }
        }
    }

    fn visit_mut_class(&mut self, class: &mut swc_core::ecma::ast::Class) {
        self.strict_stack.push(true);
        class.visit_mut_children_with(self);
        if self.target == "*" || self.whole_envelopes || self.class_methods_only {
            self.class_headers(class);
        }
        self.class_suspension_methods(class);
        self.strict_stack.pop();
    }

    fn visit_mut_class_method(&mut self, method: &mut ClassMethod) {
        self.class_method(method);
    }
    fn visit_mut_class_prop(&mut self, property: &mut ClassProp) {
        self.class_prop(property);
    }
    fn visit_mut_private_prop(&mut self, property: &mut PrivateProp) {
        self.private_prop(property);
    }
    fn visit_mut_static_block(&mut self, block: &mut StaticBlock) {
        self.static_block(block);
    }
    fn visit_mut_private_method(&mut self, method: &mut PrivateMethod) {
        self.private_method(method);
    }
    fn visit_mut_constructor(&mut self, constructor: &mut Constructor) {
        self.constructor(constructor);
    }
    fn visit_mut_getter_prop(&mut self, getter: &mut GetterProp) {
        self.getter(getter);
    }
    fn visit_mut_setter_prop(&mut self, setter: &mut SetterProp) {
        self.setter(setter);
    }

    fn visit_mut_fn_decl(&mut self, n: &mut FnDecl) {
        // Own ident is the canonical name for a function declaration.
        let name = n.ident.sym.to_string();
        if self.try_virtualize(&name, &mut n.function) {
            return; // replaced — don't recurse into the (now-thunk) body
        }
        n.visit_mut_children_with(self);
    }

    fn visit_mut_export_default_decl(&mut self, export: &mut ExportDefaultDecl) {
        if let DefaultDecl::Fn(function) = &mut export.decl {
            // SWC stores default declarations as FnExpr, but their local name
            // is a mutable module binding, not an expression's private self.
            let name = function.ident.as_ref().map_or_else(
                || format!("<anonymous@{}>", function.function.span.lo.0),
                |id| id.sym.to_string(),
            );
            if !self.try_virtualize(&name, &mut function.function) {
                function.function.visit_mut_with(self);
            }
        } else {
            export.visit_mut_children_with(self);
        }
    }

    fn visit_mut_fn_expr(&mut self, n: &mut FnExpr) {
        if self
            .outcomes
            .get(&n.function.span.lo.0)
            .is_some_and(|r| r.as_deref() == Some("excluded"))
        {
            return;
        }
        // Own ident (e.g. `var x = function render(){}`): own ident wins.
        if let Some(id) = n.ident.clone() {
            let name = id.sym.to_string();
            if self.try_virtualize_kind(&name, &mut n.function, false, Some(&name)) {
                return;
            }
        }
        if n.ident.is_none() {
            let name = format!("<anonymous@{}>", n.function.span.lo.0);
            if self.try_virtualize(&name, &mut n.function) {
                return;
            }
        }
        // No own ident: binding-name context is set by the parent visitor methods
        // (`visit_mut_var_declarator`, `visit_mut_assign_expr`,
        // `visit_mut_key_value_prop`). Those call `try_virtualize` directly before
        // delegating here, so we only recurse into children at this point.
        n.visit_mut_children_with(self);
    }

    /// Infer names from variable bindings for both functions and arrows.
    fn visit_mut_var_declarator(&mut self, n: &mut VarDeclarator) {
        n.visit_mut_children_with(self);
        if self.whole_envelopes && self.strict_stack.len() == 1 && n.span != DUMMY_SP {
            let mut pattern = Function {
                params: vec![Param {
                    span: n.span,
                    decorators: Vec::new(),
                    pat: n.name.clone(),
                }],
                ..Default::default()
            };
            if let Err(reason) = self.virtualize_native_initializers(&mut pattern, false) {
                self.top_level_errors
                    .push(format!("binding pattern: {reason}"));
            }
            n.name = pattern.params.remove(0).pat;
            let binding = match &n.name {
                Pat::Ident(binding) => Some(binding.id.sym.to_string()),
                _ => None,
            };
            if !binding
                .as_ref()
                .is_some_and(|name| self.protect_native.contains(name))
                && let Some(initializer) = &mut n.init
            {
                if protected_initializer_envelope(initializer) {
                    return;
                }
                if let Err(reason) = self.virtualize_initializer(initializer, binding.as_deref()) {
                    self.top_level_errors.push(format!(
                        "initializer {}: {reason}",
                        binding.as_deref().unwrap_or("<pattern>")
                    ));
                }
            }
        }
    }

    fn visit_mut_for_of_stmt(&mut self, loop_stmt: &mut ForOfStmt) {
        if self.whole_envelopes && self.strict_stack.len() == 1 && loop_stmt.is_await {
            self.top_level_errors.push("top_level_for_await".into());
        }
        loop_stmt.visit_mut_children_with(self);
    }

    fn visit_mut_export_default_expr(&mut self, export: &mut ExportDefaultExpr) {
        export.expr.visit_mut_with(self);
        if self.whole_envelopes
            && !protected_initializer_envelope(&export.expr)
            && let Err(reason) = self.virtualize_initializer(&mut export.expr, Some("default"))
        {
            self.top_level_errors
                .push(format!("default export: {reason}"));
        }
    }

    /// `{ render() {} }` — shorthand method property (§4.3 form 4).
    fn visit_mut_method_prop(&mut self, n: &mut MethodProp) {
        let key_name = static_prop_key_name(&n.key).unwrap_or_else(|| "<computed>".into());
        if self.try_virtualize(&key_name, &mut n.function) {
            return;
        }
        n.visit_mut_children_with(self);
    }

    /// §5a strictness propagation: descending into a (non-virtualized) function body
    /// pushes its strictness so nested functions inherit it (a function nested in a
    /// strict scope is strict even without its own directive). Reached for every
    /// `Function` whose owner visitor delegated to children (i.e. it was not
    /// virtualized in place).
    fn visit_mut_function(&mut self, n: &mut Function) {
        if self
            .outcomes
            .get(&n.span.lo.0)
            .is_some_and(|r| r.as_deref() == Some("excluded"))
        {
            return;
        }
        self.strict_stack.push(self.body_is_strict(n.body.as_ref()));
        self.function_depth += 1;
        let eval_entry =
            self.cfg.runtime_eval_frontend() && mangler_jsast::span::is_eval_entry_span(n.span);
        let mut external_private = Vec::new();
        if eval_entry && let Some(context) = self.cfg.runtime_eval_context() {
            // Once a child is protected, its native structural constants are no
            // longer traversable here. Resolve external private names while the
            // full source tree and each nested class private scope remain visible.
            let (prepared, bridges) = classes::prepare_external_private(
                n,
                self.cfg,
                self.native_apply,
                self.iterator_alias,
                context,
            );
            *n = prepared;
            external_private = bridges;
        }
        n.visit_mut_children_with(self);
        if eval_entry {
            if self
                .cfg
                .runtime_eval_context()
                .is_some_and(|context| context.allow_super_call)
            {
                classes::defer_derived_receiver(n, self.names);
            }
            // Nested source behavior was protected above. Reuse the same class
            // envelope preparation as ordinary source functions, while leaving
            // this root for the eval compiler and its caller variable record.
            let (mut prepared, bridges, hidden) = prepare_source(
                n,
                self.cfg,
                self.native_apply,
                self.iterator_alias,
                self.eval_class_contexts,
                self.cfg.runtime_eval_context(),
            );
            external_private.extend(bridges);
            self.cfg.collect_runtime_internals(&hidden);
            if let Some(body) = &mut prepared.body {
                let at = body
                    .stmts
                    .iter()
                    .take_while(|stmt| mangler_jsast::directives::is_directive(stmt))
                    .count();
                body.stmts.splice(at..at, external_private);
            }
            *n = prepared;
        }
        self.function_depth -= 1;
        self.strict_stack.pop();
    }

    /// Unnamed arrows are targets too; their native envelopes retain lexical
    /// this/arguments while the compiler receives initialized parameters as captures.
    fn visit_mut_arrow_expr(&mut self, n: &mut ArrowExpr) {
        if self
            .outcomes
            .get(&n.span.lo.0)
            .is_some_and(|r| r.as_deref() == Some("excluded"))
        {
            return;
        }
        let name = format!("<anonymous@{}>", n.span.lo.0);
        if self.try_virtualize_arrow(&name, n) {
            return;
        }
        let body = match &*n.body {
            ArrowFunctionBody::FunctionBody(b) => Some(b),
            ArrowFunctionBody::Expr(_) => None,
        };
        self.strict_stack.push(self.body_is_strict(body));
        n.visit_mut_children_with(self);
        self.strict_stack.pop();
    }
}

// ---------------------------------------------------------------------------
// Binding-name inference helpers (§4.3)
// ---------------------------------------------------------------------------

/// Infer a binding name from the last segment of a member-expression assignment
/// target (`obj.render = …` → `"render"`; `a.b.c = …` → `"c"`).
/// Only handles statically-known identifier keys; computed keys return `None`.
fn last_member_key(left: &AssignTarget) -> Option<String> {
    // Walk the assignment target to extract a MemberExpr on the left.
    let member = match left {
        AssignTarget::Simple(SimpleAssignTarget::Member(m)) => m,
        AssignTarget::Simple(SimpleAssignTarget::Ident(id)) => return Some(id.id.sym.to_string()),
        _ => return None,
    };
    match &member.prop {
        MemberProp::Ident(id) => Some(id.sym.to_string()),
        MemberProp::Computed(key) => static_expr_key_name(&key.expr),
        MemberProp::PrivateName(key) => Some(format!("#{}", key.name)),
    }
}

/// Infer a name from a static object/class property key. Returns `None` for
/// computed keys (`[expr]`) or private names (`#x`).
fn static_prop_key_name(key: &PropName) -> Option<String> {
    match key {
        PropName::Ident(id) => Some(id.sym.to_string()),
        PropName::Str(s) => s.value.as_str().map(|v| v.to_string()),
        PropName::Num(n) => Some(n.value.to_string()),
        PropName::BigInt(n) => Some(n.value.to_string()),
        PropName::Computed(key) => static_expr_key_name(&key.expr),
    }
}

fn protected_initializer_envelope(expr: &Expr) -> bool {
    if expr.span() == DUMMY_SP || mangler_jsast::span::is_runtime_span(expr.span()) {
        return true;
    }
    match expr {
        Expr::Fn(_) | Expr::Arrow(_) | Expr::Class(_) => true,
        Expr::Paren(paren) => protected_initializer_envelope(&paren.expr),
        _ => false,
    }
}

fn static_expr_key_name(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Lit(Lit::Str(s)) => s.value.as_str().map(str::to_string),
        Expr::Lit(Lit::Num(n)) => Some(n.value.to_string()),
        Expr::Lit(Lit::BigInt(n)) => Some(n.value.to_string()),
        Expr::Paren(p) => static_expr_key_name(&p.expr),
        _ => None,
    }
}

/// Source spellings are captured lazily at the actual native lexical entry.
/// Extra spellings do not read bindings, and VM-owned environments shadow these
/// descriptors. This includes bindings referenced only inside an eval string.
fn source_ambient_names(program: &Program) -> Vec<String> {
    use swc_core::ecma::visit::{Visit, VisitWith};
    struct Names(std::collections::HashSet<String>);
    impl Names {
        fn add(&mut self, id: &Ident) {
            if id.span != DUMMY_SP {
                self.0.insert(id.sym.to_string());
            }
        }
    }
    impl Visit for Names {
        fn visit_bin_expr(&mut self, binary: &BinExpr) {
            mangler_jsast::deep::walk_binary(binary, self);
        }
        fn visit_binding_ident(&mut self, binding: &BindingIdent) {
            self.add(&binding.id);
        }
        fn visit_fn_decl(&mut self, function: &FnDecl) {
            self.add(&function.ident);
            function.visit_children_with(self);
        }
        fn visit_fn_expr(&mut self, function: &FnExpr) {
            if let Some(id) = &function.ident {
                self.add(id);
            }
            function.visit_children_with(self);
        }
        fn visit_class_decl(&mut self, class: &ClassDecl) {
            self.add(&class.ident);
            class.visit_children_with(self);
        }
        fn visit_class_expr(&mut self, class: &ClassExpr) {
            if let Some(id) = &class.ident {
                self.add(id);
            }
            class.visit_children_with(self);
        }
        fn visit_import_decl(&mut self, import: &ImportDecl) {
            for specifier in &import.specifiers {
                self.add(specifier.local());
            }
        }
    }
    let mut names = Names(Default::default());
    program.visit_with(&mut names);
    let mut names: Vec<_> = names.0.into_iter().collect();
    names.sort();
    names
}
/// The native program environment has no synthetic function variable scope.
struct TopEnvironment {
    context: mangler_vm::eval::SourceContext,
    names: Vec<String>,
    lexicals: std::collections::HashSet<String>,
}
impl TopEnvironment {
    fn new(program: &Program) -> Self {
        let mut names = partition::program_var_bindings(program, program_top_is_strict(program));
        let mut lexicals = std::collections::HashSet::new();
        fn declaration(
            decl: &Decl,
            names: &mut std::collections::HashSet<String>,
            lexicals: &mut std::collections::HashSet<String>,
        ) {
            match decl {
                Decl::Var(var) => {
                    for declarator in &var.decls {
                        mangler_jsast::analysis::binding_names(&declarator.name, &mut |id| {
                            names.insert(id.sym.to_string());
                            if var.kind != VarDeclKind::Var {
                                lexicals.insert(id.sym.to_string());
                            }
                        });
                    }
                }
                Decl::Using(using) => {
                    for declarator in &using.decls {
                        mangler_jsast::analysis::binding_names(&declarator.name, &mut |id| {
                            names.insert(id.sym.to_string());
                            lexicals.insert(id.sym.to_string());
                        });
                    }
                }
                Decl::Class(class) => {
                    names.insert(class.ident.sym.to_string());
                    lexicals.insert(class.ident.sym.to_string());
                }
                Decl::Fn(function) => {
                    names.insert(function.ident.sym.to_string());
                }
                _ => {}
            }
        }
        match program {
            Program::Script(script) => {
                for stmt in &script.body {
                    if let Stmt::Decl(decl) = stmt {
                        declaration(decl, &mut names, &mut lexicals);
                    }
                }
            }
            Program::Module(module) => {
                for item in &module.body {
                    match item {
                        ModuleItem::Stmt(Stmt::Decl(decl))
                        | ModuleItem::ModuleDecl(ModuleDecl::ExportDecl(ExportDecl {
                            decl, ..
                        })) => declaration(decl, &mut names, &mut lexicals),
                        ModuleItem::ModuleDecl(ModuleDecl::Import(import)) => {
                            for specifier in &import.specifiers {
                                let name = specifier.local().sym.to_string();
                                names.insert(name.clone());
                                lexicals.insert(name);
                            }
                        }
                        ModuleItem::ModuleDecl(ModuleDecl::ExportDefaultDecl(export)) => {
                            match &export.decl {
                                DefaultDecl::Fn(function) => {
                                    if let Some(id) = &function.ident {
                                        names.insert(id.sym.to_string());
                                    }
                                }
                                DefaultDecl::Class(class) => {
                                    if let Some(id) = &class.ident {
                                        names.insert(id.sym.to_string());
                                        lexicals.insert(id.sym.to_string());
                                    }
                                }
                                _ => {}
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        let mut names: Vec<_> = names.into_iter().collect();
        names.sort();
        Self {
            context: if matches!(program, Program::Script(_)) {
                mangler_vm::eval::SourceContext::Script
            } else {
                mangler_vm::eval::SourceContext::Module
            },
            names,
            lexicals,
        }
    }
}
fn ambient_environment(names: &[String], strict: bool, top: Option<&TopEnvironment>) -> String {
    let names = top.map_or(names, |top| top.names.as_slice());
    let bindings = names
        .iter()
        .filter(|name| {
            !strict
                || !matches!(
                    name.as_str(),
                    "yield"
                        | "let"
                        | "implements"
                        | "interface"
                        | "package"
                        | "private"
                        | "protected"
                        | "public"
                        | "static"
                )
        })
        .map(|name| {
            format!(
                "[{name:?}]:{{r:{},l:{}}}",
                capture_descriptor(name, strict),
                top.is_some_and(|top| top.lexicals.contains(name))
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let global =
        top.is_some_and(|top| matches!(top.context, mangler_vm::eval::SourceContext::Script));
    format!(
        "{{b:{{__proto__:null,{bindings}}},p:null,v:{},g:{global}}}",
        top.is_some()
    )
}

/// Accessors defer resolving each source binding until the corresponding VM read.
/// Arrow functions preserve lexical `arguments` and avoid introducing a setter
/// parameter that could shadow the source name.
fn capture_descriptor(name: &str, strict: bool) -> String {
    capture_descriptor_with_capabilities(name, strict, mangler_vm::chunk::CaptureCapabilities::ALL)
}

fn capture_descriptor_with_capabilities(
    name: &str,
    strict: bool,
    capabilities: mangler_vm::chunk::CaptureCapabilities,
) -> String {
    // Descriptor fields must never be inherited from user Object.prototype.
    let mut fields = format!("__proto__:null,get:()=>{name}");
    let immutable_syntax = strict && matches!(name, "arguments" | "eval");
    let value = if name == "_value" {
        "_value2"
    } else {
        "_value"
    };
    if capabilities.writes() && !immutable_syntax {
        fields.push_str(&format!(",set:({value})=>{name}={value}"));
    }
    if capabilities.uses_typeof() {
        fields.push_str(&format!(",type:()=>typeof {name}"));
    }
    if capabilities.deletes() && !strict {
        fields.push_str(&format!(",del:()=>delete {name}"));
    }
    if capabilities.writes_strictly()
        && !strict
        && !matches!(
            name,
            "arguments"
                | "eval"
                | "yield"
                | "let"
                | "implements"
                | "interface"
                | "package"
                | "private"
                | "protected"
                | "public"
                | "static"
        )
    {
        fields.push_str(&format!(
            ",strictSet:({value})=>{{\"use strict\";{name}={value}}}"
        ));
    }
    format!("{{{fields}}}")
}

fn capture_descriptors(chunk: &mangler_vm::Chunk) -> String {
    capture_descriptors_with_self(chunk, None)
}

/// A named function expression has an immutable self binding whose sloppy writes
/// are ignored. A getter-only frame descriptor is the existing VM representation
/// of this policy: strict StoreLocal and reference writes throw, while sloppy
/// writes leave the public callable identity unchanged. Ordinary const captures
/// retain their setter, which throws even for sloppy source writes.
fn capture_descriptors_with_self(chunk: &mangler_vm::Chunk, self_binding: Option<&str>) -> String {
    format!(
        "[{}]",
        chunk
            .captures
            .iter()
            .zip(&chunk.capture_capabilities)
            .map(|(name, capabilities)| {
                if self_binding == Some(name.as_str()) {
                    // Source parameter/body shadows have local slots, so only a
                    // genuine capture of the private name reaches this branch.
                    format!(
                        "{{__proto__:null,get:()=>{name},type:()=>typeof {name},del:()=>false}}"
                    )
                } else {
                    capture_descriptor_with_capabilities(name, chunk.is_strict, *capabilities)
                }
            })
            .collect::<Vec<_>>()
            .join(",")
    )
}

/// Keep native reflection and arguments-object mode while initializing source
/// parameter bindings exactly once in bytecode. Arrow forwarding has no observable
/// own arguments object; omitted fixed entries and undefined initialize identically.
fn invocation_parameters(
    function: &Function,
    cfg: &FileConfig,
    arrow: bool,
) -> (Vec<Param>, String) {
    let length = function
        .params
        .iter()
        .take_while(|p| !matches!(p.pat, Pat::Assign(_) | Pat::Rest(_)))
        .count();
    let simple = function
        .params
        .iter()
        .all(|p| matches!(p.pat, Pat::Ident(_)));
    let mut forwarded = Vec::new();
    let mut rest_arguments = None;
    let mut params = Vec::new();
    for _ in 0..length {
        let name = cfg.fresh_name();
        params.push(Param {
            span: DUMMY_SP,
            decorators: Vec::new(),
            pat: Pat::Ident(Ident::new_no_ctxt(name.clone().into(), DUMMY_SP).into()),
        });
        forwarded.push(name);
    }
    if arrow || !simple {
        let rest = cfg.fresh_name();
        params.push(Param {
            span: DUMMY_SP,
            decorators: Vec::new(),
            pat: Pat::Rest(RestPat {
                span: DUMMY_SP,
                dot3_token: DUMMY_SP,
                arg: Box::new(Pat::Ident(
                    Ident::new_no_ctxt(rest.clone().into(), DUMMY_SP).into(),
                )),
                type_ann: None,
            }),
        });
        rest_arguments = Some(rest);
    }
    (
        params,
        if arrow {
            let rest = rest_arguments.expect("arrow wrappers have a rest parameter");
            if forwarded.is_empty() {
                rest
            } else {
                // Interpreter arguments are array-like. A null-prototype record
                // joins fixed parameters and the fresh rest array without invoking
                // user iterators, inherited setters, or mutable array methods.
                let args = cfg.fresh_name();
                let index = cfg.fresh_name();
                let fixed = forwarded
                    .iter()
                    .enumerate()
                    .map(|(index, name)| format!(",{index}:{name}"))
                    .collect::<String>();
                format!(
                    "(()=>{{let {args}={{__proto__:null,length:{length}+{rest}.length{fixed}}};for(let {index}=0;{index}<{rest}.length;{index}++){args}[{length}+{index}]={rest}[{index}];return {args}}})()"
                )
            }
        } else {
            "arguments".into()
        },
    )
}

/// Parse the VM re-entry body. The original function keeps its parameters and
/// arity; its body forwards actual arguments, lazy capture descriptors, and `this`.
/// Only an original own directive is reinserted: inherited strictness remains
/// inherited, avoiding forbidden directives in non-simple parameter functions.
fn thunk_stmts(
    names: &VmNames,
    chunk: &Chunk,
    is_strict: bool,
    actual_arguments: &str,
    parameter_refs: &str,
    environment: &str,
    new_target: &str,
    self_binding: Option<&str>,
) -> Option<Vec<Stmt>> {
    let interp = names.interp_for(chunk.needs_eh, chunk.is_strict);
    let table = &names.table;
    let caps = capture_descriptors_with_self(chunk, self_binding);
    // §5a case 1: a STRICT source function's thunk must itself be strict, so a plain
    // call (`f()`) forwards `this === undefined` (strict) rather than the boxed
    // `globalThis` a sloppy thunk would see. The `"use strict"` directive governs the
    // thunk's OWN `this` binding; the interpreter then receives the un-coerced
    // receiver. Sloppy thunks are byte-for-byte unchanged (empty prefix).
    let directive = if is_strict { "\"use strict\";" } else { "" };
    let environment_args = if environment == "null" {
        String::new()
    } else {
        format!(",{new_target},{environment}")
    };
    let src = format!(
        "function _v(){{{directive}return {interp}({table}[{idx}][0],{table}[{idx}][1],{actual_arguments},{caps},{cap_start},{pcount},this,true,{parameter_refs}{environment_args});}}",
        idx = chunk.index,
        cap_start = chunk.cap_start,
        pcount = chunk.pcount,
    );
    parse_fn_body_stmts(&src)
}

/// Parse a single function declaration `src` and return its body statements, or `None`
/// if it fails to parse.
fn parse_fn_body_stmts(src: &str) -> Option<Vec<Stmt>> {
    let mut ast = Js.parse(src, &ParseOpts::default()).ok()?;
    ast.program_mut().visit_mut_with(&mut GeneratedSpans);
    let stmts = match ast.into_program() {
        Program::Module(m) => m
            .body
            .into_iter()
            .filter_map(|i| match i {
                ModuleItem::Stmt(s) => Some(s),
                _ => None,
            })
            .collect::<Vec<_>>(),
        Program::Script(s) => s.body,
    };
    for s in stmts {
        if let Stmt::Decl(Decl::Fn(f)) = s
            && let Some(b) = f.function.body
        {
            return Some(b.stmts);
        }
    }
    Some(Vec::new())
}

/// Splice `prologue` statements at the top of `program`'s module/script body, above
/// every thunk that references them.
fn splice_prologue(program: &mut Program, prologue: Vec<Stmt>) {
    mangler_jsast::directives::insert_program_statements(program, prologue);
}

// Whole-program execution retains native binding and module envelopes.
use partition::Segment;

fn expression_has_await(expression: &Expr) -> bool {
    expression_await_count(expression) != 0
}
fn expression_await_count(expression: &Expr) -> usize {
    use swc_core::ecma::visit::{Visit, VisitWith};
    struct Await(usize);
    impl Visit for Await {
        fn visit_bin_expr(&mut self, binary: &BinExpr) {
            mangler_jsast::deep::walk_binary(binary, self);
        }
        fn visit_function(&mut self, _: &Function) {}
        fn visit_arrow_expr(&mut self, _: &ArrowExpr) {}
        fn visit_await_expr(&mut self, awaited: &AwaitExpr) {
            self.0 += 1;
            awaited.arg.visit_with(self);
        }
    }
    let mut scan = Await(0);
    expression.visit_with(&mut scan);
    scan.0
}

#[allow(clippy::too_many_arguments)]
fn compile_top_level_await(
    mut body: FunctionBody,
    cfg: &FileConfig,
    native_apply: &str,
    iterator_alias: &classes::IteratorAlias<'_>,
    names: &VmNames,
    tb: &mut TableBuilder,
    external_vars: &std::collections::HashSet<String>,
    suspensions: &std::collections::HashMap<u32, mangler_vm::SuspensionKind>,
    suspension_lexicals: &mangler_vm::eval::SuspensionLexicalScopes,
    suspension_references: &mangler_vm::eval::SuspensionLexicalReferences,
    internal_bindings: &std::collections::HashSet<String>,
    source_compiler_sites: &std::collections::HashSet<u32>,
    eval_class_contexts: &eval_contexts::SourceClassContexts,
    top_environment: &TopEnvironment,
    ambient_names: &[String],
) -> std::result::Result<(Vec<Stmt>, Box<Expr>), String> {
    let mut envelope = Function {
        body: Some(body),
        ..Default::default()
    };
    // Keep metadata-only method capsules out of the suspension producer's
    // identifier rewrite; install them after that producer has finalized names.
    let (prepared, mut bridges) =
        classes::prepare(&mut envelope, cfg, native_apply, iterator_alias);
    bridges.extend(eval_contexts::native_bridges(
        &prepared,
        eval_class_contexts,
        cfg,
        native_apply,
        iterator_alias,
    ));
    body = prepared.body.unwrap();
    let arguments_alias = cfg.fresh_name();
    struct LexicalArguments<'a>(&'a str);
    impl VisitMut for LexicalArguments<'_> {
        fn visit_mut_bin_expr(&mut self, expression: &mut BinExpr) {
            mangler_jsast::deep::walk_binary_mut(expression, self);
        }
        fn visit_mut_function(&mut self, _: &mut Function) {}
        fn visit_mut_expr(&mut self, expression: &mut Expr) {
            if let Expr::Ident(id) = expression
                && id.sym == *"arguments"
            {
                id.sym = self.0.into();
            } else {
                expression.visit_mut_children_with(self);
            }
        }
        fn visit_mut_prop(&mut self, property: &mut Prop) {
            if let Prop::Shorthand(id) = property
                && id.sym == *"arguments"
            {
                *property = Prop::KeyValue(KeyValueProp {
                    key: PropName::Ident(IdentName::new("arguments".into(), id.span)),
                    value: Box::new(Expr::Ident(Ident::new_no_ctxt(self.0.into(), id.span))),
                });
            } else {
                property.visit_mut_children_with(self);
            }
        }
    }
    body.visit_mut_with(&mut LexicalArguments(&arguments_alias));
    let mut lowered = super::suspension::lower_top_level(body);
    let mut helper_names = std::collections::HashMap::new();
    for helper in &lowered.helpers {
        if let Stmt::Decl(Decl::Fn(function)) = helper {
            helper_names.insert(function.ident.sym.to_string(), cfg.fresh_name());
        }
        if let Stmt::Decl(Decl::Var(var)) = helper {
            for declaration in &var.decls {
                mangler_jsast::analysis::binding_names(&declaration.name, &mut |id| {
                    helper_names.insert(id.sym.to_string(), cfg.fresh_name());
                });
            }
        }
    }
    struct Rename<'a>(&'a std::collections::HashMap<String, String>);
    impl VisitMut for Rename<'_> {
        fn visit_mut_bin_expr(&mut self, expression: &mut BinExpr) {
            mangler_jsast::deep::walk_binary_mut(expression, self);
        }
        fn visit_mut_ident(&mut self, id: &mut Ident) {
            if let Some(name) = self.0.get(id.sym.as_ref()) {
                id.sym = name.as_str().into();
            }
        }
    }
    lowered.helpers.visit_mut_with(&mut Rename(&helper_names));
    lowered.function.visit_mut_with(&mut Rename(&helper_names));
    let (object_bridges, hidden) = classes::prepare_nested_object_eval(
        &mut lowered.function,
        eval_class_contexts,
        cfg,
        native_apply,
        iterator_alias,
    );
    bridges.extend(object_bridges);
    let mut kinds = suspensions.clone();
    kinds.extend(lowered.suspensions);
    let mut lexical_scopes = suspension_lexicals.clone();
    lexical_scopes.extend(lowered.lexicals);
    let mut lexical_references = suspension_references.clone();
    lexical_references.extend(lowered.references);
    let mut generated_bindings = internal_bindings.clone();
    generated_bindings.extend(hidden);
    generated_bindings.extend(
        lowered
            .internals
            .into_iter()
            .map(|name| helper_names.get(&name).cloned().unwrap_or(name)),
    );
    let block = lowered
        .function
        .body
        .as_ref()
        .ok_or("top-level suspension body")?;
    if let Eligibility::Skip(reason) = classify_body(&[], block) {
        return Err(reason.into());
    }
    let compiled = compile_body_with_opts(
        &[],
        block,
        CompileOptions {
            source_utf16: cfg.source_utf16(),
            source_compiler_sites: Some(source_compiler_sites),
            eval_class_contexts: Some(&eval_class_contexts.calls),
            live_captures: true,
            lexical_arguments: true,
            strict: true,
            external_var_bindings: Some(external_vars),
            suspensions: Some(&kinds),
            suspension_lexicals: Some(&lexical_scopes),
            suspension_references: Some(&lexical_references),
            internal_bindings: Some(&generated_bindings),
            source_context: top_environment.context,
            lexical_entry: true,
            ..Default::default()
        },
    )
    .map_err(str::to_string)?;
    let environment = if mangler_vm::eval::requires_environment(&compiled) {
        ambient_environment(ambient_names, true, Some(top_environment))
    } else {
        "null".into()
    };
    let chunk = tb.add_strict(compiled, true);
    let captures = format!(
        "[{}]",
        chunk
            .captures
            .iter()
            .map(|name| capture_descriptor(
                if name == &arguments_alias {
                    "arguments"
                } else {
                    name
                },
                true
            ))
            .collect::<Vec<_>>()
            .join(",")
    );
    let interp = names.interp_for(chunk.needs_eh, true);
    let table = &names.table;
    let source = format!(
        "function _entry(){{return {interp}({table}[{index}][0],{table}[{index}][1],[],{captures},{cap_start},0,this,true,null,void 0,{environment});}}",
        index = chunk.index,
        cap_start = chunk.cap_start
    );
    let mut entry = parse_fn_body_stmts(&source).ok_or("module await entry")?;
    let Some(Stmt::Return(returned)) = entry.pop() else {
        unreachable!()
    };
    let (driver, result) = async_declaration::drive(returned.arg.expect("module iterator"), cfg);
    lowered.helpers.splice(0..0, bridges);
    lowered.helpers.extend(driver);
    Ok((lowered.helpers, result))
}

#[allow(clippy::too_many_arguments)]
fn virtualize_whole_program(
    program: &mut Program,
    cfg: &FileConfig,
    native_apply: &str,
    iterator_alias: &classes::IteratorAlias<'_>,
    names: &VmNames,
    tb: &mut TableBuilder,
    exclude: Option<&str>,
    protect_native: &[String],
    suspensions: &std::collections::HashMap<u32, mangler_vm::SuspensionKind>,
    suspension_lexicals: &mangler_vm::eval::SuspensionLexicalScopes,
    suspension_references: &mangler_vm::eval::SuspensionLexicalReferences,
    internal_bindings: &std::collections::HashSet<String>,
    source_compiler_sites: &std::collections::HashSet<u32>,
    eval_class_contexts: &eval_contexts::SourceClassContexts,
    top_environment: &TopEnvironment,
    ambient_names: &[String],
    failures: &mut Vec<String>,
) -> bool {
    let strict = program_top_is_strict(program);
    // Resource/suspension lowering can rename a lexical barrier or replace its
    // declaration shape. Source variable instantiation remains authoritative;
    // fresh compiler declarations still need their native partition storage.
    let source_names: std::collections::HashSet<_> = ambient_names.iter().collect();
    let source_variables: std::collections::HashSet<_> = top_environment
        .names
        .iter()
        .filter(|name| !top_environment.lexicals.contains(*name))
        .collect();
    let external_vars: std::collections::HashSet<_> =
        partition::program_var_bindings(program, strict)
            .into_iter()
            .filter(|name| !source_names.contains(name) || source_variables.contains(name))
            .collect();
    let mut items: Vec<ModuleItem> = match program {
        Program::Script(script) => std::mem::take(&mut script.body)
            .into_iter()
            .map(ModuleItem::Stmt)
            .collect(),
        Program::Module(module) => std::mem::take(&mut module.body),
    };
    let directive_count = items.iter().take_while(|item| matches!(item, ModuleItem::Stmt(stmt) if mangler_jsast::directives::is_directive(stmt))).count();
    let mut output: Vec<_> = items.drain(..directive_count).collect();
    let mut used = false;
    for segment in partition::segment(partition::classify_with_protected(items, protect_native)) {
        match segment {
            Segment::Native(mut item) => {
                let variable = match &mut item {
                    ModuleItem::Stmt(Stmt::Decl(Decl::Var(variable))) => {
                        let mut template = variable.clone();
                        template.decls.clear();
                        Some((&mut variable.decls, Decl::Var(template), false))
                    }
                    ModuleItem::ModuleDecl(ModuleDecl::ExportDecl(ExportDecl {
                        decl: Decl::Var(variable),
                        ..
                    })) => {
                        let mut template = variable.clone();
                        template.decls.clear();
                        Some((&mut variable.decls, Decl::Var(template), true))
                    }
                    ModuleItem::Stmt(Stmt::Decl(Decl::Using(variable))) => {
                        let mut template = variable.clone();
                        template.decls.clear();
                        Some((&mut variable.decls, Decl::Using(template), false))
                    }
                    _ => None,
                };
                if let Some((declarations, template, exported)) = variable {
                    for mut declaration in std::mem::take(declarations) {
                        if declaration
                            .init
                            .as_ref()
                            .is_some_and(|init| expression_has_await(init))
                        {
                            let value = declaration.init.take().unwrap();
                            let block = FunctionBody {
                                span: DUMMY_SP,
                                stmts: vec![Stmt::Return(ReturnStmt {
                                    span: DUMMY_SP,
                                    arg: Some(value),
                                })],
                                ..Default::default()
                            };
                            match compile_top_level_await(
                                block,
                                cfg,
                                native_apply,
                                iterator_alias,
                                names,
                                tb,
                                &external_vars,
                                suspensions,
                                suspension_lexicals,
                                suspension_references,
                                internal_bindings,
                                source_compiler_sites,
                                eval_class_contexts,
                                top_environment,
                                ambient_names,
                            ) {
                                Ok((driver, result)) => {
                                    output.extend(driver.into_iter().map(ModuleItem::Stmt));
                                    declaration.init = Some(result);
                                    used = true;
                                }
                                Err(reason) => {
                                    failures.push(format!("awaited initializer: {reason}"))
                                }
                            }
                        }
                        let mut split = template.clone();
                        match &mut split {
                            Decl::Var(variable) => variable.decls.push(declaration),
                            Decl::Using(variable) => variable.decls.push(declaration),
                            _ => unreachable!(),
                        }
                        output.push(if exported {
                            ModuleItem::ModuleDecl(ModuleDecl::ExportDecl(ExportDecl {
                                span: DUMMY_SP,
                                decl: split,
                            }))
                        } else {
                            ModuleItem::Stmt(Stmt::Decl(split))
                        });
                    }
                } else if let ModuleItem::ModuleDecl(ModuleDecl::ExportDefaultExpr(export)) =
                    &mut item
                {
                    if expression_has_await(&export.expr) {
                        let value = std::mem::replace(
                            &mut export.expr,
                            Box::new(Expr::Invalid(Invalid { span: DUMMY_SP })),
                        );
                        let block = FunctionBody {
                            span: DUMMY_SP,
                            stmts: vec![Stmt::Return(ReturnStmt {
                                span: DUMMY_SP,
                                arg: Some(value),
                            })],
                            ..Default::default()
                        };
                        match compile_top_level_await(
                            block,
                            cfg,
                            native_apply,
                            iterator_alias,
                            names,
                            tb,
                            &external_vars,
                            suspensions,
                            suspension_lexicals,
                            suspension_references,
                            internal_bindings,
                            source_compiler_sites,
                            eval_class_contexts,
                            top_environment,
                            ambient_names,
                        ) {
                            Ok((driver, result)) => {
                                output.extend(driver.into_iter().map(ModuleItem::Stmt));
                                export.expr = result;
                                used = true;
                            }
                            Err(reason) => {
                                failures.push(format!("awaited default export: {reason}"))
                            }
                        }
                    }
                    output.push(item);
                } else if matches!(&item, ModuleItem::Stmt(statement) if partition::stmt_has_top_level_await(statement))
                {
                    let ModuleItem::Stmt(statement) = item else {
                        unreachable!()
                    };
                    let block = FunctionBody {
                        span: DUMMY_SP,
                        stmts: vec![statement],
                        ..Default::default()
                    };
                    match compile_top_level_await(
                        block,
                        cfg,
                        native_apply,
                        iterator_alias,
                        names,
                        tb,
                        &external_vars,
                        suspensions,
                        suspension_lexicals,
                        suspension_references,
                        internal_bindings,
                        source_compiler_sites,
                        eval_class_contexts,
                        top_environment,
                        ambient_names,
                    ) {
                        Ok((driver, _)) => {
                            output.extend(driver.into_iter().map(ModuleItem::Stmt));
                            used = true;
                        }
                        Err(reason) => failures.push(format!("awaited statement: {reason}")),
                    }
                } else {
                    output.push(item);
                }
            }
            Segment::Run(stmts) => {
                let block = FunctionBody {
                    span: DUMMY_SP,
                    stmts,
                    ..Default::default()
                };
                let mut envelope = Function {
                    body: Some(block),
                    ..Default::default()
                };
                let (prepared, bridges, hidden) = prepare_source(
                    &mut envelope,
                    cfg,
                    native_apply,
                    iterator_alias,
                    eval_class_contexts,
                    None,
                );
                let internal_bindings = with_hidden(internal_bindings, hidden);
                let block = prepared.body.unwrap();
                if let Eligibility::Skip(reason) = classify_body(&[], &block) {
                    failures.push(format!("statement run: {reason}"));
                    output.extend(block.stmts.into_iter().map(ModuleItem::Stmt));
                    continue;
                }
                match compile_body_with_opts(
                    &[],
                    &block,
                    CompileOptions {
                        source_utf16: cfg.source_utf16(),
                        source_compiler_sites: Some(source_compiler_sites),
                        eval_class_contexts: Some(&eval_class_contexts.calls),
                        exclude,
                        live_captures: true,
                        lexical_arguments: true,
                        strict,
                        external_var_bindings: Some(&external_vars),
                        suspensions: Some(suspensions),
                        suspension_lexicals: Some(suspension_lexicals),
                        suspension_references: Some(suspension_references),
                        internal_bindings: Some(&internal_bindings),
                        source_context: top_environment.context,
                        lexical_entry: true,
                        ..Default::default()
                    },
                ) {
                    Ok(compiled) => {
                        let environment = if mangler_vm::eval::requires_environment(&compiled) {
                            ambient_environment(ambient_names, strict, Some(top_environment))
                        } else {
                            "null".into()
                        };
                        let chunk = tb.add_strict(compiled, strict);
                        if let Some(call) = top_level_call_stmt(
                            names.interp_for(chunk.needs_eh, strict),
                            &names.table,
                            &chunk,
                            &environment,
                        ) {
                            output.extend(bridges.into_iter().map(ModuleItem::Stmt));
                            output.push(ModuleItem::Stmt(call));
                            used = true;
                        } else {
                            failures.push("generated top-level thunk".into());
                            output.extend(block.stmts.into_iter().map(ModuleItem::Stmt));
                        }
                    }
                    Err(reason) => {
                        failures.push(format!("statement run: {reason}"));
                        output.extend(block.stmts.into_iter().map(ModuleItem::Stmt));
                    }
                }
            }
        }
    }
    if used && let Some(hoist) = partition::var_hoist(&external_vars) {
        output.insert(directive_count, ModuleItem::Stmt(hoist));
    }
    set_body(program, output);
    used
}

/// Install `items` as the program's top-level body (Module item list / Script body).
fn set_body(program: &mut Program, items: Vec<ModuleItem>) {
    match program {
        Program::Module(m) => m.body = items,
        Program::Script(s) => {
            s.body = items
                .into_iter()
                .filter_map(|it| match it {
                    ModuleItem::Stmt(s) => Some(s),
                    // A Script cannot hold ModuleDecls; classification never produces
                    // them for a Script, so this is unreachable in practice.
                    ModuleItem::ModuleDecl(_) => None,
                })
                .collect();
        }
    }
}

/// Format + parse the §2.1 top-level re-entry call:
///
/// ```text
/// <interp>(T[i][0], T[i][1], [], [<caps>], <cap_start>, 0, <thisExpr>);
/// ```
///
/// * `pcount = 0`, `arguments = []` (top level has no args).
/// * `<caps>` — the run's free globals spread in capture order, referenced by their
///   real module-scope names (they resolve to the real globals because the call sits
///   at module scope) — the SAME capture array the function thunk builds.
/// * `<thisExpr>` — native top-level `this`: undefined in modules, globalThis in scripts.
/// * The result is discarded (a wrapped run never returns — top-level `return` is a
///   syntax error).
fn top_level_call_stmt(
    interp: &str,
    table: &str,
    chunk: &Chunk,
    environment: &str,
) -> Option<Stmt> {
    let caps = capture_descriptors(chunk);
    // Native top-level `this` is globalThis in scripts (including strict scripts)
    // and undefined in modules. Preserve the parser/runtime distinction directly.
    let this_expr = "this";
    let environment_args = if environment == "null" {
        String::new()
    } else {
        format!(",null,void 0,{environment}")
    };
    let src = format!(
        "{interp}({table}[{idx}][0],{table}[{idx}][1],[],{caps},{cap_start},0,{this_expr},true{environment_args});",
        idx = chunk.index,
        cap_start = chunk.cap_start,
    );
    parse_one_top_stmt(&src)
}

/// Parse a single statement `src` and return it, or `None` on parse failure.
fn parse_one_top_stmt(src: &str) -> Option<Stmt> {
    let mut ast = Js.parse(src, &ParseOpts::default()).ok()?;
    ast.program_mut().visit_mut_with(&mut GeneratedSpans);
    match ast.into_program() {
        Program::Script(s) => s.body.into_iter().next(),
        Program::Module(m) => m.body.into_iter().find_map(|it| match it {
            ModuleItem::Stmt(s) => Some(s),
            _ => None,
        }),
    }
}

/// §5a: the program top-level strictness (the base of the inherited-strictness
/// stack). An ES Module is implicitly strict; a Script is strict iff its body opens
/// with a `"use strict"` directive prologue.
fn program_top_is_strict(program: &Program) -> bool {
    match program {
        Program::Module(_) => true,
        Program::Script(s) => stmts_open_with_use_strict(&s.body),
    }
}

fn has_use_strict_directive(body: &FunctionBody) -> bool {
    stmts_open_with_use_strict(&body.stmts)
}

/// True if a statement list opens with a `"use strict"` directive prologue (leading
/// string-literal expression statements, one of which is exactly `"use strict"`).
fn stmts_open_with_use_strict(stmts: &[Stmt]) -> bool {
    mangler_jsast::directives::has_use_strict(stmts)
}

#[cfg(test)]
mod tests;
