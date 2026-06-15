//! [`TableBuilder`] — the first-class shared-table assembly API that replaces the
//! 13-field `DeferredStringsVm` hand-off.
//!
//! ## The problem this kills
//!
//! Legacy strings-decode-in-VM and function-virtualize each built their OWN
//! interpreter + table, and "sharing" them meant the strings pass packing a
//! `DeferredStringsVm` struct of 13 fields (names, perms, key, decode programs,
//! decode index, skeleton/handler/dispatch/mba seeds) and handing it across passes
//! to the virtualize finalize, which had to re-derive the rest in lockstep. The
//! draw order was pinned by hand in `embed_core`, the chunk flattening duplicated in
//! `register_chunk`, and a drift between the two callers was a latent miscompile.
//!
//! ## The replacement
//!
//! [`TableBuilder`] owns ONE [`VmDiversity`] (drawn once) and ONE growing program
//! table. Either client compiles a function body to a [`Compiled`] and calls
//! [`TableBuilder::add`]; it gets back a [`Chunk`] (root table index + frame
//! metadata for its thunk). When all chunks are registered, [`TableBuilder::finish`]
//! serializes every chunk under the shared diversity and emits ONE program-table
//! statement plus the interpreter(s) — a lean interpreter always, and (only if any
//! chunk needs it) a second EH-shaped interpreter. The strings decode primitive and
//! the virtualized function bodies are now literally just two callers of `add`.

use mangler_core::Rng;
use swc_core::ecma::ast::Stmt;

use crate::chunk::{Chunk, Compiled};
use crate::diversity::VmDiversity;
use crate::emit::{emit_interpreter, InterpreterSpec};
use crate::isa::Instr;
use crate::serialize::{code_array_js, consts_array_js, program_table_js, serialize};

/// Whether a [`Compiled`] (incl. its nested children) uses any opcode that requires
/// the exception-handling / iterator / completion interpreter shape.
fn needs_eh(c: &Compiled) -> bool {
    let here = c.code.iter().any(|i| {
        matches!(
            i,
            Instr::GetIter
                | Instr::IterStep
                | Instr::IterClose
                | Instr::PushHandler(_, _)
                | Instr::PopHandler
                | Instr::EndFinally
                | Instr::RetUnwind
                | Instr::BreakUnwind(_, _)
        )
    });
    here || c.children.iter().any(|ch| needs_eh(&ch.compiled))
}

/// The names the assembled VM prologue uses. The caller supplies fresh,
/// collision-free identifiers (drawn from its own namer) so the table builder owns
/// no naming policy.
#[derive(Debug, Clone)]
pub struct VmNames {
    /// The lean interpreter function name (e.g. `V`).
    pub lean_interp: String,
    /// The EH interpreter function name (e.g. `D`). Only emitted if some chunk
    /// needs it; still supply a fresh name.
    pub eh_interp: String,
    /// The shared program-table variable name.
    pub table: String,
    /// The hoisted `Reflect.construct` alias name.
    pub rc: String,
    /// The hoisted `Symbol.iterator` alias name.
    pub sy: String,
}

/// The assembled VM artifacts: the prologue statements to splice ABOVE the
/// consumers, plus which interpreter each kind of chunk must call.
#[derive(Debug)]
pub struct VmTable {
    /// Prologue statements in splice order: `var rc=Reflect.construct;`,
    /// (optionally) `var sy=Symbol.iterator;`, the interpreter(s), and the table
    /// `var`. Splice these at module scope above any thunk that references them.
    pub prologue: Vec<Stmt>,
    /// Whether a second EH interpreter was emitted (some chunk needed it).
    pub has_eh: bool,
}

/// Accumulates compiled chunks from both VM clients and emits one shared table +
/// interpreter(s) under one diversification.
pub struct TableBuilder {
    diversity: VmDiversity,
    /// Registered roots, in registration order, each paired with whether it needs
    /// the EH interpreter. Their nested children are flattened into `programs`.
    roots: Vec<RootMeta>,
    /// The flat program table: `(code_js, consts_js)` per chunk, children first.
    programs: Vec<(String, String)>,
    /// True once any registered chunk needs the EH shape.
    any_eh: bool,
}

struct RootMeta {
    chunk: Chunk,
}

impl TableBuilder {
    /// Start a builder with a freshly-drawn diversification for this file.
    pub fn new(rng: &mut Rng) -> Self {
        Self::with_diversity(VmDiversity::draw(rng))
    }

    /// Start a builder with an explicit diversification (tests / callers that drew
    /// it themselves).
    pub fn with_diversity(diversity: VmDiversity) -> Self {
        TableBuilder {
            diversity,
            roots: Vec::new(),
            programs: Vec::new(),
            any_eh: false,
        }
    }

    /// The shared diversification (for a caller that needs the perms/key/seeds, e.g.
    /// to coordinate a stub it emits separately).
    pub fn diversity(&self) -> &VmDiversity {
        &self.diversity
    }

    /// Register a compiled function body, flattening its nested-closure children
    /// into the shared table and rewriting each `MakeClosure` child-local index to
    /// its resolved table index. Returns the [`Chunk`] handle (root index + thunk
    /// frame metadata).
    pub fn add(&mut self, compiled: Compiled) -> Chunk {
        let eh = needs_eh(&compiled);
        self.any_eh |= eh;
        let captures = compiled.captures.clone();
        let cap_start = compiled.cap_start();
        let pcount = compiled.pcount;
        let index = self.register_chunk(compiled);
        let chunk = Chunk {
            index,
            captures,
            cap_start,
            pcount,
            needs_eh: eh,
        };
        self.roots.push(RootMeta { chunk: chunk.clone() });
        chunk
    }

    /// Register a compiled chunk + nested-closure children into `programs`,
    /// rewriting each `MakeClosure` child-local-index to its table index (depth-first
    /// so children precede their parent). Returns the chunk's table index. Mirrors
    /// the legacy `register_chunk`, minus the cross-call dedupe map.
    fn register_chunk(&mut self, mut compiled: Compiled) -> usize {
        let children = std::mem::take(&mut compiled.children);
        let child_indices: Vec<usize> = children
            .into_iter()
            .map(|c| self.register_chunk(c.compiled))
            .collect();
        for instr in &mut compiled.code {
            if let Instr::MakeClosure { child, .. } = instr {
                *child = child_indices[*child as usize] as u32;
            }
        }
        let div = &self.diversity;
        let (code, consts) = serialize(&compiled, &div.perm, &div.bin_perm, &div.un_perm);
        let code_js = code_array_js(&code, div.code_key);
        let consts_js = consts_array_js(&consts, div.code_key);
        let idx = self.programs.len();
        self.programs.push((code_js, consts_js));
        idx
    }

    /// True if any registered chunk needs the EH interpreter shape.
    pub fn needs_eh_interp(&self) -> bool {
        self.any_eh
    }

    /// Emit the shared prologue: the `Reflect.construct`/`Symbol.iterator` aliases,
    /// the interpreter(s), and the program-table `var`, all as AST statements. The
    /// table `var` is built by reparsing the rendered `[[code,consts],...]` literal
    /// (the chunks are pre-rendered XOR'd numeric arrays; there is no Rust mirror to
    /// drift from). A lean interpreter is always emitted; an EH interpreter is added
    /// only when some chunk needs it.
    pub fn finish(&self, names: &VmNames) -> mangler_core::Result<VmTable> {
        let mut prologue: Vec<Stmt> = Vec::new();

        // var rc = Reflect.construct;
        prologue.push(alias_decl(&names.rc, "Reflect", "construct")?);
        if self.any_eh {
            // var sy = Symbol.iterator;
            prologue.push(alias_decl(&names.sy, "Symbol", "iterator")?);
        }

        // The lean interpreter (always) and the EH interpreter (if needed). Both
        // share the SAME diversity and table name.
        let lean_spec = InterpreterSpec {
            name: &names.lean_interp,
            table: &names.table,
            rc: &names.rc,
            sy: &names.sy,
            needs_eh: false,
            diversity: &self.diversity,
        };
        prologue.push(emit_interpreter(&lean_spec)?);
        if self.any_eh {
            let eh_spec = InterpreterSpec {
                name: &names.eh_interp,
                table: &names.table,
                rc: &names.rc,
                sy: &names.sy,
                needs_eh: true,
                diversity: &self.diversity,
            };
            prologue.push(emit_interpreter(&eh_spec)?);
        }

        // var <table> = [[code,consts],...];
        let table_src = format!("var {}={};", names.table, program_table_js(&self.programs));
        prologue.push(parse_one_stmt(&table_src)?);

        Ok(VmTable {
            prologue,
            has_eh: self.any_eh,
        })
    }

    /// The chunks registered so far, in registration order (for a caller that needs
    /// to map its inputs back to their [`Chunk`] handles).
    pub fn chunks(&self) -> impl Iterator<Item = &Chunk> {
        self.roots.iter().map(|r| &r.chunk)
    }
}

/// Build a `var <name> = <obj>.<prop>;` alias declaration as AST.
fn alias_decl(name: &str, obj: &str, prop: &str) -> mangler_core::Result<Stmt> {
    use mangler_jsast::build;
    use swc_core::ecma::ast::VarDeclKind;
    Ok(build::var_decl(
        VarDeclKind::Var,
        name,
        build::member_ident(build::ident_expr(obj), prop),
    ))
}

/// Parse a single JS statement string into a [`Stmt`] (the table-var declaration).
/// The input is a rendered numeric array literal with no Rust mirror, so this is a
/// validated fragment, not a hand-synced template.
fn parse_one_stmt(src: &str) -> mangler_core::Result<Stmt> {
    use mangler_core::Language;
    use mangler_jsast::lang::{Js, ParseOpts};
    let program = Js.parse(src, &ParseOpts::default())?.into_program();
    let stmt = match program {
        swc_core::ecma::ast::Program::Script(s) => s.body.into_iter().next(),
        swc_core::ecma::ast::Program::Module(m) => m.body.into_iter().find_map(|it| match it {
            swc_core::ecma::ast::ModuleItem::Stmt(s) => Some(s),
            _ => None,
        }),
    };
    stmt.ok_or_else(|| mangler_core::Error::transform("vm-table", "fragment produced no statement"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunk::Const;

    fn names() -> VmNames {
        VmNames {
            lean_interp: "V".into(),
            eh_interp: "D".into(),
            table: "T".into(),
            rc: "rc".into(),
            sy: "sy".into(),
        }
    }

    fn leaf(code: Vec<Instr>, consts: Vec<Const>) -> Compiled {
        Compiled { code, consts, captures: vec![], slots: 2, pcount: 1, children: vec![] }
    }

    #[test]
    fn two_clients_share_one_table() {
        let mut tb = TableBuilder::with_diversity(VmDiversity::baseline(2));
        // "decode" client chunk, then "virtualize" client chunk.
        let a = tb.add(leaf(vec![Instr::LoadLocal(0), Instr::Ret], vec![]));
        let b = tb.add(leaf(vec![Instr::PushConst(0), Instr::Ret], vec![Const::Num(5.0)]));
        assert_eq!(a.index, 0);
        assert_eq!(b.index, 1);
        let vt = tb.finish(&names()).expect("finish ok");
        // Lean only (no EH chunk): rc alias + 1 interpreter + table = 3 stmts.
        assert_eq!(vt.prologue.len(), 3);
        assert!(!vt.has_eh);
    }

    #[test]
    fn eh_chunk_adds_second_interpreter_and_sy() {
        let mut tb = TableBuilder::with_diversity(VmDiversity::baseline(2));
        tb.add(leaf(vec![Instr::GetIter, Instr::Ret], vec![]));
        assert!(tb.needs_eh_interp());
        let vt = tb.finish(&names()).expect("finish ok");
        // rc + sy + lean interp + eh interp + table = 5 stmts.
        assert_eq!(vt.prologue.len(), 5);
        assert!(vt.has_eh);
    }

    #[test]
    fn nested_children_flattened_and_rewritten() {
        // A root with one child closure; the child must land at table index 0 and
        // the root's MakeClosure.child rewritten from local 0 to table 0.
        let child = Compiled {
            code: vec![Instr::PushUndef, Instr::Ret],
            consts: vec![],
            captures: vec![],
            slots: 1,
            pcount: 0,
            children: vec![],
        };
        let root = Compiled {
            code: vec![
                Instr::MakeClosure { child: 0, is_arrow: false, cap_start: 1, pcount: 0, up_slots: vec![] },
                Instr::Ret,
            ],
            consts: vec![],
            captures: vec![],
            slots: 2,
            pcount: 1,
            children: vec![crate::chunk::ChildChunk { compiled: child, is_arrow: false }],
        };
        let mut tb = TableBuilder::with_diversity(VmDiversity::baseline(2));
        let chunk = tb.add(root);
        // Child registered first (index 0), root second (index 1).
        assert_eq!(chunk.index, 1);
        assert_eq!(tb.programs.len(), 2);
        let vt = tb.finish(&names()).expect("finish ok");
        assert!(!vt.prologue.is_empty());
    }
}
