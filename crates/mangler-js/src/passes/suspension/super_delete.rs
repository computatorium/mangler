//! Preserve native SuperReference deletion through compatibility hoisting.
//! Only the lexical delete primitive is shielded; its key expression remains
//! in the source state machine, including every await/yield and side effect.
use super::*;
use swc_core::common::{DUMMY_SP, SyntaxContext};

#[derive(Default)]
pub(super) struct Plan(HashMap<(u32, SyntaxContext), Stmt>);

struct Shield {
    bridge: Ident,
    used: bool,
}
impl VisitMut for Shield {
    fn visit_mut_bin_expr(&mut self, expression: &mut BinExpr) {
        mangler_jsast::deep::walk_binary_mut(expression, self);
    }
    fn visit_mut_function(&mut self, _: &mut Function) {}
    fn visit_mut_class(&mut self, _: &mut Class) {}
    fn visit_mut_unary_expr(&mut self, unary: &mut UnaryExpr) {
        let mut argument = &*unary.arg;
        while let Expr::Paren(parentheses) = argument {
            argument = &parentheses.expr;
        }
        if unary.op == UnaryOp::Delete
            && let Expr::SuperProp(member) = argument
        {
            let mut key = match &member.prop {
                SuperProp::Ident(name) => Box::new(Expr::Lit(Lit::Str(Str {
                    span: DUMMY_SP,
                    value: name.sym.clone().into(),
                    raw: None,
                }))),
                SuperProp::Computed(key) => key.expr.clone(),
            };
            key.visit_mut_with(self);
            unary.arg = Box::new(Expr::Call(CallExpr {
                callee: Callee::Expr(Box::new(Expr::Ident(self.bridge.clone()))),
                args: vec![ExprOrSpread {
                    spread: None,
                    expr: key,
                }],
                ..Default::default()
            }));
            self.used = true;
            return;
        }
        unary.visit_mut_children_with(self);
    }
}
fn bridge() -> Shield {
    Shield {
        bridge: Ident::new(
            "_delete_super".into(),
            DUMMY_SP,
            SyntaxContext::empty().apply_mark(Mark::new()),
        ),
        used: false,
    }
}
impl Plan {
    fn retain(&mut self, owner: u32, context: SyntaxContext, shield: Shield) {
        if !shield.used {
            return;
        }
        let key = Ident::new(
            "_key".into(),
            DUMMY_SP,
            SyntaxContext::empty().apply_mark(Mark::new()),
        );
        let primitive = Expr::Arrow(ArrowExpr {
            params: vec![Pat::Ident(key.clone().into())],
            body: Box::new(ArrowFunctionBody::Expr(Box::new(Expr::Unary(UnaryExpr {
                span: DUMMY_SP,
                op: UnaryOp::Delete,
                arg: Box::new(Expr::SuperProp(SuperPropExpr {
                    span: DUMMY_SP,
                    obj: Super { span: DUMMY_SP },
                    prop: SuperProp::Computed(ComputedPropName {
                        span: DUMMY_SP,
                        expr: Box::new(Expr::Ident(key)),
                    }),
                })),
            })))),
            ..Default::default()
        });
        self.0.insert(
            (owner, context),
            Stmt::Decl(Decl::Var(Box::new(VarDecl {
                kind: VarDeclKind::Const,
                decls: vec![VarDeclarator {
                    span: DUMMY_SP,
                    name: Pat::Ident(shield.bridge.into()),
                    init: Some(Box::new(primitive)),
                    definite: false,
                }],
                ..Default::default()
            }))),
        );
    }
    pub(super) fn restore(mut self, program: &mut Program) {
        fn insert(body: &mut FunctionBody, statement: Stmt) {
            let at = body
                .stmts
                .iter()
                .take_while(|statement| mangler_jsast::directives::is_directive(statement))
                .count();
            body.stmts.insert(at, statement);
        }
        struct Restore<'a>(&'a mut Plan);
        impl VisitMut for Restore<'_> {
            fn visit_mut_bin_expr(&mut self, expression: &mut BinExpr) {
                mangler_jsast::deep::walk_binary_mut(expression, self);
            }
            fn visit_mut_function(&mut self, function: &mut Function) {
                function.visit_mut_children_with(self);
                if let Some(statement) = self.0.0.remove(&(function.span.lo.0, function.ctxt)) {
                    insert(
                        function
                            .body
                            .as_mut()
                            .expect("suspended function body retained"),
                        statement,
                    );
                }
            }
            fn visit_mut_arrow_expr(&mut self, arrow: &mut ArrowExpr) {
                arrow.visit_mut_children_with(self);
                if let Some(statement) = self.0.0.remove(&(arrow.span.lo.0, arrow.ctxt)) {
                    insert(arrow_block(arrow), statement);
                }
            }
        }
        program.visit_mut_with(&mut Restore(&mut self));
        assert!(
            self.0.is_empty(),
            "suspended super deletion retains its lexical owner"
        );
    }
}
impl VisitMut for Plan {
    fn visit_mut_bin_expr(&mut self, expression: &mut BinExpr) {
        mangler_jsast::deep::walk_binary_mut(expression, self);
    }
    fn visit_mut_function(&mut self, function: &mut Function) {
        function.visit_mut_children_with(self);
        if function.is_async || function.is_generator {
            let mut shield = bridge();
            function.body.visit_mut_with(&mut shield);
            self.retain(function.span.lo.0, function.ctxt, shield);
        }
    }
    fn visit_mut_arrow_expr(&mut self, arrow: &mut ArrowExpr) {
        arrow.visit_mut_children_with(self);
        if arrow.is_async {
            let mut shield = bridge();
            arrow.body.visit_mut_with(&mut shield);
            self.retain(arrow.span.lo.0, arrow.ctxt, shield);
        }
    }
}
pub(super) fn prepare(program: &mut Program) -> Plan {
    let mut plan = Plan::default();
    program.visit_mut_with(&mut plan);
    plan
}
