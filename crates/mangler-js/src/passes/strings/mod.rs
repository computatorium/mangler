//! String-literal encoding + decoder injection (the legacy `strings` pass), ported
//! into the `mangler-js` pass-graph crate.
//!
//! Replaces every safe `Lit::Str` literal (and untagged-template quasi) with a call
//! into a runtime decoder, and splices that decoder's source at the top of the
//! program. The decoder call is impure-looking, so swc's constant-folder can
//! neither fold a `core(i)` call to its string nor eliminate the decoder — which is
//! what makes the anchored opaque values (expr / dead-code / cf-flatten) survive
//! minification. The pass `put`s a [`DecoderAnchorArtifact`] so those passes anchor
//! on `core`.
//!
//! ## The decomposed god function
//!
//! The legacy `replace::rewrite_program` (~392 lines) is decomposed into the
//! sequence in [`run`]:
//!
//! 1. **collect** — [`collect::StringCollector`] walks the AST, interning deduped
//!    plaintexts (respecting [`collect::SkipContext`] skips) and emitting placeholder
//!    `core(idx)` calls.
//! 2. **encode** — [`encode::encode_entries`] turns the plaintext pool into the
//!    wire-format DAG blob (+ optional runtime key).
//! 3. **shard** — partition entries across K arrays (`lut_p`/`lut_s`).
//! 4. **plan** — [`build_dispatch_plan`] picks a shim (or home-shard route) and a
//!    non-foldable index expression per call site.
//! 5. **emit-stub** — [`stub::render_stub`] renders the decoder source.
//! 6. **splice** — finalize the placeholders ([`collect::RewriteFinalizer`]) and
//!    splice the parsed-and-validated stub statements at the program top.
//!
//! ## Opt-in modes
//!
//! The DEFAULT encode/decode path (`Encode`/`Encrypt`) is fully ported with
//! sharding, shims, decoys, the masked base key, the DJB2 anti-tamper coupling, and
//! the runtime-bound (`dynamic_key`) key.
//!
//! The deep VM-coupled opt-ins are also implemented (all gated behind their flags so
//! the default-off output stays byte-identical):
//!
//! * **`in_vm`** — the per-index decode primitive ([`stub::VM_DECODE_PRIMITIVE`]) is
//!   compiled to bytecode via [`mangler_vm`] and emitted INLINE as its OWN table +
//!   interpreter (NOT the virtualize pass's shared table). The prologue is injected
//!   INSIDE the protected `core` IIFE (see [`inject_into_core_iife`]) so the opaque
//!   passes do not corrupt its bytecode arrays. `decodeOne(i)` becomes a thunk call.
//! * **`exec_trace_key`** — embeds the trace-augmented primitive
//!   ([`stub::vm_decode_primitive_with_trace`]) + the build-time accumulator
//!   ([`stub::exec_trace_acc`]) so the key is bound to the VM's runtime execution
//!   trace (re-minify-robust).
//! * **`self_coupled_key`** — binds the key to the interpreter/decoder SOURCE: the
//!   stub emits a fixed-width `SCK<digits>` sentinel, the pass `put`s a
//!   [`SelfCoupledKeyArtifact`] for the runner, and the runner's post-codegen
//!   finalizer ([`stub::patch_self_coupled_expected`]) rewrites the sentinel with the
//!   real expected hash.
//!
//! A `Note` is surfaced only if the decode primitive fails to compile (bail-to-safe:
//! the plain-JS decoder is emitted instead).

pub mod collect;
pub mod encode;
pub mod stub;

use crate::artifacts::DecoderAnchorArtifact;
use crate::config::FileConfig;
use crate::opaque::{opaque_u32, OpaqueAnchor};
use collect::{DispatchPlan, PendingCall, RewriteFinalizer, StringCollector};
use encode::{derive_runtime_key, encode_entries, mask_base_key, EncodingParams};
use mangler_config::StringMode;
use mangler_core::{Error, Language, Notes, Result, Rng};
use mangler_jsast::{Js, ParseOpts};
use mangler_passgraph::{ArtifactBus, Pass, Resource};
use stub::{render_stub, StubParams, VmDecodeParams};
use swc_core::common::{SyntaxContext, DUMMY_SP};
use swc_core::ecma::ast::*;
use swc_core::ecma::visit::VisitMutWith;

use crate::artifacts::SelfCoupledKeyArtifact;
use mangler_vm::{compile_body, TableBuilder, VmDiversity, VmNames};

/// Encodes string literals and emits the runtime decoder; provides the
/// `DecoderAnchor` artifact the opaque-value passes anchor on.
pub struct StringsPass;

impl Pass<Js, FileConfig> for StringsPass {
    fn id(&self) -> &'static str {
        "strings"
    }

    /// Reads the property-name + global-name literal markers so the scheduler runs
    /// this pass AFTER memberaccess + globalref, encoding the literals they produced.
    fn reads(&self) -> &[Resource] {
        const R: &[Resource] =
            &[Resource::property_literals(), Resource::global_name_literals()];
        R
    }

    /// Produces the decoder anchor downstream passes couple opaque values to. Does
    /// NOT declare `vm_table` — strings stays independent of virtualize: the in-VM
    /// decode table is emitted INLINE (inside the protected `core` IIFE) rather than
    /// shared, so it never rides the `vm_table` channel. The opt-in
    /// `SelfCoupledKeyArtifact` rides the already-declared `decoder_anchor` resource.
    fn writes(&self) -> &[Resource] {
        const W: &[Resource] = &[Resource::decoder_anchor()];
        W
    }

    fn enabled(&self, cfg: &FileConfig) -> bool {
        cfg.resolved().passes.strings.mode != StringMode::None
    }

    fn run(
        &self,
        ast: &mut <Js as mangler_core::Language>::Ast,
        cfg: &FileConfig,
        rng: &mut Rng,
        bus: &mut ArtifactBus,
        notes: &mut Notes,
    ) -> Result<()> {
        run_strings(ast, cfg, rng, bus, notes)
    }
}

/// Drive the whole strings pass: collect → encode → shard → plan → emit-stub →
/// splice. Leaves the program untouched (and `put`s no anchor) when there are no
/// rewritable strings.
fn run_strings(
    ast: &mut <Js as mangler_core::Language>::Ast,
    cfg: &FileConfig,
    rng: &mut Rng,
    bus: &mut ArtifactBus,
    notes: &mut Notes,
) -> Result<()> {
    let s = &cfg.resolved().passes.strings;
    let core_name = cfg.fresh_name();

    // -- 1) collect --------------------------------------------------------
    let mut collector = StringCollector::new(core_name.clone());
    ast.program_mut().visit_mut_with(&mut collector);
    if collector.plaintexts.is_empty() {
        return Ok(());
    }
    let plaintexts = std::mem::take(&mut collector.plaintexts);
    let pending = collector.pending.clone();

    // -- 2) encode ---------------------------------------------------------
    let base_key = rng.random_bytes(17);
    // Runtime-bound key (opt-in): empty when no dynamic key, so encoder output stays
    // byte-identical to a non-dynamic build.
    let runtime_key = match &s.dynamic_key {
        Some(dk) => derive_runtime_key(&dk.expected),
        None => Vec::new(),
    };
    let params = EncodingParams {
        base_key: base_key.clone(),
        junk_rate: s.junk_rate,
        runtime_key,
    };
    let blob = encode_entries(&plaintexts, &params, rng);
    let n = blob.entries.len();

    // -- 3) shard into K partitions ---------------------------------------
    let k = (s.partitions as usize).max(1);
    let mut partitions: Vec<Vec<String>> = vec![Vec::new(); k];
    let mut lut_p: Vec<u32> = Vec::with_capacity(n);
    let mut lut_s: Vec<u32> = Vec::with_capacity(n);
    for entry in &blob.entries {
        let pidx = rng.pick(k);
        let slot = partitions[pidx].len() as u32;
        partitions[pidx].push(entry.b64.clone());
        lut_p.push(pidx as u32);
        lut_s.push(slot);
    }

    // Shim metadata. M==1 → no wrappers (route to home shard directly).
    let m = (s.decoders as usize).max(1);
    let emit_shims = m >= 2;
    let mut shim_names: Vec<String> = Vec::new();
    let mut shim_masks: Vec<u32> = Vec::new();
    let mut shim_perm: Option<Vec<u32>> = None;
    if emit_shims {
        for _ in 0..m {
            shim_names.push(cfg.fresh_name());
        }
        shim_masks.push(rng.random_u32());
        if m >= 2 {
            let add = (rng.random_u32() as u64) % (n as u64).max(1);
            shim_masks.push(add as u32);
        }
        if m >= 3 {
            shim_perm = Some(build_involution(n, rng));
        }
    }

    // -- 4) dispatch plan --------------------------------------------------
    let anchor = OpaqueAnchor::decoder(core_name.clone());
    let plan = build_dispatch_plan(
        &pending,
        &core_name,
        emit_shims,
        &shim_names,
        &shim_masks,
        shim_perm.as_deref(),
        &lut_p,
        &lut_s,
        n,
        rng,
        &anchor,
    );

    // Decoys: count = decoders + partitions, sized near the average real entry.
    let decoy_count = (s.decoders as usize) + (s.partitions as usize);
    let decoys: Vec<String> = if decoy_count == 0 {
        Vec::new()
    } else {
        let avg_raw_len = if blob.entries.is_empty() {
            16usize
        } else {
            blob.entries.iter().map(|e| e.raw.len()).sum::<usize>() / blob.entries.len()
        }
        .max(8);
        (0..decoy_count)
            .map(|_| encode::base64_encode(&rng.random_bytes(avg_raw_len)))
            .collect()
    };

    // Anti-tamper key coupling: only in Encrypt mode. Forced odd (nonzero) so a
    // tampered run always corrupts the key. Drawn under the mode gate so Encode-mode
    // RNG (and output) stays byte-identical to a build without it.
    let tamper_byte = if s.mode == StringMode::Encrypt {
        Some((rng.random_u32() as u8) | 1)
    } else {
        None
    };
    let tamper_expected = encode::djb2_nums(&blob.refs, &lut_p, &lut_s);

    // -- 4b) strings-in-VM (opt-in) ----------------------------------------
    //
    // Compile the per-index decode primitive to VM bytecode and emit it INLINE in
    // this stub (its OWN table + interpreter, NOT the virtualize pass's shared
    // table). The per-index decode then runs as bytecode. On any compile/build
    // failure we fall back to the plain-JS decoder and surface a single Note, rather
    // than emit a miscompiled chunk (bail-to-safe).
    //
    // Everything is drawn from `rng` ONLY when `in_vm` is on, so the default-off RNG
    // sequence — and the entire decoder output — stays byte-identical to a non-VM
    // build (the hard invariant). Stage 5 (`exec_trace_key`) appends two trailing
    // params + the accumulator block to the primitive under its own gate; Stage 4
    // (`self_coupled_key`) only affects the wrapper + the post-codegen patch.
    let verify = cfg.resolved().engine.verify;
    let mut vm_decode: Option<VmDecodeParams> = None;
    let mut vm_prologue: Vec<Stmt> = Vec::new();
    let mut self_coupled_interp: Option<String> = None;
    let mut self_coupled_byte: u8 = 0;
    let mut exec_trace_byte: u8 = 0;
    let mut exec_trace_expected: u32 = 0;

    if s.in_vm {
        // Stage 5: embed the trace-augmented primitive only when the flag is active
        // (on + not verify). Otherwise embed the base primitive verbatim so the arity
        // matches the (un-augmented) thunk arg list.
        let want_exec_trace = s.exec_trace_key && !verify;
        let primitive: std::borrow::Cow<'static, str> = if want_exec_trace {
            std::borrow::Cow::Owned(stub::vm_decode_primitive_with_trace())
        } else {
            std::borrow::Cow::Borrowed(stub::VM_DECODE_PRIMITIVE)
        };
        match build_vm_decode(&primitive, cfg, rng) {
            Some((params, prologue)) => {
                vm_prologue = prologue;
                // Stage 4: self-coupled key — drawn first so the off-path RNG is stable.
                if s.self_coupled_key && !verify {
                    self_coupled_byte = (rng.random_u32() as u8) | 1;
                    self_coupled_interp = Some(params.interp_name.clone());
                }
                // Stage 5: exec-trace key — drawn after self_coupled so the off-path
                // RNG sequence stays byte-identical to a build without the flag.
                if want_exec_trace {
                    exec_trace_byte = (rng.random_u32() as u8) | 1;
                    // Mirror the in-VM `bk`: `base_key` (mask reversed in-VM) with the
                    // runtime keystream XOR-folded in (cycled mod 17) when a dynamic key
                    // is configured. `derive_runtime_key` is pure (no RNG), so this does
                    // not perturb the gated-off output.
                    let key_on = s.dynamic_key.is_some();
                    let runtime_key = match &s.dynamic_key {
                        Some(dk) => derive_runtime_key(&dk.expected),
                        None => Vec::new(),
                    };
                    let mut bk = base_key.clone();
                    if !runtime_key.is_empty() {
                        for (q, b) in bk.iter_mut().enumerate() {
                            *b ^= runtime_key[q % runtime_key.len()];
                        }
                    }
                    exec_trace_expected =
                        stub::exec_trace_acc(&bk, &blob.refs, &lut_p, &lut_s, key_on);
                }
                vm_decode = Some(params);
            }
            None => {
                // Bail-to-safe: the decode primitive did not compile/build. Emit the
                // plain-JS decoder and tell the caller the VM path was not taken.
                notes.push(mangler_core::Note::from(
                    "strings",
                    "--strings-in-vm: decode primitive did not compile to bytecode; emitted plain-JS decoder",
                ));
            }
        }
    }

    // -- 5) emit stub ------------------------------------------------------
    let masked_key = mask_base_key(&base_key, &blob.refs, &lut_p, &lut_s);
    let self_coupled_active = vm_decode.is_some() && self_coupled_interp.is_some();
    let stub_params = StubParams {
        core_name: core_name.clone(),
        partitions,
        lut_p,
        lut_s,
        refs: blob.refs.clone(),
        base_key_b64: encode::base64_encode(&masked_key),
        shim_names,
        shim_masks,
        shim_perm,
        decoys,
        tamper_byte,
        tamper_expected,
        runtime_key_expr: s.dynamic_key.as_ref().map(|dk| dk.source_expr.clone()),
        vm_decode,
        self_coupled_key: self_coupled_active,
        self_coupled_byte,
        exec_trace_byte,
        exec_trace_expected,
    };
    let stub_src = render_stub(&stub_params);

    // -- 6) finalize call sites + splice the stub --------------------------
    let mut finalizer = RewriteFinalizer::new(&core_name, &plan);
    ast.program_mut().visit_mut_with(&mut finalizer);

    let mut stub_stmts = parse_stub_stmts(&stub_src)?;
    // The VM prologue (interpreter + program table) is injected INSIDE the `core`
    // IIFE body (before its existing statements) rather than at the program top. This
    // keeps the whole VM table+interpreter inside the `var <core> = (function(){…})()`
    // initializer subtree, which the downstream opaque passes (expr / cf-flatten /
    // dead-code) already PRUNE via their `protect_name` skip — so they never rewrite
    // the bytecode integer arrays or the interpreter (which would corrupt the VM).
    // Declarations inside the IIFE are evaluated at its eval-time top, before any
    // `decodeOne` runs, so `interp`/`table` are defined when the first decode fires.
    if !vm_prologue.is_empty() {
        inject_into_core_iife(&core_name, &mut stub_stmts, vm_prologue);
    }
    splice_at_prologue(ast.program_mut(), stub_stmts);

    // Record the decoder anchor so downstream passes can build opaque values. Safe
    // because n >= 1 here (we returned early on an empty pool), so `core(0)` decodes.
    bus.put(DecoderAnchorArtifact { core_name })
        .map_err(|e| Error::transform("strings", e.to_string()))?;

    // Stage 4: hand the interpreter name to the runner so its post-codegen finalizer
    // can compute the build-time expected source hash and patch the `SCK<digits>`
    // sentinel. Only when the self-coupled key is actually active (flag on, VM chunk
    // produced, not verify).
    if self_coupled_active
        && let Some(interp_name) = self_coupled_interp
    {
        bus.put(SelfCoupledKeyArtifact { interp_name })
            .map_err(|e| Error::transform("strings", e.to_string()))?;
    }

    Ok(())
}

/// Compile the decode `primitive` (a `function(...){...}` expression source) to VM
/// bytecode, register it in its OWN [`TableBuilder`], and emit the interpreter +
/// program-table prologue. Returns `None` on any parse/compile/build failure
/// (bail-to-safe — the caller emits the plain-JS decoder instead).
///
/// The table is independent of the virtualize pass's shared table (per the design:
/// strings-in-VM emits its own interpreter inline and does NOT `put` a `VmTable`).
fn build_vm_decode(
    primitive: &str,
    cfg: &FileConfig,
    rng: &mut Rng,
) -> Option<(VmDecodeParams, Vec<Stmt>)> {
    let (params, body) = parse_fn_expr(primitive)?;
    let compiled = compile_body(&params, &body).ok()?;

    // Draw the diversity from the pass RNG so the whole build stays deterministic and
    // gated behind the `in_vm` flag.
    let mut tb = TableBuilder::with_diversity(VmDiversity::draw(rng));
    let chunk = tb.add(compiled);

    let names = VmNames {
        lean_interp: cfg.fresh_name(),
        eh_interp: cfg.fresh_name(),
        table: cfg.fresh_name(),
        rc: cfg.fresh_name(),
        sy: cfg.fresh_name(),
    };
    let vt = tb.finish(&names).ok()?;
    // The decode primitive is a flat function: it never needs the EH interpreter. If
    // it somehow did, the lean-interp thunk would be wrong — bail to safe.
    if chunk.needs_eh {
        return None;
    }

    let vm_decode = VmDecodeParams {
        interp_name: names.lean_interp.clone(),
        table_name: names.table.clone(),
        chunk_index: chunk.index,
        captures: chunk.captures.clone(),
        cap_start: chunk.cap_start,
        pcount: chunk.pcount,
    };
    Some((vm_decode, vt.prologue))
}

/// Parse a `function(...){...}` expression source into `(params, body)` for the VM
/// compiler. Returns `None` if the source is not a single function expression.
fn parse_fn_expr(src: &str) -> Option<(Vec<Param>, BlockStmt)> {
    let wrapped = format!("var __f = ({src});");
    let ast = Js.parse(&wrapped, &ParseOpts::default()).ok()?;
    let stmt = match ast.into_program() {
        Program::Script(s) => s.body.into_iter().next()?,
        Program::Module(m) => m.body.into_iter().find_map(|it| match it {
            ModuleItem::Stmt(s) => Some(s),
            ModuleItem::ModuleDecl(_) => None,
        })?,
    };
    let init = match stmt {
        Stmt::Decl(Decl::Var(v)) => *v.decls.into_iter().next()?.init?,
        _ => return None,
    };
    let func = match init {
        Expr::Paren(p) => match *p.expr {
            Expr::Fn(fe) => fe.function,
            _ => return None,
        },
        Expr::Fn(fe) => fe.function,
        _ => return None,
    };
    Some((func.params, func.body?))
}

/// Parse the rendered stub source into top-level statements, validating it is
/// well-formed JS. A parse failure is a hard error (a malformed stub must never
/// reach output).
fn parse_stub_stmts(stub_src: &str) -> Result<Vec<Stmt>> {
    let ast = Js
        .parse(stub_src, &ParseOpts::default())
        .map_err(|e| Error::transform("strings", format!("decoder stub failed to parse: {e}")))?;
    match ast.into_program() {
        Program::Script(s) => Ok(s.body),
        Program::Module(m) => Ok(m
            .body
            .into_iter()
            .filter_map(|it| match it {
                ModuleItem::Stmt(s) => Some(s),
                ModuleItem::ModuleDecl(_) => None,
            })
            .collect()),
    }
}

/// Splice `stmts` into `program` just after any leading directive prologue, so a
/// `"use strict"` stays first.
fn splice_at_prologue(program: &mut Program, stmts: Vec<Stmt>) {
    match program {
        Program::Script(sc) => {
            let at = leading_directive_count(&sc.body);
            sc.body.splice(at..at, stmts);
        }
        Program::Module(md) => {
            let at = md
                .body
                .iter()
                .take_while(|it| matches!(it, ModuleItem::Stmt(s) if is_directive_stmt(s)))
                .count();
            let items: Vec<ModuleItem> = stmts.into_iter().map(ModuleItem::Stmt).collect();
            md.body.splice(at..at, items);
        }
    }
}

/// Prepend `prologue` into the body of the `var <core> = (function(){…})()` IIFE so
/// the VM interpreter + program table live INSIDE the protected `core` initializer
/// subtree. The decode wrapper (`decodeOne`) references the interpreter + table by
/// name; placing them in the same IIFE scope (declarations evaluated at the IIFE's
/// eval-time top) keeps them resolvable and shields their integer arrays from the
/// downstream opaque-injection passes. A no-op if the expected shape is not found
/// (fail-safe — but `render_core` always emits this shape).
fn inject_into_core_iife(core_name: &str, stub_stmts: &mut [Stmt], prologue: Vec<Stmt>) {
    for stmt in stub_stmts.iter_mut() {
        let Stmt::Decl(Decl::Var(var)) = stmt else { continue };
        for decl in var.decls.iter_mut() {
            let Pat::Ident(bi) = &decl.name else { continue };
            if bi.id.sym.as_ref() != core_name {
                continue;
            }
            // `var <core> = (function(){…})()` — the init is a CallExpr whose callee
            // is (optionally parenthesized) a function expression.
            let Some(init) = decl.init.as_deref_mut() else { return };
            if let Some(body) = iife_body_mut(init) {
                body.stmts.splice(0..0, prologue);
            }
            return;
        }
    }
}

/// Return the body block of the function-expression callee of an IIFE
/// `(function(){…})()` (unwrapping a `Paren`), or `None` if `expr` is not that shape.
fn iife_body_mut(expr: &mut Expr) -> Option<&mut BlockStmt> {
    let Expr::Call(call) = expr else { return None };
    let callee = match &mut call.callee {
        Callee::Expr(e) => e.as_mut(),
        _ => return None,
    };
    let callee = match callee {
        Expr::Paren(p) => p.expr.as_mut(),
        other => other,
    };
    match callee {
        Expr::Fn(fe) => fe.function.body.as_mut(),
        _ => None,
    }
}

fn is_directive_stmt(stmt: &Stmt) -> bool {
    matches!(stmt, Stmt::Expr(ExprStmt { expr, .. }) if matches!(&**expr, Expr::Lit(Lit::Str(_))))
}

fn leading_directive_count(stmts: &[Stmt]) -> usize {
    stmts.iter().take_while(|s| is_directive_stmt(s)).count()
}

/// Build an involution permutation of `[0, n)` (its own inverse) via random pair
/// swaps. Deterministic given `rng`.
fn build_involution(n: usize, rng: &mut Rng) -> Vec<u32> {
    let mut perm: Vec<u32> = (0..n as u32).collect();
    if n < 2 {
        return perm;
    }
    let mut unpaired: Vec<usize> = (0..n).collect();
    while unpaired.len() >= 2 {
        let i = unpaired.remove(0);
        if rng.pick(2) == 0 {
            continue;
        }
        let j_pos = rng.pick(unpaired.len());
        let j = unpaired.remove(j_pos);
        perm.swap(i, j);
    }
    perm
}

/// Build the per-call-site dispatch plan: pick a shim (or home-shard route) and a
/// non-foldable index expression for every recorded call site.
#[allow(clippy::too_many_arguments)]
fn build_dispatch_plan(
    pending: &[PendingCall],
    core_name: &str,
    emit_shims: bool,
    shim_names: &[String],
    shim_masks: &[u32],
    shim_perm: Option<&[u32]>,
    lut_p: &[u32],
    lut_s: &[u32],
    n: usize,
    rng: &mut Rng,
    anchor: &OpaqueAnchor,
) -> DispatchPlan {
    let mut call_callees = Vec::with_capacity(pending.len());
    let mut call_index = Vec::with_capacity(pending.len());
    let m = shim_names.len();

    for pc in pending {
        let idx = pc.logical_idx as u32;
        if !emit_shims {
            // B1: no shim wrappers — route each site to its home shard
            // `core._[lutP[idx]]`, passing the (non-foldable) shard-local slot.
            let part = lut_p[idx as usize];
            let slot = lut_s[idx as usize];
            call_callees.push(shard_callee(core_name, part));
            call_index.push(index_expr(rng, anchor, slot));
            continue;
        }

        let shim_idx = (rng.random_u32() as usize) % m;
        let name = shim_names[shim_idx].clone();
        let transformed: u32 = match shim_idx {
            0 => idx ^ shim_masks[0],
            1 => {
                let add = shim_masks[1];
                (((idx as u64) + (add as u64)) % (n as u64).max(1)) as u32
            }
            2 => {
                let perm = shim_perm.expect("shim_perm must be present for shim 2");
                perm[idx as usize]
            }
            _ => unreachable!("only 3 shim transforms defined"),
        };
        call_callees.push(ident_callee(&name));
        call_index.push(index_expr(rng, anchor, transformed));
    }

    DispatchPlan { call_callees, call_index }
}

/// A bare identifier callee expression (`name`).
fn ident_callee(name: &str) -> Expr {
    Expr::Ident(Ident::new(name.into(), DUMMY_SP, SyntaxContext::empty()))
}

/// The home-shard callee `<core>._[<partition>]`.
fn shard_callee(core_name: &str, partition: u32) -> Expr {
    Expr::Member(MemberExpr {
        span: DUMMY_SP,
        obj: Box::new(Expr::Member(MemberExpr {
            span: DUMMY_SP,
            obj: Box::new(ident_callee(core_name)),
            prop: MemberProp::Ident(IdentName::new("_".into(), DUMMY_SP)),
        })),
        prop: MemberProp::Computed(ComputedPropName {
            span: DUMMY_SP,
            expr: Box::new(Expr::Lit(Lit::Num(Number {
                span: DUMMY_SP,
                value: partition as f64,
                raw: None,
            }))),
        }),
    })
}

/// Non-foldable index expression for a call site: a decoder-anchored `opaque_u32`
/// for `value >= 2` (hidden at every preset), a bare literal for `0`/`1` (trivially
/// inferable, so obfuscating them is pure bloat).
fn index_expr(rng: &mut Rng, anchor: &OpaqueAnchor, value: u32) -> Expr {
    if value >= 2 {
        opaque_u32(rng, anchor, value)
    } else {
        Expr::Lit(Lit::Num(Number { span: DUMMY_SP, value: value as f64, raw: None }))
    }
}

#[cfg(test)]
mod tests;
