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
//! statement plus only the strictness/EH interpreter variants required by its
//! chunks, specialized to their instruction/operator union. The strings decode primitive and
//! the virtualized function bodies are now literally just two callers of `add`.

use mangler_core::Rng;
use swc_core::ecma::ast::Stmt;

use crate::chunk::{Chunk, Compiled, InstructionUsage};
use crate::diversity::VmDiversity;
use crate::emit::{InterpreterSpec, emit_unprotected_interpreter};
use crate::isa::Instr;
use crate::serialize::{code_array_js, consts_array_js, serialize};

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
    /// The lean (sloppy) interpreter function name (e.g. `V`).
    pub lean_interp: String,
    /// The EH (sloppy) interpreter function name (e.g. `D`). Only emitted if some
    /// chunk needs it; still supply a fresh name.
    pub eh_interp: String,
    /// The lean STRICT interpreter function name (§5a). Only emitted if some chunk is
    /// strict and lean; still supply a fresh name.
    pub lean_interp_strict: String,
    /// The EH STRICT interpreter function name (§5a). Only emitted if some chunk is
    /// both strict and needs EH; still supply a fresh name.
    pub eh_interp_strict: String,
    /// The shared program-table variable name.
    pub table: String,
    /// The hoisted `Reflect.construct` alias name.
    pub rc: String,
    /// The hoisted `Symbol.iterator` alias name.
    pub sy: String,
}

impl VmNames {
    /// The interpreter name a chunk with the given `(needs_eh, is_strict)` shape must
    /// call. The §5a 4-way selection: `(lean,eh)×(sloppy,strict)`.
    pub fn interp_for(&self, needs_eh: bool, is_strict: bool) -> &str {
        match (needs_eh, is_strict) {
            (false, false) => &self.lean_interp,
            (true, false) => &self.eh_interp,
            (false, true) => &self.lean_interp_strict,
            (true, true) => &self.eh_interp_strict,
        }
    }
}

/// The assembled VM artifacts: the prologue statements to splice ABOVE the
/// consumers, plus which interpreter each kind of chunk must call.
#[derive(Debug)]
pub struct VmTable {
    /// Prologue statements in splice order: `var rc=Reflect.construct;`,
    /// (optionally) `var sy=Symbol.iterator;`, the interpreter(s), and the table
    /// `var`. Splice these at module scope above any thunk that references them.
    pub prologue: Vec<Stmt>,
    /// Whether any EH interpreter (sloppy or strict) was emitted (some chunk needed
    /// the EH shape).
    pub has_eh: bool,
    /// Whether any strict interpreter variant was emitted (some chunk is strict).
    pub has_strict: bool,
    /// Runtime source compilation is required by a direct eval site.
    pub has_eval: bool,
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
    program_modes: Vec<(bool, bool)>,
    program_suspensions: Vec<Option<crate::chunk::SuspensionKind>>,
    argument_factories: Vec<String>,
    has_eval: bool,
    iterator_alias: bool,
    /// Which of the four `(needs_eh, is_strict)` interpreter variants some registered
    /// chunk requires, so `finish` emits only the needed variants. Indexed by
    /// `[is_strict as usize][needs_eh as usize]`.
    variants: [[bool; 2]; 2],
    usage: [[InstructionUsage; 2]; 2],
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
            program_modes: Vec::new(),
            program_suspensions: Vec::new(),
            argument_factories: Vec::new(),
            has_eval: false,
            iterator_alias: false,
            variants: [[false; 2]; 2],
            usage: Default::default(),
        }
    }

    /// Native lexical super-constructor capabilities share the VM iterator key.
    /// Record their demand even when the bytecode itself contains no GetIter.
    pub fn require_iterator_alias(&mut self) {
        self.iterator_alias = true;
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
        self.add_strict(compiled, false)
    }

    /// Register a compiled body that must execute under the given strictness (§5a).
    /// `is_strict = true` routes the chunk's thunk to a strict interpreter variant
    /// (and the caller must emit a strict thunk so the forwarded `this` is the
    /// un-coerced strict receiver). `add` is the sloppy shorthand. Each child row
    /// selects its interpreter from inherited strictness and its own directive.
    pub fn add_strict(&mut self, compiled: Compiled, is_strict: bool) -> Chunk {
        let eh = needs_eh(&compiled);
        self.variants[is_strict as usize][eh as usize] = true;
        self.usage[is_strict as usize][eh as usize].include(&compiled);
        self.has_eval |= self.usage[is_strict as usize][eh as usize].has_eval();
        if self.has_eval {
            self.variants = [[true; 2]; 2];
        }
        let capture_capabilities = compiled.capture_capabilities(is_strict);
        let captures = compiled.captures.clone();
        let cap_start = compiled.cap_start();
        let pcount = compiled.pcount;
        let argument_mappings = compiled
            .code
            .iter()
            .filter_map(|op| match op {
                Instr::MapArgument(index, slot) => Some((*index, *slot)),
                _ => None,
            })
            .collect();
        let index = self.register_chunk(compiled, is_strict, None);
        let chunk = Chunk {
            capture_capabilities,
            argument_mappings,
            index,
            captures,
            cap_start,
            pcount,
            needs_eh: eh,
            is_strict,
        };
        self.roots.push(RootMeta {
            chunk: chunk.clone(),
        });
        chunk
    }

    /// Register a compiled chunk + nested-closure children into `programs`,
    /// rewriting each `MakeClosure` child-local-index to its table index (depth-first
    /// so children precede their parent). Returns the chunk's table index. Mirrors
    /// the legacy `register_chunk`, minus the cross-call dedupe map.
    fn register_chunk(
        &mut self,
        mut compiled: Compiled,
        is_strict: bool,
        suspension: Option<crate::chunk::SuspensionKind>,
    ) -> usize {
        let eh = needs_eh(&compiled);
        self.variants[is_strict as usize][eh as usize] = true;
        self.usage[is_strict as usize][eh as usize].include(&compiled);
        self.has_eval |= self.usage[is_strict as usize][eh as usize].has_eval();
        if self.has_eval {
            self.variants = [[true; 2]; 2];
        }
        let children = std::mem::take(&mut compiled.children);
        let child_indices: Vec<usize> = children
            .into_iter()
            .map(|c| self.register_chunk(c.compiled, is_strict || c.is_strict, c.suspension))
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
        self.program_modes.push((eh, is_strict));
        self.program_suspensions.push(suspension);
        self.argument_factories
            .push(argument_factory(&compiled, is_strict));
        idx
    }

    /// True if any registered chunk needs the EH interpreter shape (sloppy or strict).
    pub fn needs_eh_interp(&self) -> bool {
        self.variants[0][1] || self.variants[1][1]
    }

    /// True if any registered chunk is strict (so a strict interpreter variant emits).
    pub fn needs_strict_interp(&self) -> bool {
        self.variants[1][0] || self.variants[1][1]
    }

    /// Emit the shared prologue: the `Reflect.construct`/`Symbol.iterator` aliases,
    /// the interpreter(s), and the program-table `var`, all as AST statements. The
    /// table `var` is built by reparsing the rendered `[[code,consts],...]` literal.
    /// Packed code strings expand once on first execution. Only interpreter variants
    /// used by a registered root are emitted; each supports its descendants as well.
    pub fn finish(&self, names: &VmNames) -> mangler_core::Result<VmTable> {
        let mut prologue: Vec<Stmt> = Vec::new();
        let any_eh = self.needs_eh_interp();
        let any_strict = self.needs_strict_interp();

        let used = |op| self.has_eval || self.usage.iter().flatten().any(|u| u.opcode(op));
        if used(13) {
            prologue.push(alias_decl(&names.rc, "Reflect", "construct")?);
        }
        if self.iterator_alias || used(20) {
            // var sy = Symbol.iterator;
            prologue.push(alias_decl(&names.sy, "Symbol", "iterator")?);
        }

        // Up to four `(needs_eh, is_strict)` interpreter variants, each emitted only
        // if some chunk requires it, in deterministic order. All share the same
        // diversity and table name, with a distinct required-instruction union.
        let emit = |name: &str, needs_eh: bool, is_strict: bool| {
            let spec = InterpreterSpec {
                name,
                table: &names.table,
                rc: &names.rc,
                sy: &names.sy,
                needs_eh,
                is_strict,
                diversity: &self.diversity,
                usage: if self.has_eval {
                    None
                } else {
                    Some(&self.usage[is_strict as usize][needs_eh as usize])
                },
            };
            emit_unprotected_interpreter(&spec)
        };
        if self.variants[0][0] {
            prologue.push(emit(&names.lean_interp, false, false)?);
        }
        if self.variants[0][1] {
            prologue.push(emit(&names.eh_interp, true, false)?);
        }
        if self.variants[1][0] {
            prologue.push(emit(&names.lean_interp_strict, false, true)?);
        }
        if self.variants[1][1] {
            prologue.push(emit(&names.eh_interp_strict, true, true)?);
        }

        // var <table> = [[code,consts],...];
        let entries: Vec<String> = self
            .programs
            .iter()
            .zip(&self.program_modes)
            .zip(&self.program_suspensions)
            .zip(&self.argument_factories)
            .map(|((((code, consts), &(eh, strict)), suspension), factory)| {
                format!(
                    "[{code},{consts},{},{},{},{factory}]",
                    names.interp_for(eh, strict),
                    strict,
                    suspension.map_or(0, |k| k.code())
                )
            })
            .collect();
        let table_src = format!("var {}=[{}];", names.table, entries.join(","));
        prologue.push(parse_one_stmt(&table_src)?);
        if self.has_eval {
            let ints = |values: &[usize]| {
                values
                    .iter()
                    .map(usize::to_string)
                    .collect::<Vec<_>>()
                    .join(",")
            };
            let metadata = format!(
                "Object.defineProperty({},'runtime',{{value:{{compilerFingerprint:{}n,variants:[{},{},{},{}],op:[{}],bin:[{}],un:[{}],key:{}}}}});",
                names.table,
                crate::COMPILER_FINGERPRINT as i64,
                names.lean_interp,
                names.eh_interp,
                names.lean_interp_strict,
                names.eh_interp_strict,
                ints(&self.diversity.perm),
                ints(&self.diversity.bin_perm),
                ints(&self.diversity.un_perm),
                self.diversity.code_key
            );
            prologue.push(parse_one_stmt(&metadata)?);
        }

        crate::descriptors::protect(&mut prologue, &names.table)?;
        Ok(VmTable {
            prologue,
            has_eh: any_eh,
            has_strict: any_strict,
            has_eval: self.has_eval,
        })
    }

    /// The chunks registered so far, in registration order (for a caller that needs
    /// to map its inputs back to their [`Chunk`] handles).
    pub fn chunks(&self) -> impl Iterator<Item = &Chunk> {
        self.roots.iter().map(|r| &r.chunk)
    }
}

/// Native parameter cells let the host retain the arguments exotic object's
/// descriptor, deletion, mapping, callee, and brand semantics.
/// Generate the native parameter shell used by static and runtime-compiled rows.
/// The shell contains argument mapping glue; all source function bodies stay bytecode.
pub fn argument_factory(compiled: &Compiled, strict: bool) -> String {
    build_argument_factory(compiled, strict, false)
}

/// Runtime-created sloppy functions need a native arguments shell even with zero
/// mapped parameters, because their surrounding program may itself be strict.
pub fn runtime_argument_factory(compiled: &Compiled, strict: bool) -> String {
    let force = !strict
        && compiled
            .code
            .iter()
            .any(|op| matches!(op, Instr::LoadArguments))
        && !compiled
            .code
            .iter()
            .any(|op| matches!(op, Instr::UnmapArguments));
    build_argument_factory(compiled, strict, force)
}

fn build_argument_factory(compiled: &Compiled, strict: bool, force: bool) -> String {
    let mappings: Vec<_> = compiled
        .code
        .iter()
        .filter_map(|op| match op {
            Instr::MapArgument(index, slot) => Some((*index, *slot)),
            _ => None,
        })
        .collect();
    if mappings.is_empty() && !force {
        return "0".into();
    }
    let params = (0..compiled.pcount)
        .map(|i| format!("p{i}"))
        .collect::<Vec<_>>()
        .join(",");
    let refs = mappings
        .iter()
        .map(|(i, slot)| {
            format!("[{slot},{{get:function(){{return p{i};}},set:function(v){{p{i}=v;}}}}]")
        })
        .collect::<Vec<_>>()
        .join(",");
    let directive = if strict { "'use strict';" } else { "" };
    let body = format!("{directive}return invoke(this,arguments,[{refs}],new.target);");
    let setter = if compiled.pcount == 1 {
        format!(
            "if(kind===2)return Object.getOwnPropertyDescriptor({{set [key](p0){{{body}}}}},key).set;"
        )
    } else {
        String::new()
    };
    format!(
        "function(invoke,key,kind){{if(kind===0)return ({{[key]({params}){{{body}}}}})[key];{setter}return function({params}){{{body}}};}}"
    )
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
/// The input is the rendered packed-code/constant table. Parsing validates that
/// string escaping and any native factory expressions form a valid declaration.
fn parse_one_stmt(src: &str) -> mangler_core::Result<Stmt> {
    use mangler_core::Language;
    use mangler_jsast::lang::{Js, ParseOpts};
    use swc_core::ecma::visit::VisitMutWith;
    let mut program = Js.parse(src, &ParseOpts::default())?.into_program();
    program.visit_mut_with(&mut mangler_jsast::span::GeneratedSpans);
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
            lean_interp_strict: "Vs".into(),
            eh_interp_strict: "Ds".into(),
            table: "T".into(),
            rc: "rc".into(),
            sy: "sy".into(),
        }
    }

    fn leaf(code: Vec<Instr>, consts: Vec<Const>) -> Compiled {
        Compiled {
            requires_source_compiler: false,
            code,
            consts,
            captures: vec![],
            slots: 2,
            pcount: 1,
            children: vec![],
        }
    }

    #[test]
    fn two_clients_share_one_table() {
        let mut tb = TableBuilder::with_diversity(VmDiversity::baseline(2));
        // "decode" client chunk, then "virtualize" client chunk.
        let a = tb.add(leaf(vec![Instr::LoadLocal(0), Instr::Ret], vec![]));
        let b = tb.add(leaf(
            vec![Instr::PushConst(0), Instr::Ret],
            vec![Const::Num(5.0)],
        ));
        assert_eq!(a.index, 0);
        assert_eq!(b.index, 1);
        let vt = tb.finish(&names()).expect("finish ok");
        assert_layout(&vt.prologue, &["V"], false, 2);
        assert!(!vt.has_eh);
    }

    #[test]
    fn eh_chunk_adds_second_interpreter_and_sy() {
        let mut tb = TableBuilder::with_diversity(VmDiversity::baseline(2));
        tb.add(leaf(vec![Instr::GetIter, Instr::Ret], vec![]));
        assert!(tb.needs_eh_interp());
        let vt = tb.finish(&names()).expect("finish ok");
        assert_layout(&vt.prologue, &["D"], true, 1);
        assert!(vt.has_eh);
    }

    /// A sloppy lean program emits one lean interpreter and no strict
    /// machinery. Check interpreter identities independently of shared helpers.
    #[test]
    fn sloppy_only_emits_single_interpreter_no_strict() {
        let mut tb = TableBuilder::with_diversity(VmDiversity::baseline(2));
        tb.add(leaf(vec![Instr::LoadLocal(0), Instr::Ret], vec![]));
        assert!(!tb.needs_strict_interp());
        assert!(!tb.needs_eh_interp());
        let vt = tb.finish(&names()).expect("finish ok");
        assert_layout(&vt.prologue, &["V"], false, 1);
        assert!(!vt.has_strict);
        assert!(!vt.has_eh);
        let rendered = render_prologue(&vt.prologue);
        assert!(
            strict_interpreters(&vt.prologue) == 0,
            "no strict directive:\n{rendered}"
        );
    }

    /// §5a 4-way selection table: each `(needs_eh, is_strict)` combination some chunk
    /// requires must add exactly its variant, in the fixed order
    /// lean-sloppy, eh-sloppy, lean-strict, eh-strict, with `sy` emitted iff any EH.
    #[test]
    fn strict_variants_emitted_on_demand() {
        // A sloppy-lean + a strict-lean chunk: 2 interpreters + table = 3 stmts,
        // no `sy` (no EH), exactly one `"use strict"`.
        let mut tb = TableBuilder::with_diversity(VmDiversity::baseline(2));
        tb.add(leaf(vec![Instr::LoadLocal(0), Instr::Ret], vec![]));
        tb.add_strict(leaf(vec![Instr::LoadLocal(0), Instr::Ret], vec![]), true);
        assert!(tb.needs_strict_interp());
        assert!(!tb.needs_eh_interp());
        let vt = tb.finish(&names()).expect("finish ok");
        assert_layout(&vt.prologue, &["V", "Vs"], false, 2);
        assert!(vt.has_strict && !vt.has_eh);
        let rendered = render_prologue(&vt.prologue);
        assert_eq!(
            strict_interpreters(&vt.prologue),
            1,
            "one strict variant:\n{rendered}"
        );

        // All four variants: sy + 4 interpreters + table = 6 stmts, two strict.
        let mut tb = TableBuilder::with_diversity(VmDiversity::baseline(2));
        tb.add(leaf(vec![Instr::LoadLocal(0), Instr::Ret], vec![]));
        tb.add(leaf(vec![Instr::GetIter, Instr::Ret], vec![]));
        tb.add_strict(leaf(vec![Instr::LoadLocal(0), Instr::Ret], vec![]), true);
        tb.add_strict(leaf(vec![Instr::GetIter, Instr::Ret], vec![]), true);
        let vt = tb.finish(&names()).expect("finish ok");
        assert_layout(&vt.prologue, &["V", "D", "Vs", "Ds"], true, 4);
        assert!(vt.has_strict && vt.has_eh);
        let rendered = render_prologue(&vt.prologue);
        assert_eq!(
            strict_interpreters(&vt.prologue),
            2,
            "two strict variants:\n{rendered}"
        );
    }

    /// `interp_for` implements the documented 4-way routing.
    #[test]
    fn interp_for_routes_all_four() {
        let n = names();
        assert_eq!(n.interp_for(false, false), "V");
        assert_eq!(n.interp_for(true, false), "D");
        assert_eq!(n.interp_for(false, true), "Vs");
        assert_eq!(n.interp_for(true, true), "Ds");
    }

    /// A strict-ONLY program does not emit a dead sloppy-lean interpreter.
    #[test]
    fn strict_only_omits_sloppy_lean() {
        let mut tb = TableBuilder::with_diversity(VmDiversity::baseline(2));
        tb.add_strict(leaf(vec![Instr::LoadLocal(0), Instr::Ret], vec![]), true);
        let vt = tb.finish(&names()).expect("finish ok");
        assert_layout(&vt.prologue, &["Vs"], false, 1);
        assert_eq!(strict_interpreters(&vt.prologue), 1);
    }

    fn assert_layout(stmts: &[Stmt], variants: &[&str], iterator_key: bool, rows: usize) {
        use swc_core::ecma::ast::{Decl, Expr, Pat};
        let mut functions = Vec::new();
        let mut bindings = Vec::new();
        for stmt in stmts {
            match stmt {
                Stmt::Decl(Decl::Fn(function)) => functions.push(function.ident.sym.as_ref()),
                Stmt::Decl(Decl::Var(variable)) => {
                    if mangler_jsast::span::is_private_runtime_declaration_span(variable.span) {
                        continue;
                    }
                    for declaration in &variable.decls {
                        let Pat::Ident(binding) = &declaration.name else {
                            panic!("table prologue uses identifier declarations");
                        };
                        bindings.push(binding.id.sym.as_ref());
                        if binding.id.sym == "T" {
                            let Some(Expr::Array(table)) = declaration.init.as_deref() else {
                                panic!("the shared table is an array");
                            };
                            assert_eq!(table.elems.len(), rows);
                        }
                    }
                }
                _ => panic!("unexpected table prologue statement"),
            }
        }
        assert_eq!(functions, variants);
        assert_eq!(
            bindings,
            if iterator_key {
                vec!["sy", "T"]
            } else {
                vec!["T"]
            }
        );
    }

    fn strict_interpreters(stmts: &[Stmt]) -> usize {
        stmts.iter().filter(|stmt| matches!(stmt, Stmt::Decl(swc_core::ecma::ast::Decl::Fn(f)) if f.function.body.as_ref().is_some_and(|b| matches!(b.stmts.first(), Some(Stmt::Expr(e)) if matches!(&*e.expr, swc_core::ecma::ast::Expr::Lit(swc_core::ecma::ast::Lit::Str(t)) if t.value == "use strict"))))).count()
    }

    /// Render a prologue to source so tests can scan for directives / count variants.
    fn render_prologue(prologue: &[Stmt]) -> String {
        use swc_core::common::SourceMap;
        use swc_core::common::sync::Lrc;
        use swc_core::ecma::ast::{Program, Script};
        use swc_core::ecma::codegen::Emitter;
        use swc_core::ecma::codegen::text_writer::JsWriter;
        let prog = Program::Script(Script {
            span: swc_core::common::DUMMY_SP,
            body: prologue.to_vec(),
            shebang: None,
        });
        let cm: Lrc<SourceMap> = Default::default();
        let mut buf = Vec::new();
        {
            let mut emitter = Emitter {
                cfg: Default::default(),
                cm: cm.clone(),
                comments: None,
                wr: JsWriter::new(cm, "", &mut buf, None),
            };
            emitter.emit_program(&prog).unwrap();
        }
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn nested_children_flattened_and_rewritten() {
        // A root with one child closure; the child must land at table index 0 and
        // the root's MakeClosure.child rewritten from local 0 to table 0.
        let child = Compiled {
            requires_source_compiler: false,
            code: vec![Instr::PushUndef, Instr::Ret],
            consts: vec![],
            captures: vec![],
            slots: 1,
            pcount: 0,
            children: vec![],
        };
        let root = Compiled {
            requires_source_compiler: false,
            code: vec![
                Instr::MakeClosure {
                    child: 0,
                    is_arrow: false,
                    cap_start: 1,
                    pcount: 0,
                    up_slots: vec![],
                },
                Instr::Ret,
            ],
            consts: vec![],
            captures: vec![],
            slots: 2,
            pcount: 1,
            children: vec![crate::chunk::ChildChunk {
                compiled: child,
                is_arrow: false,
                is_strict: false,
                suspension: None,
            }],
        };
        let mut tb = TableBuilder::with_diversity(VmDiversity::baseline(2));
        let chunk = tb.add(root);
        // Child registered first (index 0), root second (index 1).
        assert_eq!(chunk.index, 1);
        assert_eq!(tb.programs.len(), 2);
        let vt = tb.finish(&names()).expect("finish ok");
        assert!(!vt.prologue.is_empty());
    }

    #[test]
    fn specialized_interpreter_keeps_decoys_and_descendant_instructions() {
        let mut child = leaf(
            vec![
                Instr::PushConst(0),
                Instr::PushConst(1),
                Instr::Bin(0),
                Instr::Ret,
            ],
            vec![Const::Num(20.0), Const::Num(22.0)],
        );
        child.pcount = 0;
        let mut root = leaf(
            vec![
                Instr::MakeClosure {
                    child: 0,
                    is_arrow: false,
                    cap_start: 0,
                    pcount: 0,
                    up_slots: vec![],
                },
                Instr::Ret,
            ],
            vec![],
        );
        root.children.push(crate::chunk::ChildChunk {
            compiled: child,
            is_arrow: false,
            is_strict: false,
            suspension: None,
        });
        for seed in [1, 7, 42] {
            let mut tb = TableBuilder::new(&mut Rng::for_pass(seed, "usage"));
            let chunk = tb.add(root.clone());
            let output = render_prologue(&tb.finish(&names()).unwrap().prologue);
            let code = format!(
                "{output}globalThis.__out=V(T[{}][0],T[{}][1],[],[],0,0,null)();",
                chunk.index, chunk.index
            );
            mangler_testkit::assert_behaviorally_equal("globalThis.__out=42;", &code);
            for &label in &tb.diversity.perm[crate::isa::N_OPCODES..] {
                // Baseline and diversified dispatch can use switch or indexed closures.
                assert!(
                    output.contains(&format!("case {label}:"))
                        || output.contains(&format!("F[{label}]"))
                );
            }
        }
    }

    #[test]
    fn unused_handlers_and_operator_cases_are_omitted() {
        let mut tb = TableBuilder::with_diversity(VmDiversity::baseline(2));
        tb.add(leaf(
            vec![Instr::PushConst(0), Instr::Ret],
            vec![Const::Num(42.0)],
        ));
        let output = render_prologue(&tb.finish(&names()).unwrap().prologue);
        assert!(output.contains("case 0:"));
        assert!(output.contains("case 18:"));
        assert!(!output.contains("case 5:"), "unused binary handler emitted");
        assert!(!output.contains("case 6:"), "unused unary handler emitted");
        assert!(
            !output.contains("case 13:"),
            "unused constructor handler emitted"
        );
        assert_eq!(
            output.matches("case ").count(),
            4,
            "two real instructions plus two configured decoys"
        );
    }
    fn execute_chunk(compiled: Compiled, invocation: &str, expected: &str) {
        for seed in [1, 7, 42] {
            let mut table = TableBuilder::new(&mut Rng::for_pass(seed, "runtime-semantics"));
            let chunk = table.add(compiled.clone());
            let prologue = render_prologue(&table.finish(&names()).unwrap().prologue);
            let index = chunk.index;
            let pcount = chunk.pcount;
            let output = format!(
                "{prologue}var invoke=function(receiver,args,refs,target){{return T[{index}][2](T[{index}][0],T[{index}][1],args,[],0,{pcount},receiver,false,refs,target);}};var run=T[{index}][5]?T[{index}][5](invoke):function(){{return invoke(this,arguments,undefined,new.target);}};{invocation}"
            );
            mangler_testkit::assert_behaviorally_equal(
                &format!("{expected}globalThis.__out=JSON.stringify(globalThis.__out);"),
                &format!("{output}globalThis.__out=JSON.stringify(globalThis.__out);"),
            );
        }
    }

    #[test]
    fn utf16_constants_preserve_surrogates_and_large_values() {
        execute_chunk(
            leaf(
                vec![Instr::PushConst(0), Instr::Ret],
                vec![Const::Utf16(vec![0xd800, 0, 0xdc00])],
            ),
            "var s=run();globalThis.__out=[s.length,s.charCodeAt(0),s.charCodeAt(1),s.charCodeAt(2)];",
            "globalThis.__out=[3,55296,0,56320];",
        );
        execute_chunk(
            leaf(
                vec![Instr::PushConst(0), Instr::Ret],
                vec![Const::Str("x".repeat(200_000))],
            ),
            "globalThis.__out=run().length;",
            "globalThis.__out=200000;",
        );
    }

    #[test]
    fn regexp_literals_are_fresh_on_every_evaluation() {
        execute_chunk(
            leaf(
                vec![Instr::NewRegExp(0), Instr::Ret],
                vec![Const::RegExp {
                    pattern: "x+".into(),
                    flags: "g".into(),
                }],
            ),
            "var a=run(),b=run();a.test('xx');globalThis.__out=[a!==b,a.lastIndex,b.lastIndex,b.source,b.flags];",
            "globalThis.__out=[true,2,0,'x+','g'];",
        );
    }

    #[test]
    fn bigint_bitwise_operators_preserve_arbitrary_precision() {
        execute_chunk(
            leaf(
                vec![
                    Instr::PushConst(0),
                    Instr::PushConst(1),
                    Instr::Bin(15),
                    Instr::Ret,
                ],
                vec![
                    Const::BigInt("18446744073709551616".into()),
                    Const::BigInt("3".into()),
                ],
            ),
            "globalThis.__out=String(run());",
            "globalThis.__out='18446744073709551619';",
        );
    }

    #[test]
    fn mapped_arguments_disconnect_on_descriptor_and_delete_operations() {
        execute_chunk(
            leaf(
                vec![Instr::MapArgument(0, 0), Instr::LoadArguments, Instr::Ret],
                vec![],
            ),
            "var a=run(1);a[0]=3;var v=Object.getOwnPropertyDescriptor(a,'0').value;Object.defineProperty(a,'0',{writable:false});globalThis.__out=[v,a[0],Object.getOwnPropertyDescriptor(a,'0').writable,delete a[0],a[0]];",
            "globalThis.__out=[3,3,false,true,undefined];",
        );
    }

    #[test]
    fn explicitly_strict_child_uses_its_own_interpreter_and_receiver_mode() {
        let child = Compiled {
            requires_source_compiler: false,
            code: vec![Instr::PushThis, Instr::Ret],
            consts: vec![],
            captures: vec![],
            slots: 0,
            pcount: 0,
            children: vec![],
        };
        let root = Compiled {
            requires_source_compiler: false,
            code: vec![
                Instr::MakeClosure {
                    child: 0,
                    is_arrow: false,
                    cap_start: 0,
                    pcount: 0,
                    up_slots: vec![],
                },
                Instr::Ret,
            ],
            consts: vec![],
            captures: vec![],
            slots: 0,
            pcount: 0,
            children: vec![crate::chunk::ChildChunk {
                compiled: child,
                is_arrow: false,
                is_strict: true,
                suspension: None,
            }],
        };
        execute_chunk(
            root,
            "var f=run();globalThis.__out=[f()===undefined,f.call(3)===3];",
            "globalThis.__out=[true,true];",
        );
    }

    #[test]
    fn internal_lists_ignore_mutated_array_prototype_methods() {
        execute_chunk(
            leaf(
                vec![
                    Instr::PushConst(0),
                    Instr::PushConst(1),
                    Instr::MakeArray(2),
                    Instr::Ret,
                ],
                vec![Const::Str("hello".into()), Const::Num(42.0)],
            ),
            "var saved={};for(var k of ['push','pop','splice','slice','map']){saved[k]=Array.prototype[k];Array.prototype[k]=function(){throw 'prototype hook';};}var result;try{result=run();}finally{for(var k in saved)Array.prototype[k]=saved[k];}globalThis.__out=result;",
            "globalThis.__out=['hello',42];",
        );
    }
}
