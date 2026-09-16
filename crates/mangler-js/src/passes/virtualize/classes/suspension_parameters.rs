//! Keep method-parameter class capabilities in their original home object.
//! Async compatibility lowering moves parameter execution into an ordinary
//! function. Parameter expressions still compile to bytecode; only the existing
//! lexical operation primitives stay in the native method wrapper.
use super::*;
use swc_core::ecma::visit::{Visit, VisitWith};

pub(in super::super) struct Parameters(std::collections::HashMap<u32, Vec<Stmt>>);
impl Parameters {
    pub(in super::super) fn prepare(
        program: &mut Program,
        selected: &std::collections::HashSet<u32>,
        cfg: &FileConfig,
        apply: &str,
        iterator: &IteratorAlias<'_>,
    ) -> Self {
        struct Prepare<'a> {
            selected: &'a std::collections::HashSet<u32>,
            cfg: &'a FileConfig,
            apply: &'a str,
            iterator: &'a IteratorAlias<'a>,
            declarations: std::collections::HashMap<u32, Vec<Stmt>>,
        }
        #[derive(Default)]
        struct Super(bool);
        impl Visit for Super {
            fn visit_bin_expr(&mut self, binary: &BinExpr) {
                mangler_jsast::deep::walk_binary(binary, self);
            }
            fn visit_super_prop_expr(&mut self, _: &SuperPropExpr) {
                self.0 = true;
            }
        }
        impl Prepare<'_> {
            fn method(&mut self, function: &mut Function) {
                if !function.is_async || !self.selected.contains(&function.span.lo.0) {
                    return;
                }
                let mut has_super = Super::default();
                function.params.visit_with(&mut has_super);
                if !has_super.0 {
                    return;
                }
                let mut parameters = Function {
                    params: std::mem::take(&mut function.params),
                    ..Default::default()
                };
                let (prepared, declarations) =
                    super::prepare(&mut parameters, self.cfg, self.apply, self.iterator);
                function.params = prepared.params;
                if !declarations.is_empty() {
                    self.declarations.insert(function.span.lo.0, declarations);
                }
            }
        }
        impl VisitMut for Prepare<'_> {
            fn visit_mut_bin_expr(&mut self, binary: &mut BinExpr) {
                mangler_jsast::deep::walk_binary_mut(binary, self);
            }
            fn visit_mut_method_prop(&mut self, method: &mut MethodProp) {
                method.visit_mut_children_with(self);
                self.method(&mut method.function);
            }
            fn visit_mut_class_method(&mut self, method: &mut ClassMethod) {
                method.visit_mut_children_with(self);
                self.method(&mut method.function);
            }
            fn visit_mut_private_method(&mut self, method: &mut PrivateMethod) {
                method.visit_mut_children_with(self);
                self.method(&mut method.function);
            }
        }
        let mut prepare = Prepare {
            selected,
            cfg,
            apply,
            iterator,
            declarations: Default::default(),
        };
        program.visit_mut_with(&mut prepare);
        Self(prepare.declarations)
    }
    pub(in super::super) fn restore(self, program: &mut Program) {
        struct Restore(std::collections::HashMap<u32, Vec<Stmt>>);
        impl Restore {
            fn method(&mut self, function: &mut Function) {
                if let Some(declarations) = self.0.remove(&function.span.lo.0) {
                    // Some defaults stay in the native method parameter scope.
                    // Its body bindings are invisible to closures created there;
                    // materialize each compiler primitive directly at those uses.
                    let mut primitives = std::collections::HashMap::new();
                    for declaration in &declarations {
                        let Stmt::Decl(Decl::Var(declaration)) = declaration else {
                            unreachable!("class primitive declaration")
                        };
                        for declaration in &declaration.decls {
                            let Pat::Ident(name) = &declaration.name else {
                                unreachable!("class primitive binding")
                            };
                            primitives.insert(
                                name.id.sym.clone(),
                                declaration
                                    .init
                                    .as_ref()
                                    .expect("class primitive initializer")
                                    .clone(),
                            );
                        }
                    }
                    struct Parameters(std::collections::HashMap<swc_core::atoms::Atom, Box<Expr>>);
                    impl VisitMut for Parameters {
                        fn visit_mut_bin_expr(&mut self, binary: &mut BinExpr) {
                            mangler_jsast::deep::walk_binary_mut(binary, self);
                        }
                        fn visit_mut_expr(&mut self, expression: &mut Expr) {
                            if let Expr::Ident(name) = expression
                                && let Some(primitive) = self.0.get(&name.sym)
                            {
                                *expression = *primitive.clone();
                                return;
                            }
                            expression.visit_mut_children_with(self);
                        }
                    }
                    function.params.visit_mut_with(&mut Parameters(primitives));
                    let body = function.body.as_mut().expect("suspended method has a body");
                    let at = mangler_jsast::directives::leading_directive_count(&body.stmts);
                    body.stmts.splice(at..at, declarations);
                }
            }
        }
        impl VisitMut for Restore {
            fn visit_mut_bin_expr(&mut self, binary: &mut BinExpr) {
                mangler_jsast::deep::walk_binary_mut(binary, self);
            }
            fn visit_mut_method_prop(&mut self, method: &mut MethodProp) {
                self.method(&mut method.function);
                method.visit_mut_children_with(self);
            }
            fn visit_mut_class_method(&mut self, method: &mut ClassMethod) {
                self.method(&mut method.function);
                method.visit_mut_children_with(self);
            }
            fn visit_mut_private_method(&mut self, method: &mut PrivateMethod) {
                self.method(&mut method.function);
                method.visit_mut_children_with(self);
            }
        }
        program.visit_mut_with(&mut Restore(self.0));
    }
}
