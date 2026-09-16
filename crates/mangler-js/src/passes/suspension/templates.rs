//! Keep source template sites in the VM's constant table across suspension.
//!
//! SWC's self-replacing cache function is a separate mutable binding whose
//! identity does not survive export through a runtime support dictionary. Keep
//! the source quasis intact instead: normal call lowering handles tag/operand
//! evaluation, and the bytecode compiler owns the single template-object cache.
use std::collections::HashMap;
use swc_core::common::{DUMMY_SP, Mark, SyntaxContext};
use swc_core::ecma::{
    ast::*,
    visit::{VisitMut, VisitMutWith},
};

#[derive(Default)]
pub(super) struct Sites(HashMap<Id, Box<Tpl>>);

impl Sites {
    pub(super) fn lower(&mut self, program: &mut Program) {
        struct Lower<'a>(&'a mut Sites);
        impl VisitMut for Lower<'_> {
            fn visit_mut_bin_expr(&mut self, expression: &mut BinExpr) {
                mangler_jsast::deep::walk_binary_mut(expression, self);
            }
            fn visit_mut_expr(&mut self, expression: &mut Expr) {
                expression.visit_mut_children_with(self);
                let Expr::TaggedTpl(template) = expression else {
                    return;
                };
                let site = Ident::new(
                    "_template_site".into(),
                    DUMMY_SP,
                    SyntaxContext::empty().apply_mark(Mark::new()),
                );
                let substitutions = std::mem::take(&mut template.tpl.exprs);
                self.0.0.insert(site.to_id(), template.tpl.clone());
                *expression = Expr::Call(CallExpr {
                    span: template.span,
                    callee: Callee::Expr(template.tag.clone()),
                    args: std::iter::once(Box::new(Expr::Ident(site)))
                        .chain(substitutions)
                        .map(|expr| ExprOrSpread { spread: None, expr })
                        .collect(),
                    ..Default::default()
                });
            }
        }
        program.visit_mut_with(&mut Lower(self));
    }

    pub(super) fn restore(self, program: &mut Program) {
        struct Restore(Sites);
        impl VisitMut for Restore {
            fn visit_mut_bin_expr(&mut self, expression: &mut BinExpr) {
                mangler_jsast::deep::walk_binary_mut(expression, self);
            }
            fn visit_mut_expr(&mut self, expression: &mut Expr) {
                let site = match expression {
                    Expr::Ident(id) => self.0.0.get(&id.to_id()),
                    _ => None,
                };
                if let Some(template) = site {
                    let mut template = template.clone();
                    // This literal stays valid JS for standalone native lowering.
                    // Its empty substitutions have no effects; bytecode omits
                    // the generated identity tag and reads TemplateObject only.
                    template.exprs = (1..template.quasis.len())
                        .map(|_| {
                            Box::new(mangler_jsast::build::unary(
                                UnaryOp::Void,
                                mangler_jsast::build::num(0.0),
                            ))
                        })
                        .collect();
                    let strings = Ident::new(
                        "_strings".into(),
                        DUMMY_SP,
                        SyntaxContext::empty().apply_mark(Mark::new()),
                    );
                    *expression = Expr::TaggedTpl(TaggedTpl {
                        span: template.span,
                        tag: Box::new(Expr::Arrow(ArrowExpr {
                            span: mangler_jsast::span::template_object_span(),
                            params: vec![Pat::Ident(strings.clone().into())],
                            body: Box::new(ArrowFunctionBody::Expr(Box::new(Expr::Ident(strings)))),
                            ..Default::default()
                        })),
                        tpl: template,
                        ..Default::default()
                    });
                } else {
                    expression.visit_mut_children_with(self);
                }
            }
        }
        program.visit_mut_with(&mut Restore(self));
    }
}
