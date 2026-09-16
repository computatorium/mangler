//! Expression-family compiler: literals, idents, binops (and `&&`/`||`/`??`),
//! unary, member/optional-chaining, calls/new, arrays/objects (incl. spread),
//! templates + tagged templates, assignment, and closures (via `super::emit_*`).
//!
//! Every `emit_*` here takes `&mut Cx` and shares the frame model and the other
//! construct-family emitters (`stmt`, `destructure`) via `use super::*`.

use super::*;
use crate::isa::{Instr, bin_op_code, un_op_code};
use mangler_jsast::assignment_target::unparen;

pub(crate) fn emit_expr(cx: &mut Cx<'_>, expr: &Expr) {
    if cx.bailed() {
        return;
    }
    // §4.3: a binding name (`const render = …`, `render = …`, `{render: …}`)
    // applies when the value is a function/arrow expression, allowing grouping.
    // A closure nested in another expression does not inherit that name.
    let pending_name = cx.pending_fn_name.take();
    if matches!(unparen(expr), Expr::Fn(_) | Expr::Arrow(_)) {
        cx.pending_fn_name = pending_name;
    }
    match expr {
        Expr::Fn(fe) => {
            // A (possibly named) function expression: `var g = function f(){…}`.
            // `f` (if present) is in scope inside its own body — threaded as the
            // SELF upvalue so recursion works without the closure existing yet.
            if fe.function.body.is_none() {
                cx.bail();
                return;
            }
            let body = fe.function.body.as_ref().unwrap();
            let self_name = fe.ident.as_ref().map(|i| i.sym.to_string());
            let pats: Vec<Pat> = fe.function.params.iter().map(|p| p.pat.clone()).collect();
            emit_nested_closure(
                cx,
                &pats,
                body,
                false,
                fe.function.is_async,
                fe.function.is_generator,
                self_name.as_deref(),
            );
        }
        Expr::Arrow(ar) => match &*ar.body {
            ArrowFunctionBody::FunctionBody(body) => emit_nested_closure(
                cx,
                &ar.params,
                body,
                true,
                ar.is_async,
                ar.is_generator,
                None,
            ),
            ArrowFunctionBody::Expr(e) => {
                // Expression-bodied arrow `(a)=>expr`: wrap as `{ return expr; }` so
                // the shared block compiler handles it.
                let wrapped = FunctionBody {
                    span: swc_core::common::DUMMY_SP,
                    stmts: vec![Stmt::Return(ReturnStmt {
                        span: swc_core::common::DUMMY_SP,
                        arg: Some(e.clone()),
                    })],
                    ..Default::default()
                };
                emit_nested_closure(
                    cx,
                    &ar.params,
                    &wrapped,
                    true,
                    ar.is_async,
                    ar.is_generator,
                    None,
                );
            }
        },
        Expr::Lit(Lit::Num(n)) => {
            let ci = cx.const_num(n.value);
            cx.emit(Instr::PushConst(ci));
        }
        Expr::Lit(Lit::Str(s)) => emit_string(cx, &s.value),
        Expr::Lit(Lit::BigInt(n)) => {
            let ci = cx.consts.len() as u32;
            cx.consts.push(Const::BigInt(n.value.to_string()));
            cx.emit(Instr::PushConst(ci));
        }
        Expr::Lit(Lit::Regex(r)) => {
            let ci = cx.consts.len() as u32;
            cx.consts.push(Const::RegExp {
                pattern: r.exp.to_string(),
                flags: r.flags.to_string(),
            });
            cx.emit(Instr::NewRegExp(ci));
        }
        Expr::Lit(Lit::Bool(b)) => {
            let ci = cx.const_bool(b.value);
            cx.emit(Instr::PushConst(ci));
        }
        Expr::Lit(Lit::Null(_)) => {
            cx.emit(Instr::PushNull);
        }
        Expr::Lit(_) => cx.bail_with("non_javascript_literal"),
        Expr::Ident(id) => {
            let name = id.sym.as_ref();
            if binding_has_with(cx, name) || binding_has_environment(cx, name) {
                emit_binding_ref(cx, name);
                cx.emit(Instr::GetRef);
                return;
            }
            // D1: a boxed mutable capture reads through its cell (`L[slot][0]`); a
            // plain local/param/read-only-capture reads directly (`L[slot]`).
            let boxed = cx.is_celled(name);
            let slot = cx.resolve(name);
            cx.emit(if boxed {
                Instr::LoadCell(slot)
            } else {
                Instr::LoadLocal(slot)
            });
        }
        Expr::Paren(_) | Expr::Bin(_) | Expr::Seq(_) => emit_expression_tree(cx, expr),
        Expr::Unary(u) => {
            if matches!(u.op, UnaryOp::Delete) {
                emit_delete(cx, &u.arg);
                return;
            }
            if matches!(u.op, UnaryOp::TypeOf)
                && let Expr::Ident(id) = unparen(&u.arg)
                && binding_needs_ref(cx, id.sym.as_ref())
            {
                emit_binding_ref(cx, id.sym.as_ref());
                cx.emit(Instr::TypeOfRef);
                return;
            }
            // An unresolved reference is legal for typeof. Ask the source lexical
            // environment before any capture getter is evaluated; ordinary reads
            // would throw and cannot distinguish an absent binding from its TDZ.
            if matches!(u.op, UnaryOp::TypeOf)
                && let Expr::Ident(id) = unparen(&u.arg)
                && !cx.is_param_or_local(id.sym.as_ref())
                && !cx.is_celled(id.sym.as_ref())
            {
                if !cx.opts.live_captures {
                    cx.bail_with("typeof_unresolved_binding");
                    return;
                }
                let slot = cx.resolve(id.sym.as_ref());
                cx.emit(Instr::TypeOfBinding(slot));
                return;
            }
            match un_op_code(u.op) {
                Some(code) => {
                    emit_expr(cx, &u.arg);
                    cx.emit(Instr::Un(code));
                }
                None => cx.bail(),
            }
        }
        Expr::Assign(a) => emit_assign(cx, a),
        Expr::Update(u) => emit_update(cx, u),
        Expr::Member(m) => {
            emit_expr(cx, &m.obj);
            emit_member_key(cx, &m.prop);
            if cx.bailed() {
                return;
            }
            cx.emit(Instr::GetProp);
        }
        Expr::Cond(c) => {
            emit_expr(cx, &c.test);
            let l_else = cx.code.len();
            cx.emit(Instr::JumpIfFalse(u32::MAX));
            emit_expr(cx, &c.cons);
            let l_end = cx.code.len();
            cx.emit(Instr::Jump(u32::MAX));
            patch(cx, l_else, cx.here());
            emit_expr(cx, &c.alt);
            patch(cx, l_end, cx.here());
        }
        Expr::Call(c) if matches!(c.callee, Callee::Import(_)) => {
            let ci = cx.consts.len() as u32;
            let primitive = if c.args.len() == 1 {
                "function(s){return import(s)}"
            } else {
                "function(s,o){return import(s,o)}"
            };
            cx.consts.push(Const::NativeFactory(primitive.into()));
            cx.emit(Instr::PushConst(ci));
            for arg in &c.args {
                emit_expr(cx, &arg.expr);
            }
            cx.emit(Instr::Call(c.args.len() as u32));
        }
        Expr::Call(c) => emit_call(cx, c),
        Expr::MetaProp(meta) if matches!(meta.kind, MetaPropKind::NewTarget) => {
            cx.emit(Instr::PushNewTarget);
        }
        Expr::MetaProp(meta) if matches!(meta.kind, MetaPropKind::ImportMeta) => {
            // The accessor is emitted in the source module, retaining that module's
            // host-supplied metadata and import resolution context.
            let ci = cx.consts.len() as u32;
            cx.consts
                .push(Const::NativeFactory("()=>import.meta".into()));
            cx.emit(Instr::PushConst(ci));
            cx.emit(Instr::Call(0));
        }
        Expr::New(n) => {
            let has_spread = n
                .args
                .as_ref()
                .is_some_and(|a| a.iter().any(|x| x.spread.is_some()));
            if has_spread {
                emit_expr(cx, &n.callee);
                emit_spread_array(cx, n.args.as_deref().unwrap_or(&[]));
                cx.emit(Instr::NewArray);
            } else {
                emit_expr(cx, &n.callee);
                let mut argc = 0u32;
                if let Some(args) = &n.args {
                    for a in args {
                        emit_expr(cx, &a.expr);
                        argc += 1;
                    }
                }
                cx.emit(Instr::New(argc));
            }
        }
        Expr::Array(arr) => {
            if arr
                .elems
                .iter()
                .all(|e| e.as_ref().is_some_and(|e| e.spread.is_none()))
            {
                for elem in arr.elems.iter().flatten() {
                    emit_expr(cx, &elem.expr);
                }
                cx.emit(Instr::MakeArray(arr.elems.len() as u32));
                return;
            }
            cx.emit(Instr::MakeArray(0));
            for elem in &arr.elems {
                match elem {
                    Some(e) => {
                        emit_expr(cx, &e.expr);
                        cx.emit(if e.spread.is_some() {
                            Instr::ArraySpread
                        } else {
                            Instr::ArrayAppend
                        });
                    }
                    None => cx.emit(Instr::ArrayHole),
                }
            }
        }
        Expr::Object(o) => emit_object_spread(cx, o),
        Expr::Tpl(t) => emit_template(cx, t),
        Expr::TaggedTpl(t) => emit_tagged_template(cx, t),
        Expr::OptChain(oc) => emit_opt_chain(cx, oc),
        Expr::This(_) => cx.emit(Instr::PushThis),
        _ => cx.bail(),
    }
}

/// Lower an (untagged) template literal `q0${e0}q1${e1}q2` to
/// `q0 + String(e0) + q1 + String(e1) + q2`. Each interpolation is wrapped in
/// the `ToString` unop (Un(6)) so object coercion matches the spec ToString
/// rather than `+`'s ToPrimitive-default — e.g. an object with asymmetric
/// `valueOf`/`toString` stringifies via `toString` here, as a template requires.
/// Empty quasis are dropped; an all-empty template yields `""`.
///
pub(crate) fn emit_template(cx: &mut Cx<'_>, t: &Tpl) {
    // `count` tracks un-folded operands currently on the stack (0, 1, or
    // transiently 2 → folded back to 1 with Add).
    let mut count = 0u32;
    let n = t.exprs.len();
    for i in 0..=n {
        let q = &t.quasis[i];
        let Some(cooked) = &q.cooked else {
            cx.bail_with("invalid_untagged_template");
            return;
        };
        if !cooked.is_empty() {
            emit_string(cx, cooked);
            count += 1;
            if count == 2 {
                cx.emit(Instr::Bin(0));
                count = 1;
            }
        }
        if i < n {
            emit_expr(cx, &t.exprs[i]);
            if cx.bailed() {
                return;
            }
            cx.emit(Instr::Un(6)); // ToString
            count += 1;
            if count == 2 {
                cx.emit(Instr::Bin(0)); // Add (string concat)
                count = 1;
            }
        }
    }
    if count == 0 {
        // all-empty template (e.g. `` ` ` `` with empty quasis) -> "".
        let ci = cx.const_str(String::new());
        cx.emit(Instr::PushConst(ci));
    }
}

/// Lower a tagged template `tag`q0${e0}q1`` to the call `tag(strings, e0)`.
///
/// `strings` is the spec template object: a frozen array whose elements are the
/// COOKED quasis, carrying a frozen `.raw` array of the RAW quasis. Critically,
/// the SAME object must be reused on every evaluation of this call site
/// (`GetTemplateObject` caches per site) — a `tag` may compare object identity
/// across calls. We achieve this by registering the object as a single const
/// (`Const::TemplateObject`): the interpreter builds it once and caches it in the
/// const slot, so `PushConst` returns the same reference each time.
///
/// The tag callee itself follows the method/plain-call receiver rules: an
/// `obj.tag`…`` member tag is invoked with `obj` as `this` (`CallResolved`); a
/// bare `tag`…`` is a plain call (`Call`). Cooked/raw come verbatim from the AST;
/// a cooked `None` (invalid escape — legal in a tagged template) becomes a JS
/// `undefined` element. UTF-16 constants preserve lone surrogate code units.
pub(crate) fn emit_tagged_template(cx: &mut Cx<'_>, t: &TaggedTpl) {
    let has_surrogates = t
        .tpl
        .quasis
        .iter()
        .any(|q| q.cooked.as_ref().is_some_and(|c| c.as_str().is_none()));
    let tpl_ci = if has_surrogates {
        let ci = cx.consts.len() as u32;
        cx.consts.push(Const::TemplateObjectUtf16 {
            cooked: t
                .tpl
                .quasis
                .iter()
                .map(|q| q.cooked.as_ref().map(|c| c.to_ill_formed_utf16().collect()))
                .collect(),
            raw: t
                .tpl
                .quasis
                .iter()
                .map(|q| q.raw.encode_utf16().collect())
                .collect(),
        });
        ci
    } else {
        let cooked = t
            .tpl
            .quasis
            .iter()
            .map(|q| q.cooked.as_ref().map(|c| c.as_str().unwrap().to_owned()))
            .collect();
        let raw = t.tpl.quasis.iter().map(|q| q.raw.to_string()).collect();
        cx.const_template(cooked, raw)
    };
    if cx.bailed() {
        return;
    }

    // Suspension has already separated the tag and substitutions into a call.
    // The generated identity literal is just this site's frozen template object.
    if matches!(unparen(&t.tag), Expr::Arrow(arrow) if mangler_jsast::span::is_template_object_span(arrow.span))
    {
        cx.emit(Instr::PushConst(tpl_ci));
        return;
    }

    // Resolve the tag before evaluating substitutions, preserving reference receivers.
    let receiver = if let Expr::Member(m) = unparen(&t.tag) {
        emit_expr(cx, &m.obj);
        cx.emit(Instr::Dup);
        emit_member_key(cx, &m.prop);
        cx.emit(Instr::GetProp);
        true
    } else if let Expr::Ident(id) = unparen(&t.tag)
        && binding_needs_ref(cx, id.sym.as_ref())
    {
        emit_binding_ref(cx, id.sym.as_ref());
        cx.emit(Instr::RefCall);
        true
    } else {
        emit_expr(cx, &t.tag);
        false
    };
    cx.emit(Instr::PushConst(tpl_ci));
    for expr in &t.tpl.exprs {
        emit_expr(cx, expr);
    }
    let argc = t.tpl.exprs.len() as u32 + 1;
    cx.emit(if receiver {
        Instr::CallResolved(argc)
    } else {
        Instr::Call(argc)
    });
}

/// Top-level optional chain `a?.b…` (design §L3). Every OPTIONAL link in the
/// chain short-circuits the ENTIRE chain to `undefined` the moment its base is
/// null/undefined — without evaluating the rest (including any trailing call and
/// its side-effecting arguments). One shared `END` target per top-level chain
/// makes this exact: all the optional-link short-circuit jumps are patched to the
/// single PC just past the chain's final access, where both the
/// short-circuited (`undefined`) and the fully-evaluated paths leave exactly ONE
/// value on the stack.
pub(crate) fn emit_opt_chain(cx: &mut Cx<'_>, oc: &OptChainExpr) {
    let mut sc_jumps: Vec<usize> = Vec::new();
    emit_opt_chain_node(cx, &Expr::OptChain(oc.clone()), &mut sc_jumps);
    if cx.bailed() {
        return;
    }
    // END: the single short-circuit landing PC (one past the whole chain).
    let end = cx.here();
    for j in sc_jumps {
        patch(cx, j, end);
    }
}

/// Emit the nullish short-circuit guard for the value currently on top of the
/// stack: `Dup; Un(IsNullish); JumpIfFalse cont; Pop; PushUndef; Jump END`.
/// Strict checks preserve HTML's `document.all` object. When the
/// value is non-nullish the guard falls through with the (un-Dup'd) value still on
/// the stack ready for the access; when nullish it drops the value, pushes
/// `undefined`, and records a jump to the shared END (patched by `emit_opt_chain`).
///
/// Invariant: the guard must run while the guarded value is the SOLE chain value
/// on the stack (nothing of the chain below it), so the short-circuit leaves a
/// single `undefined` at END. Callers uphold this (e.g. a method-call receiver is
/// guarded BEFORE the `Dup` that builds `[recv, method]`).
pub(crate) fn emit_oc_guard(cx: &mut Cx<'_>, sc_jumps: &mut Vec<usize>) {
    cx.emit(Instr::Dup);
    cx.emit(Instr::Un(crate::isa::UN_IS_NULLISH));
    let cont = cx.code.len();
    cx.emit(Instr::JumpIfFalse(u32::MAX)); // non-nullish -> skip the short-circuit
    cx.emit(Instr::Pop); // drop the nullish base
    cx.emit(Instr::PushUndef);
    let j = cx.code.len();
    cx.emit(Instr::Jump(u32::MAX)); // -> shared END
    sc_jumps.push(j);
    patch(cx, cont, cx.here()); // cont:
}

/// Recursively emit one link of an optional chain, leaving the link's VALUE on the
/// stack. The chain is a left-leaning nesting: each link's `obj`/`callee` is the
/// rest of the chain to its left. We recurse left-first (emitting the inner value
/// first), then apply this link's access. Optional links (`?.`) insert
/// `emit_oc_guard` on their base before the access; non-optional links inside the
/// chain (`.b`, `[k]`, `(args)`) are emitted normally.
pub(crate) fn emit_opt_chain_node(cx: &mut Cx<'_>, e: &Expr, sc_jumps: &mut Vec<usize>) {
    if cx.bailed() {
        return;
    }
    match e {
        // An optional-chain wrapper: the `?.`-ness of THIS link is `oc.optional`;
        // the actual access (member or call) is in `oc.base`.
        Expr::OptChain(oc) => match &*oc.base {
            OptChainBase::Member(m) => {
                emit_oc_member(cx, &m.obj, &m.prop, oc.optional, sc_jumps);
            }
            OptChainBase::Call(call) => {
                emit_oc_call(cx, &call.callee, &call.args, oc.optional, sc_jumps);
            }
        },
        // A non-optional member link written inside the chain (e.g. the `.b` in
        // `a?.a.b`): emit the obj (rest of chain), then a plain GetProp.
        Expr::Member(m) => {
            emit_oc_member(cx, &m.obj, &m.prop, false, sc_jumps);
        }
        // A non-optional call link inside the chain.
        Expr::Call(c) => {
            let callee = match &c.callee {
                Callee::Expr(callee) => callee,
                _ => {
                    cx.bail();
                    return;
                }
            };
            emit_oc_call(cx, callee, &c.args, false, sc_jumps);
        }
        // The chain root (an Ident, literal, etc.) — a leaf with no `?.`/access.
        other => emit_expr(cx, other),
    }
}

/// Emit a member access link `<obj>.<prop>` / `<obj>[<prop>]`. If `optional`, the
/// `?.` short-circuit guard is applied to the obj before the property read.
pub(crate) fn emit_oc_member(
    cx: &mut Cx<'_>,
    obj: &Expr,
    prop: &MemberProp,
    optional: bool,
    sc_jumps: &mut Vec<usize>,
) {
    emit_opt_chain_node(cx, obj, sc_jumps);
    if cx.bailed() {
        return;
    }
    if optional {
        emit_oc_guard(cx, sc_jumps);
    }
    emit_member_key(cx, prop);
    if cx.bailed() {
        return;
    }
    cx.emit(Instr::GetProp);
}

/// Emit a call link inside an optional chain. Mirrors `emit_call`'s receiver
/// handling: when the callee is a member access we keep the receiver under the
/// resolved method (`Dup`/`GetProp`/`CallResolved`, preserving getter-before-args
/// order); otherwise it is a plain `Call`.
///
/// Optional method calls guard the resolved method and discard both the method
/// and receiver on the short-circuit path. Arguments are evaluated only after
/// this guard, and the receiver remains intact on the calling path.
pub(crate) fn emit_oc_call(
    cx: &mut Cx<'_>,
    callee: &Expr,
    args: &[ExprOrSpread],
    call_optional: bool,
    sc_jumps: &mut Vec<usize>,
) {
    if let Expr::Ident(id) = unparen(callee)
        && binding_needs_ref(cx, id.sym.as_ref())
    {
        emit_binding_ref(cx, id.sym.as_ref());
        cx.emit(Instr::RefCall);
        if call_optional {
            emit_oc_call_guard(cx, sc_jumps);
        }
        emit_oc_arguments(cx, args, true);
        return;
    }
    // Identify a method-call callee (optional or plain member access) so the
    // receiver is preserved for `CallResolved`.
    let method: Option<(&Expr, &MemberProp, bool)> = match unparen(callee) {
        Expr::OptChain(oc) => match &*oc.base {
            OptChainBase::Member(m) => Some((&m.obj, &m.prop, oc.optional)),
            OptChainBase::Call(_) => None,
        },
        Expr::Member(m) => Some((&m.obj, &m.prop, false)),
        _ => None,
    };

    match method {
        Some((obj, prop, member_optional)) => {
            // Receiver, guarded BEFORE the Dup so a nullish receiver short-circuits
            // with a single value on the stack.
            emit_opt_chain_node(cx, obj, sc_jumps);
            if cx.bailed() {
                return;
            }
            if member_optional {
                emit_oc_guard(cx, sc_jumps);
            }
            cx.emit(Instr::Dup); // [recv, recv]
            emit_member_key(cx, prop);
            if cx.bailed() {
                return;
            }
            cx.emit(Instr::GetProp); // [recv, method]
            if call_optional {
                emit_oc_call_guard(cx, sc_jumps);
            }
            emit_oc_arguments(cx, args, true);
        }
        None => {
            let spread = args.iter().any(|a| a.spread.is_some());
            emit_opt_chain_node(cx, callee, sc_jumps);
            if cx.bailed() {
                return;
            }
            if call_optional {
                emit_oc_guard(cx, sc_jumps);
            }
            if spread {
                let function = cx.alloc_temp();
                cx.emit(Instr::StoreLocal(function));
                cx.emit(Instr::Pop);
                cx.emit(Instr::PushUndef);
                cx.emit(Instr::LoadLocal(function));
                cx.free_temp();
            }
            emit_oc_arguments(cx, args, spread);
        }
    }
}

/// Member access key for `o.k` / `o[k]`.
pub(crate) fn emit_member_key(cx: &mut Cx<'_>, prop: &MemberProp) {
    match prop {
        MemberProp::Ident(name) => {
            let ci = cx.const_str(name.sym.to_string());
            cx.emit(Instr::PushConst(ci));
        }
        MemberProp::Computed(c) => {
            // Keep the raw reference key: GetProp/DeleteProp must reject a
            // nullish base before performing an observable key conversion.
            emit_expr(cx, &c.expr);
        }
        MemberProp::PrivateName(_) => cx.bail(),
    }
}

/// Object-literal property-name key.
pub(crate) fn emit_prop_key(cx: &mut Cx<'_>, key: &PropName) {
    match key {
        PropName::Ident(name) => {
            let ci = cx.const_str(name.sym.to_string());
            cx.emit(Instr::PushConst(ci));
        }
        PropName::Str(s) => emit_string(cx, &s.value),
        PropName::Num(num) => {
            let ci = cx.const_num(num.value);
            cx.emit(Instr::PushConst(ci));
            cx.emit(Instr::Un(crate::isa::UN_TO_PROPERTY_KEY));
        }
        PropName::Computed(c) => {
            emit_expr(cx, &c.expr);
            cx.emit(Instr::Un(crate::isa::UN_TO_PROPERTY_KEY));
        }
        PropName::BigInt(n) => {
            let ci = cx.const_str(n.value.to_string());
            cx.emit(Instr::PushConst(ci));
        }
    }
}

/// Guard a method while its receiver occupies the preceding stack slot.
fn emit_oc_call_guard(cx: &mut Cx<'_>, sc_jumps: &mut Vec<usize>) {
    cx.emit(Instr::Dup);
    cx.emit(Instr::Un(crate::isa::UN_IS_NULLISH));
    let cont = cx.code.len();
    cx.emit(Instr::JumpIfFalse(u32::MAX));
    cx.emit(Instr::Pop);
    cx.emit(Instr::Pop);
    cx.emit(Instr::PushUndef);
    sc_jumps.push(cx.code.len());
    cx.emit(Instr::Jump(u32::MAX));
    patch(cx, cont, cx.here());
}

fn emit_oc_arguments(cx: &mut Cx<'_>, args: &[ExprOrSpread], receiver: bool) {
    if args.iter().any(|a| a.spread.is_some()) {
        emit_spread_array(cx, args);
        cx.emit(Instr::CallArray);
    } else {
        for arg in args {
            emit_expr(cx, &arg.expr);
        }
        cx.emit(if receiver {
            Instr::CallResolved(args.len() as u32)
        } else {
            Instr::Call(args.len() as u32)
        });
    }
}

fn emit_delete(cx: &mut Cx<'_>, arg: &Expr) {
    match unparen(arg) {
        Expr::Member(m) => {
            emit_expr(cx, &m.obj);
            emit_member_key(cx, &m.prop);
            cx.emit(Instr::DeleteProp);
        }
        Expr::OptChain(oc) => {
            let mut jumps = Vec::new();
            match &*oc.base {
                OptChainBase::Member(m) => {
                    emit_opt_chain_node(cx, &m.obj, &mut jumps);
                    if oc.optional {
                        emit_oc_guard(cx, &mut jumps);
                    }
                    emit_member_key(cx, &m.prop);
                    cx.emit(Instr::DeleteProp);
                }
                OptChainBase::Call(_) => {
                    emit_opt_chain_node(cx, arg, &mut jumps);
                    cx.emit(Instr::Pop);
                    let yes = cx.const_bool(true);
                    cx.emit(Instr::PushConst(yes));
                }
            }
            let end = cx.code.len();
            cx.emit(Instr::Jump(u32::MAX));
            for jump in jumps {
                patch(cx, jump, cx.here());
            }
            cx.emit(Instr::Pop);
            let yes = cx.const_bool(true);
            cx.emit(Instr::PushConst(yes));
            patch(cx, end, cx.here());
        }
        Expr::Ident(id) => {
            if binding_needs_ref(cx, id.sym.as_ref()) {
                emit_binding_ref(cx, id.sym.as_ref());
                cx.emit(Instr::DeleteRef);
                return;
            }
            if cx.is_param_or_local(id.sym.as_ref()) || cx.is_celled(id.sym.as_ref()) {
                let no = cx.const_bool(false);
                cx.emit(Instr::PushConst(no));
            } else if cx.opts.live_captures {
                let slot = cx.resolve(id.sym.as_ref());
                cx.emit(Instr::DeleteBinding(slot));
            } else {
                cx.bail_with("delete_global_binding");
            }
        }
        expr => {
            emit_expr(cx, expr);
            cx.emit(Instr::Pop);
            let yes = cx.const_bool(true);
            cx.emit(Instr::PushConst(yes));
        }
    }
}

/// Update expressions convert the old value once and preserve it for postfix
/// results. Member updates keep the raw reference together in an operator opcode,
/// preserving the host engine's property-key coercion and getter/setter behavior.
fn emit_update(cx: &mut Cx<'_>, update: &UpdateExpr) {
    if emit_call_assignment_target(cx, &update.arg) {
        return;
    }
    let op = match update.op {
        UpdateOp::PlusPlus => crate::isa::UN_INCREMENT,
        UpdateOp::MinusMinus => crate::isa::UN_DECREMENT,
    };
    match unparen(&update.arg) {
        Expr::Ident(id) => {
            let name = id.sym.as_ref();
            if binding_needs_ref(cx, name) {
                emit_binding_ref(cx, name);
                let mode = match (update.op, update.prefix) {
                    (UpdateOp::PlusPlus, false) => 0,
                    (UpdateOp::PlusPlus, true) => 1,
                    (UpdateOp::MinusMinus, false) => 2,
                    (UpdateOp::MinusMinus, true) => 3,
                };
                cx.emit(Instr::UpdateRef(mode));
                return;
            }
            let boxed = cx.is_celled(name);
            if !cx.is_param_or_local(name) && !boxed && !cx.opts.live_captures {
                cx.bail_with("mutable_capture");
                return;
            }
            let slot = cx.resolve(name);
            cx.emit(if boxed {
                Instr::LoadCell(slot)
            } else {
                Instr::LoadLocal(slot)
            });
            if !update.prefix {
                cx.emit(Instr::Un(crate::isa::UN_TO_NUMERIC));
                cx.emit(Instr::Dup);
            }
            cx.emit(Instr::Un(op));
            cx.emit(if boxed {
                Instr::StoreCell(slot)
            } else {
                Instr::StoreLocal(slot)
            });
            if !update.prefix {
                cx.emit(Instr::Pop);
            }
        }
        Expr::Member(member) => {
            emit_expr(cx, &member.obj);
            match &member.prop {
                MemberProp::Computed(c) => emit_expr(cx, &c.expr),
                prop => emit_member_key(cx, prop),
            }
            let mode = match (update.op, update.prefix) {
                (UpdateOp::PlusPlus, false) => 0,
                (UpdateOp::PlusPlus, true) => 1,
                (UpdateOp::MinusMinus, false) => 2,
                (UpdateOp::MinusMinus, true) => 3,
            };
            cx.emit(Instr::UpdateProp(mode));
        }
        _ => cx.bail_with("invalid_update_target"),
    }
}

/// Annex B call targets evaluate the call, then throw before reading the RHS or
/// coercing its result. Parsing restricts this extension to its legacy grammar.
pub(crate) fn emit_call_assignment_target(cx: &mut Cx<'_>, target: &Expr) -> bool {
    if !matches!(unparen(target), Expr::Call(_)) {
        return false;
    }
    emit_expr(cx, target);
    cx.emit(Instr::Pop);
    cx.emit(Instr::ThrowReferenceError);
    true
}

fn emit_string(cx: &mut Cx<'_>, value: &swc_core::atoms::Wtf8Atom) {
    let ci = match value.as_str() {
        Some(s) => cx.const_str(s.to_owned()),
        None => {
            let ci = cx.consts.len() as u32;
            cx.consts
                .push(Const::Utf16(value.to_ill_formed_utf16().collect()));
            ci
        }
    };
    cx.emit(Instr::PushConst(ci));
}

/// Keep expression spines on a heap worklist. Generated JavaScript routinely has
/// thousands of left-associated operators; compiler stack usage must not grow
/// with that depth. Continuations retain short-circuit and operand ordering.
fn emit_expression_tree(cx: &mut Cx<'_>, root: &Expr) {
    enum Task<'a> {
        Eval(&'a Expr),
        Binary(u8),
        Logical(BinaryOp, &'a Expr),
        Pop,
        End(usize),
    }
    let mut tasks = vec![Task::Eval(root)];
    while let Some(task) = tasks.pop() {
        if cx.bailed() {
            return;
        }
        match task {
            Task::Eval(Expr::Paren(p)) => tasks.push(Task::Eval(&p.expr)),
            Task::Eval(Expr::Seq(seq)) => {
                if seq.exprs.is_empty() {
                    cx.bail_with("empty_sequence");
                    return;
                }
                for (i, expr) in seq.exprs.iter().enumerate().rev() {
                    if i + 1 < seq.exprs.len() {
                        tasks.push(Task::Pop);
                    }
                    tasks.push(Task::Eval(expr));
                }
            }
            Task::Eval(Expr::Bin(bin)) => {
                if matches!(
                    bin.op,
                    BinaryOp::LogicalAnd | BinaryOp::LogicalOr | BinaryOp::NullishCoalescing
                ) {
                    tasks.push(Task::Logical(bin.op, &bin.right));
                } else if let Some(op) = bin_op_code(bin.op) {
                    tasks.push(Task::Binary(op));
                    tasks.push(Task::Eval(&bin.right));
                } else {
                    cx.bail_with("unknown_binary_operator");
                    return;
                }
                tasks.push(Task::Eval(&bin.left));
            }
            Task::Eval(expr) => emit_expr(cx, expr),
            Task::Binary(op) => cx.emit(Instr::Bin(op)),
            Task::Pop => cx.emit(Instr::Pop),
            Task::End(jump) => patch(cx, jump, cx.here()),
            Task::Logical(op, right) => {
                cx.emit(Instr::Dup);
                if op == BinaryOp::NullishCoalescing {
                    cx.emit(Instr::Un(crate::isa::UN_IS_NULLISH));
                }
                let jump = cx.code.len();
                cx.emit(Instr::JumpIfFalse(u32::MAX));
                let end = if op == BinaryOp::LogicalOr {
                    let end = cx.code.len();
                    cx.emit(Instr::Jump(u32::MAX));
                    patch(cx, jump, cx.here());
                    end
                } else {
                    jump
                };
                cx.emit(Instr::Pop);
                tasks.push(Task::End(end));
                tasks.push(Task::Eval(right));
            }
        }
    }
}
