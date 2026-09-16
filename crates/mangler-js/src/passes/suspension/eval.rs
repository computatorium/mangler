//! Save direct eval's reference before suspended argument evaluation.
use super::*;
use swc_core::common::{DUMMY_SP, SyntaxContext};

pub(super) fn split(program: &mut Program, scopes: &lexical::RawScopes) {
    struct Split {
        sites: HashSet<u32>,
        active: bool,
        temporaries: Vec<Ident>,
    }
    struct Yield(bool);
    impl Visit for Yield {
        fn visit_function(&mut self, _: &Function) {}
        fn visit_arrow_expr(&mut self, _: &ArrowExpr) {}
        fn visit_yield_expr(&mut self, _: &YieldExpr) {
            self.0 = true;
        }
        fn visit_bin_expr(&mut self, expression: &BinExpr) {
            mangler_jsast::deep::walk_binary(expression, self);
        }
    }
    fn fresh() -> Ident {
        Ident::new(
            "_eval_reference".into(),
            DUMMY_SP,
            SyntaxContext::empty().apply_mark(Mark::new()),
        )
    }
    fn ident(id: &Ident) -> Box<Expr> {
        Box::new(Expr::Ident(id.clone()))
    }
    fn call(name: &str, args: Vec<ExprOrSpread>, span: swc_core::common::Span) -> Box<Expr> {
        Box::new(Expr::Call(CallExpr {
            span,
            callee: Callee::Expr(Box::new(Expr::Ident(Ident::new(
                name.into(),
                DUMMY_SP,
                SyntaxContext::empty(),
            )))),
            args,
            ..Default::default()
        }))
    }
    fn arg(expr: Box<Expr>) -> ExprOrSpread {
        ExprOrSpread { spread: None, expr }
    }
    fn assign(id: &Ident, right: Box<Expr>) -> Box<Expr> {
        Box::new(Expr::Assign(AssignExpr {
            span: DUMMY_SP,
            op: AssignOp::Assign,
            left: AssignTarget::Simple(SimpleAssignTarget::Ident(id.clone().into())),
            right,
        }))
    }
    impl VisitMut for Split {
        fn visit_mut_bin_expr(&mut self, expression: &mut BinExpr) {
            mangler_jsast::deep::walk_binary_mut(expression, self);
        }
        fn visit_mut_function(&mut self, function: &mut Function) {
            let active = std::mem::replace(&mut self.active, function.is_generator);
            let saved = std::mem::take(&mut self.temporaries);
            function.visit_mut_children_with(self);
            if !self.temporaries.is_empty() {
                let body = function.body.as_mut().expect("generator body");
                let at = body.stmts.iter().take_while(|stmt| matches!(stmt, Stmt::Expr(expr) if matches!(&*expr.expr, Expr::Lit(Lit::Str(_))))).count();
                body.stmts.insert(
                    at,
                    Stmt::Decl(Decl::Var(Box::new(VarDecl {
                        span: DUMMY_SP,
                        kind: VarDeclKind::Var,
                        decls: std::mem::take(&mut self.temporaries)
                            .into_iter()
                            .map(|id| VarDeclarator {
                                span: DUMMY_SP,
                                name: Pat::Ident(id.into()),
                                init: None,
                                definite: false,
                            })
                            .collect(),
                        ..Default::default()
                    }))),
                );
            }
            self.temporaries = saved;
            self.active = active;
        }
        fn visit_mut_arrow_expr(&mut self, arrow: &mut ArrowExpr) {
            let active = std::mem::replace(&mut self.active, false);
            arrow.visit_mut_children_with(self);
            self.active = active;
        }
        fn visit_mut_expr(&mut self, expression: &mut Expr) {
            expression.visit_mut_children_with(self);
            if !self.active {
                return;
            }
            let Expr::Call(source) = expression else {
                return;
            };
            if !self.sites.contains(&source.span.lo.0)
                && !mangler_jsast::analysis::scope::is_direct_eval_callee(&source.callee)
            {
                return;
            }
            let mut suspended = Yield(false);
            source.args.visit_with(&mut suspended);
            if !suspended.0 {
                return;
            }
            let mut source = std::mem::take(source);
            let Callee::Expr(callee) = source.callee else {
                unreachable!()
            };
            let marker = source.args.last().is_some_and(|arg| matches!(&*arg.expr, Expr::Array(array) if matches!(array.elems.first(), Some(Some(first)) if matches!(&*first.expr, Expr::Lit(Lit::Str(value)) if value.value == lexical::EVAL_REFERENCES))));
            let aliases = if marker { source.args.pop() } else { None };
            let reference = fresh();
            let arguments = fresh();
            self.temporaries
                .extend([reference.clone(), arguments.clone()]);
            let capture = assign(
                &reference,
                call("\0mangler_eval_reference", vec![arg(callee)], DUMMY_SP),
            );
            let arguments_value = assign(
                &arguments,
                Box::new(Expr::Array(ArrayLit {
                    span: DUMMY_SP,
                    elems: source.args.into_iter().map(Some).collect(),
                })),
            );
            let mut invoke_args = vec![arg(ident(&reference)), arg(ident(&arguments))];
            invoke_args.extend(aliases);
            *expression = Expr::Seq(SeqExpr {
                span: source.span,
                exprs: vec![
                    capture,
                    arguments_value,
                    call("\0mangler_eval_invoke", invoke_args, source.span),
                ],
            });
        }
    }
    program.visit_mut_with(&mut Split {
        sites: scopes.iter().map(|(span, _)| *span).collect(),
        active: false,
        temporaries: Vec::new(),
    });
}
