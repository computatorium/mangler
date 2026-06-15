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
use mangler_vm::{classify_body, compile_body, Chunk, Eligibility, TableBuilder, VmNames};
use swc_core::common::DUMMY_SP;
use swc_core::ecma::ast::*;
use swc_core::ecma::visit::{VisitMut, VisitMutWith};

mod glob;

/// Function virtualization: compile eligible named-function bodies to VM bytecode and
/// replace them with thunks that re-enter a spliced interpreter over a shared program
/// table.
pub struct VirtualizePass;

impl Pass<Js, FileConfig> for VirtualizePass {
    fn id(&self) -> &'static str {
        "virtualize"
    }

    /// No declared reads. In particular it does NOT read `ResolvedScopes`, so the
    /// topological sort places it BEFORE the resolver pseudo-pass — the spliced
    /// interpreter then gets fresh resolver marks like the rest of the module.
    fn reads(&self) -> &[Resource] {
        &[]
    }

    /// Produces the shared VM table + interpreter. Downstream passes (expr,
    /// cf-flatten, dead-code) read this to avoid bloating the hoisted bytecode array.
    fn writes(&self) -> &[Resource] {
        const W: &[Resource] = &[Resource::vm_table()];
        W
    }

    /// Opt-in: enabled exactly when a virtualize target glob is configured.
    fn enabled(&self, cfg: &FileConfig) -> bool {
        cfg.resolved().passes.virtualize.target.is_some()
    }

    fn run(
        &self,
        ast: &mut <Js as Language>::Ast,
        cfg: &FileConfig,
        rng: &mut Rng,
        bus: &mut ArtifactBus,
        _notes: &mut Notes,
    ) -> Result<()> {
        let target = match &cfg.resolved().passes.virtualize.target {
            Some(t) => t.clone(),
            // `enabled` already gates this; defensive no-op if somehow reached.
            None => return Ok(()),
        };

        // ONE diversification per file, drawn once from this pass's RNG. Every chunk in
        // the shared table is serialized under it, so a single interpreter decodes them
        // all.
        let mut tb = TableBuilder::new(rng);

        // Draw the prologue names UP FRONT so the thunks and the spliced interpreter
        // agree (see module docs). File-wide-unique via the shared allocator.
        let names = VmNames {
            lean_interp: cfg.fresh_name(),
            eh_interp: cfg.fresh_name(),
            table: cfg.fresh_name(),
            rc: cfg.fresh_name(),
            sy: cfg.fresh_name(),
        };

        let mut v = Virtualizer {
            target: &target,
            names: &names,
            tb: &mut tb,
            used: false,
        };
        ast.program_mut().visit_mut_with(&mut v);
        let used = v.used;

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
            interp_name: names.lean_interp,
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
    /// The prologue names (drawn up-front) every thunk references.
    names: &'a VmNames,
    tb: &'a mut TableBuilder,
    /// Set true once any function in this file was virtualized.
    used: bool,
}

impl Virtualizer<'_> {
    /// Try to virtualize `function` (whose own name is `name`). Returns true if its
    /// body was replaced with a thunk. **Bail-to-safe**: any reason to skip returns
    /// false and leaves the function untouched (never a miscompile).
    fn try_virtualize(&mut self, name: &str, function: &mut Function) -> bool {
        if !glob::matches(self.target, name) {
            return false;
        }
        // Generators/async are not modeled by the flat-slot VM.
        if function.is_generator || function.is_async {
            return false;
        }
        let body = match &function.body {
            Some(b) => b,
            None => return false,
        };
        // Structural eligibility (with/eval/await/yield + sloppy arguments-alias bail).
        if let Eligibility::Skip(_) = classify_body(&function.params, body) {
            return false;
        }
        // A function strict via its OWN leading `"use strict"` cannot be virtualized
        // soundly: the VM runs the body as opcodes (no directive effect) and the
        // directive does not survive on the replacement thunk, so a plain call would
        // observe a sloppy `this` (globalThis) where the original sees `undefined`.
        if has_use_strict_directive(body) {
            return false;
        }

        // Compile the body to bytecode. `compile_body` is the final authority: on any
        // residual unsupported shape (incl. a nested closure that cannot compile) it
        // returns Err and we leave the whole function un-virtualized.
        let compiled = match compile_body(&function.params, body) {
            Ok(c) => c,
            Err(_) => return false,
        };

        // Register the chunk tree (children flattened, MakeClosure child-indices
        // resolved to table indices) and get the root chunk handle.
        let chunk = self.tb.add(compiled);

        // Build the re-entry thunk and install it as the function's new body. A parse
        // failure here (it never should, the source is machine-generated) is a sound
        // skip — but note the chunk is already in the table; leaving the original body
        // would call the un-thunked function while its table entry sits unused, which
        // is fine (extra dead table entry, never a miscompile).
        let interp = if chunk.needs_eh {
            &self.names.eh_interp
        } else {
            &self.names.lean_interp
        };
        let stmts = match thunk_stmts(interp, &self.names.table, &chunk) {
            Some(s) => s,
            None => return false,
        };
        function.body = Some(BlockStmt {
            span: DUMMY_SP,
            stmts,
            ..Default::default()
        });
        self.used = true;
        true
    }
}

impl VisitMut for Virtualizer<'_> {
    fn visit_mut_fn_decl(&mut self, n: &mut FnDecl) {
        let name = n.ident.sym.to_string();
        if self.try_virtualize(&name, &mut n.function) {
            return; // replaced — don't recurse into the (now-thunk) body
        }
        n.visit_mut_children_with(self);
    }

    fn visit_mut_fn_expr(&mut self, n: &mut FnExpr) {
        if let Some(id) = n.ident.clone() {
            let name = id.sym.to_string();
            if self.try_virtualize(&name, &mut n.function) {
                return;
            }
        }
        n.visit_mut_children_with(self);
    }
}

/// Format + parse the thunk statements that re-enter the interpreter for `chunk`:
///
/// ```text
/// function _v(p0,p1,…){ return <interp>(T[i][0], T[i][1], arguments, [caps], capStart, pcount, this); }
/// ```
///
/// `interp` is the interpreter the chunk must call (EH or lean); `table` is the shared
/// program-table name. The thunk declares one formal per positional param (`p0..`) so
/// a `Function.prototype.length` read sees the right arity, but the real arguments are
/// forwarded via `arguments`. The captured free-globals are spread into an array in
/// the order the interpreter threads them. Built by formatting the call as source and
/// reparsing it (the table index and capture names are the only variable parts; there
/// is no Rust AST mirror to drift from), mirroring the legacy `thunk_body_src`.
fn thunk_stmts(interp: &str, table: &str, chunk: &Chunk) -> Option<Vec<Stmt>> {
    let caps = format!("[{}]", chunk.captures.join(","));
    let params: Vec<String> = (0..chunk.pcount).map(|i| format!("p{i}")).collect();
    let src = format!(
        "function _v({}){{return {interp}({table}[{idx}][0],{table}[{idx}][1],arguments,{caps},{cap_start},{pcount},this);}}",
        params.join(","),
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
    match program {
        Program::Script(s) => {
            let mut body = prologue;
            body.append(&mut s.body);
            s.body = body;
        }
        Program::Module(m) => {
            let mut body: Vec<ModuleItem> = prologue.into_iter().map(ModuleItem::Stmt).collect();
            body.append(&mut m.body);
            m.body = body;
        }
    }
}

/// True if `body` begins with its OWN `"use strict"` directive (strict regardless of
/// the enclosing scope). The VM cannot preserve that strictness on the replacement
/// thunk, so such functions are skipped to keep `this`/strict-mode semantics sound.
/// Scans only the leading directive prologue (leading string-literal statements).
fn has_use_strict_directive(body: &BlockStmt) -> bool {
    for s in &body.stmts {
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
