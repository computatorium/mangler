//! Keep class execution contexts separate from the enclosing state machine.
//!
//! SWC's generator visitor skips function bodies but walks constructors and
//! static blocks as though their statements belonged to the outer generator.
//! Temporary function frames make that boundary explicit without moving source
//! evaluation: the frames disappear immediately after the transform.
use super::*;
use swc_core::common::{DUMMY_SP, SyntaxContext};

pub(super) struct ClassFrames(HashSet<SyntaxContext>);

impl ClassFrames {
    pub(super) fn take(program: &mut Program) -> Self {
        struct Take(HashSet<SyntaxContext>);
        impl Take {
            fn block(&mut self, span: swc_core::common::Span, statements: &mut Vec<Stmt>) {
                let ctxt = SyntaxContext::empty().apply_mark(Mark::new());
                self.0.insert(ctxt);
                let body = std::mem::take(statements);
                statements.push(Stmt::Expr(ExprStmt {
                    span: DUMMY_SP,
                    expr: Box::new(Expr::Fn(FnExpr {
                        ident: None,
                        function: Box::new(Function {
                            ctxt,
                            body: Some(FunctionBody { span, stmts: body }),
                            ..Default::default()
                        }),
                    })),
                }));
            }
        }
        impl VisitMut for Take {
            fn visit_mut_bin_expr(&mut self, binary: &mut BinExpr) {
                mangler_jsast::deep::walk_binary_mut(binary, self);
            }
            fn visit_mut_constructor(&mut self, constructor: &mut Constructor) {
                constructor.visit_mut_children_with(self);
                if let Some(body) = &mut constructor.body {
                    self.block(body.span, &mut body.stmts);
                }
            }
            fn visit_mut_static_block(&mut self, block: &mut StaticBlock) {
                block.visit_mut_children_with(self);
                self.block(block.body.span, &mut block.body.stmts);
            }
        }
        let mut take = Take(HashSet::new());
        program.visit_mut_with(&mut take);
        Self(take.0)
    }

    pub(super) fn restore(self, program: &mut Program) {
        struct Restore(HashSet<SyntaxContext>);
        impl Restore {
            fn block(&mut self, statements: &mut Vec<Stmt>) {
                let [Stmt::Expr(statement)] = statements.as_mut_slice() else {
                    return;
                };
                let Expr::Fn(function) = &mut *statement.expr else {
                    return;
                };
                if self.0.remove(&function.function.ctxt) {
                    *statements = std::mem::take(
                        &mut function
                            .function
                            .body
                            .as_mut()
                            .expect("class execution frame has a body")
                            .stmts,
                    );
                }
            }
        }
        impl VisitMut for Restore {
            fn visit_mut_bin_expr(&mut self, binary: &mut BinExpr) {
                mangler_jsast::deep::walk_binary_mut(binary, self);
            }
            fn visit_mut_constructor(&mut self, constructor: &mut Constructor) {
                constructor.visit_mut_children_with(self);
                if let Some(body) = &mut constructor.body {
                    self.block(&mut body.stmts);
                }
            }
            fn visit_mut_static_block(&mut self, block: &mut StaticBlock) {
                block.visit_mut_children_with(self);
                self.block(&mut block.body.stmts);
            }
        }
        let mut restore = Restore(self.0);
        program.visit_mut_with(&mut restore);
        // The state-machine builder drops unreachable statements. Their frames
        // need no restoration, but no surviving private frame may escape.
        struct Remaining<'a>(&'a HashSet<SyntaxContext>);
        impl Visit for Remaining<'_> {
            fn visit_bin_expr(&mut self, binary: &BinExpr) {
                mangler_jsast::deep::walk_binary(binary, self);
            }
            fn visit_function(&mut self, function: &Function) {
                assert!(
                    !self.0.contains(&function.ctxt),
                    "class execution frame restored"
                );
                function.visit_children_with(self);
            }
        }
        program.visit_with(&mut Remaining(&restore.0));
    }
}
