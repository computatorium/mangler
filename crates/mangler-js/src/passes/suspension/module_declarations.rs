//! Module suspension declarations exist at linking, before their module's
//! evaluation. Record native envelopes and make async entries produce the raw
//! state iterator consumed by the declaration's native suspension kernel.
use super::*;

#[derive(Default)]
pub(super) struct Declarations(pub HashMap<u32, NativeDeclaration>);
impl Declarations {
    pub fn collect(program: &Program) -> Self {
        let Program::Module(module) = program else {
            return Self::default();
        };
        Self(
            module
                .body
                .iter()
                .filter_map(|item| {
                    let function = match item {
                        ModuleItem::Stmt(Stmt::Decl(Decl::Fn(function))) => &function.function,
                        ModuleItem::ModuleDecl(ModuleDecl::ExportDecl(ExportDecl {
                            decl: Decl::Fn(function),
                            ..
                        })) => &function.function,
                        ModuleItem::ModuleDecl(ModuleDecl::ExportDefaultDecl(
                            ExportDefaultDecl {
                                decl: DefaultDecl::Fn(function),
                                ..
                            },
                        )) => &function.function,
                        _ => return None,
                    };
                    suspension_kind(function.is_async, function.is_generator).map(|kind| {
                        (
                            function.span.lo.0,
                            NativeDeclaration {
                                kind,
                                finally_depth: 0,
                            },
                        )
                    })
                })
                .collect(),
        )
    }

    pub fn lower(&mut self, program: &mut Program) {
        self.producers(program, SuspensionKind::Async, "_async_to_generator");
        self.producers(
            program,
            SuspensionKind::AsyncGenerator,
            "_wrap_async_generator",
        );
    }

    pub fn prepare_generators(&mut self, program: &mut Program) {
        struct Prepare<'a>(&'a mut HashMap<u32, NativeDeclaration>);
        impl VisitMut for Prepare<'_> {
            fn visit_mut_function(&mut self, function: &mut Function) {
                if let Some(declaration) = self.0.get_mut(&function.span.lo.0)
                    && declaration.kind != SuspensionKind::Async
                {
                    declaration.finally_depth = kernel::prepare_events(
                        function.body.as_mut().expect("native generator body"),
                    );
                }
                function.visit_mut_children_with(self);
            }
            fn visit_mut_bin_expr(&mut self, expression: &mut BinExpr) {
                mangler_jsast::deep::walk_binary_mut(expression, self);
            }
        }
        program.visit_mut_with(&mut Prepare(&mut self.0));
    }

    fn producers(&mut self, program: &mut Program, kind: SuspensionKind, helper: &str) {
        struct Outer<'a> {
            helper: &'a str,
            kind: SuspensionKind,
            count: usize,
            generator: Option<Function>,
        }
        impl VisitMut for Outer<'_> {
            fn visit_mut_function(&mut self, _: &mut Function) {}
            fn visit_mut_arrow_expr(&mut self, _: &mut ArrowExpr) {}
            fn visit_mut_expr(&mut self, expression: &mut Expr) {
                if let Expr::Call(call) = expression
                    && matches!(&call.callee, Callee::Expr(callee) if matches!(&**callee, Expr::Ident(id) if id.span.is_dummy() && id.sym == self.helper))
                    && call.args.len() == 1
                    && matches!(&*call.args[0].expr, Expr::Fn(_))
                {
                    let value = *call.args.remove(0).expr;
                    if self.kind == SuspensionKind::AsyncGenerator {
                        let Expr::Fn(generator) = value else {
                            unreachable!()
                        };
                        self.generator = Some(*generator.function);
                        *expression = Expr::Invalid(Invalid {
                            span: swc_core::common::DUMMY_SP,
                        });
                    } else {
                        *expression = value;
                    }
                    self.count += 1;
                } else {
                    expression.visit_mut_children_with(self);
                }
            }
            fn visit_mut_bin_expr(&mut self, expression: &mut BinExpr) {
                mangler_jsast::deep::walk_binary_mut(expression, self);
            }
        }
        struct Lower<'a> {
            declarations: &'a mut HashMap<u32, NativeDeclaration>,
            kind: SuspensionKind,
            helper: &'a str,
        }
        impl VisitMut for Lower<'_> {
            fn visit_mut_function(&mut self, function: &mut Function) {
                if let Some(declaration) = self.declarations.get_mut(&function.span.lo.0)
                    && declaration.kind == self.kind
                {
                    let mut outer = Outer {
                        helper: self.helper,
                        kind: self.kind,
                        count: 0,
                        generator: None,
                    };
                    function.body.visit_mut_with(&mut outer);
                    assert_eq!(
                        outer.count, 1,
                        "module suspension declaration owns one state producer"
                    );
                    if let Some(generator) = outer.generator {
                        // SWC retains liftable parameters on the declaration,
                        // otherwise its producer owns the source parameter list
                        // and the outer list contains only arity placeholders.
                        // Restore whichever list owns source initialization.
                        if !generator.params.is_empty() {
                            function.params = generator.params;
                        }
                        let body = function
                            .body
                            .as_mut()
                            .expect("module generator wrapper body");
                        assert!(
                            matches!(body.stmts.pop(), Some(Stmt::Return(_))),
                            "module generator wrapper ends with its producer call"
                        );
                        body.stmts.extend(
                            generator
                                .body
                                .expect("module generator producer body")
                                .stmts,
                        );
                        function.is_generator = true;
                    }
                }
                function.visit_mut_children_with(self);
            }
            fn visit_mut_bin_expr(&mut self, expression: &mut BinExpr) {
                mangler_jsast::deep::walk_binary_mut(expression, self);
            }
        }
        program.visit_mut_with(&mut Lower {
            declarations: &mut self.0,
            kind,
            helper,
        });
    }
}
