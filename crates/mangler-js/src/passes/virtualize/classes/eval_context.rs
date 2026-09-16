//! Native lexical primitives shared by static class bodies and direct eval.
use super::*;
use mangler_vm::eval::EvalClassContext;

use mangler_vm::eval_class::provider;

/// This declaration stays in its original native lexical envelope. It contains
/// only host reference operations; dynamically compiled source never enters it.
pub(in crate::passes::virtualize) fn native_capsule_with_parent(
    context: &EvalClassContext,
    inherited: Option<&str>,
    local_private_names: &[String],
    _cfg: &FileConfig,
    apply: &str,
    iterator: &IteratorAlias<'_>,
) -> Vec<Stmt> {
    let fields = context
        .private_names
        .iter()
        .map(|name| format!("#{name};"))
        .collect::<String>();
    let private = context
        .private_names
        .iter()
        .map(|name| {
            format!(
                "[{}]:{}",
                quoted_private_name(name),
                if let Some(parent) = inherited.filter(|_| !local_private_names.contains(name)) {
                    format!("{parent}.p[{}]", quoted_private_name(name))
                } else {
                    provider(Some(name), apply, iterator.name, false)
                }
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let super_provider = if context.allow_super_property || context.allow_super_call {
        provider(
            None,
            apply,
            if context.allow_super_call {
                iterator.require()
            } else {
                iterator.name
            },
            context.allow_super_call,
        )
    } else {
        "null".into()
    };
    let source = format!(
        "class _Capsule extends _Base{{{fields}constructor(){{const {}={{__proto__:null,p:{{__proto__:null,{private}}},s:{super_provider},t:()=>this,n:()=>new.target}};}}}}",
        context.capsule_binding
    );
    let program = Js
        .parse(&source, &ParseOpts::default())
        .expect("lexical capsule parses")
        .into_program();
    let Program::Script(script) = program else {
        unreachable!()
    };
    let Stmt::Decl(Decl::Class(class)) = script.body.into_iter().next().unwrap() else {
        unreachable!()
    };
    let mut statements = class
        .class
        .body
        .into_iter()
        .find_map(|member| match member {
            ClassMember::Constructor(c) => c.body.map(|body| body.stmts),
            _ => None,
        })
        .unwrap();
    statements.visit_mut_with(&mut GeneratedSpans);
    statements
}
