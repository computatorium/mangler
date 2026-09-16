//! Name resolution across lexical bindings and active `with` object scopes.
use super::Cx;
use crate::isa::Instr;

/// A lexical declaration made inside an object scope shadows that object's
/// properties. A declaration outside it remains the fallback for dynamic lookup.
fn binding_scope(cx: &Cx<'_>, name: &str) -> usize {
    cx.scopes
        .iter()
        .rposition(|scope| scope.contains_key(name))
        .unwrap_or(0)
}

pub(crate) fn binding_has_with(cx: &Cx<'_>, name: &str) -> bool {
    if cx.hidden_environment_bindings.contains(name) {
        return false;
    }
    let lexical = binding_scope(cx, name);
    cx.with_scopes.iter().any(|&(depth, _)| lexical < depth)
}

/// Live captures can themselves be dynamic references inherited from a parent
/// object scope. Their calls/writes must retain reference semantics even when the
/// child has no syntactic `with` statement of its own.
pub(crate) fn binding_has_environment(cx: &Cx<'_>, name: &str) -> bool {
    cx.dynamic_variables
        && (!cx.is_param_or_local(name)
            || cx
                .lookup(name)
                .is_some_and(|slot| cx.var_binding_slots.contains(&slot)))
        && !cx.hidden_environment_bindings.contains(name)
}

pub(crate) fn binding_needs_ref(cx: &Cx<'_>, name: &str) -> bool {
    if cx.hidden_environment_bindings.contains(name) { return false; }
    binding_has_with(cx, name)
        || binding_has_environment(cx, name)
        || (cx.opts.live_captures
            && !cx.is_param_or_local(name)
            && (cx.opts.eval_context
                || cx
                    .opts
                    .dynamic_captures
                    .is_some_and(|names| names.contains(name))))
}

/// Push an unresolved reference. Resolve it before evaluating an assignment RHS
/// or call arguments; retain it unresolved when installing a closure capture.
pub(crate) fn emit_binding_ref(cx: &mut Cx<'_>, name: &str) {
    let lexical = binding_scope(cx, name);
    let internal = cx.hidden_environment_bindings.contains(name);
    let objects: Vec<u32> = cx
        .with_scopes
        .iter()
        .filter(|&&(depth, _)| !internal && lexical < depth)
        .map(|&(_, slot)| slot)
        .collect();
    let slot = cx.resolve(name);
    cx.emit(Instr::LocalRef(slot * 2 + u32::from(cx.is_celled(name))));
    if binding_has_environment(cx, name) {
        let key = cx.const_str(name.to_string());
        cx.emit(Instr::EnvironmentRef(key));
    }
    if !objects.is_empty() {
        let key = cx.const_str(name.to_string());
        for object in objects {
            cx.emit(Instr::WithRef(key, object));
        }
    }
}
