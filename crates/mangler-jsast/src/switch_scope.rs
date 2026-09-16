//! Repair forward lexical references across the shared scope of switch cases.
//! The discriminant is evaluated outside that scope; case tests and consequents
//! all observe every direct lexical declaration, including its initial TDZ.
use std::collections::HashMap;
use swc_core::ecma::ast::*;
use swc_core::ecma::visit::{VisitMut, VisitMutWith, VisitWith};

pub(crate) struct RepairSwitchBindings;
impl VisitMut for RepairSwitchBindings {
    fn visit_mut_switch_stmt(&mut self, switch: &mut SwitchStmt) {
        switch.visit_mut_children_with(self);
        let mut bindings = HashMap::new();
        for case in &switch.cases {
            for statement in &case.cons {
                match statement {
                    Stmt::Decl(Decl::Var(declaration)) if declaration.kind != VarDeclKind::Var => {
                        for variable in &declaration.decls {
                            crate::analysis::binding_names(&variable.name, &mut |id| {
                                bindings.insert(id.sym.to_string(), id.ctxt);
                            });
                        }
                    }
                    Stmt::Decl(Decl::Class(class)) => {
                        bindings.insert(class.ident.sym.to_string(), class.ident.ctxt);
                    }
                    Stmt::Decl(Decl::Fn(function)) => {
                        bindings.insert(function.ident.sym.to_string(), function.ident.ctxt);
                    }
                    _ => {}
                }
            }
        }
        if bindings.is_empty() {
            return;
        }
        let mut declared = crate::scope_bindings::DeclaredBindings::default();
        switch.cases.visit_with(&mut declared);
        struct Rebind<'a> {
            bindings: &'a HashMap<String, swc_core::common::SyntaxContext>,
            declared: &'a std::collections::HashSet<Id>,
            own_arguments: bool,
        }
        impl VisitMut for Rebind<'_> {
            fn visit_mut_function(&mut self, function: &mut Function) {
                let previous = self.own_arguments;
                self.own_arguments = true;
                function.visit_mut_children_with(self);
                self.own_arguments = previous;
            }
            fn visit_mut_constructor(&mut self, constructor: &mut Constructor) {
                let previous = self.own_arguments;
                self.own_arguments = true;
                constructor.visit_mut_children_with(self);
                self.own_arguments = previous;
            }
            fn visit_mut_ident(&mut self, ident: &mut Ident) {
                if self.own_arguments && ident.sym.as_ref() == "arguments" {
                    return;
                }
                if let Some(context) = self.bindings.get(ident.sym.as_ref())
                    && !self.declared.contains(&ident.to_id())
                {
                    ident.ctxt = *context;
                }
            }
            fn visit_mut_labeled_stmt(&mut self, statement: &mut LabeledStmt) {
                statement.body.visit_mut_with(self);
            }
            fn visit_mut_break_stmt(&mut self, _: &mut BreakStmt) {}
            fn visit_mut_continue_stmt(&mut self, _: &mut ContinueStmt) {}
        }
        switch.cases.visit_mut_with(&mut Rebind {
            bindings: &bindings,
            declared: &declared.0,
            own_arguments: false,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Js, ParseOpts};
    use mangler_core::Language;

    #[test]
    fn forward_case_reference_binds_to_shared_lexical_scope() {
        Js::with_globals(|| {
            let mut ast = Js
                .parse(
                    "let x=1;switch(x){case 1:typeof x;case 2:let x=3}",
                    &ParseOpts::default(),
                )
                .unwrap();
            Js::resolve(&mut ast);
            let Program::Script(script) = ast.program() else {
                unreachable!()
            };
            let Stmt::Switch(switch) = &script.body[1] else {
                unreachable!()
            };
            let Expr::Ident(discriminant) = &*switch.discriminant else {
                unreachable!()
            };
            let Stmt::Expr(reference) = &switch.cases[0].cons[0] else {
                unreachable!()
            };
            let Expr::Unary(reference) = &*reference.expr else {
                unreachable!()
            };
            let Expr::Ident(reference) = &*reference.arg else {
                unreachable!()
            };
            let Stmt::Decl(Decl::Var(declaration)) = &switch.cases[1].cons[0] else {
                unreachable!()
            };
            let Pat::Ident(binding) = &declaration.decls[0].name else {
                unreachable!()
            };
            assert_eq!(reference.to_id(), binding.id.to_id());
            assert_ne!(discriminant.to_id(), binding.id.to_id());
        });
    }
}

/// Retain lexical declaration boundaries when suspension lowering turns a
/// generator/async declaration into an ordinary function declaration.
#[derive(Default)]
pub struct SuspensionDeclarations(std::collections::HashSet<u32>);
impl SuspensionDeclarations {
    pub fn contains(&self, ident: &Ident) -> bool {
        self.0.contains(&ident.span.lo.0)
    }
    pub fn capture(program: &Program) -> Self {
        use swc_core::ecma::visit::Visit;
        struct Collect(std::collections::HashSet<u32>);
        impl Visit for Collect {
            fn visit_bin_expr(&mut self, expression: &BinExpr) {
                crate::deep::walk_binary(expression, self);
            }
            fn visit_switch_stmt(&mut self, switch: &SwitchStmt) {
                for case in &switch.cases {
                    for statement in &case.cons {
                        if let Stmt::Decl(Decl::Fn(function)) = statement
                            && (function.function.is_async || function.function.is_generator)
                        {
                            self.0.insert(function.ident.span.lo.0);
                        }
                    }
                }
                switch.visit_children_with(self);
            }
        }
        let mut collect = Collect(Default::default());
        program.visit_with(&mut collect);
        Self(collect.0)
    }
    pub fn protect(&self, program: &mut Program) {
        if self.0.is_empty() {
            return;
        }
        use swc_core::common::{DUMMY_SP, SyntaxContext};
        struct Restore(std::collections::HashSet<u32>);
        impl VisitMut for Restore {
            fn visit_mut_bin_expr(&mut self, expression: &mut BinExpr) {
                crate::deep::walk_binary_mut(expression, self);
            }
            fn visit_mut_stmt(&mut self, statement: &mut Stmt) {
                statement.visit_mut_children_with(self);
                let Stmt::Switch(switch) = statement else {
                    return;
                };
                // An enclosing lexical binding blocks Annex B's synthetic var
                // alias. The same resolved ID keeps both guard and declaration
                // spelled alike; the unused guard never replaces the binding
                // instantiated by the original CaseBlock.
                let guards: Vec<_> = switch
                    .cases
                    .iter()
                    .flat_map(|case| &case.cons)
                    .filter_map(|statement| {
                        if let Stmt::Decl(Decl::Fn(function)) = statement
                            && self.0.contains(&function.ident.span.lo.0)
                        {
                            Some(VarDeclarator {
                                span: DUMMY_SP,
                                name: Pat::Ident(function.ident.clone().into()),
                                init: None,
                                definite: false,
                            })
                        } else {
                            None
                        }
                    })
                    .collect();
                if guards.is_empty() {
                    return;
                }
                let value = Ident::new_private("_switch_value".into(), DUMMY_SP);
                let discriminant = std::mem::replace(
                    &mut switch.discriminant,
                    Box::new(Expr::Ident(value.clone())),
                );
                let bind = |decls| {
                    Stmt::Decl(Decl::Var(Box::new(VarDecl {
                        span: DUMMY_SP,
                        ctxt: SyntaxContext::empty(),
                        kind: VarDeclKind::Let,
                        declare: false,
                        decls,
                    })))
                };
                let initialize = bind(vec![VarDeclarator {
                    span: DUMMY_SP,
                    name: Pat::Ident(value.into()),
                    init: Some(discriminant),
                    definite: false,
                }]);
                // Present declarations as lexical bindings only while hygiene
                // computes names. Restore their hoisted declaration syntax after
                // hygiene, in this same CaseBlock, before any source execution.
                for case in &mut switch.cases {
                    for statement in &mut case.cons {
                        if matches!(statement, Stmt::Decl(Decl::Fn(function)) if self.0.contains(&function.ident.span.lo.0))
                        {
                            let Stmt::Decl(Decl::Fn(function)) = std::mem::replace(
                                statement,
                                Stmt::Empty(EmptyStmt { span: DUMMY_SP }),
                            ) else {
                                unreachable!()
                            };
                            let span = function.ident.span;
                            *statement = bind(vec![VarDeclarator {
                                span,
                                name: Pat::Ident(function.ident.into()),
                                init: Some(Box::new(Expr::Fn(FnExpr {
                                    ident: None,
                                    function: function.function,
                                }))),
                                definite: false,
                            }]);
                        }
                    }
                }
                let switch =
                    std::mem::replace(statement, Stmt::Empty(EmptyStmt { span: DUMMY_SP }));
                *statement = Stmt::Block(BlockStmt {
                    span: DUMMY_SP,
                    ctxt: SyntaxContext::empty(),
                    stmts: vec![
                        initialize,
                        Stmt::Block(BlockStmt {
                            span: DUMMY_SP,
                            ctxt: SyntaxContext::empty(),
                            stmts: vec![bind(guards), switch],
                        }),
                    ],
                });
            }
        }
        program.visit_mut_with(&mut Restore(self.0.clone()));
    }
    pub fn hygiene(
        &self,
        program: &mut Program,
        config: swc_core::ecma::transforms::base::hygiene::Config,
    ) {
        if self.0.is_empty() {
            swc_core::ecma::transforms::base::hygiene::hygiene_with_config(config).process(program);
            return;
        }
        use swc_core::ecma::transforms::base::rename::{Renamer, renamer};
        // Use SWC's normal scope/name allocator with its supported preservation
        // hook, keeping callable .name without post-hoc textual rebinding.
        struct Names(std::collections::HashSet<Id>);
        impl Renamer for Names {
            type Target = swc_core::atoms::Atom;
            const MANGLE: bool = false;
            const RESET_N: bool = true;
            fn preserve_name(&self, id: &Id) -> bool {
                self.0.contains(id)
            }
            fn new_name_for(&self, id: &Id, n: &mut usize) -> swc_core::atoms::Atom {
                let name = if *n == 0 {
                    id.0.clone()
                } else {
                    format!("{}{}", id.0, n).into()
                };
                *n += 1;
                name
            }
        }
        struct Collect<'a>(
            &'a std::collections::HashSet<u32>,
            std::collections::HashSet<Id>,
        );
        impl swc_core::ecma::visit::Visit for Collect<'_> {
            fn visit_bin_expr(&mut self, expression: &BinExpr) {
                crate::deep::walk_binary(expression, self);
            }
            fn visit_binding_ident(&mut self, binding: &BindingIdent) {
                if self.0.contains(&binding.id.span.lo.0) {
                    self.1.insert(binding.id.to_id());
                }
            }
        }
        let mut collect = Collect(&self.0, Default::default());
        program.visit_with(&mut collect);
        renamer(config, Names(collect.1)).process(program);
        struct Clear;
        impl VisitMut for Clear {
            fn visit_mut_bin_expr(&mut self, expression: &mut BinExpr) {
                crate::deep::walk_binary_mut(expression, self);
            }
            fn visit_mut_ident(&mut self, ident: &mut Ident) {
                ident.ctxt = Default::default();
            }
        }
        program.visit_mut_with(&mut Clear);
    }
    pub fn restore(self, program: &mut Program) {
        if self.0.is_empty() {
            return;
        }
        struct Restore(std::collections::HashSet<u32>);
        impl VisitMut for Restore {
            fn visit_mut_bin_expr(&mut self, expression: &mut BinExpr) {
                crate::deep::walk_binary_mut(expression, self);
            }
            fn visit_mut_stmt(&mut self, statement: &mut Stmt) {
                statement.visit_mut_children_with(self);
                let Stmt::Decl(Decl::Var(variable)) = statement else {
                    return;
                };
                if variable.kind != VarDeclKind::Let || variable.decls.len() != 1 {
                    return;
                }
                let declaration = &variable.decls[0];
                if !self.0.contains(&declaration.span.lo.0)
                    || !matches!(&declaration.name, Pat::Ident(_))
                    || !matches!(&declaration.init, Some(expr) if matches!(&**expr, Expr::Fn(function) if function.ident.is_none()))
                {
                    return;
                }
                let declaration = variable.decls.pop().unwrap();
                let Pat::Ident(binding) = declaration.name else {
                    unreachable!()
                };
                let Expr::Fn(function) = *declaration.init.unwrap() else {
                    unreachable!()
                };
                *statement = Stmt::Decl(Decl::Fn(FnDecl {
                    ident: binding.id,
                    declare: false,
                    function: function.function,
                }));
            }
        }
        program.visit_mut_with(&mut Restore(self.0));
    }
}
