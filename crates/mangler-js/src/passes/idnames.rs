//! Confusing local-identifier renamer.
//!
//! Replaces swc's short-name local mangle with a seeded confusing scheme:
//!
//! * `Hex`   → `_0x4e2a`-style hexadecimal names (High/Max default), via
//!   [`FileConfig::fresh_name`](crate::config::FileConfig::fresh_name) — the
//!   file-wide-unique, deterministic allocator shared across all passes.
//! * `Soup`  → `lIl1I`-style homoglyph names (opt-in), generated from the
//!   per-pass `rng`.
//! * `Short` → defer to swc's built-in short-name mangle: rename NOTHING here and
//!   leave `MangleControlArtifact.suppress_builtin_mangle == false`.
//!
//! Locals only and runtime-free (names only — zero runtime cost). It declares
//! `reads() = [resolved_scopes()]`, so the scheduler lands it POST-resolver,
//! seeing a fully-marked tree; the resolver marks come off
//! [`ResolvedScopesArtifact`].
//!
//! ## How it stays sound
//!
//! After the resolver, every identifier carries a `SyntaxContext`. A binding and
//! all of its references share the same `(sym, ctxt)` pair (`Id`); free globals
//! carry `unresolved_mark`, top-level bindings carry `top_level_mark`, and
//! property names / labels are not value-namespace `Ident`s here. A local is
//! therefore exactly an `Ident` whose `ctxt` is non-empty and whose outer mark
//! is neither `unresolved_mark` nor `top_level_mark` — the same set swc would
//! mangle under `top_level = false` (see [`analysis::is_local`]).
//!
//! Because a binding and its references share their `Id`, renaming every `Ident`
//! whose `Id` is in the map renames the binding and all uses *consistently*, with
//! no per-scope bookkeeping. Correctness rests on three invariants:
//!
//! 1. Each distinct binding has a distinct `Id` (resolver guarantee), so two
//!    different bindings never collapse to one name.
//! 2. Generated names avoid every identifier name already present in the file
//!    (the `reserved` set), so a renamed local can never capture a global or a
//!    preserved top-level name. (`fresh_name` additionally reserves every source
//!    ident file-wide; the local `taken` set guards same-pass collisions.)
//! 3. Distinct locals get distinct generated names, so renamed locals never
//!    collide with each other.
//!
//! Two shorthand forms make an `Ident` double as a *property key*; renaming them
//! naively would change the key, so they are expanded to explicit key/value
//! form, preserving the original property name:
//!
//! * object-pattern destructuring shorthand `{ x }` / `{ x = d }`
//!   (`ObjectPatProp::Assign`), and
//! * object-literal shorthand `{ x }` (`Prop::Shorthand`).
//!
//! Labels live in a separate namespace and are left untouched. Files containing
//! direct `eval` or `with` are skipped entirely (no confusing scheme applied,
//! `suppress_builtin_mangle` left `false`), because `eval` can reference locals
//! by their source name and `with` introduces dynamic scope the resolver cannot
//! model; the terminal codegen then falls back to swc's short-name mangle.

use crate::artifacts::{MangleControlArtifact, ResolvedScopesArtifact};
use crate::config::FileConfig;
use mangler_core::{Error, Notes, Result, Rng};
use mangler_jsast::Js;
use mangler_jsast::analysis::{is_direct_eval_callee, is_local};
use mangler_passgraph::{ArtifactBus, Pass, Resource};
use std::collections::{HashMap, HashSet};
use swc_core::common::Mark;
use swc_core::ecma::ast::*;
use swc_core::ecma::atoms::Atom;
use swc_core::ecma::visit::{Visit, VisitMut, VisitMutWith, VisitWith};

#[cfg(test)]
mod tests;

/// Confusing local-identifier renamer as a scheduler [`Pass`].
///
/// `reads() = [resolved_scopes()]` (lands post-resolver); `writes() =
/// [mangle_control()]` (the terminal codegen reads it to decide whether swc's
/// built-in mangle runs).
pub struct IdNamesPass;

impl Pass<Js, FileConfig> for IdNamesPass {
    fn id(&self) -> &'static str {
        "idnames"
    }

    /// Needs the resolver marks to classify locals, so the scheduler orders it
    /// after the resolver pseudo-pass.
    fn reads(&self) -> &[Resource] {
        const R: &[Resource] = &[Resource::resolved_scopes()];
        R
    }

    /// Produces the local-mangling decision the terminal codegen consumes.
    fn writes(&self) -> &[Resource] {
        const W: &[Resource] = &[Resource::mangle_control()];
        W
    }

    /// Gated by the mangle knob. When disabled, the pass does not run, no
    /// `MangleControlArtifact` is written, and the codegen soft-degrades to its
    /// default (which keeps swc mangle off too, since `mangle.enabled` is false).
    fn enabled(&self, cfg: &FileConfig) -> bool {
        cfg.resolved().passes.mangle.enabled
    }

    fn run(
        &self,
        ast: &mut <Js as mangler_core::Language>::Ast,
        cfg: &FileConfig,
        rng: &mut Rng,
        bus: &mut ArtifactBus,
        _notes: &mut Notes,
    ) -> Result<()> {
        // The resolver always precedes us (declared read), so the marks are
        // present. Defensive fallback to fresh marks only if absent (no resolver
        // mark matches a fresh one, so every ident would classify as a local —
        // but this branch is never taken in the real schedule).
        let (unresolved_mark, top_level_mark) = bus
            .get::<ResolvedScopesArtifact>()
            .map_err(|e| Error::transform(self.id(), e.to_string()))?
            .map(|a| (a.unresolved_mark, a.top_level_mark))
            .unwrap_or_else(|| (Mark::new(), Mark::new()));

        // `--keep-names` globs (locals matching one are never renamed; on the
        // fallback path swc's mangle reserves them too).
        let keep_globs = cfg.resolved().passes.mangle.keep_names.clone();

        let keep = KeepSet::new(&keep_globs);
        let mut reserved = ReservedNames {
            keep: &keep,
            names: HashSet::new(),
            bindings: HashSet::new(),
        };
        ast.program().visit_with(&mut reserved);
        let preserved = reserved.bindings;
        let mut reserved: Vec<String> = reserved.names.into_iter().collect();
        reserved.sort();

        let suppress = rename(
            ast.program_mut(),
            cfg,
            rng,
            unresolved_mark,
            top_level_mark,
            &keep_globs,
            &preserved,
        );

        // The terminal codegen reads this to decide swc-mangle on/off and which
        // names to reserve on the fallback path.
        bus.put(MangleControlArtifact {
            suppress_builtin_mangle: suppress,
            reserved,
        })
        .map_err(|e| Error::transform(self.id(), e.to_string()))?;
        Ok(())
    }
}

/// Rename resolver-classified locals to seeded confusing names.
///
/// Returns `true` (→ `suppress_builtin_mangle`) iff a renaming scheme was applied
/// (so the caller disables swc's built-in mangle to avoid double-renaming).
/// Returns `false` when the program is left untouched — when the scheme is
/// `Short`, or the file uses direct `eval`/`with` — in which case swc's
/// short-name mangle should run as the fallback.
fn rename(
    program: &mut Program,
    cfg: &FileConfig,
    rng: &mut Rng,
    unresolved_mark: Mark,
    top_level_mark: Mark,
    keep_globs: &[String],
    preserved: &HashSet<Id>,
) -> bool {
    let scheme = match cfg.resolved().passes.mangle.naming {
        // `Short` defers to swc's built-in short-name mangle.
        mangler_config::IdNaming::Short => return false,
        mangler_config::IdNaming::Hex => Scheme::Hex,
        mangler_config::IdNaming::Soup => Scheme::Soup,
    };

    let keep = KeepSet::new(keep_globs);

    // 1. Single read-only walk over the resolved tree that BOTH (a) detects the
    //    eval/with bail condition and (b) collects every local `(sym, ctxt)`
    //    occurrence (in first-seen order, for deterministic name assignment)
    //    plus the set of all identifier names already present (so generated
    //    names never collide → no capture). Fusing is sound because no RNG /
    //    fresh_name draw happens during collection: when eval/with is found we
    //    bail below before any name draw and before deciding the suppress flag.
    let mut collector = Collector {
        unresolved_mark,
        top_level_mark,
        locals: Vec::new(),
        seen: HashSet::new(),
        reserved: HashSet::new(),
        eval_or_with: false,
        keep: &keep,
        preserved,
    };
    program.visit_with(&mut collector);

    // Bail on constructs that make local renaming unsound. Fall back to swc's
    // short-name mangle (which is itself eval/with-aware). Discards the collected
    // locals untouched — no name draw or flag mutation has occurred.
    if collector.eval_or_with {
        return false;
    }

    // The scheme is active even when there is nothing to rename (an empty or
    // globals-only file): suppressing swc mangle keeps it disabled, which is a
    // no-op either way.
    if collector.locals.is_empty() {
        return true;
    }

    // 2. Assign each distinct local a fresh confusing name, avoiding collisions
    //    with any existing identifier (`reserved`) and with each other. Driven
    //    entirely by the seeded allocator / `rng`, so output is reproducible.
    let mut taken = collector.reserved;
    let mut map: HashMap<Id, Atom> = HashMap::with_capacity(collector.locals.len());
    for id in collector.locals {
        let name = gen_name(cfg, rng, scheme, &mut taken);
        map.insert(id, name);
    }

    // 3. Rewrite. Labels are skipped (separate namespace, left untouched).
    let mut renamer = Renamer { map };
    program.visit_mut_with(&mut renamer);
    true
}

#[derive(Clone, Copy)]
enum Scheme {
    Hex,
    Soup,
}

// ---------------------------------------------------------------------------
// --keep-names glob matching (locals whose name matches are never renamed)
// ---------------------------------------------------------------------------

/// A compiled `--keep-names` keep set. Each entry is a `*`/`?` glob (the same
/// surface the legacy `glob::Pattern` matched); an identifier is preserved iff
/// its symbol matches any entry. Empty (the common case) is a fast no-op.
struct KeepSet {
    globs: Vec<String>,
}

impl KeepSet {
    fn new(globs: &[String]) -> Self {
        KeepSet {
            globs: globs.to_vec(),
        }
    }

    fn is_empty(&self) -> bool {
        self.globs.is_empty()
    }

    /// True iff `name` matches any keep glob.
    fn matches(&self, name: &str) -> bool {
        self.globs.iter().any(|g| glob_match(g, name))
    }
}

/// SWC reserves concrete symbols, not glob expressions. Collect before custom
/// renaming so the fallback and short naming paths share the same keep contract.
struct ReservedNames<'a> {
    keep: &'a KeepSet,
    names: HashSet<String>,
    bindings: HashSet<Id>,
}

fn anonymous_definition(expression: &Expr) -> bool {
    match expression {
        Expr::Arrow(_)
        | Expr::Fn(FnExpr { ident: None, .. })
        | Expr::Class(ClassExpr { ident: None, .. }) => true,
        Expr::Paren(parenthesized) => anonymous_definition(&parenthesized.expr),
        _ => false,
    }
}

impl ReservedNames<'_> {
    fn preserve(&mut self, identifier: &Ident) {
        self.names.insert(identifier.sym.to_string());
        self.bindings.insert(identifier.to_id());
    }
}

impl Visit for ReservedNames<'_> {
    fn visit_fn_decl(&mut self, function: &FnDecl) {
        self.preserve(&function.ident);
        function.visit_children_with(self);
    }
    fn visit_fn_expr(&mut self, function: &FnExpr) {
        if let Some(name) = &function.ident {
            self.preserve(name);
        }
        function.visit_children_with(self);
    }
    fn visit_class_decl(&mut self, class: &ClassDecl) {
        self.preserve(&class.ident);
        class.visit_children_with(self);
    }
    fn visit_class_expr(&mut self, class: &ClassExpr) {
        if let Some(name) = &class.ident {
            self.preserve(name);
        }
        class.visit_children_with(self);
    }
    fn visit_var_declarator(&mut self, declaration: &VarDeclarator) {
        if let (Pat::Ident(binding), Some(value)) = (&declaration.name, &declaration.init)
            && anonymous_definition(value)
        {
            self.preserve(&binding.id);
        }
        declaration.visit_children_with(self);
    }
    fn visit_assign_expr(&mut self, assignment: &AssignExpr) {
        if matches!(
            assignment.op,
            AssignOp::Assign | AssignOp::AndAssign | AssignOp::OrAssign | AssignOp::NullishAssign
        ) && anonymous_definition(&assignment.right)
            && let AssignTarget::Simple(SimpleAssignTarget::Ident(binding)) = &assignment.left
        {
            self.preserve(&binding.id);
        }
        assignment.visit_children_with(self);
    }
    fn visit_assign_pat(&mut self, pattern: &AssignPat) {
        if anonymous_definition(&pattern.right)
            && let Pat::Ident(binding) = &*pattern.left
        {
            self.preserve(&binding.id);
        }
        pattern.visit_children_with(self);
    }
    fn visit_assign_pat_prop(&mut self, pattern: &AssignPatProp) {
        if pattern
            .value
            .as_ref()
            .is_some_and(|value| anonymous_definition(value))
        {
            self.preserve(&pattern.key.id);
        }
        pattern.visit_children_with(self);
    }
    fn visit_ident(&mut self, ident: &Ident) {
        if self.keep.matches(ident.sym.as_ref()) {
            self.names.insert(ident.sym.to_string());
        }
    }
}

/// Minimal shell-style glob match supporting `*` (any run, incl. empty) and `?`
/// (one char). Every other char is literal. Mirrors the subset of
/// `glob::Pattern` the legacy keep-names path relied on; recursion-free
/// backtracking on bytes (identifier names are ASCII-ish and short).
fn glob_match(pat: &str, name: &str) -> bool {
    let p: Vec<char> = pat.chars().collect();
    let s: Vec<char> = name.chars().collect();
    let (mut pi, mut si) = (0usize, 0usize);
    // Last position where we matched a `*` and the input index then; used to
    // backtrack when a later literal fails.
    let mut star: Option<(usize, usize)> = None;
    while si < s.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == s[si]) {
            pi += 1;
            si += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, si));
            pi += 1;
        } else if let Some((star_pi, star_si)) = star {
            // Backtrack: let the `*` absorb one more input char.
            pi = star_pi + 1;
            si = star_si + 1;
            star = Some((star_pi, si));
        } else {
            return false;
        }
    }
    // Trailing `*`s match the empty suffix.
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

// ---------------------------------------------------------------------------
// Collection (also detects the eval/with soundness bail in the same walk)
// ---------------------------------------------------------------------------

struct Collector<'a> {
    unresolved_mark: Mark,
    top_level_mark: Mark,
    /// Distinct local binding `Id`s, in first-seen DFS order (deterministic).
    locals: Vec<Id>,
    seen: HashSet<Id>,
    /// Every value-namespace identifier name present in the file. Generated
    /// names must avoid all of these so a renamed local cannot capture a global
    /// or a preserved top-level name.
    reserved: HashSet<Atom>,
    /// Set when a direct `eval(...)` call or a `with` statement is seen — either
    /// makes local renaming unsound, so the caller bails to swc mangle.
    eval_or_with: bool,
    /// Names to PRESERVE (`--keep-names`): a local whose symbol matches a keep
    /// glob is never collected for renaming, so it keeps its source name.
    keep: &'a KeepSet,
    /// Bindings whose spelling supplies an observable function or class name.
    preserved: &'a HashSet<Id>,
}

impl Visit for Collector<'_> {
    // -- eval / with detection (soundness bail) --
    //
    // A direct `eval` can reference locals by source name, and `with` introduces
    // dynamic scope the resolver cannot model; either forces the caller to forgo
    // the confusing scheme. Detection is folded into this walk (no separate
    // traversal); collection continues regardless, since the caller discards the
    // result before any name draw when the flag is set.

    fn visit_with_stmt(&mut self, n: &WithStmt) {
        self.eval_or_with = true;
        n.visit_children_with(self);
    }

    fn visit_call_expr(&mut self, n: &CallExpr) {
        // Direct `eval(...)` (shared paren-peeling policy). Conservative: a local
        // function literally named `eval` also bails — safe, it only forgoes the
        // confusing scheme for that file.
        if is_direct_eval_callee(&n.callee) {
            self.eval_or_with = true;
        }
        n.visit_children_with(self);
    }

    fn visit_ident(&mut self, n: &Ident) {
        self.reserved.insert(n.sym.clone());
        // A --keep-names match is preserved: reserve its name (so other renamed
        // locals avoid it) but never collect it as a rename target.
        if self.preserved.contains(&n.to_id())
            || (!self.keep.is_empty() && self.keep.matches(n.sym.as_ref()))
        {
            return;
        }
        if is_local(n.ctxt, self.unresolved_mark, self.top_level_mark) {
            let id = n.to_id();
            if self.seen.insert(id.clone()) {
                self.locals.push(id);
            }
        }
    }

    // Labels are a separate namespace: reserve the name (so generated names avoid
    // it) but never treat it as a renamable local. Descend into the body only.
    fn visit_labeled_stmt(&mut self, n: &LabeledStmt) {
        self.reserved.insert(n.label.sym.clone());
        n.body.visit_with(self);
    }

    fn visit_break_stmt(&mut self, n: &BreakStmt) {
        if let Some(l) = &n.label {
            self.reserved.insert(l.sym.clone());
        }
    }

    fn visit_continue_stmt(&mut self, n: &ContinueStmt) {
        if let Some(l) = &n.label {
            self.reserved.insert(l.sym.clone());
        }
    }
}

// ---------------------------------------------------------------------------
// Name generation
// ---------------------------------------------------------------------------

/// Produce a fresh name not already in `taken`, then mark it taken.
///
/// `Soup` has bounded entropy, so it tries the homoglyph generator up to 64
/// times before falling through. `Hex` is already monotone-unique
/// (`fresh_name()` is a strictly increasing allocator), so it goes straight to
/// the fallback — the retry loop would only burn allocator slots without changing
/// the outcome, leaving a discontiguous gap in the `_0x` space.
///
/// Termination is guaranteed: the fallback `fresh_name()` yields collision-free
/// names against a finite `taken` set within `|taken| + 1` iterations, so this
/// never hangs even if the scheme's entropy is exhausted.
fn gen_name(cfg: &FileConfig, rng: &mut Rng, scheme: Scheme, taken: &mut HashSet<Atom>) -> Atom {
    if let Scheme::Soup = scheme {
        for _ in 0..64 {
            let atom = Atom::from(soup_name(rng));
            if taken.insert(atom.clone()) {
                return atom;
            }
        }
    }
    loop {
        let atom = Atom::from(cfg.fresh_name());
        if taken.insert(atom.clone()) {
            return atom;
        }
    }
}

/// A homoglyph "soup" name: visually confusable glyphs. The first character is a
/// letter (an identifier cannot start with a digit); later characters add the
/// confusable digit homoglyphs (`l`/`1`, `O`/`0`, `S`/`5`, `Z`/`2`, `B`/`8`,
/// `G`/`6`). Length 6..=9. Collisions are resolved by `gen_name`'s retry loop.
///
/// The alphabet contains no keyword-forming letters (no `a`/`e`/`f`/`n`/`r`/`t`/
/// `u`/`c`/`d`/`h`/`v`/`w`/`y`…), so a generated name can never spell a JS
/// reserved word — only a fresh, never-a-keyword identifier.
fn soup_name(rng: &mut Rng) -> String {
    const FIRST: &[char] = &['l', 'I', 'O', 'o', 'i', 'L', 'S', 'Z', 'B', 'G'];
    const REST: &[char] = &[
        'l', 'I', '1', 'O', 'o', '0', 'i', 'L', 'S', '5', 'Z', '2', 'B', '8', 'G', '6',
    ];
    let len = 6 + rng.pick(4);
    let mut s = String::with_capacity(len);
    s.push(FIRST[rng.pick(FIRST.len())]);
    for _ in 1..len {
        s.push(REST[rng.pick(REST.len())]);
    }
    s
}

// ---------------------------------------------------------------------------
// Rewrite
// ---------------------------------------------------------------------------

struct Renamer {
    map: HashMap<Id, Atom>,
}

impl VisitMut for Renamer {
    fn visit_mut_ident(&mut self, n: &mut Ident) {
        if let Some(new_sym) = self.map.get(&n.to_id()) {
            n.sym = new_sym.clone();
        }
    }

    /// Expand object-literal shorthand `{ x }` → `{ x: <renamed> }` when `x` is a
    /// renamed local, so the property key `x` is preserved. Other prop kinds
    /// descend normally (values, method bodies, and nested objects get renamed;
    /// `PropName::Ident` keys are `IdentName`s with no context and are untouched).
    fn visit_mut_prop(&mut self, n: &mut Prop) {
        if let Prop::Shorthand(ident) = n {
            if let Some(new_sym) = self.map.get(&ident.to_id()).cloned() {
                let orig_sym = ident.sym.clone();
                let span = ident.span;
                let mut renamed = ident.clone();
                renamed.sym = new_sym;
                *n = Prop::KeyValue(KeyValueProp {
                    key: PropName::Ident(IdentName {
                        span,
                        sym: orig_sym,
                    }),
                    value: Box::new(Expr::Ident(renamed)),
                });
            }
            // A shorthand whose binding is not renamed has no inner nodes to
            // visit; leave it as-is.
            return;
        }
        n.visit_mut_children_with(self);
    }

    /// Expand object-pattern destructuring shorthand `{ x }` / `{ x = d }` →
    /// `{ x: <renamed> }` / `{ x: <renamed> = d }` when `x` is a renamed local,
    /// preserving the source property key `x`. Other pattern props descend
    /// normally.
    fn visit_mut_object_pat_prop(&mut self, n: &mut ObjectPatProp) {
        // Decide with only a short borrow whether this is an expandable shorthand.
        let new_sym = match n {
            ObjectPatProp::Assign(assign) => self.map.get(&assign.key.id.to_id()).cloned(),
            _ => None,
        };

        let Some(new_sym) = new_sym else {
            // Not an expandable shorthand. Descend normally: this renames a
            // KeyValue's value pattern, a Rest target, and any default value of
            // an Assign whose key is NOT a renamed local.
            n.visit_mut_children_with(self);
            return;
        };

        if let ObjectPatProp::Assign(assign) = n {
            let orig_sym = assign.key.id.sym.clone();
            let span = assign.key.id.span;
            let mut renamed = assign.key.id.clone();
            renamed.sym = new_sym;
            // Visit the default value (if present) so inner references rename.
            let mut default = assign.value.take();
            if let Some(d) = default.as_mut() {
                d.visit_mut_with(self);
            }
            let value_pat = match default {
                Some(d) => Pat::Assign(AssignPat {
                    span,
                    left: Box::new(Pat::Ident(renamed.into())),
                    right: d,
                }),
                None => Pat::Ident(renamed.into()),
            };
            *n = ObjectPatProp::KeyValue(KeyValuePatProp {
                key: PropName::Ident(IdentName {
                    span,
                    sym: orig_sym,
                }),
                value: Box::new(value_pat),
            });
        }
    }

    // Labels are never renamed (separate namespace). Skip the label ident and
    // descend into the body only; break/continue carry only a label, so they are
    // left entirely untouched.
    fn visit_mut_labeled_stmt(&mut self, n: &mut LabeledStmt) {
        n.body.visit_mut_with(self);
    }

    fn visit_mut_break_stmt(&mut self, _n: &mut BreakStmt) {}

    fn visit_mut_continue_stmt(&mut self, _n: &mut ContinueStmt) {}
}
