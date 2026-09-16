//! Source lexical references retained across suspension lowering. The adapter is
//! materialized before an assignment RHS can suspend, so resumed writes use the
//! same resolved binding even if an object environment changes meanwhile.
use super::*;

pub(crate) fn emit_projected_reference(cx: &mut Cx<'_>, call: &CallExpr) -> bool {
    if !emit_reference(cx, call) {
        return false;
    }
    cx.emit(Instr::RefAdapter);
    true
}

pub(crate) fn emit_projected_call_reference(cx: &mut Cx<'_>, call: &CallExpr) -> bool {
    if !emit_reference(cx, call) {
        return false;
    }
    cx.emit(Instr::RefCall);
    true
}

fn emit_reference(cx: &mut Cx<'_>, call: &CallExpr) -> bool {
    let Some(reference) = cx
        .opts
        .suspension_references
        .and_then(|references| references.get(&call.span.lo.0))
        .cloned()
    else {
        return false;
    };
    // A source call and its callee identifier start at the same byte position.
    // Only the generated reference helper has this private argument signature;
    // the enclosing source call must still evaluate its arguments and invoke it.
    if !matches!(&call.callee, Callee::Expr(callee) if matches!(&**callee, Expr::Ident(id) if id.span.is_dummy()))
        || call.args.len() != reference.objects.len() + 1
        || call.args.iter().zip(std::iter::once(&reference.cell).chain(&reference.objects))
            .any(|(argument, name)| argument.spread.is_some()
                || !matches!(&*argument.expr, Expr::Ident(id) if id.sym.as_ref() == name.as_str())) {
        return false;
    }
    let cell = cx.resolve(&reference.cell);
    cx.emit(Instr::AccessorRef(
        cell * 2 + u32::from(cx.is_celled(&reference.cell)),
    ));
    if !reference.objects.is_empty() {
        let name = cx.const_str(reference.name);
        for object in &reference.objects {
            let slot = cx.resolve(object);
            cx.emit(if cx.is_celled(object) {
                Instr::WithRefCell(name, slot)
            } else {
                Instr::WithRef(name, slot)
            });
        }
    }
    cx.emit(Instr::ResolveRef);
    true
}
