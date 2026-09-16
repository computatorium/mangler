//! Mediate source-known native bind producers after selected bodies become VM
//! thunks. The native bound value itself is never replaced. Argument expressions
//! stay in their source scope, including await/yield, eval, and parameter defaults.
mod optional;

use std::collections::HashSet;
use swc_core::common::{DUMMY_SP, SyntaxContext};
use swc_core::ecma::ast::*;
use swc_core::ecma::visit::{VisitMut, VisitMutWith};

pub(super) fn mediate(program: &mut Program, sites: &HashSet<u32>, table: &str) {
    if sites.is_empty() {
        return;
    }
    struct Producers<'a> {
        sites: &'a HashSet<u32>,
        table: &'a str,
        with_depth: usize,
        strict: bool,
    }
    impl VisitMut for Producers<'_> {
        fn visit_mut_expr(&mut self, expression: &mut Expr) {
            if optional::rewrite(
                expression,
                self.sites,
                self.table,
                self.with_depth,
                self.strict,
                self,
            ) {
                return;
            }
            expression.visit_mut_children_with(self);
        }
        fn visit_mut_function(&mut self, function: &mut Function) {
            let previous = self.strict;
            self.strict |= function
                .body
                .as_ref()
                .is_some_and(|body| mangler_jsast::directives::has_use_strict(&body.stmts));
            function.visit_mut_children_with(self);
            self.strict = previous;
        }
        fn visit_mut_arrow_expr(&mut self, arrow: &mut ArrowExpr) {
            let previous = self.strict;
            if let ArrowFunctionBody::FunctionBody(body) = &*arrow.body {
                self.strict |= mangler_jsast::directives::has_use_strict(&body.stmts);
            }
            arrow.visit_mut_children_with(self);
            self.strict = previous;
        }
        fn visit_mut_class(&mut self, class: &mut Class) {
            let previous = self.strict;
            self.strict = true;
            class.visit_mut_children_with(self);
            self.strict = previous;
        }
        fn visit_mut_bin_expr(&mut self, binary: &mut BinExpr) {
            mangler_jsast::deep::walk_binary_mut(binary, self);
        }
        fn visit_mut_with_stmt(&mut self, statement: &mut WithStmt) {
            statement.obj.visit_mut_with(self);
            self.with_depth += 1;
            statement.body.visit_mut_with(self);
            self.with_depth -= 1;
        }
        fn visit_mut_call_expr(&mut self, call: &mut CallExpr) {
            call.visit_mut_children_with(self);
            if !self.sites.contains(&call.span.lo.0) {
                return;
            }
            let Callee::Expr(callee) = &call.callee else {
                return;
            };
            let callee = mangler_jsast::assignment_target::unparen(callee);
            // Bare eval must preserve its native lexical-eval test even when
            // conservative provenance includes an invocation adapter. Private
            // and super references require their existing lexical capability path. A bare name inside with needs its environment
            // receiver; ordinary bare source expressions have undefined receiver.
            if matches!(callee, Expr::Ident(id) if id.sym == "eval")
                || matches!(callee, Expr::SuperProp(_))
                || matches!(callee, Expr::Member(member) if matches!(member.prop, MemberProp::PrivateName(_)))
                || self.with_depth != 0 && matches!(callee, Expr::Ident(_))
            {
                return;
            }
            let Callee::Expr(mut callee) = std::mem::replace(
                &mut call.callee,
                Callee::Expr(Box::new(Expr::Invalid(Invalid { span: DUMMY_SP }))),
            ) else {
                unreachable!()
            };
            while let Expr::Paren(parenthesis) = *callee {
                callee = parenthesis.expr;
            }
            let (receiver, key, value) = match *callee {
                Expr::Member(member) => {
                    let key = match member.prop {
                        MemberProp::Ident(id) => Box::new(Expr::Lit(Lit::Str(Str {
                            span: DUMMY_SP,
                            value: id.sym.into(),
                            raw: None,
                        }))),
                        MemberProp::Computed(key) => key.expr,
                        MemberProp::PrivateName(_) => unreachable!(),
                    };
                    (member.obj, key, false)
                }
                expression => (
                    Box::new(Expr::Unary(UnaryExpr {
                        span: DUMMY_SP,
                        op: UnaryOp::Void,
                        arg: Box::new(Expr::Lit(Lit::Num(Number {
                            span: DUMMY_SP,
                            value: 0.0,
                            raw: None,
                        }))),
                    })),
                    Box::new(expression),
                    true,
                ),
            };
            call.callee = Callee::Expr(Box::new(Expr::Call(CallExpr {
                span: DUMMY_SP,
                ctxt: SyntaxContext::empty(),
                callee: Callee::Expr(Box::new(Expr::Member(MemberExpr {
                    span: DUMMY_SP,
                    obj: Box::new(Expr::Ident(Ident::new(
                        self.table.into(),
                        DUMMY_SP,
                        SyntaxContext::empty(),
                    ))),
                    prop: MemberProp::Ident(IdentName::new("captureBind".into(), DUMMY_SP)),
                }))),
                args: [
                    receiver,
                    key,
                    Box::new(Expr::Lit(Lit::Bool(Bool {
                        span: DUMMY_SP,
                        value,
                    }))),
                ]
                .into_iter()
                .map(|expr| ExprOrSpread { spread: None, expr })
                .collect(),
                type_args: None,
            })));
        }
    }
    let strict = match program {
        Program::Module(_) => true,
        Program::Script(script) => mangler_jsast::directives::has_use_strict(&script.body),
    };
    program.visit_mut_with(&mut Producers {
        sites,
        table,
        with_depth: 0,
        strict,
    });
}
