//! Destructuring (object/array patterns), spread (array/call/new/object),
//! iterator-protocol helpers (`for-of`-style array destructure with close), and the
//! handler-patch helpers shared by the control-flow emitters.
//!
//! Every `emit_*` here takes `&mut Cx` and shares the frame model and the other
//! construct-family emitters (`stmt`, `expr`) via `use super::*`.

use super::dynamic_scope::{binding_needs_ref, emit_binding_ref};
use super::*;
use crate::isa::{Instr, compound_op_code};
use mangler_jsast::assignment_target;

/// Resolve a destructuring target identifier to its (writable) slot, returning
/// `(slot, boxed)`. `boxed` is true for a D1 boxed mutable capture (the store goes
/// through its cell). Writing an UNboxed captured outer binding is not propagated
/// back to the enclosing scope, so a non-local, non-boxed target bails
/// `mutable_capture` (read-only capture only). Declaration leaves are always locals
/// (slotted by `DeclCollector`), so they pass.
pub(crate) fn resolve_writable(cx: &mut Cx<'_>, name: &str) -> Option<(u32, bool)> {
    let boxed = cx.is_celled(name);
    if !cx.is_param_or_local(name) && !boxed && !cx.opts.live_captures {
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
    cx.emit(if cx.initializing && cx.lexical_slots.contains_key(&slot) {
        Instr::InitLocal(slot)
    } else if boxed {
        Instr::StoreCell(slot)
    } else {
        Instr::StoreLocal(slot)
    });
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
    let initializing = cx.initializing;
    cx.initializing = false;
    emit_expr(cx, default);
    cx.initializing = initializing;
    let done = cx.code.len();
    cx.emit(Instr::Jump(u32::MAX));
    let merge = cx.here();
    patch(cx, use_v, merge);
    patch(cx, done, merge);
}

/// Bind the value on top of the stack to a destructuring target pattern,
/// consuming it. Handles defaults (`= d`), simple idents, nested object patterns
/// (store to a temp + recurse), array patterns, and assignment references.
pub(crate) fn emit_bind_target(cx: &mut Cx<'_>, target: &Pat) {
    match target {
        Pat::Assign(ap) => {
            if let Pat::Ident(name) = &*ap.left {
                cx.pending_fn_name = Some(name.id.sym.to_string());
            }
            emit_value_default(cx, &ap.right);
            emit_bind_target(cx, &ap.left);
        }
        Pat::Ident(bi) => {
            if !cx.initializing && binding_needs_ref(cx, bi.id.sym.as_ref()) {
                let value = cx.alloc_temp();
                cx.emit(Instr::StoreLocal(value));
                cx.emit(Instr::Pop);
                emit_binding_ref(cx, bi.id.sym.as_ref());
                cx.emit(Instr::ResolveRef);
                cx.emit(Instr::LoadLocal(value));
                cx.emit(Instr::PutRef);
                cx.emit(Instr::Pop);
                cx.free_temp();
                return;
            }
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
            Expr::Ident(id) => emit_bind_target(
                cx,
                &Pat::Ident(BindingIdent {
                    id: id.clone(),
                    type_ann: None,
                }),
            ),
            Expr::Member(m) => {
                let value = cx.alloc_temp();
                cx.emit(Instr::StoreLocal(value));
                cx.emit(Instr::Pop);
                emit_expr(cx, &m.obj);
                emit_assignment_member_key(cx, &m.prop);
                cx.emit(Instr::LoadLocal(value));
                cx.emit(Instr::SetProp);
                cx.emit(Instr::Pop);
                cx.free_temp();
            }
            Expr::Paren(p) => emit_bind_target(cx, &Pat::Expr(p.expr.clone())),
            Expr::Call(_) => {
                cx.emit(Instr::Pop);
                super::expr::emit_call_assignment_target(cx, e);
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
/// throws before evaluating any keys, including an empty pattern.
pub(crate) fn emit_destructure_object(cx: &mut Cx<'_>, pat: &ObjectPat, src: u32) {
    cx.emit(Instr::LoadLocal(src));
    cx.emit(Instr::RequireObject);
    cx.emit(Instr::Pop);
    // Retain canonical keys as values: Symbols and computed names must be
    // excluded by identity, and coercion runs exactly once before target reads.
    let has_rest = pat
        .props
        .iter()
        .any(|prop| matches!(prop, ObjectPatProp::Rest(_)));
    let mut taken = Vec::new();
    for prop in &pat.props {
        if cx.bailed() {
            return;
        }
        match prop {
            ObjectPatProp::KeyValue(kv) => {
                emit_prop_key(cx, &kv.key);
                let key = cx.alloc_temp();
                cx.emit(Instr::StoreLocal(key));
                cx.emit(Instr::Pop);
                if has_rest {
                    taken.push(key);
                }
                let reference = emit_prepare_target(cx, &kv.value);
                cx.emit(Instr::LoadLocal(src));
                cx.emit(Instr::LoadLocal(key));
                cx.emit(Instr::GetProp);
                emit_prepared_target(cx, &kv.value, reference);
                if !has_rest {
                    cx.free_temp();
                }
            }
            ObjectPatProp::Assign(a) => {
                let name = a.key.id.sym.to_string();
                let ci = cx.const_str(name);
                let key = cx.alloc_temp();
                cx.emit(Instr::PushConst(ci));
                cx.emit(Instr::StoreLocal(key));
                cx.emit(Instr::Pop);
                if has_rest {
                    taken.push(key);
                }
                let target = Pat::Ident(a.key.clone());
                let reference = emit_prepare_target(cx, &target);
                cx.emit(Instr::LoadLocal(src));
                cx.emit(Instr::LoadLocal(key));
                cx.emit(Instr::GetProp);
                if let Some(def) = &a.value {
                    cx.pending_fn_name = Some(a.key.id.sym.to_string());
                    emit_value_default(cx, def);
                }
                emit_prepared_target(cx, &target, reference);
                if !has_rest {
                    cx.free_temp();
                }
            }
            ObjectPatProp::Rest(r) => {
                let reference = emit_prepare_target(cx, &r.arg);
                cx.emit(Instr::LoadLocal(src));
                for key in &taken {
                    cx.emit(Instr::LoadLocal(*key));
                }
                cx.emit(Instr::MakeArray(taken.len() as u32));
                cx.emit(Instr::RestProps);
                emit_prepared_target(cx, &r.arg, reference);
            }
        }
    }
    for _ in taken {
        cx.free_temp();
    }
}

/// A property Reference retains the raw computed key. GetValue and PutValue
/// each perform their own coercion, which can observably differ.
fn emit_assignment_member_key(cx: &mut Cx<'_>, key: &MemberProp) {
    match key {
        MemberProp::Computed(c) => emit_expr(cx, &c.expr),
        _ => emit_member_key(cx, key),
    }
}

/// Assignment references are evaluated before reading a property/iterator value.
/// Keeping the object and canonical key in temporaries also survives defaults
/// that mutate the expression's source bindings.
enum PreparedTarget {
    Member(u32, u32),
}

fn emit_prepare_target(cx: &mut Cx<'_>, target: &Pat) -> Option<PreparedTarget> {
    if let Pat::Assign(a) = target {
        return emit_prepare_target(cx, &a.left);
    }
    if let Pat::Expr(e) = target {
        if let Expr::Paren(p) = &**e {
            return emit_prepare_target(cx, &Pat::Expr(p.expr.clone()));
        }
        if let Expr::Member(m) = &**e {
            let object = cx.alloc_temp();
            let key = cx.alloc_temp();
            emit_expr(cx, &m.obj);
            cx.emit(Instr::StoreLocal(object));
            cx.emit(Instr::Pop);
            emit_assignment_member_key(cx, &m.prop);
            cx.emit(Instr::StoreLocal(key));
            cx.emit(Instr::Pop);
            return Some(PreparedTarget::Member(object, key));
        }
    }
    None
}

fn emit_prepared_target(cx: &mut Cx<'_>, target: &Pat, reference: Option<PreparedTarget>) {
    if let Some(reference) = reference {
        if let Pat::Assign(a) = target {
            emit_value_default(cx, &a.right);
        }
        let value = cx.alloc_temp();
        cx.emit(Instr::StoreLocal(value));
        cx.emit(Instr::Pop);
        match reference {
            PreparedTarget::Member(object, key) => {
                cx.emit(Instr::LoadLocal(object));
                cx.emit(Instr::LoadLocal(key));
                cx.emit(Instr::LoadLocal(value));
                cx.emit(Instr::SetProp);
                cx.free_temp();
                cx.free_temp();
            }
        }
        cx.emit(Instr::Pop);
        cx.free_temp();
    } else {
        emit_bind_target(cx, target);
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
                let reference = emit_prepare_target(cx, p);
                emit_arr_next(cx, it, done);
                emit_prepared_target(cx, p, reference);
            }
        }
    }
    if cx.bailed() {
        return;
    }

    // Normal completion: drop the handler, then close iff not exhausted.
    cx.emit(Instr::PopHandler);
    cx.handler_depth -= 1;
    // IteratorClose receives this destructuring's normal completion, rather
    // than any saved throw from an enclosing source finally. If close throws,
    // the next enclosing handler's pending-stack snapshot removes this entry;
    // normal close consumes it immediately through EndFinally.
    cx.emit(Instr::BeginFinally);
    emit_arr_close_if_open(cx, it, done);
    cx.emit(Instr::EndFinally);
    let exit_j = cx.code.len();
    cx.emit(Instr::Jump(u32::MAX));

    // CLOSE: abrupt path — close iff not exhausted, then resume the completion.
    let close_pc = cx.here();
    patch_handler_fin(cx, ph, close_pc);
    cx.emit(Instr::BeginFinally);
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
    cx.emit(Instr::LoadLocal(done));
    let to_step = cx.code.len();
    cx.emit(Instr::JumpIfFalse(u32::MAX));
    cx.emit(Instr::PushUndef);
    let exhausted = cx.code.len();
    cx.emit(Instr::Jump(u32::MAX));
    patch(cx, to_step, cx.here());
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
    patch(cx, exhausted, cx.here());
}

/// Advance `it` one step discarding the value (an array elision/hole), setting
/// `done` on exhaustion. Stack-neutral.
pub(crate) fn emit_arr_skip(cx: &mut Cx<'_>, it: u32, done: u32) {
    cx.emit(Instr::LoadLocal(done));
    let to_step = cx.code.len();
    cx.emit(Instr::JumpIfFalse(u32::MAX));
    let exhausted = cx.code.len();
    cx.emit(Instr::Jump(u32::MAX));
    patch(cx, to_step, cx.here());
    cx.emit(Instr::LoadLocal(it));
    cx.emit(Instr::IterElide);
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
    patch(cx, exhausted, cx.here());
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
/// array to the rest target. An exhausted source is never stepped again.
pub(crate) fn emit_arr_rest(cx: &mut Cx<'_>, it: u32, done: u32, rest_target: &Pat) {
    let reference = emit_prepare_target(cx, rest_target);
    cx.emit(Instr::MakeArray(0));
    let arr = cx.alloc_temp();
    cx.emit(Instr::StoreLocal(arr));
    cx.emit(Instr::Pop);
    cx.emit(Instr::LoadLocal(done));
    let to_loop = cx.code.len();
    cx.emit(Instr::JumpIfFalse(u32::MAX));
    let already_done = cx.code.len();
    cx.emit(Instr::Jump(u32::MAX));
    let loop_pc = cx.here();
    patch(cx, to_loop, loop_pc);
    cx.emit(Instr::LoadLocal(arr));
    cx.emit(Instr::LoadLocal(it));
    cx.emit(Instr::IterStep);
    let to_done = cx.code.len();
    cx.emit(Instr::JumpIfFalse(u32::MAX));
    cx.emit(Instr::ArrayAppend);
    cx.emit(Instr::Pop);
    cx.emit(Instr::Jump(loop_pc));
    patch(cx, to_done, cx.here());
    cx.emit(Instr::Pop); // the accumulator beneath the exhausted step
    let t = cx.const_bool(true);
    cx.emit(Instr::PushConst(t));
    cx.emit(Instr::StoreLocal(done));
    cx.emit(Instr::Pop);
    patch(cx, already_done, cx.here());
    cx.emit(Instr::LoadLocal(arr));
    cx.free_temp();
    emit_prepared_target(cx, rest_target, reference);
}

/// Build an array from a sequence of call args / array-literal elements, lowering
/// each spread (`...x`) via the iterator protocol so any iterable is honored
/// (design "Spread/rest"). Leaves the built array on the stack. Used for array
/// literals with spread and for the args array of spread calls / `new`.
pub(crate) fn emit_spread_array(cx: &mut Cx<'_>, elems: &[ExprOrSpread]) {
    cx.emit(Instr::MakeArray(0));
    for e in elems {
        emit_expr(cx, &e.expr);
        if cx.bailed() {
            return;
        }
        cx.emit(if e.spread.is_some() {
            Instr::ArraySpread
        } else {
            Instr::ArrayAppend
        });
    }
}

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
            let value = assignment_target::unparen(&kv.value);
            let infer_name = matches!(value, Expr::Fn(f) if f.ident.is_none())
                || matches!(value, Expr::Arrow(_));
            let key = if infer_name {
                let key = cx.alloc_temp();
                cx.emit(Instr::StoreLocal(key));
                Some(key)
            } else {
                None
            };
            cx.pending_fn_name = static_prop_key_name(&kv.key);
            emit_expr(cx, &kv.value);
            cx.pending_fn_name = None;
            if let Some(key) = key {
                cx.emit(Instr::LoadLocal(key));
                cx.emit(Instr::SetFunctionName);
                cx.free_temp();
            }
        }
        Prop::Shorthand(ident) => {
            // Shorthand names always define data properties, including __proto__.
            let ci = cx.const_str(ident.sym.to_string());
            cx.emit(Instr::PushConst(ci));
            emit_expr(cx, &Expr::Ident(ident.clone()));
        }
        _ => cx.bail(),
    }
}

/// Build each property on the actual home object in source order. Descriptor
/// operations preserve accessors and CopyProps implements spread's data copy.
pub(crate) fn emit_object_spread(cx: &mut Cx<'_>, o: &ObjectLit) {
    cx.emit(Instr::MakeObject(0));
    struct UsesSuper<'a> {
        found: bool,
        contexts: Option<&'a crate::eval::EvalClassContexts>,
    }
    impl Visit for UsesSuper<'_> {
        fn visit_function(&mut self, _: &Function) {}
        fn visit_class(&mut self, _: &Class) {}
        fn visit_bin_expr(&mut self, expression: &BinExpr) {
            mangler_jsast::deep::walk_binary(expression, self);
        }
        fn visit_super_prop_expr(&mut self, _: &SuperPropExpr) {
            self.found = true;
        }
        fn visit_call_expr(&mut self, call: &CallExpr) {
            self.found |= self
                .contexts
                .and_then(|contexts| contexts.get(&call.span.lo.0))
                .is_some_and(|context| context.allow_super_property)
                || matches!(&call.callee, Callee::Expr(callee)
                    if matches!(&**callee, Expr::Ident(ident)
                        if ident.sym == *"\0mangler_object_super_provider"));
            call.visit_children_with(self);
        }
    }
    let mut uses_super = UsesSuper {
        found: false,
        contexts: cx.opts.eval_class_contexts,
    };
    // Only method activations acquire this literal as their home. A provider
    // call in an ordinary property value uses an enclosing method's home; giving
    // that value its own home would shadow the captured home slot.
    for property in &o.props {
        if let PropOrSpread::Prop(property) = property {
            let function = match &**property {
                Prop::Method(method) => Some(&method.function),
                Prop::Getter(getter) => Some(&getter.function),
                Prop::Setter(setter) => Some(&setter.function),
                _ => None,
            };
            if let Some(function) = function {
                function.visit_children_with(&mut uses_super);
            }
        }
    }
    let home = uses_super.found.then(|| cx.alloc_temp());
    let home_name = format!("\0mangler_home_{}", home.unwrap_or(0));
    if let Some(home) = home {
        // A fresh descriptor lets methods retain their home when this scratch
        // slot is reused or the literal executes again in a loop.
        cx.emit(Instr::BeginLexical(home * 2));
        cx.emit(Instr::InitLocal(home));
        cx.scopes.push(HashMap::from([(home_name.clone(), home)]));
    }
    for prop in &o.props {
        match prop {
            PropOrSpread::Spread(s) => {
                emit_expr(cx, &s.expr);
                cx.emit(Instr::CopyProps);
            }
            PropOrSpread::Prop(p) => match &**p {
                Prop::KeyValue(kv)
                    if static_prop_key_name(&kv.key).as_deref() == Some("__proto__") =>
                {
                    emit_expr(cx, &kv.value);
                    cx.emit(Instr::SetPrototype);
                }
                Prop::Getter(g) => {
                    emit_prop_key(cx, &g.key);
                    if let Some(body) = &g.function.body {
                        let body = super::object_super::lower(
                            body,
                            &home_name,
                            cx.opts.strict || has_use_strict_directive_block(body),
                        );
                        emit_nested_closure(cx, &[], &body, false, false, false, None);
                    } else {
                        cx.bail_with("type_only_object_member");
                        return;
                    }
                    cx.emit(Instr::DefineGetter);
                }
                Prop::Setter(s) => {
                    emit_prop_key(cx, &s.key);
                    if let Some(body) = &s.function.body {
                        let params = super::object_super::lower_params(
                            &s.function
                                .params
                                .iter()
                                .map(|parameter| parameter.pat.clone())
                                .collect::<Vec<_>>(),
                            &home_name,
                            cx.opts.strict || has_use_strict_directive_block(body),
                        );
                        let body = super::object_super::lower(
                            body,
                            &home_name,
                            cx.opts.strict || has_use_strict_directive_block(body),
                        );
                        emit_nested_closure(cx, &params, &body, false, false, false, None);
                    } else {
                        cx.bail_with("type_only_object_member");
                        return;
                    }
                    cx.emit(Instr::DefineSetter);
                }
                Prop::Method(m) => {
                    emit_prop_key(cx, &m.key);
                    let params: Vec<Pat> =
                        m.function.params.iter().map(|p| p.pat.clone()).collect();
                    if let Some(body) = &m.function.body {
                        let params = super::object_super::lower_params(
                            &params,
                            &home_name,
                            cx.opts.strict || has_use_strict_directive_block(body),
                        );
                        let body = super::object_super::lower(
                            body,
                            &home_name,
                            cx.opts.strict || has_use_strict_directive_block(body),
                        );
                        emit_nested_closure(
                            cx,
                            &params,
                            &body,
                            false,
                            m.function.is_async,
                            m.function.is_generator,
                            None,
                        );
                    } else {
                        cx.bail_with("type_only_object_member");
                        return;
                    }
                    cx.emit(Instr::DefineMethod);
                }
                _ => {
                    emit_object_entry(cx, p);
                    cx.emit(Instr::DefineData);
                }
            },
        }
        if cx.bailed() {
            return;
        }
    }
    if let Some(home) = home {
        cx.scopes.pop();
        // Detach the captured home value before returning this slot to the pool.
        cx.emit(Instr::BeginLexical(home * 2));
        cx.emit(Instr::PushUndef);
        cx.emit(Instr::InitLocal(home));
        cx.emit(Instr::Pop);
        cx.free_temp();
    }
}

pub(crate) fn emit_assign(cx: &mut Cx<'_>, a: &AssignExpr) {
    // Even inside a binding key/default, an assignment expression performs a
    // write, never initialization of an otherwise uninitialized lexical slot.
    let initializing = cx.initializing;
    cx.initializing = false;
    emit_assign_inner(cx, a);
    cx.initializing = initializing;
}

fn emit_assign_inner(cx: &mut Cx<'_>, a: &AssignExpr) {
    use mangler_jsast::assignment_target::{self, Reference};
    let target = assignment_target::reference(&a.left);
    let inferred_name = assignment_target::inferred_name(&a.left);
    if let Reference::Call(call) = target {
        super::expr::emit_call_assignment_target(cx, call);
        return;
    }
    if a.op == AssignOp::Assign {
        match target {
            Reference::Ident(id) => {
                let name = id.sym.as_ref();
                if binding_needs_ref(cx, name) {
                    emit_binding_ref(cx, name);
                    cx.emit(Instr::ResolveRef);
                    cx.pending_fn_name = inferred_name.map(str::to_owned);
                    emit_expr(cx, &a.right);
                    cx.emit(Instr::PutRef);
                    return;
                }
                // D1: a BOXED capture is writable through its cell; an unboxed
                // capture is read-only -> bail.
                let boxed = cx.is_celled(name);
                if !cx.is_param_or_local(name) && !boxed && !cx.opts.live_captures {
                    // Writing to a captured outer binding: the VM frame write is not
                    // propagated back to the enclosing scope. Skip to stay sound
                    // (read-only capture only).
                    cx.bail_with("mutable_capture");
                    return;
                }
                let slot = cx.resolve(name);
                cx.pending_fn_name = inferred_name.map(str::to_owned);
                emit_expr(cx, &a.right);
                cx.emit(if boxed {
                    Instr::StoreCell(slot)
                } else {
                    Instr::StoreLocal(slot)
                });
            }
            Reference::Member(m) => {
                emit_expr(cx, &m.obj);
                emit_assignment_member_key(cx, &m.prop);
                if cx.bailed() {
                    return;
                }
                // Assignment to a property does not request NamedEvaluation.
                // Suppress inference before evaluation, including class statics.
                cx.pending_fn_name = None;
                emit_expr(cx, &a.right);
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
            Reference::Object(obj) => {
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
            Reference::Array(arr) => {
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
        let (slot, boxed, member, reference) = match target {
            Reference::Ident(id) if binding_needs_ref(cx, id.sym.as_ref()) => {
                let reference = cx.alloc_temp();
                emit_binding_ref(cx, id.sym.as_ref());
                cx.emit(Instr::ResolveRef);
                cx.emit(Instr::StoreLocal(reference));
                cx.emit(Instr::GetRef);
                (0, false, None, Some(reference))
            }
            Reference::Ident(id) => {
                let Some((slot, boxed)) = resolve_writable(cx, id.sym.as_ref()) else {
                    return;
                };
                cx.emit(if boxed {
                    Instr::LoadCell(slot)
                } else {
                    Instr::LoadLocal(slot)
                });
                (slot, boxed, None, None)
            }
            Reference::Member(m) => {
                let object = cx.alloc_temp();
                let key = cx.alloc_temp();
                emit_expr(cx, &m.obj);
                cx.emit(Instr::StoreLocal(object));
                cx.emit(Instr::Pop);
                emit_assignment_member_key(cx, &m.prop);
                cx.emit(Instr::StoreLocal(key));
                cx.emit(Instr::Pop);
                cx.emit(Instr::LoadLocal(object));
                cx.emit(Instr::LoadLocal(key));
                cx.emit(Instr::GetProp);
                (0, false, Some((object, key)), None)
            }
            _ => {
                cx.bail_with("assignment_target");
                return;
            }
        };
        let mut short_circuit = Vec::new();
        match a.op {
            AssignOp::AndAssign => {
                cx.emit(Instr::Dup);
                short_circuit.push(cx.code.len());
                cx.emit(Instr::JumpIfFalse(u32::MAX));
                cx.emit(Instr::Pop);
                cx.pending_fn_name = inferred_name.map(str::to_owned);
                emit_expr(cx, &a.right);
            }
            AssignOp::OrAssign => {
                cx.emit(Instr::Dup);
                let rhs = cx.code.len();
                cx.emit(Instr::JumpIfFalse(u32::MAX));
                short_circuit.push(cx.code.len());
                cx.emit(Instr::Jump(u32::MAX));
                patch(cx, rhs, cx.here());
                cx.emit(Instr::Pop);
                cx.pending_fn_name = inferred_name.map(str::to_owned);
                emit_expr(cx, &a.right);
            }
            AssignOp::NullishAssign => {
                cx.emit(Instr::Dup);
                cx.emit(Instr::PushNull);
                cx.emit(Instr::Bin(7));
                let check_undefined = cx.code.len();
                cx.emit(Instr::JumpIfFalse(u32::MAX));
                let rhs = cx.code.len();
                cx.emit(Instr::Jump(u32::MAX));
                patch(cx, check_undefined, cx.here());
                cx.emit(Instr::Dup);
                cx.emit(Instr::PushUndef);
                cx.emit(Instr::Bin(7));
                short_circuit.push(cx.code.len());
                cx.emit(Instr::JumpIfFalse(u32::MAX));
                patch(cx, rhs, cx.here());
                cx.emit(Instr::Pop);
                cx.pending_fn_name = inferred_name.map(str::to_owned);
                emit_expr(cx, &a.right);
            }
            _ => {
                let Some(code) = compound_op_code(a.op) else {
                    cx.bail_with("assignment_operator");
                    return;
                };
                emit_expr(cx, &a.right);
                cx.emit(Instr::Bin(code));
            }
        }
        if let Some(reference) = reference {
            let value = cx.alloc_temp();
            cx.emit(Instr::StoreLocal(value));
            cx.emit(Instr::Pop);
            cx.emit(Instr::LoadLocal(reference));
            cx.emit(Instr::LoadLocal(value));
            cx.emit(Instr::PutRef);
            cx.free_temp();
            cx.free_temp();
        } else if let Some((object, key)) = member {
            let value = cx.alloc_temp();
            cx.emit(Instr::StoreLocal(value));
            cx.emit(Instr::Pop);
            cx.emit(Instr::LoadLocal(object));
            cx.emit(Instr::LoadLocal(key));
            cx.emit(Instr::LoadLocal(value));
            cx.emit(Instr::SetProp);
            cx.free_temp();
            cx.free_temp();
            cx.free_temp();
        } else {
            cx.emit(if boxed {
                Instr::StoreCell(slot)
            } else {
                Instr::StoreLocal(slot)
            });
        }
        for jump in short_circuit {
            patch(cx, jump, cx.here());
        }
    }
}

pub(crate) fn emit_call(cx: &mut Cx<'_>, c: &CallExpr) {
    if super::environment::emit_suspended_eval_marker(cx, c) {
        return;
    }
    if super::projection::emit_projected_reference(cx, c) {
        return;
    }
    let callee = match &c.callee {
        Callee::Expr(e) => &**e,
        _ => {
            cx.bail();
            return;
        }
    };
    let mut callee = callee;
    while let Expr::Paren(p) = callee {
        callee = &p.expr;
    }
    // Pattern suspension helpers share the source execution context, including
    // direct eval's receiver, while owning only temporary iterator state.
    if c.args.is_empty()
        && let Expr::Fn(function) = callee
        && mangler_jsast::span::is_suspension_entry_span(function.function.span)
    {
        cx.emit(Instr::PushThis);
        emit_expr(cx, callee);
        cx.emit(Instr::CallResolved(0));
        return;
    }
    // A suspension lexical alias plan also identifies an originally direct
    // eval whose callee is now the projected value of a lexical cell.
    let source_direct_eval = cx
        .opts
        .suspension_lexicals
        .is_some_and(|scopes| scopes.contains_key(&c.span.lo.0));
    if source_direct_eval || mangler_jsast::analysis::scope::is_direct_eval_callee(&c.callee) {
        super::environment::emit_eval_reference(cx, callee);
        emit_spread_array(cx, &c.args);
        let environment = super::environment::eval_environment_snapshot(cx, c.span.lo.0);
        cx.emit(Instr::EvalCall(environment));
        return;
    }
    if let Expr::Ident(id) = callee {
        if id.sym == *"\0mangler_object_super_provider" {
            let [mode, home, receiver] = c.args.as_slice() else {
                unreachable!("object super provider operands")
            };
            let Expr::Lit(Lit::Num(mode)) = &*mode.expr else {
                unreachable!("object super provider strictness")
            };
            emit_expr(cx, &home.expr);
            emit_expr(cx, &receiver.expr);
            cx.emit(Instr::MakeSuperProvider(mode.value as u32));
            return;
        }
        let super_op = match id.sym.as_ref() {
            "\0mangler_super_assign" => Some(false),
            "\0mangler_super_update" => Some(true),
            _ => None,
        };
        if let Some(update) = super_op {
            let Some(ExprOrSpread { expr, .. }) = c.args.first() else {
                unreachable!("super bridge mode")
            };
            let Expr::Lit(Lit::Num(mode)) = &**expr else {
                unreachable!("super bridge literal mode")
            };
            for arg in c.args.iter().skip(1) {
                emit_expr(cx, &arg.expr);
            }
            cx.emit(if update {
                Instr::SuperUpdate(mode.value as u32)
            } else {
                Instr::SuperAssign(mode.value as u32)
            });
            return;
        }
    }
    let has_spread = c.args.iter().any(|a| a.spread.is_some());
    if let Expr::Ident(id) = callee
        && binding_needs_ref(cx, id.sym.as_ref())
    {
        emit_binding_ref(cx, id.sym.as_ref());
        cx.emit(Instr::RefCall);
        if has_spread {
            emit_spread_array(cx, &c.args);
            cx.emit(Instr::CallArray);
        } else {
            for arg in &c.args {
                emit_expr(cx, &arg.expr);
            }
            cx.emit(Instr::CallResolved(c.args.len() as u32));
        }
        return;
    }
    // Parentheses end optional short-circuiting but preserve a member Reference.
    // `(object?.method)(args)` therefore always evaluates args and calls, even
    // when resolving the enclosed chain produced undefined.
    if let Expr::OptChain(oc) = callee
        && let OptChainBase::Member(m) = &*oc.base
    {
        let mut jumps = Vec::new();
        emit_opt_chain_node(cx, &m.obj, &mut jumps);
        if oc.optional {
            emit_oc_guard(cx, &mut jumps);
        }
        cx.emit(Instr::Dup);
        emit_member_key(cx, &m.prop);
        cx.emit(Instr::GetProp);
        let ready = cx.code.len();
        cx.emit(Instr::Jump(u32::MAX));
        for jump in jumps {
            patch(cx, jump, cx.here());
        }
        cx.emit(Instr::PushUndef);
        patch(cx, ready, cx.here());
        if has_spread {
            emit_spread_array(cx, &c.args);
            cx.emit(Instr::CallArray);
        } else {
            for arg in &c.args {
                emit_expr(cx, &arg.expr);
            }
            cx.emit(Instr::CallResolved(c.args.len() as u32));
        }
        return;
    }
    if let Expr::Member(m) = callee {
        if has_spread {
            emit_expr(cx, &m.obj);
            cx.emit(Instr::Dup);
            emit_member_key(cx, &m.prop);
            cx.emit(Instr::GetProp);
            emit_spread_array(cx, &c.args);
            cx.emit(Instr::CallArray);
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
        cx.emit(Instr::PushUndef);
        emit_expr(cx, callee);
        emit_spread_array(cx, &c.args);
        cx.emit(Instr::CallArray);
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
