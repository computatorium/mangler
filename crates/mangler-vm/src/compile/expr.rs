//! Expression-family compiler: literals, idents, binops (and `&&`/`||`/`??`),
//! unary, member/optional-chaining, calls/new, arrays/objects (incl. spread),
//! templates + tagged templates, assignment, and closures (via `super::emit_*`).
//!
//! Every `emit_*` here takes `&mut Cx` and shares the frame model and the other
//! construct-family emitters (`stmt`, `destructure`) via `use super::*`.

use swc_core::ecma::ast::*;

use super::*;
use crate::isa::{bin_op_code, un_op_code, Instr};

pub(crate) fn emit_expr(cx: &mut Cx<'_>, expr: &Expr) {
    if cx.bailed() {
        return;
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
            BlockStmtOrExpr::BlockStmt(body) => emit_nested_closure(
                cx,
                &ar.params,
                body,
                true,
                ar.is_async,
                ar.is_generator,
                None,
            ),
            BlockStmtOrExpr::Expr(e) => {
                // Expression-bodied arrow `(a)=>expr`: wrap as `{ return expr; }` so
                // the shared block compiler handles it.
                let wrapped = BlockStmt {
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
        Expr::Lit(Lit::Str(s)) => {
            // `Str.value` is a `Wtf8Atom`; bail on lone surrogates (not
            // representable as a clean Rust string for our Const::Str).
            match s.value.as_str() {
                Some(v) => {
                    let ci = cx.const_str(v.to_string());
                    cx.emit(Instr::PushConst(ci));
                }
                None => cx.bail(),
            }
        }
        Expr::Lit(Lit::Bool(b)) => {
            let ci = cx.const_bool(b.value);
            cx.emit(Instr::PushConst(ci));
        }
        Expr::Lit(Lit::Null(_)) => {
            cx.emit(Instr::PushNull);
        }
        Expr::Lit(_) => cx.bail(), // regex / bigint
        Expr::Ident(id) => {
            let name = id.sym.as_ref();
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
        Expr::Paren(p) => emit_expr(cx, &p.expr),
        Expr::Bin(b) => match b.op {
            BinaryOp::LogicalAnd | BinaryOp::LogicalOr => emit_logical(cx, b),
            BinaryOp::NullishCoalescing => emit_nullish(cx, b),
            _ => match bin_op_code(b.op) {
                Some(code) => {
                    emit_expr(cx, &b.left);
                    emit_expr(cx, &b.right);
                    cx.emit(Instr::Bin(code));
                }
                None => cx.bail(),
            },
        },
        Expr::Unary(u) => {
            // `delete o.k` / `delete o[k]`: lower to DeleteProp (pushes the boolean
            // result). Only a member target is modeled; a `delete x` of a local in
            // sloppy mode is a no-op we don't reproduce, so bail on it.
            if matches!(u.op, UnaryOp::Delete) {
                if let Expr::Member(m) = &*u.arg {
                    emit_expr(cx, &m.obj);
                    emit_member_key(cx, &m.prop);
                    if cx.bailed() {
                        return;
                    }
                    cx.emit(Instr::DeleteProp);
                } else {
                    cx.bail_with("delete_target");
                }
                return;
            }
            // soundness bail: typeof <free ident capture>. A BOXED capture is fine
            // (its cell always exists, so `typeof x[0]` reads the boxed value — the
            // ident emit below loads through the cell); only a read-only/unknown
            // free name (which `typeof` must tolerate as "undefined") bails.
            if matches!(u.op, UnaryOp::TypeOf)
                && let Expr::Ident(id) = &*u.arg
            {
                let name = id.sym.as_ref();
                if !cx.is_param_or_local(name) && !cx.is_celled(name) {
                    cx.bail();
                    return;
                }
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
        Expr::Update(_) => cx.bail(), // only handled as statement
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
        Expr::Call(c) => emit_call(cx, c),
        Expr::New(n) => {
            let has_spread = n
                .args
                .as_ref()
                .is_some_and(|a| a.iter().any(|x| x.spread.is_some()));
            if has_spread {
                // `new C(...a)` -> `Reflect.construct(C, [args])` (captured global).
                let reflect = cx.resolve("Reflect");
                cx.emit(Instr::LoadLocal(reflect));
                cx.emit(Instr::Dup);
                let construct_key = cx.const_str("construct".to_string());
                cx.emit(Instr::PushConst(construct_key));
                cx.emit(Instr::GetProp); // [Reflect, construct]
                emit_expr(cx, &n.callee); // [Reflect, construct, C]
                if cx.bailed() {
                    return;
                }
                emit_spread_array(cx, n.args.as_deref().unwrap_or(&[]));
                cx.emit(Instr::CallResolved(2));
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
            let has_spread = arr.elems.iter().flatten().any(|e| e.spread.is_some());
            if has_spread {
                // A spread array is built via the iterator helper; holes mixed with
                // spread are out of scope (rare) -> bail.
                let mut elems = Vec::with_capacity(arr.elems.len());
                for elem in &arr.elems {
                    match elem {
                        Some(e) => elems.push(e.clone()),
                        None => {
                            cx.bail_with("spread_array_hole");
                            return;
                        }
                    }
                }
                emit_spread_array(cx, &elems);
            } else {
                let mut n = 0u32;
                for elem in &arr.elems {
                    match elem {
                        Some(e) => {
                            emit_expr(cx, &e.expr);
                            n += 1;
                        }
                        None => {
                            // array hole
                            cx.bail();
                            return;
                        }
                    }
                }
                cx.emit(Instr::MakeArray(n));
            }
        }
        Expr::Object(o) => {
            let has_spread = o
                .props
                .iter()
                .any(|p| matches!(p, PropOrSpread::Spread(_)));
            if has_spread {
                emit_object_spread(cx, o);
            } else {
                let mut n = 0u32;
                for prop in &o.props {
                    match prop {
                        PropOrSpread::Prop(p) => {
                            emit_object_entry(cx, p);
                            if cx.bailed() {
                                return;
                            }
                            n += 1;
                        }
                        PropOrSpread::Spread(_) => unreachable!("no spread on this path"),
                    }
                }
                cx.emit(Instr::MakeObject(n));
            }
        }
        Expr::Seq(seq) => {
            // (a, b, c): evaluate each, discard all but the last.
            if seq.exprs.is_empty() {
                cx.bail();
                return;
            }
            let last = seq.exprs.len() - 1;
            for (i, e) in seq.exprs.iter().enumerate() {
                emit_expr(cx, e);
                if cx.bailed() {
                    return;
                }
                if i != last {
                    cx.emit(Instr::Pop);
                }
            }
        }
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
/// Known limitation: a Symbol interpolation throws TypeError in a real template
/// but `String(sym)` succeeds, so a virtualized `` `${sym}` `` returns a string
/// where the original throws. This is unobservable for the differential corpus
/// (no Symbols flow into virtualized templates) and not statically detectable,
/// so it is accepted rather than bailed.
pub(crate) fn emit_template(cx: &mut Cx<'_>, t: &Tpl) {
    // `count` tracks un-folded operands currently on the stack (0, 1, or
    // transiently 2 → folded back to 1 with Add).
    let mut count = 0u32;
    let n = t.exprs.len();
    for i in 0..=n {
        let q = &t.quasis[i];
        let cooked = match &q.cooked {
            Some(c) => match c.as_str() {
                Some(s) => s.to_string(),
                // lone surrogate in cooked value: not representable.
                None => {
                    cx.bail_with("template_surrogate");
                    return;
                }
            },
            // No cooked value (invalid escape) — only possible for tagged
            // templates, which are rejected; guard anyway.
            None => {
                cx.bail_with("template_cooked");
                return;
            }
        };
        if !cooked.is_empty() {
            let ci = cx.const_str(cooked);
            cx.emit(Instr::PushConst(ci));
            count += 1;
            if count == 2 {
                cx.emit(Instr::Bin(0)); // Add (string concat)
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
/// `undefined` hole. A lone surrogate in a string is not representable -> bail.
pub(crate) fn emit_tagged_template(cx: &mut Cx<'_>, t: &TaggedTpl) {
    // Gather cooked (Option for invalid-escape holes) and raw quasis.
    let mut cooked: Vec<Option<String>> = Vec::with_capacity(t.tpl.quasis.len());
    let mut raw: Vec<String> = Vec::with_capacity(t.tpl.quasis.len());
    for q in &t.tpl.quasis {
        match &q.cooked {
            Some(c) => match c.as_str() {
                Some(s) => cooked.push(Some(s.to_string())),
                None => {
                    // lone surrogate in the cooked value: not representable.
                    cx.bail_with("template_surrogate");
                    return;
                }
            },
            // No cooked value: an invalid escape in a tagged template -> undefined.
            None => cooked.push(None),
        }
        raw.push(q.raw.as_str().to_string());
    }
    let tpl_ci = cx.const_template(cooked, raw);
    if cx.bailed() {
        return;
    }

    // Compile the tag callee with the correct receiver, then push the template
    // object as the first argument, then each substitution expression, then call.
    let subs = &t.tpl.exprs;
    if let Expr::Member(m) = &*t.tag {
        // `obj.tag`…`` -> obj is `this`. Mirror the method-call lowering in
        // `emit_call`: leave the receiver under the resolved method on the stack.
        emit_expr(cx, &m.obj);
        cx.emit(Instr::Dup);
        emit_member_key(cx, &m.prop);
        if cx.bailed() {
            return;
        }
        cx.emit(Instr::GetProp);
        cx.emit(Instr::PushConst(tpl_ci));
        let mut argc = 1u32;
        for e in subs {
            emit_expr(cx, e);
            if cx.bailed() {
                return;
            }
            argc += 1;
        }
        cx.emit(Instr::CallResolved(argc));
    } else {
        // Bare `tag`…`` -> plain call (`this` is undefined).
        emit_expr(cx, &t.tag);
        if cx.bailed() {
            return;
        }
        cx.emit(Instr::PushConst(tpl_ci));
        let mut argc = 1u32;
        for e in subs {
            emit_expr(cx, e);
            if cx.bailed() {
                return;
            }
            argc += 1;
        }
        cx.emit(Instr::Call(argc));
    }
}

/// Nullish coalescing `a ?? b`: yields `b` iff `a` is null/undefined.
pub(crate) fn emit_nullish(cx: &mut Cx<'_>, b: &BinExpr) {
    // eval a; Dup; PushNull; Bin(==); JumpIfFalse END; Pop; eval b; END:
    // `a == null` (loose) is true for exactly null and undefined.
    emit_expr(cx, &b.left);
    cx.emit(Instr::Dup);
    cx.emit(Instr::PushNull);
    cx.emit(Instr::Bin(6)); // == (loose)
    let end_j = cx.code.len();
    cx.emit(Instr::JumpIfFalse(u32::MAX));
    cx.emit(Instr::Pop);
    emit_expr(cx, &b.right);
    let here = cx.here();
    patch(cx, end_j, here);
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
/// stack: `Dup; PushNull; Bin(== loose); JumpIfFalse cont; Pop; PushUndef; Jump
/// END`. `a == null` (loose) is true for exactly `null` and `undefined`. When the
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
    cx.emit(Instr::PushNull);
    cx.emit(Instr::Bin(6)); // == (loose): true for null and undefined only
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
/// `call_optional` is the `?.` on the call itself (`f?.(args)` / `o?.()`). For a
/// plain call the function value is simply guarded. A `?.` call on a *method*
/// receiver (`o.m?.()`) is the one shape whose short-circuit would strand the
/// receiver under the `undefined` at END — we bail on it rather than miscompile.
pub(crate) fn emit_oc_call(
    cx: &mut Cx<'_>,
    callee: &Expr,
    args: &[ExprOrSpread],
    call_optional: bool,
    sc_jumps: &mut Vec<usize>,
) {
    // Identify a method-call callee (optional or plain member access) so the
    // receiver is preserved for `CallResolved`.
    let method: Option<(&Expr, &MemberProp, bool)> = match callee {
        Expr::OptChain(oc) => match &*oc.base {
            OptChainBase::Member(m) => Some((&m.obj, &m.prop, oc.optional)),
            OptChainBase::Call(_) => None,
        },
        Expr::Member(m) => Some((&m.obj, &m.prop, false)),
        _ => None,
    };

    match method {
        Some((obj, prop, member_optional)) => {
            // A `?.` on the CALL of a method would need to guard the resolved
            // method while the receiver sits beneath it — the short-circuit would
            // leave `[recv, undefined]` at END (stack-imbalanced). Bail.
            if call_optional {
                cx.bail_with("optional_chain_unsupported");
                return;
            }
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
            let mut argc = 0u32;
            for a in args {
                if a.spread.is_some() {
                    cx.bail();
                    return;
                }
                emit_expr(cx, &a.expr);
                if cx.bailed() {
                    return;
                }
                argc += 1;
            }
            cx.emit(Instr::CallResolved(argc));
        }
        None => {
            // Plain call: the callee value is the function. If the call is
            // optional (`f?.(args)`), guard the function value (sole stack value).
            emit_opt_chain_node(cx, callee, sc_jumps);
            if cx.bailed() {
                return;
            }
            if call_optional {
                emit_oc_guard(cx, sc_jumps);
            }
            let mut argc = 0u32;
            for a in args {
                if a.spread.is_some() {
                    cx.bail();
                    return;
                }
                emit_expr(cx, &a.expr);
                if cx.bailed() {
                    return;
                }
                argc += 1;
            }
            cx.emit(Instr::Call(argc));
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
        MemberProp::Computed(c) => emit_expr(cx, &c.expr),
        MemberProp::PrivateName(_) => cx.bail(),
    }
}

/// Object-literal property-name key.
pub(crate) fn emit_prop_key(cx: &mut Cx<'_>, key: &PropName) {
    match key {
        PropName::Ident(name) => {
            if name.sym.as_ref() == "__proto__" {
                cx.bail();
                return;
            }
            let ci = cx.const_str(name.sym.to_string());
            cx.emit(Instr::PushConst(ci));
        }
        PropName::Str(s) => {
            // `Str.value` is a `Wtf8Atom`; bail on lone surrogates.
            match s.value.as_str() {
                Some(v) => {
                    if v == "__proto__" {
                        cx.bail();
                        return;
                    }
                    let ci = cx.const_str(v.to_string());
                    cx.emit(Instr::PushConst(ci));
                }
                None => cx.bail(),
            }
        }
        PropName::Num(num) => {
            // Only safe-range integers stringify identically in Rust and JS.
            // Other magnitudes (>= 2^53, or values JS renders in exponential
            // notation) would diverge from Number.prototype.toString — bail
            // rather than build a wrong property key.
            let v = num.value;
            if v.fract() == 0.0 && v.is_finite() && v.abs() < 9007199254740992.0 {
                let ci = cx.const_str(format!("{}", v as i64));
                cx.emit(Instr::PushConst(ci));
            } else {
                cx.bail_with("numeric_prop_key");
            }
        }
        PropName::Computed(c) => emit_expr(cx, &c.expr),
        PropName::BigInt(_) => cx.bail(),
    }
}

pub(crate) fn emit_logical(cx: &mut Cx<'_>, b: &BinExpr) {
    match b.op {
        BinaryOp::LogicalAnd => {
            // eval a; Dup; JumpIfFalse END; Pop; eval b; END:
            emit_expr(cx, &b.left);
            cx.emit(Instr::Dup);
            let end_j = cx.code.len();
            cx.emit(Instr::JumpIfFalse(u32::MAX));
            cx.emit(Instr::Pop);
            emit_expr(cx, &b.right);
            patch(cx, end_j, cx.here());
        }
        BinaryOp::LogicalOr => {
            // eval a; Dup; JumpIfFalse EVALB; Jump END; EVALB: Pop; eval b; END:
            emit_expr(cx, &b.left);
            cx.emit(Instr::Dup);
            let evalb_j = cx.code.len();
            cx.emit(Instr::JumpIfFalse(u32::MAX));
            let end_j = cx.code.len();
            cx.emit(Instr::Jump(u32::MAX));
            patch(cx, evalb_j, cx.here());
            cx.emit(Instr::Pop);
            emit_expr(cx, &b.right);
            patch(cx, end_j, cx.here());
        }
        _ => cx.bail(),
    }
}
