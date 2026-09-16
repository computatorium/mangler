//! Native optional chains use the same lazy reference shape as lexical class
//! capabilities. Native optional calls guard each source key/argument list; no
//! source expression moves into a callback or needs a shared temporary binding.
use mangler_jsast::{assignment_target::unparen, build as b, span::injected_span};
use std::collections::HashSet;
use swc_core::common::SyntaxContext;
use swc_core::ecma::ast::*;
use swc_core::ecma::visit::{VisitMut, VisitMutWith};

enum Link {
    Member(MemberProp, bool),
    Call(Vec<ExprOrSpread>, bool),
    Group,
}

fn lexical_call(callee: &Expr, with_depth: usize) -> bool {
    matches!(unparen(callee), Expr::Ident(id) if id.sym == "eval" || with_depth != 0)
        || matches!(unparen(callee), Expr::SuperProp(_))
        || matches!(unparen(callee), Expr::Member(member) if matches!(member.prop, MemberProp::PrivateName(_)))
}

fn eligible(mut expression: &Expr, sites: &HashSet<u32>, with_depth: usize) -> bool {
    let mut producer = false;
    loop {
        match expression {
            Expr::OptChain(chain) => match &*chain.base {
                OptChainBase::Member(member) => {
                    if matches!(member.prop, MemberProp::PrivateName(_)) {
                        return false;
                    }
                    expression = &member.obj;
                }
                OptChainBase::Call(call) => {
                    if matches!(unparen(&call.callee), Expr::Ident(_) if with_depth != 0)
                        || matches!(unparen(&call.callee), Expr::SuperProp(_))
                    {
                        return false;
                    }
                    producer |= sites.contains(&chain.span.lo.0) || sites.contains(&call.span.lo.0);
                    expression = &call.callee;
                }
            },
            Expr::Member(member) => {
                if matches!(member.prop, MemberProp::PrivateName(_)) {
                    return false;
                }
                expression = &member.obj;
            }
            Expr::Call(call) => {
                let Callee::Expr(callee) = &call.callee else {
                    break;
                };
                // A direct eval or native environment-dependent call before the
                // optional region retains its source lexical execution boundary.
                if lexical_call(callee, with_depth) {
                    break;
                }
                producer |= sites.contains(&call.span.lo.0);
                expression = callee;
            }
            Expr::Paren(parenthesis) => expression = &parenthesis.expr,
            Expr::SuperProp(_) => return false,
            _ => break,
        }
    }
    producer
}

/// Visit independent operands without partially rewriting a continuous chain.
/// A partial rewrite could turn its earlier short circuit into a real undefined
/// value and incorrectly evaluate a later property or argument.
fn operands<V: VisitMut>(mut expression: &mut Expr, visitor: &mut V) {
    loop {
        match expression {
            Expr::OptChain(chain) => match &mut *chain.base {
                OptChainBase::Member(member) => {
                    if let MemberProp::Computed(key) = &mut member.prop {
                        key.expr.visit_mut_with(visitor);
                    }
                    expression = &mut member.obj;
                }
                OptChainBase::Call(call) => {
                    call.args.visit_mut_with(visitor);
                    expression = &mut call.callee;
                }
            },
            Expr::Member(member) => {
                if let MemberProp::Computed(key) = &mut member.prop {
                    key.expr.visit_mut_with(visitor);
                }
                expression = &mut member.obj;
            }
            Expr::Call(call) => {
                call.args.visit_mut_with(visitor);
                let Callee::Expr(callee) = &mut call.callee else {
                    break;
                };
                expression = callee;
            }
            Expr::Paren(parenthesis) => expression = &mut parenthesis.expr,
            other => {
                other.visit_mut_children_with(visitor);
                break;
            }
        }
    }
}

fn frame(value: Expr) -> Expr {
    Expr::Object(ObjectLit {
        span: injected_span(),
        props: [
            ("__proto__", b::null()),
            (
                "v",
                *mangler_jsast::assignment_target::named_value("", Box::new(value)),
            ),
        ]
        .into_iter()
        .map(|(key, value)| {
            PropOrSpread::Prop(Box::new(Prop::KeyValue(KeyValueProp {
                key: PropName::Ident(IdentName::new(key.into(), injected_span())),
                value: Box::new(value),
            })))
        })
        .collect(),
    })
}
fn table_call(table: &str, name: &str, args: Vec<Expr>) -> Expr {
    b::call(b::member_ident(b::ident_expr(table), name), args)
}
fn optional_call(callee: Expr, args: Vec<ExprOrSpread>) -> Expr {
    Expr::OptChain(OptChainExpr {
        span: injected_span(),
        optional: true,
        base: Box::new(OptChainBase::Call(OptCall {
            span: injected_span(),
            ctxt: SyntaxContext::empty(),
            callee: Box::new(callee),
            args,
            type_args: None,
        })),
    })
}
fn value(reference: Expr) -> Expr {
    Expr::OptChain(OptChainExpr {
        span: injected_span(),
        optional: true,
        base: Box::new(OptChainBase::Member(MemberExpr {
            span: injected_span(),
            obj: Box::new(reference),
            prop: MemberProp::Ident(IdentName::new("v".into(), injected_span())),
        })),
    })
}

fn lower<V: VisitMut>(
    mut expression: Expr,
    table: &str,
    with_depth: usize,
    visitor: &mut V,
) -> Expr {
    let mut links = Vec::new();
    loop {
        match expression {
            Expr::OptChain(chain) => match *chain.base {
                OptChainBase::Member(member) => {
                    links.push(Link::Member(member.prop, chain.optional));
                    expression = *member.obj;
                }
                OptChainBase::Call(call) => {
                    links.push(Link::Call(call.args, chain.optional));
                    expression = *call.callee;
                }
            },
            Expr::Member(member) => {
                links.push(Link::Member(member.prop, false));
                expression = *member.obj;
            }
            Expr::Call(call) => {
                if matches!(&call.callee, Callee::Expr(callee) if !lexical_call(callee, with_depth))
                {
                    let Callee::Expr(callee) = call.callee else {
                        unreachable!()
                    };
                    links.push(Link::Call(call.args, false));
                    expression = *callee;
                } else {
                    expression = Expr::Call(call);
                    break;
                }
            }
            Expr::Paren(parenthesis) => {
                links.push(Link::Group);
                expression = *parenthesis.expr;
            }
            _ => break,
        }
    }
    expression.visit_mut_with(visitor);
    let mut reference = frame(expression);
    let mut guarded = false;
    for link in links.into_iter().rev() {
        match link {
            Link::Group if guarded => {
                reference = b::bin(
                    BinaryOp::NullishCoalescing,
                    reference,
                    frame(b::unary(UnaryOp::Void, b::num(0.0))),
                );
                guarded = false;
            }
            Link::Group => {}
            Link::Member(property, optional) => {
                let mut key = match property {
                    MemberProp::Ident(id) => Expr::Lit(Lit::Str(Str {
                        span: injected_span(),
                        value: id.sym.into(),
                        raw: None,
                    })),
                    MemberProp::Computed(key) => *key.expr,
                    MemberProp::PrivateName(_) => {
                        unreachable!("lexical private references are not ordinary chain members")
                    }
                };
                key.visit_mut_with(visitor);
                let stage = table_call(
                    table,
                    "chainReference",
                    vec![reference, b::bool_lit(false), b::bool_lit(optional)],
                );
                reference = optional_call(
                    stage,
                    vec![ExprOrSpread {
                        spread: None,
                        expr: Box::new(key),
                    }],
                );
                guarded |= optional;
            }
            Link::Call(mut args, optional) => {
                args.visit_mut_with(visitor);
                let stage = table_call(
                    table,
                    "chainReference",
                    vec![reference, b::bool_lit(true), b::bool_lit(optional)],
                );
                reference = optional_call(stage, args);
                guarded |= optional;
            }
        }
    }
    reference
}

pub(super) fn rewrite<V: VisitMut>(
    expression: &mut Expr,
    sites: &HashSet<u32>,
    table: &str,
    with_depth: usize,
    strict: bool,
    visitor: &mut V,
) -> bool {
    enum Context {
        Value,
        Delete,
        Tag,
    }
    let (context, chain) = match expression {
        Expr::OptChain(_) => (Context::Value, &mut *expression),
        Expr::Call(call) if matches!(&call.callee, Callee::Expr(callee) if matches!(unparen(callee), Expr::OptChain(_))) => {
            (Context::Value, &mut *expression)
        }
        Expr::Unary(unary)
            if unary.op == UnaryOp::Delete && matches!(unparen(&unary.arg), Expr::OptChain(_)) =>
        {
            (Context::Delete, &mut *unary.arg)
        }
        Expr::TaggedTpl(template) if matches!(unparen(&template.tag), Expr::OptChain(_)) => {
            (Context::Tag, &mut *template.tag)
        }
        _ => return false,
    };
    if !eligible(chain, sites, with_depth) {
        operands(chain, visitor);
        if let Expr::TaggedTpl(template) = expression {
            template.tpl.visit_mut_with(visitor);
        }
        return true;
    }
    let original = std::mem::replace(
        chain,
        Expr::Invalid(Invalid {
            span: injected_span(),
        }),
    );
    let reference = lower(original, table, with_depth, visitor);
    match context {
        Context::Value => *expression = value(reference),
        Context::Delete => {
            *expression = table_call(
                table,
                "deleteReference",
                vec![reference, b::bool_lit(strict)],
            )
        }
        Context::Tag => {
            let Expr::TaggedTpl(template) = expression else {
                unreachable!()
            };
            *template.tag = table_call(
                table,
                "chainReference",
                vec![
                    reference,
                    b::bool_lit(true),
                    b::bool_lit(false),
                    b::bool_lit(true),
                ],
            );
            template.tpl.visit_mut_with(visitor);
        }
    }
    true
}
