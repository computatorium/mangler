//! Function-virtualization pass — the GLUE that wires the [`mangler_vm`] engine into
//! the pass pipeline.
//!
//! The VM ISA / compiler / serializer / interpreter all live in the `mangler-vm`
//! crate; this pass only:
//!
//! 1. walks the program and, for each named function whose name matches the
//!    configured glob target and that is structurally eligible
//!    ([`mangler_vm::classify_body`]), compiles its body to bytecode
//!    ([`mangler_vm::compile_body`]) — **bail-to-safe**: any `Err` leaves the
//!    function un-virtualized (never a miscompile);
//! 2. registers each compiled body with one per-file [`mangler_vm::TableBuilder`]
//!    (seeded from a single [`mangler_vm::VmDiversity::draw`]), getting back a
//!    [`mangler_vm::Chunk`] (root table index + thunk frame metadata);
//! 3. replaces each virtualized function's body with a thunk that re-enters the VM
//!    interpreter over its table entry;
//! 4. after the walk, [`finish`](mangler_vm::TableBuilder::finish)es the builder,
//!    splices the program-table `var` + interpreter(s) at module top, and `bus.put`s a
//!    [`VmTableArtifact`](crate::artifacts::VmTableArtifact).
//!
//! ## Pass shape
//!
//! * `id() = "virtualize"`
//! * `reads() = []` — runs PRE-resolver, so its spliced interpreter gets fresh marks.
//! * `writes() = [Resource::vm_table()]`
//! * `enabled() = cfg.resolved().passes.virtualize.target.is_some()`
//!
//! ## Names are drawn up-front
//!
//! The replacement thunk references the interpreter + table names, which the prologue
//! emitted by [`finish`](mangler_vm::TableBuilder::finish) also uses. Both must agree,
//! so this pass draws [`VmNames`] from [`FileConfig::fresh_name`] ONCE, before the
//! walk, and threads the SAME names into every thunk and into `finish`. Each thunk
//! picks the lean vs. EH interpreter from its own chunk's
//! [`needs_eh`](mangler_vm::Chunk::needs_eh).
//!
//! ## Why only NAMED functions are targeted
//!
//! The target is a glob over function names. Top-level virtualization candidates are
//! therefore named `function`-declarations and named `function`-expressions. Anonymous
//! functions and arrows have no name to match the glob against — they are virtualized
//! only as *nested children* of an eligible body (the compiler lowers a nested
//! `function`/arrow into a `MakeClosure` child chunk under the SAME interpreter).

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
use swc_core::common::DUMMY_SP;
use swc_core::ecma::ast::*;
use swc_core::ecma::visit::{VisitMut, VisitMutWith};

mod coverage;
mod glob;
mod partition;

/// Function virtualization: compile eligible named-function bodies to VM bytecode and
/// replace them with thunks that re-enter a spliced interpreter over a shared program
/// table.
pub struct VirtualizePass;

pub(crate) fn source_functions(program: &Program) -> Vec<(u32, String)> {
    coverage::candidates(program)
        .into_iter()
        .map(|c| (c.span, c.name))
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
        // §5a byte-identity: the two STRICT interpreter names are drawn from the shared
        // allocator ONLY when the program actually contains a strict virtualization
        // candidate. A fully-sloppy program therefore draws the SAME five names in the
        // SAME order as before strict support, so every name allocated by this and all
        // downstream passes is unchanged — output is byte-for-byte identical. When no
        // strict candidate exists the strict fields reuse the sloppy names (they are
        // never emitted, since no strict chunk is registered).
        let lean_interp = cfg.fresh_name();
        let eh_interp = cfg.fresh_name();
        let table = cfg.fresh_name();
        let rc = cfg.fresh_name();
        let sy = cfg.fresh_name();
        // Whole-program: the synthetic top-level wrapper is strict iff the Program top
        // level is strict (ES Module / `"use strict"` Script). Named mode: scan for a
        // strict candidate. Either way we only draw the strict names when needed.
        let need_strict_names = if whole_program {
            program_top_is_strict(ast.program()) || vcfg.desugar_class
        } else {
            program_has_strict_candidate(ast.program_mut(), &target, exclude.as_deref())
        };
        let (lean_interp_strict, eh_interp_strict) = if need_strict_names {
            (cfg.fresh_name(), cfg.fresh_name())
        } else {
            (lean_interp.clone(), eh_interp.clone())
        };
        let names = VmNames {
            lean_interp,
            eh_interp,
            lean_interp_strict,
            eh_interp_strict,
            table,
            rc,
            sy,
        };

        // The strings-decoder stub (and its `core` declaration), if strings ran
        // before us, must stay NATIVE in whole-program mode (a top-level run that
        // swallowed it into the VM would leave the virtualized `core(idx)` decode
        // calls referencing a name that no longer exists at module scope).
        let protect_native: Vec<String> = match bus.get::<crate::artifacts::DecoderAnchorArtifact>()
        {
            Ok(Some(d)) => vec![d.core_name.clone()],
            _ => Vec::new(),
        };

        let original_candidates: Vec<_> = coverage::candidates(ast.program())
            .into_iter()
            .filter(|c| {
                cfg.source_functions()
                    .is_none_or(|source| source.contains(&(c.span, c.name.clone())))
            })
            .collect();
        let mut outcomes = std::collections::HashMap::new();
        if let Some(name) = runtime_intrinsic_shadow(ast.program()) {
            for c in &original_candidates {
                outcomes.insert(c.span, Some(format!("runtime_intrinsic_shadow: {name}")));
            }
            coverage::report(&original_candidates, &outcomes, vcfg, notes)?;
            return Ok(());
        }
        let mut class_used = false;
        if whole_program && vcfg.desugar_class {
            let mut v = Virtualizer {
                target: "*",
                exclude: exclude.as_deref(),
                names: &names,
                tb: &mut tb,
                used: false,
                strict_stack: vec![program_top_is_strict(ast.program())],
                excluded: Vec::new(),
                outcomes: &mut outcomes,
                class_methods_only: true,
                source_functions: cfg.source_functions(),
            };
            ast.program_mut().visit_mut_with(&mut v);
            class_used = v.used;
        }
        let used = if whole_program {
            // Phase 1: all-or-nothing top-level wrapper. Returns true iff the whole
            // top level was virtualized (else the program is left native).
            virtualize_whole_program(
                ast.program_mut(),
                &names,
                &mut tb,
                exclude.as_deref(),
                &protect_native,
            )
        } else {
            let mut v = Virtualizer {
                target: &target,
                exclude: exclude.as_deref(),
                names: &names,
                tb: &mut tb,
                used: false,
                strict_stack: vec![program_top_is_strict(ast.program())],
                excluded: Vec::new(),
                outcomes: &mut outcomes,
                class_methods_only: false,
                source_functions: cfg.source_functions(),
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
            let native = coverage::candidates(ast.program());
            for c in &original_candidates {
                if outcomes.contains_key(&c.span) {
                    continue;
                }
                let reason = if exclude.as_deref().is_some_and(|g| {
                    original_candidates
                        .iter()
                        .any(|p| p.span <= c.span && c.span < p.end && glob::matches(g, &p.name))
                }) {
                    Some("excluded".to_string())
                } else if let Some(c) = native.iter().find(|n| n.span == c.span && n.name == c.name)
                {
                    Some(c.reason.unwrap_or("native_partition").to_string())
                } else if used {
                    original_candidates
                        .iter()
                        .filter(|p| p.span <= c.span && c.span < p.end)
                        .find_map(|p| p.reason.filter(|r| *r != "arrow"))
                        .map(str::to_string)
                } else {
                    Some("native_partition".to_string())
                };
                outcomes.insert(c.span, reason);
            }
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

        if !used {
            // Nothing virtualized: no table, no artifact (a reader that declared
            // `vm_table` soft-degrades on the absent artifact). The names drawn above
            // are simply unused — harmless and deterministic.
            return Ok(());
        }

        // Emit the shared prologue (rc/sy aliases, interpreter(s), table `var`) under
        // the SAME names the thunks reference, and splice it at module top — ABOVE
        // every thunk, the ordering guarantee the table initializer relies on.
        let vt = tb.finish(&names)?;
        splice_prologue(ast.program_mut(), vt.prologue);

        bus.put(VmTableArtifact {
            interpreter_names: vec![
                names.lean_interp,
                names.eh_interp,
                names.lean_interp_strict,
                names.eh_interp_strict,
            ],
            program_table_name: names.table,
        })
        .map_err(|e| mangler_core::Error::transform(self.id(), e.to_string()))?;

        Ok(())
    }
}

/// The mutable walk: virtualize each eligible named function in place.
struct Virtualizer<'a> {
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
    source_functions: Option<&'a std::collections::HashSet<(u32, String)>>,
}

impl Virtualizer<'_> {
    /// Try to virtualize `function` (whose inferred name is `name`). Returns true if
    /// its body was replaced with a thunk. **Bail-to-safe**: any reason to skip
    /// returns false and leaves the function untouched (never a miscompile).
    fn try_virtualize(&mut self, name: &str, function: &mut Function) -> bool {
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
        if let Some(excl) = self.exclude
            && glob::matches(excl, name)
        {
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
        if parameter_dynamic_scope(function) {
            self.outcomes.insert(
                function.span.lo.0,
                Some("parameter_direct_eval".to_string()),
            );
            return false;
        }
        let body = match &function.body {
            Some(b) => b,
            None => return false,
        };
        // Structural eligibility (with/eval/await/yield + sloppy arguments-alias bail
        // + §5a `arguments.callee/.caller` bail).
        if let Eligibility::Skip(reason) = classify_body(&function.params, body)
            && !matches!(reason, "arguments_alias" | "arguments_callee")
        {
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
        let native_parameters = needs_native_parameters(function);
        let opts = CompileOptions {
            exclude: self.exclude,
            divert_ineligible: false,
            live_captures: true,
            native_parameters,
        };
        // Native JavaScript owns parameter initialization. Body parameter references
        // use the same lazy binding descriptors as outer variables, preserving defaults,
        // destructuring, rest, aliases captured by defaults, and Function.length.
        if native_parameters && parameter_body_collision(function) {
            self.outcomes.insert(
                function.span.lo.0,
                Some("parameter_body_redeclaration".to_string()),
            );
            return false;
        }
        let params = if native_parameters {
            &[][..]
        } else {
            &function.params[..]
        };
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
        let chunk = self.tb.add_strict(compiled, is_strict);

        // Build the re-entry thunk and install it as the function's new body. A parse
        // failure here (it never should, the source is machine-generated) is a sound
        // skip — but note the chunk is already in the table; leaving the original body
        // would call the un-thunked function while its table entry sits unused, which
        // is fine (extra dead table entry, never a miscompile).
        let interp = self.names.interp_for(chunk.needs_eh, chunk.is_strict);
        let stmts = match thunk_stmts(
            interp,
            &self.names.table,
            &chunk,
            has_use_strict_directive(body),
        ) {
            Some(s) => s,
            None => return false,
        };
        // A successfully compiled body also protects nested child chunks, except
        // explicit native escapes. Record original spans before installing the thunk.
        let descendants = coverage::function_candidates(function);
        for candidate in &descendants {
            let reason = self
                .exclude
                .filter(|g| {
                    descendants.iter().any(|parent| {
                        parent.span <= candidate.span
                            && candidate.span < parent.end
                            && glob::matches(g, &parent.name)
                    })
                })
                .map(|_| "excluded".to_string());
            self.outcomes.insert(candidate.span, reason);
        }
        self.outcomes.insert(function.span.lo.0, None);
        function.body = Some(BlockStmt {
            span: DUMMY_SP,
            stmts,
            ..Default::default()
        });
        self.used = true;
        true
    }

    fn is_source(&self, name: &str, function: &Function) -> bool {
        self.source_functions
            .is_none_or(|source| source.contains(&(function.span.lo.0, name.to_string())))
    }

    /// The strictness inherited by the scope currently being walked (the top of the
    /// stack). Used to decide a function's strictness before `try_virtualize`.
    fn current_strict(&self) -> bool {
        *self.strict_stack.last().unwrap_or(&false)
    }

    /// Whether the body of a function/arrow we are about to descend into is strict:
    /// strict if the enclosing scope is strict OR the body opens with `"use strict"`.
    fn body_is_strict(&self, body: Option<&BlockStmt>) -> bool {
        self.current_strict() || body.is_some_and(has_use_strict_directive)
    }
}

impl VisitMut for Virtualizer<'_> {
    fn visit_mut_class(&mut self, class: &mut swc_core::ecma::ast::Class) {
        self.strict_stack.push(true);
        class.visit_mut_children_with(self);
        self.strict_stack.pop();
    }

    fn visit_mut_class_method(&mut self, method: &mut ClassMethod) {
        self.strict_stack.push(true);
        let only = self.class_methods_only;
        self.class_methods_only = false;
        let replaced = method.kind == MethodKind::Method
            && static_prop_key_name(&method.key)
                .is_some_and(|name| self.try_virtualize(&name, &mut method.function));
        self.class_methods_only = only;
        if !replaced {
            method.visit_mut_children_with(self);
        }
        self.strict_stack.pop();
    }

    fn visit_mut_fn_decl(&mut self, n: &mut FnDecl) {
        // Own ident is the canonical name for a function declaration.
        let name = n.ident.sym.to_string();
        if !self.is_source(&name, &n.function) {
            return;
        }
        if self.try_virtualize(&name, &mut n.function) {
            return; // replaced — don't recurse into the (now-thunk) body
        }
        n.visit_mut_children_with(self);
    }

    fn visit_mut_fn_expr(&mut self, n: &mut FnExpr) {
        // Own ident (e.g. `var x = function render(){}`): own ident wins.
        if let Some(id) = n.ident.clone() {
            let name = id.sym.to_string();
            if !self.is_source(&name, &n.function) {
                return;
            }
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

    /// `const render = function(){}` — binding-name inference from the declarator's
    /// `Ident` pattern (§4.3 form 2). Arrow expressions are left as-is because the
    /// thunk body uses `arguments`, which arrows do not have; they remain native or
    /// become child chunks inside a parent virtualized function.
    fn visit_mut_var_declarator(&mut self, n: &mut VarDeclarator) {
        if let Pat::Ident(binding) = &n.name {
            let binding_name = binding.id.sym.to_string();
            if let Some(Expr::Fn(fn_expr)) = n.init.as_deref_mut()
                && fn_expr.ident.is_none()
            {
                if !self.is_source(&binding_name, &fn_expr.function) {
                    return;
                }
                // Anonymous function expression: try to virtualize under the
                // binding name. On success, stop; on skip, fall through to
                // children (the fn_expr visitor will re-encounter it but find
                // no own ident and do nothing).
                if self.try_virtualize(&binding_name, &mut fn_expr.function) {
                    return;
                }
            }
        }
        n.visit_mut_children_with(self);
    }

    /// `obj.render = function(){}` — binding-name inference from the last
    /// member-expression segment (§4.3 form 3). Arrow right-hand sides are skipped
    /// for the same reason as above (thunk uses `arguments`).
    fn visit_mut_assign_expr(&mut self, n: &mut AssignExpr) {
        // Only simple `=` assignment, not compound (`+=` etc.).
        if n.op == AssignOp::Assign
            && let Some(member_name) = last_member_key(&n.left)
            && let Expr::Fn(fn_expr) = n.right.as_mut()
            && fn_expr.ident.is_none()
            && self.try_virtualize(&member_name, &mut fn_expr.function)
        {
            return;
        }
        n.visit_mut_children_with(self);
    }

    /// `{ render: function(){} }` — key-value property binding-name inference
    /// (§4.3 form 4a). Arrow values are skipped (thunk uses `arguments`).
    fn visit_mut_key_value_prop(&mut self, n: &mut KeyValueProp) {
        if let Some(key_name) = static_prop_key_name(&n.key)
            && let Expr::Fn(fn_expr) = n.value.as_mut()
            && fn_expr.ident.is_none()
            && self.try_virtualize(&key_name, &mut fn_expr.function)
        {
            return;
        }
        n.visit_mut_children_with(self);
    }

    /// `{ render() {} }` — shorthand method property (§4.3 form 4).
    fn visit_mut_method_prop(&mut self, n: &mut MethodProp) {
        if let Some(key_name) = static_prop_key_name(&n.key)
            && self.try_virtualize(&key_name, &mut n.function)
        {
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
        self.strict_stack.push(self.body_is_strict(n.body.as_ref()));
        n.visit_mut_children_with(self);
        self.strict_stack.pop();
    }

    /// Arrows have no own `this`/`arguments` and are never virtualization entry points
    /// here, but they ARE a lexical scope whose strictness (inherited, or via their own
    /// block-body `"use strict"`) is inherited by nested functions.
    fn visit_mut_arrow_expr(&mut self, n: &mut ArrowExpr) {
        let body = match &*n.body {
            BlockStmtOrExpr::BlockStmt(b) => Some(b),
            BlockStmtOrExpr::Expr(_) => None,
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
        _ => return None,
    };
    match &member.prop {
        MemberProp::Ident(id) => Some(id.sym.to_string()),
        MemberProp::PrivateName(_) | MemberProp::Computed(_) => None,
    }
}

/// Infer a name from a static object/class property key. Returns `None` for
/// computed keys (`[expr]`) or private names (`#x`).
fn static_prop_key_name(key: &PropName) -> Option<String> {
    match key {
        PropName::Ident(id) => Some(id.sym.to_string()),
        PropName::Str(s) => s.value.as_str().map(|v| v.to_string()),
        PropName::Num(_) | PropName::BigInt(_) | PropName::Computed(_) => None,
    }
}

/// Runtime helpers live at program scope; a source binding with the same name
/// would redirect their intrinsic operations or trigger a lexical TDZ on startup.
fn runtime_intrinsic_shadow(program: &Program) -> Option<String> {
    use swc_core::ecma::visit::{Visit, VisitWith};
    struct Bindings(Option<String>);
    impl Bindings {
        fn check(&mut self, name: &str) {
            if self.0.is_none()
                && matches!(
                    name,
                    "Object"
                        | "Array"
                        | "Reflect"
                        | "String"
                        | "Symbol"
                        | "TypeError"
                        | "ReferenceError"
                )
            {
                self.0 = Some(name.to_string());
            }
        }
    }
    impl Visit for Bindings {
        fn visit_binding_ident(&mut self, binding: &BindingIdent) {
            self.check(binding.id.sym.as_ref());
        }
        fn visit_fn_decl(&mut self, f: &FnDecl) {
            self.check(f.ident.sym.as_ref());
        }
        fn visit_class_decl(&mut self, c: &ClassDecl) {
            self.check(c.ident.sym.as_ref());
        }
        fn visit_function(&mut self, _: &Function) {}
        fn visit_arrow_expr(&mut self, _: &ArrowExpr) {}
        fn visit_import_specifier(&mut self, s: &ImportSpecifier) {
            self.check(s.local().sym.as_ref());
        }
    }
    let mut bindings = Bindings(None);
    program.visit_with(&mut bindings);
    bindings.0
}

/// Accessors defer resolving each source binding until the corresponding VM read.
/// Arrow functions preserve lexical `arguments` and avoid introducing a setter
/// parameter that could shadow the source name.
fn capture_descriptors(captures: &[String], strict: bool) -> String {
    let descriptors = captures
        .iter()
        .map(|name| {
            if strict && matches!(name.as_str(), "arguments" | "eval") {
                return format!("{{get:()=>{name}}}");
            }
            let value = if name == "_value" {
                "_value2"
            } else {
                "_value"
            };
            format!("{{get:()=>{name},set:({value})=>{name}={value}}}")
        })
        .collect::<Vec<_>>();
    format!("[{}]", descriptors.join(","))
}

fn parameter_dynamic_scope(function: &Function) -> bool {
    use swc_core::ecma::visit::{Visit, VisitWith};
    struct Scan(bool);
    impl Visit for Scan {
        fn visit_call_expr(&mut self, call: &CallExpr) {
            self.0 |= matches!(&call.callee, Callee::Expr(expr) if matches!(&**expr, Expr::Ident(id) if id.sym.as_ref() == "eval"));
            call.visit_children_with(self);
        }
    }
    let mut scan = Scan(false);
    function.params.visit_with(&mut scan);
    scan.0
}

/// Simple positional parameters need no accessor allocation: rebinding identifiers
/// is side-effect-free and no default initializer can expose their native scope.
/// Arguments references retain native binding ownership to preserve mapped aliases.
fn needs_native_parameters(function: &Function) -> bool {
    use swc_core::ecma::visit::{Visit, VisitWith};
    if function
        .params
        .iter()
        .any(|p| !matches!(p.pat, Pat::Ident(_)))
    {
        return true;
    }
    struct Arguments(bool);
    impl Visit for Arguments {
        fn visit_ident(&mut self, id: &Ident) {
            self.0 |= id.sym.as_ref() == "arguments";
        }
    }
    let mut arguments = Arguments(false);
    if let Some(body) = &function.body {
        body.visit_with(&mut arguments);
    }
    arguments.0
}

/// A body var/function redeclaration shares native parameter storage. Until the
/// compiler can represent that shared declaration directly, keep this shape native.
fn parameter_body_collision(function: &Function) -> bool {
    use swc_core::ecma::visit::{Visit, VisitWith};
    let mut params = std::collections::HashSet::new();
    for param in &function.params {
        mangler_jsast::analysis::binding_names(&param.pat, &mut |id| {
            params.insert(id.sym.to_string());
        });
    }
    struct Scan {
        params: std::collections::HashSet<String>,
        collision: bool,
    }
    impl Visit for Scan {
        fn visit_var_decl(&mut self, var: &VarDecl) {
            if var.kind == VarDeclKind::Var {
                for decl in &var.decls {
                    mangler_jsast::analysis::binding_names(&decl.name, &mut |id| {
                        self.collision |= self.params.contains(id.sym.as_ref());
                    });
                }
            }
            var.visit_children_with(self);
        }
        fn visit_fn_decl(&mut self, f: &FnDecl) {
            self.collision |= self.params.contains(f.ident.sym.as_ref());
        }
        fn visit_function(&mut self, _: &Function) {}
        fn visit_arrow_expr(&mut self, _: &ArrowExpr) {}
    }
    let mut scan = Scan {
        params,
        collision: false,
    };
    if let Some(body) = &function.body {
        body.visit_with(&mut scan);
    }
    scan.collision
}

/// Parse the VM re-entry body. The original function keeps its parameters and
/// arity; its body forwards actual arguments, lazy capture descriptors, and `this`.
/// Only an original own directive is reinserted: inherited strictness remains
/// inherited, avoiding forbidden directives in non-simple parameter functions.
fn thunk_stmts(interp: &str, table: &str, chunk: &Chunk, is_strict: bool) -> Option<Vec<Stmt>> {
    let caps = capture_descriptors(&chunk.captures, chunk.is_strict);
    // §5a case 1: a STRICT source function's thunk must itself be strict, so a plain
    // call (`f()`) forwards `this === undefined` (strict) rather than the boxed
    // `globalThis` a sloppy thunk would see. The `"use strict"` directive governs the
    // thunk's OWN `this` binding; the interpreter then receives the un-coerced
    // receiver. Sloppy thunks are byte-for-byte unchanged (empty prefix).
    let directive = if is_strict { "\"use strict\";" } else { "" };
    let src = format!(
        "function _v(){{{directive}return {interp}({table}[{idx}][0],{table}[{idx}][1],arguments,{caps},{cap_start},{pcount},this,true);}}",
        idx = chunk.index,
        cap_start = chunk.cap_start,
        pcount = chunk.pcount,
    );
    parse_fn_body_stmts(&src)
}

/// Parse a single function declaration `src` and return its body statements, or `None`
/// if it fails to parse. Mirrors the legacy `parse_fn_body_stmts`.
fn parse_fn_body_stmts(src: &str) -> Option<Vec<Stmt>> {
    let ast = Js.parse(src, &ParseOpts::default()).ok()?;
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

// ---------------------------------------------------------------------------
// Phase 2: whole-program virtualization with PARTITION + adaptive bisection
// (§2, §2.1, §3.1, §3.3, §5 export-bound names / cross-run cells)
// ---------------------------------------------------------------------------

use partition::{Class, Segment};

/// Partition the top-level program (§3.1), wrap each maximal run of wrappable
/// statements as its own VM chunk (bisecting a run that fails to compile, §3.3), and
/// re-emit native items (import/export, top-level await) in program order. Cross-run
/// `var`/`function`/`let`/`const` bindings are preserved via native cells (§2.1).
///
/// Returns `true` iff at least one run was virtualized (some chunk registered). On a
/// program with no wrappable runs (e.g. only imports) returns `false` and leaves the
/// program native (bail-to-safe). Determinism (§7): the classification, run
/// boundaries, cell set, and bisection split order are pure functions of the AST, so
/// the output — including partition boundaries — is byte-identical for a given seed.
///
/// Phase-2 limitation (Phase 3 owns it): no native-closure escape hatch — a run
/// containing a nested ineligible function bails its (sub-)run, which then bisects
/// down to keep that single statement native while its neighbors still virtualize.
#[allow(clippy::too_many_arguments)]
fn virtualize_whole_program(
    program: &mut Program,
    names: &VmNames,
    tb: &mut TableBuilder,
    exclude: Option<&str>,
    protect_native: &[String],
) -> bool {
    // Each chunk's strictness matches the program top level; its re-entry call
    // forwards native top-level `this`, preserving the script/module distinction.
    let strict = program_top_is_strict(program);

    // Lift the top-level body into a uniform `Vec<ModuleItem>` (a Script body has no
    // ModuleDecls, so each statement just wraps).
    let mut items: Vec<ModuleItem> = match program {
        Program::Script(s) => std::mem::take(&mut s.body)
            .into_iter()
            .map(ModuleItem::Stmt)
            .collect(),
        Program::Module(m) => std::mem::take(&mut m.body),
    };
    // Pristine copy to restore verbatim if nothing virtualizes (bail-to-safe: leave
    // the program exactly as it was — no spurious cell churn). The pristine copy is the
    // ORIGINAL (un-desugared) program, so a no-op restore is byte-identical to input.
    let pristine = items.clone();

    // Directives govern every native partition and every generated binding getter.
    // Keep them at program scope rather than compiling them as string expressions.
    let count = items.iter().take_while(|item| matches!(item, ModuleItem::Stmt(stmt) if mangler_jsast::directives::is_directive(stmt))).count();
    let directives: Vec<_> = items.drain(..count).collect();

    // §5: export-bound names must stay resolvable as module bindings. We treat them as
    // cross-run (the `export` reads them outside any run), so they are hoisted to a
    // native cell and the `export` references the cell binding. Compute BEFORE we move
    // the items into segments.
    let export_names = partition::export_bound_names(&items);

    // §3.1 classify + segment into native items and maximal wrappable runs. The
    // strings-decoder stub (and anything declaring/referencing a protected name) is
    // forced NATIVE so the partition never swallows machine-generated decode
    // infrastructure into the VM.
    let classes: Vec<Class> = partition::classify_with_protected(items, protect_native);
    let mut segs: Vec<Segment> = partition::segment(classes);

    // §2.1 cross-run binding analysis: which top-level names declared in one run are
    // referenced outside it (another run / native item / export). Restrict to the set
    // we can cell-ify SOUNDLY; a run that touches an UNSAFE cross-run name is kept
    // native wholesale (conservative, bail-to-safe).
    let cross = partition::analyze_cross_run(&segs, &export_names);
    let cells = partition::restrict_to_safe(&segs, &cross.cells, &export_names);
    // Any cross-run name NOT in the safe cell set is a hazard: a run referencing or
    // declaring it cannot fully virtualize, so we force such runs native wholesale
    // (the export / other run keeps reading the native binding).
    let unsafe_cross: std::collections::HashSet<String> =
        cross.cells.difference(&cells).cloned().collect();

    // Build the new top-level item list, run by run, splicing native items verbatim.
    let mut out_items: Vec<ModuleItem> = directives;
    let mut cells_emitted = false;
    let mut any_virtualized = false;

    for seg in std::mem::take(&mut segs) {
        match seg {
            Segment::Native(it) => out_items.push(it),
            Segment::Run(mut stmts) => {
                // A run that touches an unsafe cross-run name stays native wholesale.
                if run_touches(&stmts, &unsafe_cross) {
                    out_items.extend(stmts.into_iter().map(ModuleItem::Stmt));
                    continue;
                }
                // Cell-ify the safe cross-run names referenced/declared in this run.
                partition::cellify_run(&mut stmts, &cells);
                // Splice the native cell declarations once, above the first run that
                // emits (cells `var`-hoist to top, so above-first-run is correct).
                if !cells_emitted {
                    if let Some(decl) = partition::cell_hoist_decls(&cells) {
                        out_items.push(ModuleItem::Stmt(decl));
                    }
                    cells_emitted = true;
                }
                // Compile the run (bisecting on failure) into chunk re-entry calls
                // interleaved with any sub-statements that had to stay native.
                let produced = compile_run(stmts, names, tb, strict, exclude, &mut any_virtualized);
                out_items.extend(produced.into_iter().map(ModuleItem::Stmt));
            }
        }
    }

    if !any_virtualized {
        // Nothing virtualized — restore the PRISTINE body verbatim (bail-to-safe; no
        // cell churn). The caller then emits no table/prologue/artifact.
        set_body(program, pristine);
        return false;
    }

    set_body(program, out_items);
    true
}

/// True if any top-level statement in `stmts` references or declares a name in `set`.
fn run_touches(stmts: &[Stmt], set: &std::collections::HashSet<String>) -> bool {
    if set.is_empty() {
        return false;
    }
    use swc_core::ecma::visit::{Visit, VisitWith};
    struct Scan<'a> {
        set: &'a std::collections::HashSet<String>,
        hit: bool,
    }
    impl Visit for Scan<'_> {
        fn visit_ident(&mut self, id: &Ident) {
            if self.set.contains(id.sym.as_ref()) {
                self.hit = true;
            }
        }
    }
    let mut sc = Scan { set, hit: false };
    for s in stmts {
        s.visit_with(&mut sc);
    }
    sc.hit
}

/// Compile a maximal wrappable RUN into a sequence of top-level statements: §2.1
/// re-entry interpreter calls for the (sub-)runs that compiled, plus any single
/// statement that could not compile left NATIVE in place (§3.3 adaptive bisection).
///
/// The bisection is deterministic: a run that fails to compile as one chunk is split
/// at its MIDPOINT (left half first), each half recursively attempted; a single
/// statement that still fails is emitted native. Empty halves are skipped. This
/// preserves program order (left sub-runs precede right) and is a pure function of the
/// statement list, so the offender-isolation sequence is reproducible (§7).
fn compile_run(
    stmts: Vec<Stmt>,
    names: &VmNames,
    tb: &mut TableBuilder,
    strict: bool,
    exclude: Option<&str>,
    any_virtualized: &mut bool,
) -> Vec<Stmt> {
    if stmts.is_empty() {
        return Vec::new();
    }
    // Try the whole (sub-)run as one chunk.
    let block = BlockStmt {
        span: DUMMY_SP,
        stmts: stmts.clone(),
        ..Default::default()
    };
    // Phase 3 (§4): thread the exclude glob so a NESTED function matching it stays a
    // native closure. The ineligible-divert (async/generator/`"use strict"`/
    // structurally-ineligible nested fns → native closures) is enabled ONLY when the
    // user supplied an exclude glob — i.e. they opted into the native-closure escape
    // hatch. Without an exclude, whole-program keeps the Phase-1/2 behavior exactly
    // (an ineligible nested fn bails its run, which then bisects to keep that single
    // statement native while neighbors virtualize) — no regression, byte-for-byte.
    let opts = CompileOptions {
        exclude,
        divert_ineligible: exclude.is_some(),
        live_captures: true,
        native_parameters: false,
    };
    if let Ok(compiled) = compile_body_with_opts(&[], &block, opts) {
        let chunk = tb.add_strict(compiled, strict);
        let interp = names.interp_for(chunk.needs_eh, chunk.is_strict);
        if let Some(call) = top_level_call_stmt(interp, &names.table, &chunk) {
            *any_virtualized = true;
            return vec![call];
        }
        // Machine-generated call failed to parse (never expected) → fall through to
        // native for this (sub-)run.
        return stmts;
    }

    // Did not compile. A single statement cannot bisect further → stays native.
    if stmts.len() == 1 {
        return stmts;
    }

    // §3.3: bisect at the midpoint, left half first. But a split is only SOUND if it
    // does not sever a binding shared across the halves (a name declared at the top
    // level of one half and referenced in the other would become an invisible VM-local
    // after the split). Cross-RUN bindings were already cell-ified; an intra-run
    // binding shared across the split point is NOT a cell, so splitting here would lose
    // it. In that case keep the WHOLE failing (sub-)run native — bail-to-safe.
    let mid = stmts.len() / 2;
    if split_severs_binding(&stmts, mid) {
        return stmts;
    }
    let mut left = stmts;
    let right = left.split_off(mid);
    let mut out = compile_run(left, names, tb, strict, exclude, any_virtualized);
    out.extend(compile_run(
        right,
        names,
        tb,
        strict,
        exclude,
        any_virtualized,
    ));
    out
}

/// True if splitting `stmts` at index `mid` would sever a top-level binding shared
/// across the two halves: a `var`/`function`/`let`/`const` name declared at the top
/// level of one half that the OTHER half references. Such a name is not a cross-run
/// cell, so the split would turn it into an invisible per-chunk local. Conservative
/// (over-approximate via names): any uncertainty blocks the split.
fn split_severs_binding(stmts: &[Stmt], mid: usize) -> bool {
    use swc_core::ecma::visit::{Visit, VisitWith};
    fn top_decls(stmts: &[Stmt]) -> std::collections::HashSet<String> {
        let mut out = std::collections::HashSet::new();
        struct VarScan<'a>(&'a mut std::collections::HashSet<String>);
        impl Visit for VarScan<'_> {
            fn visit_var_decl(&mut self, v: &VarDecl) {
                if matches!(v.kind, VarDeclKind::Var) {
                    for d in &v.decls {
                        mangler_jsast::analysis::binding_names(&d.name, &mut |id| {
                            self.0.insert(id.sym.to_string());
                        });
                    }
                }
                v.visit_children_with(self);
            }
            fn visit_function(&mut self, _: &Function) {}
            fn visit_arrow_expr(&mut self, _: &ArrowExpr) {}
        }
        for s in stmts {
            s.visit_with(&mut VarScan(&mut out));
            match s {
                Stmt::Decl(Decl::Fn(f)) => {
                    out.insert(f.ident.sym.to_string());
                }
                Stmt::Decl(Decl::Var(v))
                    if matches!(v.kind, VarDeclKind::Let | VarDeclKind::Const) =>
                {
                    for d in &v.decls {
                        mangler_jsast::analysis::binding_names(&d.name, &mut |id| {
                            out.insert(id.sym.to_string());
                        });
                    }
                }
                _ => {}
            }
        }
        out
    }
    fn refs(stmts: &[Stmt]) -> std::collections::HashSet<String> {
        let mut out = std::collections::HashSet::new();
        struct RefScan<'a>(&'a mut std::collections::HashSet<String>);
        impl Visit for RefScan<'_> {
            fn visit_ident(&mut self, id: &Ident) {
                self.0.insert(id.sym.to_string());
            }
            fn visit_member_expr(&mut self, m: &MemberExpr) {
                m.obj.visit_with(self);
                if let MemberProp::Computed(c) = &m.prop {
                    c.visit_with(self);
                }
            }
        }
        for s in stmts {
            s.visit_with(&mut RefScan(&mut out));
        }
        out
    }
    let (left, right) = stmts.split_at(mid);
    let ld = top_decls(left);
    let rd = top_decls(right);
    let lr = refs(left);
    let rr = refs(right);
    // left-declared name used in right, or right-declared name used in left.
    ld.iter().any(|n| rr.contains(n)) || rd.iter().any(|n| lr.contains(n))
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
fn top_level_call_stmt(interp: &str, table: &str, chunk: &Chunk) -> Option<Stmt> {
    let caps = capture_descriptors(&chunk.captures, chunk.is_strict);
    // Native top-level `this` is globalThis in scripts (including strict scripts)
    // and undefined in modules. Preserve the parser/runtime distinction directly.
    let this_expr = "this";
    let src = format!(
        "{interp}({table}[{idx}][0],{table}[{idx}][1],[],{caps},{cap_start},0,{this_expr},true);",
        idx = chunk.index,
        cap_start = chunk.cap_start,
    );
    parse_one_top_stmt(&src)
}

/// Parse a single statement `src` and return it, or `None` on parse failure.
fn parse_one_top_stmt(src: &str) -> Option<Stmt> {
    let ast = Js.parse(src, &ParseOpts::default()).ok()?;
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

/// §5a byte-identity guard: a cheap over-approximate pre-scan for "does this program
/// contain a function we might virtualize STRICT?". Only when this is true do we draw
/// the two extra strict-interpreter names — so a fully-sloppy program's name
/// allocation (and thus its byte output) is unchanged. Over-approximation is safe: a
/// false positive merely draws two unused names; a strict function that later bails
/// compilation simply leaves the strict interpreter unemitted (the names are unused,
/// harmless and deterministic).
fn program_has_strict_candidate(program: &Program, target: &str, exclude: Option<&str>) -> bool {
    use swc_core::ecma::visit::{Visit, VisitWith};

    struct Scan<'a> {
        target: &'a str,
        exclude: Option<&'a str>,
        strict_stack: Vec<bool>,
        found: bool,
    }
    impl Scan<'_> {
        fn cur(&self) -> bool {
            *self.strict_stack.last().unwrap_or(&false)
        }
        /// Record a named candidate whose body would be virtualized strict.
        fn consider(&mut self, name: &str, body_strict: bool) {
            if self.found || !glob::matches(self.target, name) {
                return;
            }
            if let Some(e) = self.exclude
                && glob::matches(e, name)
            {
                return;
            }
            if self.cur() || body_strict {
                self.found = true;
            }
        }
    }
    impl Visit for Scan<'_> {
        fn visit_function(&mut self, n: &Function) {
            let strict = self.cur() || n.body.as_ref().is_some_and(has_use_strict_directive);
            self.strict_stack.push(strict);
            n.visit_children_with(self);
            self.strict_stack.pop();
        }
        fn visit_arrow_expr(&mut self, n: &ArrowExpr) {
            let body_strict = match &*n.body {
                BlockStmtOrExpr::BlockStmt(b) => has_use_strict_directive(b),
                BlockStmtOrExpr::Expr(_) => false,
            };
            self.strict_stack.push(self.cur() || body_strict);
            n.visit_children_with(self);
            self.strict_stack.pop();
        }
        fn visit_fn_decl(&mut self, n: &FnDecl) {
            self.consider(
                n.ident.sym.as_ref(),
                n.function
                    .body
                    .as_ref()
                    .is_some_and(has_use_strict_directive),
            );
            n.visit_children_with(self);
        }
        fn visit_fn_expr(&mut self, n: &FnExpr) {
            if let Some(id) = &n.ident {
                self.consider(
                    id.sym.as_ref(),
                    n.function
                        .body
                        .as_ref()
                        .is_some_and(has_use_strict_directive),
                );
            }
            n.visit_children_with(self);
        }
        fn visit_var_declarator(&mut self, n: &VarDeclarator) {
            if let (Pat::Ident(b), Some(Expr::Fn(fe))) = (&n.name, n.init.as_deref())
                && fe.ident.is_none()
            {
                self.consider(
                    b.id.sym.as_ref(),
                    fe.function
                        .body
                        .as_ref()
                        .is_some_and(has_use_strict_directive),
                );
            }
            n.visit_children_with(self);
        }
        fn visit_assign_expr(&mut self, n: &AssignExpr) {
            if n.op == AssignOp::Assign
                && let (Some(name), Expr::Fn(fe)) = (last_member_key(&n.left), n.right.as_ref())
                && fe.ident.is_none()
            {
                self.consider(
                    &name,
                    fe.function
                        .body
                        .as_ref()
                        .is_some_and(has_use_strict_directive),
                );
            }
            n.visit_children_with(self);
        }
        fn visit_key_value_prop(&mut self, n: &KeyValueProp) {
            if let (Some(name), Expr::Fn(fe)) = (static_prop_key_name(&n.key), n.value.as_ref())
                && fe.ident.is_none()
            {
                self.consider(
                    &name,
                    fe.function
                        .body
                        .as_ref()
                        .is_some_and(has_use_strict_directive),
                );
            }
            n.visit_children_with(self);
        }
        fn visit_method_prop(&mut self, n: &MethodProp) {
            if let Some(name) = static_prop_key_name(&n.key) {
                self.consider(
                    &name,
                    n.function
                        .body
                        .as_ref()
                        .is_some_and(has_use_strict_directive),
                );
            }
            n.visit_children_with(self);
        }
    }

    let mut scan = Scan {
        target,
        exclude,
        strict_stack: vec![program_top_is_strict(program)],
        found: false,
    };
    program.visit_with(&mut scan);
    scan.found
}

/// True if `body` begins with its OWN `"use strict"` directive (strict regardless of
/// the enclosing scope). Strict functions are now virtualized via a strict thunk +
/// strict interpreter variant (§5a); this still computes the function's own
/// directive contribution to the inherited-strictness attribute.
/// Scans only the leading directive prologue (leading string-literal statements).
fn has_use_strict_directive(body: &BlockStmt) -> bool {
    stmts_open_with_use_strict(&body.stmts)
}

/// True if a statement list opens with a `"use strict"` directive prologue (leading
/// string-literal expression statements, one of which is exactly `"use strict"`).
fn stmts_open_with_use_strict(stmts: &[Stmt]) -> bool {
    for s in stmts {
        match s {
            Stmt::Expr(es) => match &*es.expr {
                Expr::Lit(Lit::Str(lit)) => {
                    if lit.value.as_str() == Some("use strict") {
                        return true;
                    }
                    // another directive (e.g. "use asm") — keep scanning the prologue.
                }
                _ => return false, // first non-string-literal expr ends the prologue
            },
            _ => return false, // first non-expr statement ends the prologue
        }
    }
    false
}

#[cfg(test)]
mod tests;
