//! Destructuring (object/array patterns), spread (array/call/new/object),
//! iterator-protocol helpers (`for-of`-style array destructure with close), and the
//! handler-patch helpers shared by the control-flow emitters.
//!
//! Every `emit_*` here takes `&mut Cx` and shares the frame model and the other
//! construct-family emitters (`stmt`, `expr`) via `use super::*`.

use swc_core::ecma::ast::*;

use super::*;
use crate::isa::{compound_op_code, Instr};

/// The static string key of a destructuring property name, or `None` for a
/// computed key (`{[e]: t}`), a lone-surrogate string, or an out-of-range numeric
/// key — the caller bails on `None`. Numeric keys reuse the safe-integer rule from
/// `emit_prop_key` so the property name stringifies identically in Rust and JS.
pub(crate) fn destructure_key_string(key: &PropName) -> Option<String> {
    match key {
        PropName::Ident(n) => Some(n.sym.to_string()),
        PropName::Str(s) => s.value.as_str().map(|v| v.to_string()),
        PropName::Num(num) => {
            let v = num.value;
            if v.fract() == 0.0 && v.is_finite() && v.abs() < 9007199254740992.0 {
                Some(format!("{}", v as i64))
            } else {
                None
            }
        }
        PropName::Computed(_) | PropName::BigInt(_) => None,
    }
}

/// Resolve a destructuring target identifier to its (writable) slot, returning
/// `(slot, boxed)`. `boxed` is true for a D1 boxed mutable capture (the store goes
/// through its cell). Writing an UNboxed captured outer binding is not propagated
/// back to the enclosing scope, so a non-local, non-boxed target bails
/// `mutable_capture` (read-only capture only). Declaration leaves are always locals
/// (slotted by `DeclCollector`), so they pass.
pub(crate) fn resolve_writable(cx: &mut Cx<'_>, name: &str) -> Option<(u32, bool)> {
    let boxed = cx.is_celled(name);
    if !cx.is_param_or_local(name) && !boxed {
        cx.bail_with("mutable_capture");
        return None;
    }
    Some((cx.resolve(name), boxed))
}

/// Emit a store to a resolved writable target: `StoreCell` when boxed (write
/// through the cell so the mutation propagates to the enclosing scope), else
/// `StoreLocal`. Leaves the value on the stack (both ops do), then `Pop` it — the
/// destructuring caller is stack-neutral.
pub(crate) fn emit_store_writable(cx: &mut Cx<'_>, slot: u32, boxed: bool) {
    cx.emit(if boxed { Instr::StoreCell(slot) } else { Instr::StoreLocal(slot) });
    cx.emit(Instr::Pop);
}

/// Apply a destructuring/parameter default to the value on top of the stack:
/// `[v] -> [v === undefined ? default : v]`. Reuses the default-param prologue
/// shape (a strict `=== undefined` test), so a present-but-`undefined` property
/// triggers the default exactly as the spec requires.
pub(crate) fn emit_value_default(cx: &mut Cx<'_>, default: &Expr) {
    cx.emit(Instr::Dup);
    cx.emit(Instr::PushUndef);
    cx.emit(Instr::Bin(7)); // ===
    let use_v = cx.code.len();
    cx.emit(Instr::JumpIfFalse(u32::MAX)); // not undefined -> keep v
    cx.emit(Instr::Pop); // drop the undefined value
    emit_expr(cx, default);
    let done = cx.code.len();
    cx.emit(Instr::Jump(u32::MAX));
    let merge = cx.here();
    patch(cx, use_v, merge);
    patch(cx, done, merge);
}

/// Bind the value on top of the stack to a destructuring target pattern,
/// consuming it. Handles defaults (`= d`), simple idents, nested object patterns
/// (store to a temp + recurse), and the assignment-pattern ident form
/// (`Pat::Expr(Ident)`). Array sub-patterns and member-expression targets are
/// Tier-2 / out of scope here and bail with a precise reason.
pub(crate) fn emit_bind_target(cx: &mut Cx<'_>, target: &Pat) {
    match target {
        Pat::Assign(ap) => {
            emit_value_default(cx, &ap.right);
            emit_bind_target(cx, &ap.left);
        }
        Pat::Ident(bi) => {
            if let Some((slot, boxed)) = resolve_writable(cx, bi.id.sym.as_ref()) {
                emit_store_writable(cx, slot, boxed);
            }
        }
        Pat::Object(o) => {
            let t = cx.alloc_temp();
            cx.emit(Instr::StoreLocal(t));
            cx.emit(Instr::Pop);
            emit_destructure_object(cx, o, t);
            cx.free_temp();
        }
        // Assignment-pattern leaf referencing an existing binding (`({k: x} = o)`).
        Pat::Expr(e) => match &**e {
            Expr::Ident(id) => {
                if let Some((slot, boxed)) = resolve_writable(cx, id.sym.as_ref()) {
                    emit_store_writable(cx, slot, boxed);
                }
            }
            _ => cx.bail_with("destructure_member_target"),
        },
        // Array sub-pattern: the iterator is taken straight off the stacked value.
        Pat::Array(a) => emit_destructure_array(cx, a),
        Pat::Rest(_) | Pat::Invalid(_) => cx.bail_with("destructure_target"),
    }
}

/// Lower an object destructuring pattern, reading from the source already stored
/// in slot `src` (design §L6). Stack-neutral: each property reads `src[key]`,
/// applies any default, and binds the result; an object-rest collects the
/// remaining own-enumerable properties. The source is read once per property
/// (matching JS `[[Get]]` order); destructuring a `null`/`undefined` source
/// throws via the first `GetProp`, matching the spec.
pub(crate) fn emit_destructure_object(cx: &mut Cx<'_>, pat: &ObjectPat, src: u32) {
    // Static keys consumed by the named props — excluded from any rest copy.
    let mut taken: Vec<String> = Vec::new();
    let mut rest: Option<&Pat> = None;
    for prop in &pat.props {
        if cx.bailed() {
            return;
        }
        match prop {
            ObjectPatProp::KeyValue(kv) => {
                let key = match destructure_key_string(&kv.key) {
                    Some(k) => k,
                    None => {
                        cx.bail_with("destructure_computed_key");
                        return;
                    }
                };
                taken.push(key.clone());
                cx.emit(Instr::LoadLocal(src));
                let ci = cx.const_str(key);
                cx.emit(Instr::PushConst(ci));
                cx.emit(Instr::GetProp);
                emit_bind_target(cx, &kv.value);
            }
            // Shorthand `{x}` / `{x = d}`: the key name is also the binding target.
            ObjectPatProp::Assign(a) => {
                let name = a.key.id.sym.to_string();
                taken.push(name.clone());
                cx.emit(Instr::LoadLocal(src));
                let ci = cx.const_str(name);
                cx.emit(Instr::PushConst(ci));
                cx.emit(Instr::GetProp);
                if let Some(def) = &a.value {
                    emit_value_default(cx, def);
                }
                if let Some((slot, boxed)) = resolve_writable(cx, a.key.id.sym.as_ref()) {
                    emit_store_writable(cx, slot, boxed);
                }
            }
            ObjectPatProp::Rest(r) => rest = Some(&r.arg),
        }
    }
    if let Some(rest_target) = rest {
        emit_object_rest(cx, src, &taken, rest_target);
    }
}

/// Object-rest `...r = Object.assign({}, src)` minus the statically-taken keys
/// (design §L6). Uses the captured global `Object`; the named-property getters of
/// taken keys therefore also fire during the copy, then those keys are deleted —
/// an accepted divergence for side-effecting getters (the differential corpus
/// uses plain data properties). The rest target must be a simple binding ident.
pub(crate) fn emit_object_rest(cx: &mut Cx<'_>, src: u32, taken: &[String], rest_target: &Pat) {
    let rest_name = match rest_target {
        Pat::Ident(bi) => bi.id.sym.to_string(),
        Pat::Expr(e) => match &**e {
            Expr::Ident(id) => id.sym.to_string(),
            _ => {
                cx.bail_with("rest_destructure");
                return;
            }
        },
        _ => {
            cx.bail_with("rest_destructure");
            return;
        }
    };
    let (slot, boxed) = match resolve_writable(cx, &rest_name) {
        Some(s) => s,
        None => return,
    };
    // A boxed rest target reads/writes through its cell (`L[slot][0]`); an unboxed
    // local reads/writes the slot directly.
    let load = |cx: &mut Cx| cx.emit(if boxed { Instr::LoadCell(slot) } else { Instr::LoadLocal(slot) });

    // copy = Object.assign({}, src)  (CallResolved: [recv, fn, ...args] -> result).
    let object_global = cx.resolve("Object");
    cx.emit(Instr::LoadLocal(object_global)); // receiver
    cx.emit(Instr::Dup);
    let assign_key = cx.const_str("assign".to_string());
    cx.emit(Instr::PushConst(assign_key));
    cx.emit(Instr::GetProp); // function Object.assign
    cx.emit(Instr::MakeObject(0)); // arg0 = {}
    cx.emit(Instr::LoadLocal(src)); // arg1 = src
    cx.emit(Instr::CallResolved(2));
    cx.emit(if boxed { Instr::StoreCell(slot) } else { Instr::StoreLocal(slot) });
    cx.emit(Instr::Pop);

    // Drop the taken keys from the copy so the rest holds only the remainder.
    for key in taken {
        load(cx);
        let ci = cx.const_str(key.clone());
        cx.emit(Instr::PushConst(ci));
        cx.emit(Instr::DeleteProp);
        cx.emit(Instr::Pop);
    }
}

/// Lower an array destructuring pattern (design §L6 + the iterator mechanism).
/// Expects the SOURCE VALUE on top of the stack and consumes it — the iterator is
/// taken immediately so no source temp is reserved. Elements bind left-to-right
/// via `IterStep`; holes advance-and-discard; a `...rest` drains the remainder
/// into a fresh array. The iterator is closed (via the §X completion machinery)
/// on any abrupt completion AND on normal completion when it is not yet exhausted
/// — except when a `...rest` already drained it — matching ECMAScript
/// IteratorBindingInitialization. A `done` flag (set whenever a step reports
/// exhaustion or a rest drains the iterator) guards every close so an already-done
/// iterator is never re-closed.
pub(crate) fn emit_destructure_array(cx: &mut Cx<'_>, pat: &ArrayPat) {
    // it = source[Symbol.iterator]()   (consumes the stacked source value).
    cx.emit(Instr::GetIter);
    let it = cx.alloc_temp();
    cx.emit(Instr::StoreLocal(it));
    cx.emit(Instr::Pop);
    // done = false.
    let done = cx.alloc_temp();
    let f = cx.const_bool(false);
    cx.emit(Instr::PushConst(f));
    cx.emit(Instr::StoreLocal(done));
    cx.emit(Instr::Pop);

    // Finally-only close handler active across element binding.
    let ph = cx.code.len();
    cx.emit(Instr::PushHandler(u32::MAX, u32::MAX));
    cx.handler_depth += 1;

    for elem in &pat.elems {
        if cx.bailed() {
            return;
        }
        match elem {
            None => emit_arr_skip(cx, it, done),
            Some(Pat::Rest(r)) => emit_arr_rest(cx, it, done, &r.arg),
            Some(p) => {
                emit_arr_next(cx, it, done);
                emit_bind_target(cx, p);
            }
        }
    }
    if cx.bailed() {
        return;
    }

    // Normal completion: drop the handler, then close iff not exhausted.
    cx.emit(Instr::PopHandler);
    cx.handler_depth -= 1;
    emit_arr_close_if_open(cx, it, done);
    let exit_j = cx.code.len();
    cx.emit(Instr::Jump(u32::MAX));

    // CLOSE: abrupt path — close iff not exhausted, then resume the completion.
    let close_pc = cx.here();
    patch_handler_fin(cx, ph, close_pc);
    emit_arr_close_if_open(cx, it, done);
    cx.emit(Instr::EndFinally);

    let exit = cx.here();
    patch(cx, exit_j, exit);

    cx.free_temp(); // done
    cx.free_temp(); // it
}

/// Advance `it` one step, leaving the value (or `undefined` if exhausted) on the
/// stack and setting `done` when the iterator reports exhaustion.
pub(crate) fn emit_arr_next(cx: &mut Cx<'_>, it: u32, done: u32) {
    cx.emit(Instr::LoadLocal(it));
    cx.emit(Instr::IterStep); // -> [false] (done) | [value, true]
    let to_setdone = cx.code.len();
    cx.emit(Instr::JumpIfFalse(u32::MAX)); // done -> SETDONE; else pops `true`, [value]
    let to_have = cx.code.len();
    cx.emit(Instr::Jump(u32::MAX)); // not done: [value] -> HAVE
    let setdone = cx.here();
    patch(cx, to_setdone, setdone);
    let t = cx.const_bool(true);
    cx.emit(Instr::PushConst(t));
    cx.emit(Instr::StoreLocal(done));
    cx.emit(Instr::Pop);
    cx.emit(Instr::PushUndef);
    let have = cx.here();
    patch(cx, to_have, have);
}

/// Advance `it` one step discarding the value (an array elision/hole), setting
/// `done` on exhaustion. Stack-neutral.
pub(crate) fn emit_arr_skip(cx: &mut Cx<'_>, it: u32, done: u32) {
    cx.emit(Instr::LoadLocal(it));
    cx.emit(Instr::IterStep);
    let to_setdone = cx.code.len();
    cx.emit(Instr::JumpIfFalse(u32::MAX)); // done -> SETDONE
    cx.emit(Instr::Pop); // not done: drop the value
    let to_after = cx.code.len();
    cx.emit(Instr::Jump(u32::MAX));
    let setdone = cx.here();
    patch(cx, to_setdone, setdone);
    let t = cx.const_bool(true);
    cx.emit(Instr::PushConst(t));
    cx.emit(Instr::StoreLocal(done));
    cx.emit(Instr::Pop);
    let after = cx.here();
    patch(cx, to_after, after);
}

/// `if (!done) it.return()` — close a non-exhausted iterator. Stack-neutral.
pub(crate) fn emit_arr_close_if_open(cx: &mut Cx<'_>, it: u32, done: u32) {
    cx.emit(Instr::LoadLocal(done));
    let to_close = cx.code.len();
    cx.emit(Instr::JumpIfFalse(u32::MAX)); // done falsy (not done) -> CLOSE
    let to_skip = cx.code.len();
    cx.emit(Instr::Jump(u32::MAX)); // done -> SKIP
    let do_close = cx.here();
    patch(cx, to_close, do_close);
    cx.emit(Instr::LoadLocal(it));
    cx.emit(Instr::IterClose);
    let skip = cx.here();
    patch(cx, to_skip, skip);
}

/// `...rest`: drain the remaining iterator values into a fresh array, mark the
/// iterator exhausted (`done = true`, so no later close fires), and bind the
/// array to the rest target. Uses two temps (the accumulator + a value scratch),
/// both freed before binding.
pub(crate) fn emit_arr_rest(cx: &mut Cx<'_>, it: u32, done: u32, rest_target: &Pat) {
    cx.emit(Instr::MakeArray(0));
    let arr = cx.alloc_temp();
    cx.emit(Instr::StoreLocal(arr));
    cx.emit(Instr::Pop);
    let v = cx.alloc_temp();

    let loop_pc = cx.here();
    cx.emit(Instr::LoadLocal(it));
    cx.emit(Instr::IterStep);
    let to_done = cx.code.len();
    cx.emit(Instr::JumpIfFalse(u32::MAX)); // exhausted -> REST_DONE
    // not done: [value] -> arr.push(value)
    cx.emit(Instr::StoreLocal(v));
    cx.emit(Instr::Pop);
    cx.emit(Instr::LoadLocal(arr)); // receiver
    cx.emit(Instr::Dup);
    let push_key = cx.const_str("push".to_string());
    cx.emit(Instr::PushConst(push_key));
    cx.emit(Instr::GetProp);
    cx.emit(Instr::LoadLocal(v));
    cx.emit(Instr::CallResolved(1));
    cx.emit(Instr::Pop);
    cx.emit(Instr::Jump(loop_pc));

    let rest_done = cx.here();
    patch(cx, to_done, rest_done);
    let t = cx.const_bool(true);
    cx.emit(Instr::PushConst(t));
    cx.emit(Instr::StoreLocal(done));
    cx.emit(Instr::Pop);

    cx.emit(Instr::LoadLocal(arr)); // [arr] for binding
    cx.free_temp(); // v
    cx.free_temp(); // arr
    emit_bind_target(cx, rest_target);
}

/// Build an array from a sequence of call args / array-literal elements, lowering
/// each spread (`...x`) via the iterator protocol so any iterable is honored
/// (design "Spread/rest"). Leaves the built array on the stack. Used for array
/// literals with spread and for the args array of spread calls / `new`.
pub(crate) fn emit_spread_array(cx: &mut Cx<'_>, elems: &[ExprOrSpread]) {
    cx.emit(Instr::MakeArray(0));
    let arr = cx.alloc_temp();
    cx.emit(Instr::StoreLocal(arr));
    cx.emit(Instr::Pop);

    for e in elems {
        if cx.bailed() {
            cx.free_temp();
            return;
        }
        if e.spread.is_some() {
            // it = e.expr[Symbol.iterator](); then push each yielded value.
            emit_expr(cx, &e.expr);
            if cx.bailed() {
                cx.free_temp();
                return;
            }
            cx.emit(Instr::GetIter);
            let it = cx.alloc_temp();
            cx.emit(Instr::StoreLocal(it));
            cx.emit(Instr::Pop);
            let v = cx.alloc_temp();
            let loop_pc = cx.here();
            cx.emit(Instr::LoadLocal(it));
            cx.emit(Instr::IterStep);
            let to_done = cx.code.len();
            cx.emit(Instr::JumpIfFalse(u32::MAX));
            cx.emit(Instr::StoreLocal(v));
            cx.emit(Instr::Pop);
            cx.emit(Instr::LoadLocal(arr));
            cx.emit(Instr::Dup);
            let push_key = cx.const_str("push".to_string());
            cx.emit(Instr::PushConst(push_key));
            cx.emit(Instr::GetProp);
            cx.emit(Instr::LoadLocal(v));
            cx.emit(Instr::CallResolved(1));
            cx.emit(Instr::Pop);
            cx.emit(Instr::Jump(loop_pc));
            let done = cx.here();
            patch(cx, to_done, done);
            cx.free_temp(); // v
            cx.free_temp(); // it
        } else {
            // arr.push(value): build [arr, push] then eval the value on top.
            cx.emit(Instr::LoadLocal(arr));
            cx.emit(Instr::Dup);
            let push_key = cx.const_str("push".to_string());
            cx.emit(Instr::PushConst(push_key));
            cx.emit(Instr::GetProp);
            emit_expr(cx, &e.expr);
            if cx.bailed() {
                cx.free_temp();
                return;
            }
            cx.emit(Instr::CallResolved(1));
            cx.emit(Instr::Pop);
        }
    }

    cx.emit(Instr::LoadLocal(arr));
    cx.free_temp(); // arr
}

/// Emit one object-literal entry as a key then a value (two stack slots), shared
/// by the plain `MakeObject` path and the spread `Object.assign` segments. Only
/// data props are modeled; `__proto__` and accessor/method props bail.
/// §4.3 binding-name inference from a static object/class property key. Returns
/// `None` for computed (`[expr]`), numeric, bigint, or private-name keys.
pub(crate) fn static_prop_key_name(key: &PropName) -> Option<String> {
    match key {
        PropName::Ident(id) => Some(id.sym.to_string()),
        PropName::Str(s) => s.value.as_str().map(|v| v.to_string()),
        PropName::Num(_) | PropName::BigInt(_) | PropName::Computed(_) => None,
    }
}

pub(crate) fn emit_object_entry(cx: &mut Cx<'_>, prop: &Prop) {
    match prop {
        Prop::KeyValue(kv) => {
            emit_prop_key(cx, &kv.key);
            if cx.bailed() {
                return;
            }
            // §4.3: infer the binding name from a static property key
            // (`{ render: () => … }` → `render`) for the native-closure divert.
            cx.pending_fn_name = static_prop_key_name(&kv.key);
            emit_expr(cx, &kv.value);
            cx.pending_fn_name = None;
        }
        Prop::Shorthand(ident) => {
            // `{x}` -> key "x", value = binding `x`. `obj[k]=v` would set the
            // prototype for "__proto__"; bail to match `emit_prop_key`.
            if ident.sym.as_ref() == "__proto__" {
                cx.bail();
                return;
            }
            let ci = cx.const_str(ident.sym.to_string());
            cx.emit(Instr::PushConst(ci));
            let slot = cx.resolve(ident.sym.as_ref());
            cx.emit(Instr::LoadLocal(slot));
        }
        _ => cx.bail(),
    }
}

/// Object literal with spread (`{a, ...o, b}`) -> `Object.assign({}, seg0, …)`,
/// preserving source order (design "Spread/rest"). Consecutive fixed props form
/// one segment object; each spread is its own source arg; the fresh `{}` is the
/// mutated target. Uses the captured global `Object` (a non-shadowed `Object` and
/// real `Object.assign` semantics — own-enumerable copy, getters invoked — are
/// what the original observes in the normal case).
pub(crate) fn emit_object_spread(cx: &mut Cx<'_>, o: &ObjectLit) {
    let object = cx.resolve("Object");
    cx.emit(Instr::LoadLocal(object)); // receiver
    cx.emit(Instr::Dup);
    let assign_key = cx.const_str("assign".to_string());
    cx.emit(Instr::PushConst(assign_key));
    cx.emit(Instr::GetProp); // [Object, assign]
    cx.emit(Instr::MakeObject(0)); // target {}

    let mut fixed_in_seg = 0u32;
    let mut seg_count = 0u32; // source args after the target {}
    for prop in &o.props {
        match prop {
            PropOrSpread::Prop(p) => {
                emit_object_entry(cx, p);
                if cx.bailed() {
                    return;
                }
                fixed_in_seg += 1;
            }
            PropOrSpread::Spread(s) => {
                if fixed_in_seg > 0 {
                    cx.emit(Instr::MakeObject(fixed_in_seg));
                    seg_count += 1;
                    fixed_in_seg = 0;
                }
                emit_expr(cx, &s.expr);
                if cx.bailed() {
                    return;
                }
                seg_count += 1;
            }
        }
    }
    if fixed_in_seg > 0 {
        cx.emit(Instr::MakeObject(fixed_in_seg));
        seg_count += 1;
    }
    // args = [{}, seg0, seg1, …]; `Object.assign` mutates and returns the target.
    cx.emit(Instr::CallResolved(1 + seg_count));
}

pub(crate) fn emit_assign(cx: &mut Cx<'_>, a: &AssignExpr) {
    if a.op == AssignOp::Assign {
        match &a.left {
            AssignTarget::Simple(SimpleAssignTarget::Ident(bi)) => {
                let name = bi.id.sym.as_ref();
                // D1: a BOXED capture is writable through its cell; an unboxed
                // capture is read-only -> bail.
                let boxed = cx.is_celled(name);
                if !cx.is_param_or_local(name) && !boxed {
                    // Writing to a captured outer binding: the VM frame write is not
                    // propagated back to the enclosing scope. Skip to stay sound
                    // (read-only capture only).
                    cx.bail_with("mutable_capture");
                    return;
                }
                let slot = cx.resolve(name);
                emit_expr(cx, &a.right);
                cx.emit(if boxed { Instr::StoreCell(slot) } else { Instr::StoreLocal(slot) });
            }
            AssignTarget::Simple(SimpleAssignTarget::Member(m)) => {
                emit_expr(cx, &m.obj);
                emit_member_key(cx, &m.prop);
                if cx.bailed() {
                    return;
                }
                // §4.3: infer the binding name from the last static member segment
                // (`obj.render = function(){}` → `render`) for the native-closure
                // divert. Computed/private keys yield no name.
                if let MemberProp::Ident(id) = &m.prop {
                    cx.pending_fn_name = Some(id.sym.to_string());
                }
                emit_expr(cx, &a.right);
                cx.pending_fn_name = None;
                // Per the VM contract, SetProp does `o[k]=v; push v` — it leaves
                // the assigned value on the stack, exactly like StoreLocal. So a
                // member assignment used as an expression (`y = (o.k = v)`) is
                // stack-balanced and correct, and as a statement the trailing Pop
                // in emit_expr_stmt discards it.
                cx.emit(Instr::SetProp);
            }
            // Destructuring assignment `({a, b: c, ...r} = src)`. The assignment
            // expression's value is the RHS, so a `Dup`'d copy is kept on the stack
            // (used as the expr value / discarded by the statement's trailing Pop)
            // while the other copy drives the destructure from a temp.
            AssignTarget::Pat(AssignTargetPat::Object(obj)) => {
                emit_expr(cx, &a.right);
                if cx.bailed() {
                    return;
                }
                cx.emit(Instr::Dup);
                let t = cx.alloc_temp();
                cx.emit(Instr::StoreLocal(t));
                cx.emit(Instr::Pop);
                emit_destructure_object(cx, obj, t);
                cx.free_temp();
            }
            // `[m, n] = src` — `Dup` keeps the RHS as the expression value while
            // the other copy feeds the iterator destructure.
            AssignTarget::Pat(AssignTargetPat::Array(arr)) => {
                emit_expr(cx, &a.right);
                if cx.bailed() {
                    return;
                }
                cx.emit(Instr::Dup);
                emit_destructure_array(cx, arr);
            }
            _ => cx.bail(),
        }
    } else {
        // compound assign; target MUST be a slot ident in v1.
        let code = match compound_op_code(a.op) {
            Some(c) => c,
            None => {
                cx.bail();
                return;
            }
        };
        match &a.left {
            AssignTarget::Simple(SimpleAssignTarget::Ident(bi)) => {
                let name = bi.id.sym.as_ref();
                // D1: a BOXED capture is read-modify-written through its cell.
                let boxed = cx.is_celled(name);
                if !cx.is_param_or_local(name) && !boxed {
                    // Compound-assigning a captured outer binding cannot be
                    // written back to the enclosing scope. Skip to stay sound.
                    cx.bail_with("mutable_capture");
                    return;
                }
                let slot = cx.resolve(name);
                cx.emit(if boxed { Instr::LoadCell(slot) } else { Instr::LoadLocal(slot) });
                emit_expr(cx, &a.right);
                cx.emit(Instr::Bin(code));
                cx.emit(if boxed { Instr::StoreCell(slot) } else { Instr::StoreLocal(slot) });
            }
            _ => cx.bail(),
        }
    }
}

pub(crate) fn emit_call(cx: &mut Cx<'_>, c: &CallExpr) {
    let callee = match &c.callee {
        Callee::Expr(e) => e,
        _ => {
            cx.bail();
            return;
        }
    };
    let has_spread = c.args.iter().any(|a| a.spread.is_some());
    if let Expr::Member(m) = &**callee {
        if has_spread {
            // `o.m(...a)` -> `o.m.apply(o, [args])`. Evaluate the receiver once into
            // a temp, read the method off it, then `method.apply(recv, ARR)`.
            emit_expr(cx, &m.obj);
            let recv = cx.alloc_temp();
            cx.emit(Instr::StoreLocal(recv));
            cx.emit(Instr::Pop);
            cx.emit(Instr::LoadLocal(recv));
            emit_member_key(cx, &m.prop);
            if cx.bailed() {
                cx.free_temp();
                return;
            }
            cx.emit(Instr::GetProp); // [method]
            cx.emit(Instr::Dup);
            let apply_key = cx.const_str("apply".to_string());
            cx.emit(Instr::PushConst(apply_key));
            cx.emit(Instr::GetProp); // [method, apply]
            cx.emit(Instr::LoadLocal(recv)); // [method, apply, recv]
            emit_spread_array(cx, &c.args); // [method, apply, recv, ARR]
            cx.emit(Instr::CallResolved(2));
            cx.free_temp(); // recv
            return;
        }
        // method call `o.m(args)`. The spec reads the method (`o.m`, GetValue)
        // *before* ArgumentListEvaluation, observable when `m` is a side-effecting
        // getter. So resolve the function onto the stack first — leaving the
        // receiver under it — then evaluate args, then `CallResolved`:
        //   eval obj; Dup; eval key; GetProp  => stack [obj, method]
        //   eval args                          => stack [obj, method, ...args]
        //   CallResolved(argc): a=splice; f=pop; o=pop; push f.apply(o,a)
        emit_expr(cx, &m.obj);
        cx.emit(Instr::Dup);
        emit_member_key(cx, &m.prop);
        if cx.bailed() {
            return;
        }
        cx.emit(Instr::GetProp);
        let mut argc = 0u32;
        for a in &c.args {
            emit_expr(cx, &a.expr);
            argc += 1;
        }
        cx.emit(Instr::CallResolved(argc));
    } else if has_spread {
        // `f(...a)` -> `f.apply(undefined, [args])`.
        emit_expr(cx, callee);
        cx.emit(Instr::Dup);
        let apply_key = cx.const_str("apply".to_string());
        cx.emit(Instr::PushConst(apply_key));
        cx.emit(Instr::GetProp); // [f, apply]
        cx.emit(Instr::PushUndef); // [f, apply, undefined]
        emit_spread_array(cx, &c.args); // [f, apply, undefined, ARR]
        cx.emit(Instr::CallResolved(2));
    } else {
        emit_expr(cx, callee);
        let mut argc = 0u32;
        for a in &c.args {
            emit_expr(cx, &a.expr);
            argc += 1;
        }
        cx.emit(Instr::Call(argc));
    }
}

pub(crate) fn patch(cx: &mut Cx<'_>, idx: usize, target: u32) {
    match &mut cx.code[idx] {
        // All control-transfer instructions carry their target PC as operand 0,
        // so one patch covers plain jumps and the unwinding `break`/`continue`.
        Instr::Jump(t) | Instr::JumpIfFalse(t) | Instr::BreakUnwind(t, _) => *t = target,
        _ => unreachable!("patch on non-jump instruction"),
    }
}

/// Patch the catch-target operand (operand 0) of a `PushHandler` at `idx`.
pub(crate) fn patch_handler_catch(cx: &mut Cx<'_>, idx: usize, target: u32) {
    match &mut cx.code[idx] {
        Instr::PushHandler(c, _) => *c = target,
        _ => unreachable!("patch_handler_catch on non-PushHandler"),
    }
}

/// Patch the finally-target operand (operand 1) of a `PushHandler` at `idx`.
pub(crate) fn patch_handler_fin(cx: &mut Cx<'_>, idx: usize, target: u32) {
    match &mut cx.code[idx] {
        Instr::PushHandler(_, fpc) => *fpc = target,
        _ => unreachable!("patch_handler_fin on non-PushHandler"),
    }
}

