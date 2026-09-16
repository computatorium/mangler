//! Statement-family compiler: block scopes, decls, control flow, loops, switch,
//! try/catch/finally, the arguments object, and nested-closure emission.
//!
//! Every `emit_*` here takes `&mut Cx` and shares the frame model and the other
//! construct-family emitters (`expr`, `destructure`) via `use super::*`.

use swc_core::ecma::visit::{Visit, VisitWith};

use super::*;
use crate::chunk::ChildChunk;
use crate::isa::{Instr, SELF_UPVALUE};
use mangler_jsast::analysis::binding_names;

pub(crate) fn emit_block_scope(cx: &mut Cx<'_>, stmts: &[Stmt]) {
    cx.push_scope();
    let bindings = direct_block_bindings(stmts);
    bind_declarations_in_scope(cx, &bindings);
    emit_block_function_declarations(cx, stmts);
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
        Stmt::Empty(_) | Stmt::Debugger(_) => {}
        Stmt::Decl(Decl::Var(v)) => {
            let was_initializing = cx.initializing;
            cx.initializing = false;
            for d in &v.decls {
                match &d.name {
                    Pat::Ident(bi) => {
                        if d.init.is_some() || v.kind != VarDeclKind::Var {
                            let name = bi.id.sym.as_ref();
                            // D5: a boxed local's cell was already seeded at the
                            // prologue, so its declaration writes THROUGH the cell
                            // (`StoreCell`) to keep the shared array that closures
                            // capture; a plain local uses `StoreLocal`.
                            let celled = cx.is_boxed_local(name);
                            let slot = cx.resolve(name);
                            // §4.3: infer the binding name for an anon fn/arrow init
                            // (`const render = () => …`) so the native-closure divert
                            // can match the exclude glob.
                            cx.pending_fn_name = Some(name.to_string());
                            let dynamic = v.kind == VarDeclKind::Var && binding_has_with(cx, name);
                            if dynamic {
                                emit_binding_ref(cx, name);
                                cx.emit(Instr::ResolveRef);
                            }
                            if let Some(init) = &d.init {
                                emit_expr(cx, init);
                            } else {
                                cx.emit(Instr::PushUndef);
                            }
                            cx.pending_fn_name = None;
                            if dynamic {
                                cx.emit(Instr::PutRef);
                                cx.emit(Instr::Pop);
                                continue;
                            }
                            cx.emit(if v.kind != VarDeclKind::Var {
                                Instr::InitLocal(slot)
                            } else if celled {
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
                                cx.initializing = v.kind != VarDeclKind::Var;
                                emit_destructure_object(cx, obj, t);
                                cx.initializing = false;
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
                                cx.initializing = v.kind != VarDeclKind::Var;
                                emit_destructure_array(cx, arr);
                                cx.initializing = false;
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
            cx.initializing = was_initializing;
        }
        Stmt::Expr(es) => {
            emit_expr_stmt(cx, &es.expr);
        }
        Stmt::Decl(Decl::Fn(declaration)) => {
            let key = DeclarationKey::of(&declaration.ident);
            if let Some(&slot) = cx.decl_slots.get(&key) {
                // Annex B permits a declaration as an if/label body. Give that
                // statement the same lexical initialization as an explicit block.
                if cx.lookup(declaration.ident.sym.as_ref()) != Some(slot) {
                    emit_block_scope(cx, std::slice::from_ref(stmt));
                    return;
                }
                if let Some(&alias) = cx.block_fn_aliases.get(&key) {
                    cx.emit(Instr::LoadLocal(slot));
                    // The lexical declaration and its Annex B variable alias
                    // share a spelling, but may have different storage kinds.
                    let boxed = if alias < cx.cap_floor {
                        cx.boxed_locals.contains(declaration.ident.sym.as_ref())
                            && !cx.lexical_slots.contains_key(&alias)
                    } else {
                        cx.boxed_caps.contains(declaration.ident.sym.as_ref())
                    };
                    cx.emit(if boxed {
                        Instr::StoreCell(alias)
                    } else {
                        Instr::StoreLocal(alias)
                    });
                    cx.emit(Instr::Pop);
                }
            }
        }
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
                labels: std::mem::take(&mut cx.pending_labels),
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
            let head_bindings = match &f.init {
                Some(VarDeclOrExpr::VarDecl(v)) => for_var_decl_block_bindings(v),
                _ => Vec::new(),
            };
            cx.push_scope();
            bind_declarations_in_scope(cx, &head_bindings);
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
            for (_, declaration) in &head_bindings {
                if let Some(&slot) = cx.decl_slots.get(declaration) {
                    cx.emit(Instr::CloneLexical(slot));
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
                labels: std::mem::take(&mut cx.pending_labels),
                handler_depth: cx.handler_depth,
                continue_handler_depth: cx.handler_depth,
                break_jumps: Vec::new(),
                continue_jumps: Vec::new(),
            });
            emit_stmt(cx, &f.body);
            let update_pc = cx.here();
            for (_, declaration) in &head_bindings {
                if let Some(&slot) = cx.decl_slots.get(declaration) {
                    cx.emit(Instr::CloneLexical(slot));
                }
            }
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
                labels: std::mem::take(&mut cx.pending_labels),
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
        Stmt::Switch(s) => emit_switch(cx, s),
        Stmt::ForIn(s) => {
            // A `for (let k in …)` head opens a per-loop lexical scope (D3). Push it
            // around the whole lowering so the binding resolves to its own slot even
            // when it shadows an outer name; `var`/pattern heads add nothing.
            let bindings = for_head_block_bindings(&s.left);
            cx.push_scope();
            bind_declarations_in_scope(cx, &bindings);
            emit_for_in(cx, s);
            cx.pop_scope();
        }
        Stmt::ForOf(s) => {
            let bindings = for_head_block_bindings(&s.left);
            cx.push_scope();
            bind_declarations_in_scope(cx, &bindings);
            emit_for_of(cx, s);
            cx.pop_scope();
        }
        Stmt::Try(t) => emit_try(cx, t),
        Stmt::With(statement) => {
            emit_expr(cx, &statement.obj);
            let object = cx.alloc_temp();
            cx.emit(Instr::EnterWith(object));
            cx.with_scopes.push((cx.scopes.len(), object));
            emit_stmt(cx, &statement.body);
            cx.with_scopes.pop();
            cx.free_temp();
        }
        Stmt::Labeled(l) => {
            let mut names = vec![l.label.sym.to_string()];
            let mut body = &*l.body;
            while let Stmt::Labeled(inner) = body {
                names.push(inner.label.sym.to_string());
                body = &inner.body;
            }
            match body {
                // A labeled loop: stash the label so the loop arm consumes it
                // into its own `Frame.labels` (so `break/continue label` resolve
                // to that loop). Do NOT push a Block frame here.
                Stmt::While(_)
                | Stmt::For(_)
                | Stmt::DoWhile(_)
                | Stmt::ForIn(_)
                | Stmt::ForOf(_) => {
                    cx.pending_labels = names;
                    emit_stmt(cx, body);
                }
                // A labeled non-loop (e.g. a block): push a `Block` frame so that
                // `break label` can target the construct's end. `continue label`
                // to a non-loop is a JS syntax error and never parses, so a Block
                // frame only ever receives break jumps.
                _ => {
                    cx.frames.push(Frame {
                        kind: FrameKind::Block,
                        labels: names,
                        handler_depth: cx.handler_depth,
                        continue_handler_depth: cx.handler_depth,
                        break_jumps: Vec::new(),
                        continue_jumps: Vec::new(),
                    });
                    emit_stmt(cx, body);
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

    // The discriminant is evaluated before entering the switch lexical scope.
    let mut bindings = Vec::new();
    for case in &s.cases {
        bindings.extend(direct_block_bindings(&case.cons));
    }
    cx.push_scope();
    bind_declarations_in_scope(cx, &bindings);
    for case in &s.cases {
        emit_block_function_declarations(cx, &case.cons);
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
        labels: std::mem::take(&mut cx.pending_labels),
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
    cx.pop_scope();
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
                Pat::Ident(bi) => {
                    let name = bi.id.sym.as_ref();
                    if v.kind == VarDeclKind::Var && binding_has_with(cx, name) {
                        return Some(ForHeadTarget::Pat(&v.decls[0].name));
                    }
                    Some(ForHeadTarget::Slot(cx.resolve(name), cx.is_celled(name)))
                }
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
                if binding_needs_ref(cx, name) {
                    return Some(ForHeadTarget::Pat(p));
                }
                // D1: `for (x of …)` where `x` is a boxed capture stores per
                // iteration through the cell; an unboxed capture is read-only -> bail.
                let boxed = cx.is_celled(name);
                if !cx.is_param_or_local(name) && !boxed && !cx.opts.live_captures {
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
            cx.emit(if cx.initializing && cx.lexical_slots.contains_key(slot) {
                Instr::InitLocal(*slot)
            } else if *boxed {
                Instr::StoreCell(*slot)
            } else {
                Instr::StoreLocal(*slot)
            });
            cx.emit(Instr::Pop);
        }
        ForHeadTarget::Pat(p) => emit_bind_target(cx, p),
    }
}

/// Suspend native enumeration between iterations, preserving deletion and
/// prototype behavior while the VM executes each loop body. Uses one iterator slot.
pub(crate) fn emit_for_in(cx: &mut Cx<'_>, s: &ForInStmt) {
    // Annex B permits an initializer on a sloppy `var` for-in head. It runs
    // once, before evaluating the enumerated expression, even for an empty object.
    if let ForHead::VarDecl(declaration) = &s.left
        && declaration
            .decls
            .iter()
            .any(|binding| binding.init.is_some())
    {
        emit_stmt(cx, &Stmt::Decl(Decl::Var(declaration.clone())));
    }
    let Some(target) = for_head_target(cx, &s.left) else {
        return;
    };
    emit_expr(cx, &s.right);
    cx.emit(Instr::EnumKeys);
    let iterator = cx.alloc_temp();
    cx.emit(Instr::StoreLocal(iterator));
    cx.emit(Instr::Pop);
    let loop_pc = cx.here();
    cx.emit(Instr::LoadLocal(iterator));
    cx.emit(Instr::IterStep);
    let exit = cx.code.len();
    cx.emit(Instr::JumpIfFalse(u32::MAX));
    initialize_for_head(cx, &s.left);
    emit_for_head_bind(cx, &target);
    cx.initializing = false;
    cx.frames.push(Frame {
        kind: FrameKind::Loop,
        labels: std::mem::take(&mut cx.pending_labels),
        handler_depth: cx.handler_depth,
        continue_handler_depth: cx.handler_depth,
        break_jumps: Vec::new(),
        continue_jumps: Vec::new(),
    });
    emit_stmt(cx, &s.body);
    cx.emit(Instr::Jump(loop_pc));
    let end = cx.here();
    patch(cx, exit, end);
    let frame = cx.frames.pop().unwrap();
    for j in frame.break_jumps {
        patch(cx, j, end);
    }
    for j in frame.continue_jumps {
        patch(cx, j, loop_pc);
    }
    cx.free_temp();
}

fn initialize_for_head(cx: &mut Cx<'_>, head: &ForHead) {
    let bindings = for_head_block_bindings(head);
    cx.initializing = !bindings.is_empty();
    bind_declarations_in_scope(cx, &bindings);
}

/// Lower a `for (x of ITER)` via the iterator opcodes + a close-on-abrupt handler
/// (design "Iterator mechanism" + §X). The iterator is obtained once; each step
/// reads the next value (the `IterStep` flag drives the existing `JumpIfFalse`);
/// `break`/`return`/`throw` route through the close handler (`IterClose`) via the
/// unwind machinery, while `continue` and normal exhaustion do NOT close.
///
/// ```text
/// eval ITER; GetIter; ->it
/// LOOP: LoadLocal(it); IterStep; JumpIfFalse EXIT
///   ->value; PushHandler(absent, CLOSE); value ->x; BODY
/// CONT: PopHandler; Jump LOOP
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

    // IteratorStep runs outside the close handler: a throwing next()/done/value
    // accessor terminates iteration without IteratorClose. Binding and body
    // evaluation run inside it and do close on abrupt completion.
    let outer_depth = cx.handler_depth;
    cx.frames.push(Frame {
        kind: FrameKind::Loop,
        labels: std::mem::take(&mut cx.pending_labels),
        handler_depth: outer_depth,
        continue_handler_depth: outer_depth + 1,
        break_jumps: Vec::new(),
        continue_jumps: Vec::new(),
    });
    let loop_pc = cx.here();
    cx.emit(Instr::LoadLocal(it_temp));
    cx.emit(Instr::IterStep);
    let normal_exit_j = cx.code.len();
    cx.emit(Instr::JumpIfFalse(u32::MAX));
    // Establish the handler at the surrounding expression stack depth, without
    // retaining an iteration value when a binding or body completes abruptly.
    let value = cx.alloc_temp();
    cx.emit(Instr::StoreLocal(value));
    cx.emit(Instr::Pop);
    let ph = cx.code.len();
    cx.emit(Instr::PushHandler(u32::MAX, u32::MAX));
    cx.handler_depth += 1;
    cx.emit(Instr::LoadLocal(value));
    cx.free_temp();
    // Bind the value: a plain store, or a per-iteration destructure (whose own
    // close handler nests inside this loop's, then balances before the body).
    initialize_for_head(cx, &s.left);
    emit_for_head_bind(cx, &target);
    cx.initializing = false;
    if cx.bailed() {
        cx.frames.pop();
        return;
    }

    // BODY.
    emit_stmt(cx, &s.body);

    // Continuing normally removes this iteration's handler before stepping.
    let cont_pc = cx.here();
    cx.emit(Instr::PopHandler);
    cx.handler_depth -= 1;
    cx.emit(Instr::Jump(loop_pc));

    // CLOSE: the finally body that closes the iterator on abrupt completion.
    let close_pc = cx.here();
    patch_handler_fin(cx, ph, close_pc);
    cx.emit(Instr::BeginFinally);
    cx.emit(Instr::LoadLocal(it_temp));
    cx.emit(Instr::IterClose);
    cx.emit(Instr::EndFinally);

    // EXIT: patch normal-exit jump + frame break/continue jumps.
    let exit = cx.here();
    patch(cx, normal_exit_j, exit);
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
        let mut bindings = Vec::new();
        collect_pattern_bindings(p, &mut bindings);
        bind_declarations_in_scope(cx, &bindings);
    }
    cx.initializing = true;
    bind_catch_param(cx, handler);
    cx.initializing = false;
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
            cx.emit(Instr::InitLocal(slot));
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
/// The original handler is gone when `F` runs. A completion-scope handler inside
/// `emit_finally_body` discards the saved completion on an abrupt exit before
/// routing through still-active enclosing handlers.
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
    emit_finally_body(cx, fin);
}

/// A source finally body owns one pending completion. Its synthetic handler
/// snapshots the pending stack before BeginFinally saves that completion. Any
/// abrupt exit crosses this handler and discards that saved completion through
/// the existing unwind mechanism; a local break/continue keeps it. Normal exit
/// pops the guard without unwinding and EndFinally resumes the saved completion.
fn emit_finally_body(cx: &mut Cx<'_>, fin: &BlockStmt) {
    cx.emit(Instr::PushHandler(u32::MAX, u32::MAX));
    cx.handler_depth += 1;
    cx.emit(Instr::BeginFinally);
    emit_block_scope(cx, &fin.stmts);
    cx.handler_depth -= 1;
    if cx.bailed() {
        return;
    }
    cx.emit(Instr::PopHandler);
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
    emit_finally_body(cx, fin);
}

/// Expression statements use the same lowering as value-producing expressions,
/// then discard the result. This keeps member updates and coercion order identical.
pub(crate) fn emit_expr_stmt(cx: &mut Cx<'_>, expr: &Expr) {
    emit_expr(cx, expr);
    cx.emit(Instr::Pop);
}

// ---------------------------------------------------------------------------
// D5 — nested closures: compile a nested function/arrow to its own chunk and
// emit a `MakeClosure` that builds the JS closure, threading upvalue cells.
// ---------------------------------------------------------------------------

/// Captured bindings are shared from function entry, including bindings whose
/// initializer runs after a closure is created. Declaration initialization is a
/// write too: capturing `var x` by value would freeze its initial `undefined`.
/// This scan intentionally over-approximates shadowed names; the compiler's slot
/// resolver determines actual captures, and an unused cell is harmless.
pub(crate) fn compute_boxed_locals(
    params: &[Param],
    body: &FunctionBody,
) -> std::collections::HashSet<String> {
    let mut locals = std::collections::HashSet::new();
    for param in params {
        binding_names(&param.pat, &mut |id| {
            locals.insert(id.sym.to_string());
        });
    }
    struct Locals<'a>(&'a mut std::collections::HashSet<String>);
    impl Visit for Locals<'_> {
        fn visit_bin_expr(&mut self, node: &BinExpr) {
            walk_binary_chain(node, self);
        }
        fn visit_var_decl(&mut self, declaration: &VarDecl) {
            for binding in &declaration.decls {
                binding_names(&binding.name, &mut |id| {
                    self.0.insert(id.sym.to_string());
                });
            }
            declaration.visit_children_with(self);
        }
        fn visit_fn_decl(&mut self, function: &FnDecl) {
            self.0.insert(function.ident.sym.to_string());
        }
        fn visit_function(&mut self, _: &Function) {}
        fn visit_arrow_expr(&mut self, _: &ArrowExpr) {}
    }
    body.visit_with(&mut Locals(&mut locals));
    let mut references = std::collections::HashSet::new();
    struct Closures<'a>(&'a mut std::collections::HashSet<String>);
    impl Visit for Closures<'_> {
        fn visit_bin_expr(&mut self, node: &BinExpr) {
            walk_binary_chain(node, self);
        }
        fn visit_function(&mut self, function: &Function) {
            function.visit_with(&mut NestedReferenceScan { refs: self.0 });
        }
        fn visit_arrow_expr(&mut self, arrow: &ArrowExpr) {
            arrow.visit_with(&mut NestedReferenceScan { refs: self.0 });
        }
    }
    body.visit_with(&mut Closures(&mut references));
    locals.retain(|name| references.contains(name));
    locals
}

/// Conservative reference scan. Block-local declarations cannot exclude a name
/// from the whole function: references outside that block may still capture it.
struct NestedReferenceScan<'a> {
    refs: &'a mut std::collections::HashSet<String>,
}
impl Visit for NestedReferenceScan<'_> {
    fn visit_bin_expr(&mut self, node: &BinExpr) {
        walk_binary_chain(node, self);
    }
    fn visit_ident(&mut self, id: &Ident) {
        self.refs.insert(id.sym.to_string());
    }
    fn visit_member_expr(&mut self, member: &MemberExpr) {
        member.obj.visit_with(self);
        if let MemberProp::Computed(key) = &member.prop {
            key.visit_with(self);
        }
    }
    fn visit_prop_name(&mut self, name: &PropName) {
        if let PropName::Computed(key) = name {
            key.visit_with(self);
        }
    }
}

/// Candidate captured names, including parameter-default references. The actual
/// compiler resolves lexical scope and records the authoritative capture list;
/// this conservative set only determines which parent cells may be threaded.
pub(crate) fn nested_free_names(
    params: &[Pat],
    body: &FunctionBody,
    _self_name: Option<&str>,
) -> std::collections::HashSet<String> {
    let mut refs = std::collections::HashSet::new();
    let mut scan = NestedReferenceScan { refs: &mut refs };
    params.visit_with(&mut scan);
    body.visit_with(&mut scan);
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
    body: &FunctionBody,
    is_arrow: bool,
    is_async: bool,
    is_generator: bool,
    self_name: Option<&str>,
) {
    // §4.3: the binding name to test against the exclude glob — the function's own
    // ident (`function render`) wins, else the binding context the emit site stashed
    // in `pending_fn_name` (`const render = …` / `obj.render = …` / `{render: …}`).
    // Consume the pending name regardless so it never leaks to a sibling expression.
    let pending = cx.pending_fn_name.take();
    let inferred_name = self_name.map(|s| s.to_string()).or(pending);

    // The class preparation pass marks its structural factory with a reserved
    // span. All original class behavior has already become VM entry callbacks;
    // this exact factory retains native private-brand and class construction
    // semantics. Ordinary source functions never take this path.
    let class_factory = self_name.is_none()
        && body.span.lo.0 == 0
        && body.span.hi.0 == 1
        && matches!(
            body.stmts.as_slice(),
            [Stmt::Return(ReturnStmt { arg: Some(_), .. })]
        );
    let generated_factory = self_name.is_none()
        && is_arrow
        && !is_async
        && !is_generator
        && mangler_jsast::span::is_generated_factory_span(body.span);
    if class_factory || generated_factory {
        emit_native_closure(cx, params, body, is_arrow, is_async, is_generator, None);
        return;
    }

    // §4.1 divert decision. A nested function is diverted to a NATIVE closure when:
    //   (a) its inferred name matches `--virtualize-exclude`; or
    //   (b) `divert_ineligible` is set AND the fn cannot be virtualized (async /
    //       generator / own `"use strict"` / structurally ineligible / would hit a
    //       compile bail). Otherwise the existing child-chunk path runs.
    let name_excluded = match (&cx.opts.exclude, &inferred_name) {
        (Some(glob), Some(n)) => super::glob_matches(glob, n),
        _ => false,
    };
    let structurally_ineligible = is_async
        || is_generator
        || matches!(
            crate::eligibility::classify_body(
                &params
                    .iter()
                    .map(|p| Param {
                        span: swc_core::common::DUMMY_SP,
                        decorators: vec![],
                        pat: p.clone()
                    })
                    .collect::<Vec<_>>(),
                body,
            ),
            crate::eligibility::Eligibility::Skip(_)
        );

    if name_excluded || (cx.opts.divert_ineligible && structurally_ineligible) {
        emit_native_closure(
            cx,
            params,
            body,
            is_arrow,
            is_async,
            is_generator,
            self_name,
        );
        emit_closure_name(cx, inferred_name.as_deref());
        return;
    }

    if is_async || is_generator {
        cx.bail_with("nested_async_generator");
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
    let mut child_free = nested_free_names(params, body, self_name);
    child_free.extend(super::environment::metadata_capture_names(
        &wrapped, body, cx.opts,
    ));
    let child_dynamic: std::collections::HashSet<String> = child_free
        .iter()
        .filter(|name| binding_needs_ref(cx, name))
        .cloned()
        .collect();
    let mut child_boxed: std::collections::HashSet<String> = child_free
        .iter()
        .filter(|n| cx.is_celled(n) && !child_dynamic.contains(*n))
        .cloned()
        .collect();
    if let Some(p) = cx.box_plan {
        child_boxed.extend(p.boxed_for(body.span.lo));
    }
    // Dynamic references already dereference their lexical cell fallback; the
    // child receives a value descriptor regardless of the enclosing box plan.
    child_boxed.retain(|name| !child_dynamic.contains(name));

    // Recursively compile the child chunk WITH the same plan AND options so
    // grandchildren box + divert correctly too. On an `Err`: if the divert-ineligible
    // option is set, this nested fn hit a compile bail the classifier didn't catch
    // (§4.1) — divert it to a native closure instead of bailing the whole parent.
    // Otherwise the bail propagates (pre-Phase-3 behavior).
    let compiled = match compile_body_inner_opts(
        &wrapped,
        body,
        &child_boxed,
        cx.box_plan,
        CompileOptions {
            native_parameters: false,
            dynamic_captures: Some(&child_dynamic),
            eval_context: false,
            lexical_entry: mangler_jsast::span::is_suspension_entry_span(body.span),
            source_context: if is_arrow || mangler_jsast::span::is_suspension_entry_span(body.span)
            {
                cx.opts.source_context
            } else {
                crate::eval::SourceContext::Function
            },
            external_var_bindings: None,
            self_binding: self_name,
            lexical_arguments: is_arrow || mangler_jsast::span::is_suspension_entry_span(body.span),
            strict: cx.opts.strict || has_use_strict_directive_block(body),
            live_captures: true,
            ..cx.opts
        },
    ) {
        Ok(c) => c,
        Err(r) => {
            if cx.opts.divert_ineligible {
                emit_native_closure(
                    cx,
                    params,
                    body,
                    is_arrow,
                    is_async,
                    is_generator,
                    self_name,
                );
                emit_closure_name(cx, inferred_name.as_deref());
            } else {
                cx.bail_with(r);
            }
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
    let mut capture_temps = 0;
    for cap in &compiled.captures {
        if Some(cap.as_str()) == self_name {
            up_slots.push(SELF_UPVALUE);
        } else if binding_has_with(cx, cap) || binding_has_environment(cx, cap) {
            emit_binding_ref(cx, cap);
            let slot = cx.alloc_temp();
            cx.emit(Instr::CaptureRef(slot));
            up_slots.push(slot);
            capture_temps += 1;
        } else {
            // Resolve the capture name in the enclosing frame. If it is a parent
            // local/param it yields that slot; if free in the parent too, the parent
            // captures it (a new slot >= cap_floor), threaded up the chain.
            let slot = cx.resolve(cap);
            up_slots.push(slot);
        }
    }

    let child_index = cx.children.len() as u32;
    cx.children.push(ChildChunk {
        compiled,
        is_arrow,
        is_strict: has_use_strict_directive_block(body),
        suspension: cx
            .opts
            .suspensions
            .and_then(|kinds| kinds.get(&body.span.lo.0))
            .copied(),
    });
    if cx.needs_environment {
        let snapshot = environment_snapshot(cx);
        cx.emit(Instr::CaptureClosureEnvironment(snapshot));
    }
    cx.emit(Instr::MakeClosure {
        child: child_index,
        is_arrow,
        cap_start,
        pcount,
        up_slots,
    });
    for _ in 0..capture_temps {
        cx.emit(Instr::ClearRef(cx.temp_top - 1));
        cx.free_temp();
    }
    let function_length = params
        .iter()
        .take_while(|param| !matches!(param, Pat::Assign(_) | Pat::Rest(_)))
        .count() as u32;
    cx.emit(Instr::SetFunctionLength(function_length));
    emit_closure_name(cx, inferred_name.as_deref());
}

fn emit_closure_name(cx: &mut Cx<'_>, name: Option<&str>) {
    let name = cx.const_str(name.unwrap_or_default().to_owned());
    cx.emit(Instr::PushConst(name));
    cx.emit(Instr::SetFunctionName);
}

/// §4: emit a NATIVE closure for an excluded / ineligible nested function. The
/// original function/arrow is stored as a factory function-expression const
/// (`crate::compile::native`); the threaded upvalues are the enclosing-VM-frame
/// bindings it references (params/locals/cells/upvalues), plus the enclosing `this`
/// for an arrow. Module globals it references are left untouched (resolved at module
/// scope). `MakeNativeClosure` builds the closure at runtime by calling the factory
/// with the up-slot values.
///
/// §4.4 soundness guards (bail to keep the enclosing subtree native via §3.3
/// bisection rather than risk a wrong capture):
///   * the native fn captures the enclosing frame's `arguments`;
///   * it WRITES a free enclosing binding that is NOT a cell (the write could not
///     propagate back to the VM frame).
pub(crate) fn emit_native_closure(
    cx: &mut Cx<'_>,
    params: &[Pat],
    body: &FunctionBody,
    is_arrow: bool,
    is_async: bool,
    is_generator: bool,
    self_name: Option<&str>,
) {
    use crate::compile::native::{Upvalue, build_factory_src};

    if let Some(reason) = super::native::unsupported_scope(params, body) {
        cx.bail_with(reason);
        return;
    }

    // Free names of the native fn (its own params/locals/self-name excluded), in
    // deterministic first-encounter order. A regular function has its OWN
    // `arguments`, so it is local there; an arrow has no `arguments`, so a reference
    // is a (lexical) capture that the §4.4 guard rejects below.
    let mut free = ordered_nested_free_names(params, body, self_name);
    if !is_arrow {
        free.retain(|n| n != "arguments");
    }
    let mut upvalues: Vec<Upvalue> = Vec::new();
    let mut up_slots: Vec<u32> = Vec::new();
    let mut capture_temps = 0;

    for name in &free {
        // §4.4: capturing the enclosing frame's implicit `arguments` cannot be
        // exposed as a slot — bail (the run bisects, this subtree stays native).
        if name == "arguments" && !cx.is_param_or_local("arguments") && !cx.opts.live_captures {
            cx.bail_with("native_closure_captures_arguments");
            return;
        }
        // Live root captures also include native wrapper parameters that have not
        // otherwise been referenced yet. Ref descriptors defer the read until use.
        let dynamic = binding_has_with(cx, name) || binding_has_environment(cx, name);
        let slot = if dynamic {
            emit_binding_ref(cx, name);
            let slot = cx.alloc_temp();
            cx.emit(Instr::CaptureRef(slot));
            capture_temps += 1;
            slot
        } else if cx.opts.live_captures {
            cx.resolve(name)
        } else if let Some(slot) = cx.lookup(name) {
            slot
        } else {
            continue;
        };
        let celled = !dynamic && cx.is_celled(name);
        upvalues.push(Upvalue {
            name: Some(name.clone()),
            celled,
            is_this: false,
        });
        up_slots.push(slot);
    }

    // An arrow's lexical `this` is threaded as a trailing synthetic upvalue (the
    // RECEIVER sentinel), and the factory closes over it. A regular function gets
    // its own `this` at call time, so no `this` upvalue.
    if is_arrow {
        upvalues.push(Upvalue {
            name: None,
            celled: false,
            is_this: true,
        });
        up_slots.push(crate::isa::RECEIVER_UPVALUE);
    }

    let Some(src) = build_factory_src(
        params,
        body,
        is_arrow,
        is_async,
        is_generator,
        self_name,
        cx.opts.strict,
        &upvalues,
    ) else {
        // Codegen / reparse failure (never expected) — bail to native subtree.
        cx.bail_with("native_closure_codegen");
        return;
    };

    let const_idx = cx.consts.len() as u32;
    if cx.consts.len() >= u32::MAX as usize {
        cx.bail_with("too_large");
        return;
    }
    cx.consts.push(crate::chunk::Const::NativeFactory(src));
    cx.emit(Instr::MakeNativeClosure {
        const_idx,
        is_arrow,
        up_slots,
    });
    for _ in 0..capture_temps {
        cx.emit(Instr::ClearRef(cx.temp_top - 1));
        cx.free_temp();
    }
}

/// Free names a nested fn references (its own params/locals/self-name excluded), in
/// deterministic first-encounter (source) order. Mirrors `nested_free_names` (which
/// returns an unordered set) but preserves order so the threaded-upvalue list — and
/// thus the serialized bytecode + factory params — is byte-stable for a given seed.
fn ordered_nested_free_names(
    params: &[Pat],
    body: &FunctionBody,
    self_name: Option<&str>,
) -> Vec<String> {
    super::native::free_names(params, body, self_name)
}

/// True if a block body begins with its own `"use strict"` directive. Mirrors
/// `crate::cells::has_use_strict_directive` (private) but is reachable from the compiler (which only
/// imports `bytecode`'s module). Kept tiny and local.
pub(crate) fn has_use_strict_directive_block(body: &FunctionBody) -> bool {
    mangler_jsast::directives::has_use_strict(&body.stmts)
}
