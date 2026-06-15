//! Statement-family compiler: block scopes, decls, control flow, loops, switch,
//! try/catch/finally, the `arguments` snapshot, and nested-closure emission.
//!
//! Every `emit_*` here takes `&mut Cx` and shares the frame model and the other
//! construct-family emitters (`expr`, `destructure`) via `use super::*`.


use swc_core::ecma::ast::*;
use swc_core::ecma::visit::{Visit, VisitWith};

use super::*;
use crate::chunk::ChildChunk;
use crate::isa::{Instr, SELF_UPVALUE};
use mangler_jsast::analysis::binding_names;

pub(crate) fn emit_block_scope(cx: &mut Cx<'_>, stmts: &[Stmt]) {
    cx.push_scope();
    let lows = direct_block_bindings(stmts);
    bind_lows_in_scope(cx, &lows);
    for s in stmts {
        emit_stmt(cx, s);
        if cx.bailed() {
            break;
        }
    }
    cx.pop_scope();
}

pub(crate) fn emit_stmt(cx: &mut Cx<'_>, stmt: &Stmt) {
    if cx.bailed() {
        return;
    }
    match stmt {
        Stmt::Block(b) => emit_block_scope(cx, &b.stmts),
        Stmt::Empty(_) => {}
        Stmt::Decl(Decl::Var(v)) => {
            for d in &v.decls {
                match &d.name {
                    Pat::Ident(bi) => {
                        if let Some(init) = &d.init {
                            let name = bi.id.sym.as_ref();
                            // D5: a boxed local's cell was already seeded at the
                            // prologue, so its declaration writes THROUGH the cell
                            // (`StoreCell`) to keep the shared array that closures
                            // capture; a plain local uses `StoreLocal`.
                            let celled = cx.is_boxed_local(name);
                            let slot = cx.resolve(name);
                            emit_expr(cx, init);
                            cx.emit(if celled {
                                Instr::StoreCell(slot)
                            } else {
                                Instr::StoreLocal(slot)
                            });
                            cx.emit(Instr::Pop);
                        }
                    }
                    Pat::Object(obj) => {
                        // `var {a, b: c, ...r} = init;` — destructuring requires an
                        // initializer (a bindingless `var {a};` is a syntax error).
                        match &d.init {
                            Some(init) => {
                                emit_expr(cx, init);
                                if cx.bailed() {
                                    return;
                                }
                                let t = cx.alloc_temp();
                                cx.emit(Instr::StoreLocal(t));
                                cx.emit(Instr::Pop);
                                emit_destructure_object(cx, obj, t);
                                cx.free_temp();
                            }
                            None => {
                                cx.bail();
                                return;
                            }
                        }
                    }
                    Pat::Array(arr) => {
                        // `var [a, , c = 1, ...r] = init;` — the iterator is taken
                        // off the init value directly (no source temp).
                        match &d.init {
                            Some(init) => {
                                emit_expr(cx, init);
                                if cx.bailed() {
                                    return;
                                }
                                emit_destructure_array(cx, arr);
                            }
                            None => {
                                cx.bail();
                                return;
                            }
                        }
                    }
                    _ => {
                        cx.bail();
                        return;
                    }
                }
            }
        }
        Stmt::Expr(es) => {
            emit_expr_stmt(cx, &es.expr);
        }
        // D5: a top-level `function f(){…}` declaration was already hoisted (its
        // closure built and stored into `f`'s slot) before the body emission loop —
        // see the fn-decl hoisting prologue in `compile_body_inner`. So at its
        // textual position it is a no-op. (A block-nested fn-decl never reaches here:
        // `check_no_nested_block_fn_decls` bailed the whole function.)
        Stmt::Decl(Decl::Fn(_)) => {}
        Stmt::Return(r) => {
            match &r.arg {
                Some(a) => emit_expr(cx, a),
                None => cx.emit(Instr::PushUndef),
            }
            // With no handler active, the fast `Ret` returns directly. With a
            // handler active, `RetUnwind` runs any intervening finally/close first.
            if cx.handler_depth == 0 {
                cx.emit(Instr::Ret);
            } else {
                cx.emit(Instr::RetUnwind);
            }
        }
        Stmt::Throw(t) => {
            // Evaluate the operand and `throw` it from inside the VM frame; the
            // exception propagates out to whatever surrounds the (non-virtualized)
            // call site.
            emit_expr(cx, &t.arg);
            cx.emit(Instr::Throw);
        }
        Stmt::If(i) => {
            emit_expr(cx, &i.test);
            let l1 = cx.code.len();
            cx.emit(Instr::JumpIfFalse(u32::MAX));
            emit_stmt(cx, &i.cons);
            match &i.alt {
                Some(alt) => {
                    let l2 = cx.code.len();
                    cx.emit(Instr::Jump(u32::MAX));
                    patch(cx, l1, cx.here());
                    emit_stmt(cx, alt);
                    patch(cx, l2, cx.here());
                }
                None => {
                    patch(cx, l1, cx.here());
                }
            }
        }
        Stmt::While(w) => {
            let test_pc = cx.here();
            emit_expr(cx, &w.test);
            let exit = cx.code.len();
            cx.emit(Instr::JumpIfFalse(u32::MAX));
            cx.frames.push(Frame {
                kind: FrameKind::Loop,
                label: cx.pending_label.take(),
                handler_depth: cx.handler_depth,
                continue_handler_depth: cx.handler_depth,
                break_jumps: Vec::new(),
                continue_jumps: Vec::new(),
            });
            emit_stmt(cx, &w.body);
            cx.emit(Instr::Jump(test_pc));
            let end = cx.here();
            patch(cx, exit, end);
            let lp = cx.frames.pop().unwrap();
            for j in lp.break_jumps {
                patch(cx, j, end);
            }
            for j in lp.continue_jumps {
                patch(cx, j, test_pc);
            }
        }
        Stmt::For(f) => {
            // A `for (let i …)` head opens a per-loop lexical scope (D3): push a
            // frame holding the head's let/const bindings so the init store, test,
            // body and update all resolve `i` to its own slot (shadowing any outer
            // `i`). A `var`/expression head adds nothing and the frame stays empty.
            let head_lows = match &f.init {
                Some(VarDeclOrExpr::VarDecl(v)) => for_var_decl_block_bindings(v),
                _ => Vec::new(),
            };
            cx.push_scope();
            bind_lows_in_scope(cx, &head_lows);
            // init
            if let Some(init) = &f.init {
                match init {
                    VarDeclOrExpr::VarDecl(v) => {
                        emit_stmt(cx, &Stmt::Decl(Decl::Var(v.clone())));
                    }
                    VarDeclOrExpr::Expr(e) => {
                        emit_expr_stmt(cx, e);
                    }
                }
            }
            let test_pc = cx.here();
            match &f.test {
                Some(t) => emit_expr(cx, t),
                None => {
                    let ci = cx.const_bool(true);
                    cx.emit(Instr::PushConst(ci));
                }
            }
            let exit = cx.code.len();
            cx.emit(Instr::JumpIfFalse(u32::MAX));
            cx.frames.push(Frame {
                kind: FrameKind::Loop,
                label: cx.pending_label.take(),
                handler_depth: cx.handler_depth,
                continue_handler_depth: cx.handler_depth,
                break_jumps: Vec::new(),
                continue_jumps: Vec::new(),
            });
            emit_stmt(cx, &f.body);
            let update_pc = cx.here();
            if let Some(u) = &f.update {
                emit_expr_stmt(cx, u);
            }
            cx.emit(Instr::Jump(test_pc));
            let end = cx.here();
            patch(cx, exit, end);
            let lp = cx.frames.pop().unwrap();
            for j in lp.break_jumps {
                patch(cx, j, end);
            }
            for j in lp.continue_jumps {
                patch(cx, j, update_pc);
            }
            cx.pop_scope(); // close the per-loop head scope
        }
        Stmt::DoWhile(d) => {
            // Body-first loop: run body, then loop back while test is truthy.
            let start = cx.here();
            cx.frames.push(Frame {
                kind: FrameKind::Loop,
                label: cx.pending_label.take(),
                handler_depth: cx.handler_depth,
                continue_handler_depth: cx.handler_depth,
                break_jumps: Vec::new(),
                continue_jumps: Vec::new(),
            });
            emit_stmt(cx, &d.body);
            let cont = cx.here(); // `continue` jumps to the test
            emit_expr(cx, &d.test);
            // test false -> JumpIfFalse jumps to `end`; test true -> falls through to Jump(start).
            let exit_j = cx.code.len();
            cx.emit(Instr::JumpIfFalse(u32::MAX));
            cx.emit(Instr::Jump(start));
            let end = cx.here();
            patch(cx, exit_j, end);
            let lp = cx.frames.pop().unwrap();
            for j in lp.break_jumps {
                patch(cx, j, end);
            }
            for j in lp.continue_jumps {
                patch(cx, j, cont);
            }
        }
        Stmt::Break(b) => {
            // A labeled break targets the nearest enclosing construct carrying
            // that label (loop/switch/block); an unlabeled break targets the
            // nearest enclosing loop OR switch.
            let target = b.label.as_ref().map(|l| l.sym.to_string());
            match find_break_target(cx, &target) {
                Some(i) => {
                    let frame_depth = cx.frames[i].handler_depth;
                    let idx = cx.code.len();
                    emit_cf_jump(cx, frame_depth);
                    cx.frames[i].break_jumps.push(idx);
                }
                None => {
                    cx.bail();
                }
            }
        }
        Stmt::Continue(c) => {
            // A labeled continue targets the nearest enclosing loop carrying that
            // label; an unlabeled continue targets the nearest enclosing loop
            // (never a switch).
            let target = c.label.as_ref().map(|l| l.sym.to_string());
            match find_continue_target(cx, &target) {
                Some(i) => {
                    let frame_depth = cx.frames[i].continue_handler_depth;
                    let idx = cx.code.len();
                    emit_cf_jump(cx, frame_depth);
                    cx.frames[i].continue_jumps.push(idx);
                }
                None => {
                    cx.bail();
                }
            }
        }
        Stmt::Switch(s) => {
            // A `switch` body is ONE lexical block scope shared across all cases
            // (D3): a `let` in any case is scoped to the whole switch. Push a frame
            // holding every case's direct let/const bindings around the lowering.
            let mut lows = Vec::new();
            for case in &s.cases {
                lows.extend(direct_block_bindings(&case.cons));
            }
            cx.push_scope();
            bind_lows_in_scope(cx, &lows);
            emit_switch(cx, s);
            cx.pop_scope();
        }
        Stmt::ForIn(s) => {
            // A `for (let k in …)` head opens a per-loop lexical scope (D3). Push it
            // around the whole lowering so the binding resolves to its own slot even
            // when it shadows an outer name; `var`/pattern heads add nothing.
            let lows = for_head_block_bindings(&s.left);
            cx.push_scope();
            bind_lows_in_scope(cx, &lows);
            emit_for_in(cx, s);
            cx.pop_scope();
        }
        Stmt::ForOf(s) => {
            let lows = for_head_block_bindings(&s.left);
            cx.push_scope();
            bind_lows_in_scope(cx, &lows);
            emit_for_of(cx, s);
            cx.pop_scope();
        }
        Stmt::Try(t) => emit_try(cx, t),
        Stmt::Labeled(l) => {
            let name = l.label.sym.to_string();
            match &*l.body {
                // A labeled loop: stash the label so the loop arm consumes it
                // into its own `Frame.label` (so `break/continue label` resolve
                // to that loop). Do NOT push a Block frame here.
                Stmt::While(_) | Stmt::For(_) | Stmt::DoWhile(_) => {
                    cx.pending_label = Some(name);
                    emit_stmt(cx, &l.body);
                }
                // A labeled non-loop (e.g. a block): push a `Block` frame so that
                // `break label` can target the construct's end. `continue label`
                // to a non-loop is a JS syntax error and never parses, so a Block
                // frame only ever receives break jumps.
                _ => {
                    cx.frames.push(Frame {
                        kind: FrameKind::Block,
                        label: Some(name),
                        handler_depth: cx.handler_depth,
                        continue_handler_depth: cx.handler_depth,
                        break_jumps: Vec::new(),
                        continue_jumps: Vec::new(),
                    });
                    emit_stmt(cx, &l.body);
                    let end = cx.here();
                    let frame = cx.frames.pop().unwrap();
                    for j in frame.break_jumps {
                        patch(cx, j, end);
                    }
                }
            }
        }
        _ => cx.bail(),
    }
}

/// Lower a `switch` via a `Dup`-compare chain (design §L1).
///
/// The discriminant is evaluated once and kept on the stack. For each case (in
/// source order) we `Dup` it, evaluate the case test, compare with `===`, and on
/// a match jump to that case's body; otherwise fall through to the next test. A
/// non-matching scan ends by jumping to the `default` body (if any) or the exit.
///
/// Crucially, JS switch semantics are: the comparison scan runs in *source*
/// order over the case labels, but once a body is selected, execution
/// fall-throughs into the following bodies (in source order) until a `break` or
/// the end. `default` participates only in the fall-through chain at its source
/// position; the scan jumps to it only after all case tests fail. We therefore
/// emit every body (cases + default) contiguously in source order so natural
/// fall-through is just "no jump between bodies", and we wire each compare-chain
/// match to the corresponding body's start PC.
pub(crate) fn emit_switch(cx: &mut Cx<'_>, s: &SwitchStmt) {
    // 1. Evaluate the discriminant once; leave it on the stack for the chain.
    emit_expr(cx, &s.discriminant);
    if cx.bailed() {
        return;
    }

    // 2. Compare chain, in source order. For each NON-default case we record the
    //    body-jump instruction index to patch once the body's start PC is known.
    //    `default_idx` notes the default case's position in source order (if any).
    let mut body_jumps: Vec<(usize, usize)> = Vec::new(); // (case_index, jump_instr_idx)
    let mut default_idx: Option<usize> = None;
    for (ci, case) in s.cases.iter().enumerate() {
        match &case.test {
            Some(test) => {
                // Dup discriminant; eval test; ===; if false skip to next test.
                cx.emit(Instr::Dup);
                emit_expr(cx, test);
                if cx.bailed() {
                    return;
                }
                cx.emit(Instr::Bin(7)); // ===
                let next = cx.code.len();
                cx.emit(Instr::JumpIfFalse(u32::MAX));
                // Matched: drop the dup'd discriminant copy is not needed (the
                // Dup result was consumed by the compare). Pop the ORIGINAL
                // discriminant, then jump to this case's body.
                cx.emit(Instr::Pop);
                let bj = cx.code.len();
                cx.emit(Instr::Jump(u32::MAX));
                body_jumps.push((ci, bj));
                // Wire the failed-compare jump to the next test (i.e. here).
                patch(cx, next, cx.here());
            }
            None => {
                default_idx = Some(ci);
            }
        }
    }

    // 3. End of the scan: no case matched. Pop the discriminant, then jump to the
    //    default body (if present) or to the switch exit. We can't know either PC
    //    yet, so record the jump to patch after the bodies are emitted.
    cx.emit(Instr::Pop);
    let scan_fallthrough = cx.code.len();
    cx.emit(Instr::Jump(u32::MAX));

    // 4. Push the switch frame so `break` inside a body targets the exit (a
    //    switch has NO continue target).
    cx.frames.push(Frame {
        kind: FrameKind::Switch,
        label: cx.pending_label.take(),
        handler_depth: cx.handler_depth,
        continue_handler_depth: cx.handler_depth,
        break_jumps: Vec::new(),
        continue_jumps: Vec::new(),
    });

    // 5. Emit each case body (and the default body) in SOURCE order. Record each
    //    body's start PC so the compare chain (and the no-match fallthrough for
    //    default) can be patched to it. Consecutive bodies fall through naturally.
    let mut body_starts: Vec<u32> = Vec::with_capacity(s.cases.len());
    for case in &s.cases {
        body_starts.push(cx.here());
        for stmt in &case.cons {
            emit_stmt(cx, stmt);
            if cx.bailed() {
                // Pop the frame to keep the stack balanced before returning.
                cx.frames.pop();
                return;
            }
        }
    }

    // 6. Switch exit PC (one past the last body).
    let exit = cx.here();

    // 7. Patch each matched-case body jump to its body start.
    for (ci, bj) in body_jumps {
        patch(cx, bj, body_starts[ci]);
    }

    // 8. Patch the no-match fallthrough: to the default body if one exists, else
    //    to the exit.
    let no_match_target = match default_idx {
        Some(di) => body_starts[di],
        None => exit,
    };
    patch(cx, scan_fallthrough, no_match_target);

    // 9. Pop the frame and patch all `break` jumps to the exit. (No continue.)
    let frame = cx.frames.pop().unwrap();
    for j in frame.break_jumps {
        patch(cx, j, exit);
    }
}

/// Where a `for-in`/`for-of` loop head binds each per-iteration value: either a
/// single slot (the common `for (var x …)` / `for (x …)` case) or a destructuring
/// pattern bound via `emit_bind_target`.
pub(crate) enum ForHeadTarget<'a> {
    /// A plain slot binding. `boxed` is true when the target is a D1 boxed mutable
    /// capture (`for (x of …)` where `x` is a cell), so the per-iteration bind
    /// stores through the cell (`StoreCell`) instead of `StoreLocal`.
    Slot(u32, bool),
    Pat(&'a Pat),
}

/// Resolve a `for-in`/`for-of` loop head to its binding target. Supports
/// `for (var x …)`/`for (x …)` (simple slot — writing a captured outer binding is
/// unsound, so a non-local plain ident bails), and `for (var [a]/{a} …)` /
/// `for ([a]/{a} …)` destructuring heads (bound per iteration). Multi-declarator
/// heads and `using` decls bail with a specific reason. Returns `None` on bail.
pub(crate) fn for_head_target<'a>(cx: &mut Cx<'_>, head: &'a ForHead) -> Option<ForHeadTarget<'a>> {
    match head {
        ForHead::VarDecl(v) => {
            if v.decls.len() != 1 {
                cx.bail_with("for_head");
                return None;
            }
            match &v.decls[0].name {
                // A `for (var x …)` head declares a fresh function-local `x` — never
                // a boxed capture (the loop var is a local binding of this body).
                Pat::Ident(bi) => Some(ForHeadTarget::Slot(cx.resolve(bi.id.sym.as_ref()), false)),
                p @ (Pat::Array(_) | Pat::Object(_)) => Some(ForHeadTarget::Pat(p)),
                _ => {
                    cx.bail_with("for_head_destructure");
                    None
                }
            }
        }
        ForHead::Pat(p) => match &**p {
            Pat::Ident(bi) => {
                let name = bi.id.sym.as_ref();
                // D1: `for (x of …)` where `x` is a boxed capture stores per
                // iteration through the cell; an unboxed capture is read-only -> bail.
                let boxed = cx.is_celled(name);
                if !cx.is_param_or_local(name) && !boxed {
                    // Writing the loop var back to a captured outer binding is not
                    // modeled (read-only capture only) — bail.
                    cx.bail_with("mutable_capture");
                    return None;
                }
                Some(ForHeadTarget::Slot(cx.resolve(name), boxed))
            }
            // Array/object destructuring head, or a member/assignment target
            // (`emit_bind_target` binds the former and bails cleanly on the latter).
            _ => Some(ForHeadTarget::Pat(p)),
        },
        ForHead::UsingDecl(_) => {
            cx.bail_with("for_head_using");
            None
        }
    }
}

/// Bind the per-iteration value on top of the stack to a resolved loop-head
/// target, consuming it (a plain slot store, or a destructure).
pub(crate) fn emit_for_head_bind(cx: &mut Cx<'_>, target: &ForHeadTarget) {
    match target {
        ForHeadTarget::Slot(slot, boxed) => {
            cx.emit(if *boxed { Instr::StoreCell(*slot) } else { Instr::StoreLocal(*slot) });
            cx.emit(Instr::Pop);
        }
        ForHeadTarget::Pat(p) => emit_bind_target(cx, p),
    }
}

/// Lower a `for (k in obj)` via the `EnumKeys` snapshot opcode + an indexed loop
/// (design §L5). The object's enumerable keys are snapshotted once into a temp
/// array; the loop then walks that array by index, assigning each key to the loop
/// binding before running the body. This matches the JS observable order and is
/// immune to in-loop key mutation of the source object (which JS leaves
/// implementation-defined — snapshotting is a sound, common choice).
///
/// Uses two reserved temps (the keys array + the index), released at loop end.
pub(crate) fn emit_for_in(cx: &mut Cx<'_>, s: &ForInStmt) {
    // 1. Loop binding target (the `k` in `for (k in obj)`, or a destructure head).
    let target = match for_head_target(cx, &s.left) {
        Some(t) => t,
        None => return,
    };

    // 2. Snapshot the source object's enumerable keys into `keys_temp`.
    emit_expr(cx, &s.right);
    if cx.bailed() {
        return;
    }
    cx.emit(Instr::EnumKeys);
    let keys_temp = cx.alloc_temp();
    cx.emit(Instr::StoreLocal(keys_temp));
    cx.emit(Instr::Pop);

    // 3. idx = 0.
    let idx_temp = cx.alloc_temp();
    let zero = cx.const_num(0.0);
    cx.emit(Instr::PushConst(zero));
    cx.emit(Instr::StoreLocal(idx_temp));
    cx.emit(Instr::Pop);

    // 4. test: idx < keys.length.
    let test_pc = cx.here();
    cx.emit(Instr::LoadLocal(idx_temp));
    cx.emit(Instr::LoadLocal(keys_temp));
    let len_key = cx.const_str("length".to_string());
    cx.emit(Instr::PushConst(len_key));
    cx.emit(Instr::GetProp);
    cx.emit(Instr::Bin(10)); // <
    let exit = cx.code.len();
    cx.emit(Instr::JumpIfFalse(u32::MAX));

    // 5. binding = keys[idx]  (a plain store, or a per-iteration destructure).
    cx.emit(Instr::LoadLocal(keys_temp));
    cx.emit(Instr::LoadLocal(idx_temp));
    cx.emit(Instr::GetProp);
    emit_for_head_bind(cx, &target);
    if cx.bailed() {
        return;
    }

    // 6. body (break -> exit, continue -> the increment at `cont`).
    cx.frames.push(Frame {
        kind: FrameKind::Loop,
        label: cx.pending_label.take(),
        handler_depth: cx.handler_depth,
        continue_handler_depth: cx.handler_depth,
        break_jumps: Vec::new(),
        continue_jumps: Vec::new(),
    });
    emit_stmt(cx, &s.body);

    // 7. increment: idx = idx + 1; loop back to the test.
    let cont = cx.here();
    cx.emit(Instr::LoadLocal(idx_temp));
    let one = cx.const_num(1.0);
    cx.emit(Instr::PushConst(one));
    cx.emit(Instr::Bin(0)); // +
    cx.emit(Instr::StoreLocal(idx_temp));
    cx.emit(Instr::Pop);
    cx.emit(Instr::Jump(test_pc));

    // 8. exit + patch break/continue.
    let end = cx.here();
    patch(cx, exit, end);
    let lp = cx.frames.pop().unwrap();
    for j in lp.break_jumps {
        patch(cx, j, end);
    }
    for j in lp.continue_jumps {
        patch(cx, j, cont);
    }

    // 9. Release the two temps (LIFO).
    cx.free_temp(); // idx_temp
    cx.free_temp(); // keys_temp
}

/// Lower a `for (x of ITER)` via the iterator opcodes + a close-on-abrupt handler
/// (design "Iterator mechanism" + §X). The iterator is obtained once; each step
/// reads the next value (the `IterStep` flag drives the existing `JumpIfFalse`);
/// `break`/`return`/`throw` route through the close handler (`IterClose`) via the
/// unwind machinery, while `continue` and normal exhaustion do NOT close.
///
/// ```text
/// eval ITER; GetIter; ->it
/// PushHandler(absent, CLOSE)             ; finally-only handler -> IterClose
/// LOOP: LoadLocal(it); IterStep; JumpIfFalse NORMALEXIT
///   ->x; BODY; CONT: Jump LOOP
/// NORMALEXIT: PopHandler; Jump EXIT      ; exhausted -> drop handler, NO close
/// CLOSE: LoadLocal(it); IterClose; EndFinally
/// EXIT:
/// ```
pub(crate) fn emit_for_of(cx: &mut Cx<'_>, s: &ForOfStmt) {
    // `for await` is async (Tier 2); await/yield eligibility rejects it, guard too.
    if s.is_await {
        cx.bail_with("for_await");
        return;
    }
    let target = match for_head_target(cx, &s.left) {
        Some(t) => t,
        None => return,
    };

    // iterator = ITER[Symbol.iterator]()
    emit_expr(cx, &s.right);
    if cx.bailed() {
        return;
    }
    cx.emit(Instr::GetIter);
    let it_temp = cx.alloc_temp();
    cx.emit(Instr::StoreLocal(it_temp));
    cx.emit(Instr::Pop);

    // Close handler (finally-only) active across the loop body.
    let outer_depth = cx.handler_depth;
    let ph = cx.code.len();
    cx.emit(Instr::PushHandler(u32::MAX, u32::MAX)); // catch absent; CLOSE patched
    cx.handler_depth += 1;

    // `break` unwinds the close handler (handler_depth = outer); `continue` stays
    // inside it (continue_handler_depth = current = outer + 1), so a `continue`
    // does not close the iterator but still unwinds any deeper handlers.
    cx.frames.push(Frame {
        kind: FrameKind::Loop,
        label: cx.pending_label.take(),
        handler_depth: outer_depth,
        continue_handler_depth: cx.handler_depth,
        break_jumps: Vec::new(),
        continue_jumps: Vec::new(),
    });

    // LOOP: x = step(it); exit when done.
    let loop_pc = cx.here();
    cx.emit(Instr::LoadLocal(it_temp));
    cx.emit(Instr::IterStep);
    let normal_exit_j = cx.code.len();
    cx.emit(Instr::JumpIfFalse(u32::MAX));
    // Bind the value: a plain store, or a per-iteration destructure (whose own
    // close handler nests inside this loop's, then balances before the body).
    emit_for_head_bind(cx, &target);
    if cx.bailed() {
        cx.frames.pop();
        return;
    }

    // BODY.
    emit_stmt(cx, &s.body);

    // CONT (continue target): loop back.
    let cont_pc = cx.here();
    cx.emit(Instr::Jump(loop_pc));

    // NORMALEXIT: exhausted -> drop the handler (no close), jump past CLOSE.
    let normal_exit = cx.here();
    patch(cx, normal_exit_j, normal_exit);
    cx.emit(Instr::PopHandler);
    cx.handler_depth -= 1;
    let exit_j = cx.code.len();
    cx.emit(Instr::Jump(u32::MAX));

    // CLOSE: the finally body that closes the iterator on abrupt completion.
    let close_pc = cx.here();
    patch_handler_fin(cx, ph, close_pc);
    cx.emit(Instr::LoadLocal(it_temp));
    cx.emit(Instr::IterClose);
    cx.emit(Instr::EndFinally);

    // EXIT: patch normal-exit jump + frame break/continue jumps.
    let exit = cx.here();
    patch(cx, exit_j, exit);
    let lp = cx.frames.pop().unwrap();
    for j in lp.break_jumps {
        patch(cx, j, exit);
    }
    for j in lp.continue_jumps {
        patch(cx, j, cont_pc);
    }

    cx.free_temp(); // it_temp
}

/// Emit the control-transfer instruction for a `break`/`continue` whose target
/// frame was pushed at `frame_depth`. When no handler is crossed (the live
/// `handler_depth` equals the target's) this is the fast plain `Jump`; otherwise
/// `BreakUnwind(target, frame_depth)` runs the intervening finally/close handlers
/// before landing. The target PC is `u32::MAX` here and patched by the frame's
/// break/continue patch loop (both `Jump` and `BreakUnwind` patch operand 0).
pub(crate) fn emit_cf_jump(cx: &mut Cx<'_>, frame_depth: u32) {
    if cx.handler_depth == frame_depth {
        cx.emit(Instr::Jump(u32::MAX));
    } else {
        cx.emit(Instr::BreakUnwind(u32::MAX, frame_depth));
    }
}

/// Lower a `try` statement (design §X3). The four shapes compose from two
/// primitives — a catch handler and a finally handler — so `try/catch/finally` is
/// just the catch primitive wrapped in a finally handler.
pub(crate) fn emit_try(cx: &mut Cx<'_>, t: &TryStmt) {
    match (&t.handler, &t.finalizer) {
        // `try {}` with neither catch nor finally is just its block.
        (None, None) => emit_block_scope(cx, &t.block.stmts),
        (Some(h), None) => emit_try_catch(cx, &t.block, h),
        (None, Some(fin)) => emit_try_finally(cx, &t.block, fin),
        (Some(h), Some(fin)) => emit_try_catch_finally(cx, &t.block, h, fin),
    }
}

/// The catch primitive (design §X3):
/// ```text
/// PushHandler(CATCH, absent)     ; handler_depth += 1
/// T (try block)
/// PopHandler                     ; handler_depth -= 1
/// Jump END
/// CATCH: bind e (StoreLocal; Pop); C (catch block)
/// END:
/// ```
/// On a throw in `T`, the interpreter's `unwind` pops this handler, pushes the
/// exception onto the stack, and jumps to `CATCH`. Normal completion of `T` falls
/// through `PopHandler`/`Jump END`, skipping the catch body.
pub(crate) fn emit_try_catch(cx: &mut Cx<'_>, block: &BlockStmt, handler: &CatchClause) {
    // PushHandler(CATCH, absent-finally). The catch PC is patched once known.
    let ph = cx.code.len();
    cx.emit(Instr::PushHandler(u32::MAX, u32::MAX));
    cx.handler_depth += 1;

    // The try block is its own lexical scope (D3).
    emit_block_scope(cx, &block.stmts);
    if cx.bailed() {
        return;
    }

    // Normal completion: drop the handler and jump past the catch body.
    cx.emit(Instr::PopHandler);
    cx.handler_depth -= 1;
    let end_j = cx.code.len();
    cx.emit(Instr::Jump(u32::MAX));

    // CATCH: the interpreter has already popped the handler and pushed the
    // exception value onto the stack. The catch param + body form a lexical scope
    // (D3): push a frame, bind the param (from its `DeclCollector`-allocated slot,
    // so `catch (e)` shadowing an outer `e` lands on its own slot), then run the
    // body as its own nested block scope.
    let catch_pc = cx.here();
    patch_handler_catch(cx, ph, catch_pc);
    cx.push_scope();
    if let Some(p) = &handler.param {
        let mut lows = Vec::new();
        collect_pat_binding_lows(p, &mut lows);
        bind_lows_in_scope(cx, &lows);
    }
    bind_catch_param(cx, handler);
    if cx.bailed() {
        cx.pop_scope();
        return;
    }
    emit_block_scope(cx, &handler.body.stmts);
    cx.pop_scope();

    // END: normal-completion jump lands here.
    let end = cx.here();
    patch(cx, end_j, end);
}

/// Bind (or discard) the exception value the interpreter pushed at catch entry.
/// A simple-ident binding stores it into the catch slot; an optional `catch {}`
/// discards it; a destructuring catch binding rides the destructuring task.
pub(crate) fn bind_catch_param(cx: &mut Cx<'_>, handler: &CatchClause) {
    match &handler.param {
        Some(Pat::Ident(bi)) => {
            let slot = cx.resolve(bi.id.sym.as_ref());
            cx.emit(Instr::StoreLocal(slot));
            cx.emit(Instr::Pop);
        }
        // Destructuring catch binding `catch ([a, b]) {}` / `catch ({code}) {}`:
        // the exception value is on the stack -> destructure it.
        Some(p @ (Pat::Array(_) | Pat::Object(_))) => emit_bind_target(cx, p),
        Some(_) => cx.bail_with("catch_destructure"),
        None => cx.emit(Instr::Pop),
    }
}

/// The finally primitive (design §X3):
/// ```text
/// PushHandler(absent, FIN)       ; handler_depth += 1
/// T (try block)
/// PopHandler                     ; handler_depth -= 1
/// FIN: F (finally block); EndFinally
/// AFTER:
/// ```
/// Normal completion of `T` falls through `PopHandler` into `FIN` with `comp`
/// normal, so `EndFinally` continues to `AFTER`. An abrupt completion (throw /
/// return / break / continue) is routed to `FIN` by `unwind` with `comp` set, and
/// `EndFinally` re-performs that pending completion after the finally body runs.
/// The finally body is emitted at the OUTER handler depth (this try's handler is
/// already gone by the time `F` runs), so a `return`/`break` inside `F` routes
/// through any still-active enclosing handlers.
pub(crate) fn emit_try_finally(cx: &mut Cx<'_>, block: &BlockStmt, fin: &BlockStmt) {
    let ph = cx.code.len();
    cx.emit(Instr::PushHandler(u32::MAX, u32::MAX)); // no catch; finally PC patched
    cx.handler_depth += 1;

    // The try block is its own lexical scope (D3).
    emit_block_scope(cx, &block.stmts);
    if cx.bailed() {
        return;
    }

    cx.emit(Instr::PopHandler);
    cx.handler_depth -= 1;

    // FIN: reached by normal fall-through (comp normal) or by `unwind` (comp set).
    let fin_pc = cx.here();
    patch_handler_fin(cx, ph, fin_pc);
    emit_block_scope(cx, &fin.stmts);
    if cx.bailed() {
        return;
    }
    cx.emit(Instr::EndFinally);
}

/// `try/catch/finally` = the catch primitive wrapped in a finally handler
/// (design §X3). The outer finally handler stays active across the catch arm, so
/// a throw/return inside `catch` still runs the finally.
pub(crate) fn emit_try_catch_finally(
    cx: &mut Cx<'_>,
    block: &BlockStmt,
    handler: &CatchClause,
    fin: &BlockStmt,
) {
    let ph = cx.code.len();
    cx.emit(Instr::PushHandler(u32::MAX, u32::MAX)); // outer finally handler
    cx.handler_depth += 1;

    emit_try_catch(cx, block, handler);
    if cx.bailed() {
        return;
    }

    cx.emit(Instr::PopHandler);
    cx.handler_depth -= 1;

    let fin_pc = cx.here();
    patch_handler_fin(cx, ph, fin_pc);
    emit_block_scope(cx, &fin.stmts);
    if cx.bailed() {
        return;
    }
    cx.emit(Instr::EndFinally);
}

/// Emit an expression statement, special-casing UpdateExpr (i++, --i, ...).
pub(crate) fn emit_expr_stmt(cx: &mut Cx<'_>, expr: &Expr) {
    if let Expr::Update(u) = expr {
        // target must be a slot ident.
        if let Expr::Ident(id) = &*u.arg {
            let name = id.sym.as_ref();
            // D1: a BOXED capture is writable via its cell; a non-local, non-boxed
            // capture is read-only -> bail (the write would be lost).
            let boxed = cx.is_celled(name);
            if !cx.is_param_or_local(name) && !boxed {
                // ++/-- on a captured outer binding cannot be written back to
                // the enclosing scope. Skip to stay sound.
                cx.bail_with("mutable_capture");
                return;
            }
            let slot = cx.resolve(name);
            cx.emit(if boxed { Instr::LoadCell(slot) } else { Instr::LoadLocal(slot) });
            let ci = cx.const_num(1.0);
            cx.emit(Instr::PushConst(ci));
            let code = match u.op {
                UpdateOp::PlusPlus => 0,
                UpdateOp::MinusMinus => 1,
            };
            cx.emit(Instr::Bin(code));
            cx.emit(if boxed { Instr::StoreCell(slot) } else { Instr::StoreLocal(slot) });
            cx.emit(Instr::Pop);
        } else {
            cx.bail();
        }
        return;
    }
    emit_expr(cx, expr);
    cx.emit(Instr::Pop);
}

// ---------------------------------------------------------------------------
// D5 — nested closures: compile a nested function/arrow to its own chunk and
// emit a `MakeClosure` that builds the JS closure, threading upvalue cells.
// ---------------------------------------------------------------------------

/// D5: compute the set of THIS body's own function-frame locals that must be boxed
/// into in-VM cells because a nested closure captures-and-mutates them. A name is
/// boxed iff it is (a) a function-frame binding of this body (param / `var` /
/// body-top-level `let`/`const` / fn-decl name), (b) referenced free by some nested
/// function/arrow (a capture), and (c) written somewhere (this body or a nested fn).
/// Read-only captures stay by-value (no cell, no regression). A captured-mutated name
/// that is NOT a function-frame binding here (e.g. a deeper block `let`) is not boxed
/// — the child's write then bails `mutable_capture`, bailing the parent (sound, just
/// less coverage).
pub(crate) fn compute_boxed_locals(params: &[Param], body: &BlockStmt) -> std::collections::HashSet<String> {
    // (a) function-frame local names.
    let mut locals: std::collections::HashSet<String> = std::collections::HashSet::new();
    for p in params {
        binding_names(&p.pat, &mut |id| {
            locals.insert(id.sym.to_string());
        });
    }
    // `var` (function-scoped, any depth but not nested fns) + fn-decl names.
    {
        struct L<'a>(&'a mut std::collections::HashSet<String>);
        impl Visit for L<'_> {
            fn visit_var_decl(&mut self, v: &VarDecl) {
                if matches!(v.kind, VarDeclKind::Var) {
                    for d in &v.decls {
                        binding_names(&d.name, &mut |id| {
                            self.0.insert(id.sym.to_string());
                        });
                    }
                }
                v.visit_children_with(self);
            }
            fn visit_fn_decl(&mut self, n: &FnDecl) {
                self.0.insert(n.ident.sym.to_string());
            }
            fn visit_function(&mut self, _: &Function) {}
            fn visit_arrow_expr(&mut self, _: &ArrowExpr) {}
        }
        let mut l = L(&mut locals);
        body.visit_with(&mut l);
    }
    // body-top-level `let`/`const`.
    for stmt in &body.stmts {
        if let Stmt::Decl(Decl::Var(v)) = stmt
            && matches!(v.kind, VarDeclKind::Let | VarDeclKind::Const)
        {
            for d in &v.decls {
                binding_names(&d.name, &mut |id| {
                    locals.insert(id.sym.to_string());
                });
            }
        }
    }

    // (b) names captured (referenced free) by a nested function/arrow.
    let mut nested_caps: std::collections::HashSet<String> = std::collections::HashSet::new();
    {
        struct N<'a>(&'a mut std::collections::HashSet<String>);
        impl N<'_> {
            fn note_fn(&mut self, locals: std::collections::HashSet<String>, body: &BlockStmt) {
                let mut d = BodyLocalDecls { names: locals };
                body.visit_with(&mut d);
                let mut s = NestedFreeScan {
                    local: d.names,
                    refs: self.0,
                };
                body.visit_with(&mut s);
            }
        }
        impl Visit for N<'_> {
            fn visit_fn_decl(&mut self, n: &FnDecl) {
                let Some(b) = &n.function.body else { return };
                let mut l = std::collections::HashSet::new();
                l.insert(n.ident.sym.to_string());
                for p in &n.function.params {
                    binding_names(&p.pat, &mut |id| {
                        l.insert(id.sym.to_string());
                    });
                }
                self.note_fn(l, b);
            }
            fn visit_fn_expr(&mut self, n: &FnExpr) {
                let Some(b) = &n.function.body else { return };
                let mut l = std::collections::HashSet::new();
                if let Some(id) = &n.ident {
                    l.insert(id.sym.to_string());
                }
                for p in &n.function.params {
                    binding_names(&p.pat, &mut |id| {
                        l.insert(id.sym.to_string());
                    });
                }
                self.note_fn(l, b);
            }
            fn visit_arrow_expr(&mut self, n: &ArrowExpr) {
                let mut l = std::collections::HashSet::new();
                for p in &n.params {
                    binding_names(p, &mut |id| {
                        l.insert(id.sym.to_string());
                    });
                }
                match &*n.body {
                    BlockStmtOrExpr::BlockStmt(b) => self.note_fn(l, b),
                    BlockStmtOrExpr::Expr(e) => {
                        let mut s = NestedFreeScan { local: l, refs: self.0 };
                        e.visit_with(&mut s);
                    }
                }
            }
        }
        let mut n = N(&mut nested_caps);
        body.visit_with(&mut n);
    }

    // (c) names written anywhere in this body OR a nested fn (reuse WriteScan-style).
    let written = collect_all_writes(body);

    locals
        .into_iter()
        .filter(|n| nested_caps.contains(n) && written.contains(n))
        .collect()
}

/// Collects every name a function body binds locally (params seeded by the caller):
/// `var`, fn-decl, `let`/`const`, `catch` params, destructuring leaves — at any
/// nesting WITHIN this function (NOT descending into nested functions, whose bindings
/// are their own scope). Used to exclude locals from a free-ref scan.
struct BodyLocalDecls {
    names: std::collections::HashSet<String>,
}
impl Visit for BodyLocalDecls {
    fn visit_var_decl(&mut self, v: &VarDecl) {
        for d in &v.decls {
            binding_names(&d.name, &mut |id| {
                self.names.insert(id.sym.to_string());
            });
        }
        v.visit_children_with(self);
    }
    fn visit_fn_decl(&mut self, n: &FnDecl) {
        self.names.insert(n.ident.sym.to_string());
    }
    fn visit_catch_clause(&mut self, c: &CatchClause) {
        if let Some(p) = &c.param {
            binding_names(p, &mut |id| {
                self.names.insert(id.sym.to_string());
            });
        }
        c.body.visit_with(self);
    }
    fn visit_function(&mut self, _: &Function) {}
    fn visit_arrow_expr(&mut self, _: &ArrowExpr) {}
}

/// Free-name collector for a nested function body, excluding its own locals. A name
/// not bound locally and not `arguments`/`undefined` is a capture. Descends into
/// deeper nested fns (their free names are this body's captures too, transitively) —
/// the conservative over-approximation only ever boxes MORE candidates, and a name
/// boxed but not actually captured is harmless (its cell is simply never threaded).
struct NestedFreeScan<'a> {
    local: std::collections::HashSet<String>,
    refs: &'a mut std::collections::HashSet<String>,
}
impl Visit for NestedFreeScan<'_> {
    fn visit_ident(&mut self, id: &Ident) {
        let n = id.sym.as_ref();
        if !self.local.contains(n) && n != "arguments" && n != "undefined" {
            self.refs.insert(n.to_string());
        }
    }
    fn visit_member_expr(&mut self, m: &MemberExpr) {
        m.obj.visit_with(self);
        if let MemberProp::Computed(c) = &m.prop {
            c.visit_with(self);
        }
    }
    fn visit_prop_name(&mut self, p: &PropName) {
        if let PropName::Computed(c) = p {
            c.visit_with(self);
        }
    }
}

/// Collect every name WRITTEN (assign / `++`/`--` / compound / for-head /
/// destructuring target) anywhere in `body`, INCLUDING nested functions (a boxed
/// local may be written by an inner closure). Reuses the same target shapes as the
/// D1 `cells::WriteScan`, kept local to the compiler module.
pub(crate) fn collect_all_writes(body: &BlockStmt) -> std::collections::HashSet<String> {
    struct W(std::collections::HashSet<String>);
    impl W {
        fn pat(&mut self, pat: &Pat) {
            match pat {
                Pat::Ident(bi) => {
                    self.0.insert(bi.id.sym.to_string());
                }
                Pat::Expr(e) => {
                    if let Expr::Ident(id) = &**e {
                        self.0.insert(id.sym.to_string());
                    }
                }
                Pat::Assign(ap) => self.pat(&ap.left),
                Pat::Array(arr) => {
                    for el in arr.elems.iter().flatten() {
                        self.pat(el);
                    }
                }
                Pat::Object(obj) => {
                    for prop in &obj.props {
                        match prop {
                            ObjectPatProp::KeyValue(kv) => self.pat(&kv.value),
                            ObjectPatProp::Assign(a) => {
                                self.0.insert(a.key.id.sym.to_string());
                            }
                            ObjectPatProp::Rest(r) => self.pat(&r.arg),
                        }
                    }
                }
                Pat::Rest(r) => self.pat(&r.arg),
                Pat::Invalid(_) => {}
            }
        }
    }
    impl Visit for W {
        fn visit_assign_expr(&mut self, n: &AssignExpr) {
            match &n.left {
                AssignTarget::Simple(SimpleAssignTarget::Ident(bi)) => {
                    self.0.insert(bi.id.sym.to_string());
                }
                AssignTarget::Pat(AssignTargetPat::Array(arr)) => {
                    for el in arr.elems.iter().flatten() {
                        self.pat(el);
                    }
                }
                AssignTarget::Pat(AssignTargetPat::Object(obj)) => {
                    for prop in &obj.props {
                        match prop {
                            ObjectPatProp::KeyValue(kv) => self.pat(&kv.value),
                            ObjectPatProp::Assign(a) => {
                                self.0.insert(a.key.id.sym.to_string());
                            }
                            ObjectPatProp::Rest(r) => self.pat(&r.arg),
                        }
                    }
                }
                _ => {}
            }
            n.visit_children_with(self);
        }
        fn visit_update_expr(&mut self, n: &UpdateExpr) {
            if let Expr::Ident(id) = &*n.arg {
                self.0.insert(id.sym.to_string());
            }
            n.visit_children_with(self);
        }
        fn visit_for_in_stmt(&mut self, n: &ForInStmt) {
            if let ForHead::Pat(p) = &n.left {
                self.pat(p);
            }
            n.visit_children_with(self);
        }
        fn visit_for_of_stmt(&mut self, n: &ForOfStmt) {
            if let ForHead::Pat(p) = &n.left {
                self.pat(p);
            }
            n.visit_children_with(self);
        }
    }
    let mut w = W(std::collections::HashSet::new());
    body.visit_with(&mut w);
    w.0
}

/// The free names a nested function/arrow body references (its capture candidates),
/// excluding its own params/locals and `self_name`. Used by `emit_nested_closure` to
/// decide which of the child's captures the parent holds as cells (so the child reads
/// them through cells). A conservative over-approximation is harmless: a name the
/// child does not actually capture is never installed, and an unboxed mutated capture
/// bails the child (then the parent), never miscompiling.
pub(crate) fn nested_free_names(
    params: &[Pat],
    body: &BlockStmt,
    self_name: Option<&str>,
) -> std::collections::HashSet<String> {
    let mut local = std::collections::HashSet::new();
    for p in params {
        binding_names(p, &mut |id| {
            local.insert(id.sym.to_string());
        });
    }
    if let Some(n) = self_name {
        local.insert(n.to_string());
    }
    // Exclude the body's own declared locals.
    let mut d = BodyLocalDecls { names: local };
    body.visit_with(&mut d);
    let mut refs = std::collections::HashSet::new();
    let mut s = NestedFreeScan {
        local: d.names,
        refs: &mut refs,
    };
    body.visit_with(&mut s);
    refs
}

/// Compile a nested `function`/`arrow` to its own VM chunk, register it as a child
/// of the current chunk, and emit a `MakeClosure` that pushes the built closure.
/// Returns without emitting (after a `bail_with`) if the nested function cannot be
/// virtualized for any reason — the parent then bails too, staying un-virtualized
/// (sound: never a miscompile, just less coverage). `self_name` is set for a named
/// function expression so its own-name reference threads the closure itself.
pub(crate) fn emit_nested_closure(
    cx: &mut Cx<'_>,
    params: &[Pat],
    body: &BlockStmt,
    is_arrow: bool,
    is_async: bool,
    is_generator: bool,
    self_name: Option<&str>,
) {
    if is_async || is_generator {
        cx.bail_with("nested_async_generator");
        return;
    }
    // An arrow has no own `this`/`arguments`; we thread the enclosing `this`
    // lexically (via the closure's captured `receiver`), but cannot model a lexical
    // `arguments` — bail an arrow that references it. (A regular nested function has
    // its own `arguments`, handled by the chunk's D2 materialization, so only the
    // arrow case is restricted here.)
    if is_arrow && uses_arguments(body) {
        cx.bail_with("arrow_uses_arguments");
        return;
    }
    if has_use_strict_directive_block(body) {
        cx.bail_with("nested_use_strict");
        return;
    }
    // Wrap the arrow params (`Pat`) as `Param`s for the shared compiler, which keys
    // on `params: &[Param]`. (A regular function already has `Param`s; the caller
    // passes its `pat`s here so both paths share one entry.)
    let wrapped: Vec<Param> = params
        .iter()
        .map(|p| Param {
            span: swc_core::common::DUMMY_SP,
            decorators: vec![],
            pat: p.clone(),
        })
        .collect();

    // Structural eligibility of the nested body (with / direct eval / await / yield
    // / arguments-aliasing). Nested functions are now eligible, so this no longer
    // rejects them; a structurally-ineligible nested body bails the parent.
    if let crate::eligibility::Eligibility::Skip(r) =
        crate::eligibility::classify_body(&wrapped, body)
    {
        cx.bail_with(r);
        return;
    }

    // Boxed-capture set for the child. Two sources, unioned:
    //   (1) D5 IN-VM boxing: any of the child's free names that THIS (parent) frame
    //       holds as a cell — a boxed local of the parent or a boxed upvalue the
    //       parent itself received — must be read/written through a cell in the child
    //       too (LoadCell/StoreCell on its upvalue slot), since the slot value the
    //       parent threads is the shared cell array.
    //   (2) D1 plan boxing: the `cells.rs` enclosing-JS-scope rewrite (when the
    //       enclosing scope is plain JS, not a VM frame) records the same per-inner-fn
    //       boxed sets keyed by body position.
    // A free name the child captures and writes that is NOT celled in the parent is
    // left unboxed — the child then bails `mutable_capture` (a write to a read-only
    // capture), bailing the parent (sound).
    let child_free = nested_free_names(params, body, self_name);
    let mut child_boxed: std::collections::HashSet<String> = child_free
        .iter()
        .filter(|n| cx.is_boxed_local(n) || cx.boxed_caps.contains(n.as_str()))
        .cloned()
        .collect();
    if let Some(p) = cx.box_plan {
        child_boxed.extend(p.boxed_for(body.span.lo));
    }

    // Recursively compile the child chunk WITH the same plan so grandchildren box
    // correctly too. A bail propagates to the parent.
    let compiled = match compile_body_inner(&wrapped, body, &child_boxed, cx.box_plan) {
        Ok(c) => c,
        Err(r) => {
            cx.bail_with(r);
            return;
        }
    };

    // The child's capture order (from its own compile) is authoritative; recompute
    // the same first-encounter free-name order to map each capture to a parent slot.
    // `compile_body` records captures in `compiled.captures`; resolve each name in
    // THIS frame to get the upvalue slot. A name that is itself free in the parent
    // becomes the parent's own capture (threaded up recursively).
    let cap_start = compiled.slots - compiled.captures.len() as u32;
    let pcount = compiled.pcount;
    let mut up_slots: Vec<u32> = Vec::with_capacity(compiled.captures.len());
    for cap in &compiled.captures {
        if Some(cap.as_str()) == self_name {
            up_slots.push(SELF_UPVALUE);
        } else {
            // Resolve the capture name in the enclosing frame. If it is a parent
            // local/param it yields that slot; if free in the parent too, the parent
            // captures it (a new slot >= cap_floor), threaded up the chain.
            let slot = cx.resolve(cap);
            up_slots.push(slot);
        }
    }

    let child_index = cx.children.len() as u32;
    cx.children.push(ChildChunk { compiled, is_arrow });
    cx.emit(Instr::MakeClosure {
        child: child_index,
        is_arrow,
        cap_start,
        pcount,
        up_slots,
    });
}

/// True if a block body begins with its own `"use strict"` directive. Mirrors
/// `crate::cells::has_use_strict_directive` (private) but is reachable from the compiler (which only
/// imports `bytecode`'s module). Kept tiny and local.
pub(crate) fn has_use_strict_directive_block(body: &BlockStmt) -> bool {
    for s in &body.stmts {
        match s {
            Stmt::Expr(es) => match &*es.expr {
                Expr::Lit(Lit::Str(lit)) => {
                    if lit.value.as_str() == Some("use strict") {
                        return true;
                    }
                }
                _ => return false,
            },
            _ => return false,
        }
    }
    false
}

