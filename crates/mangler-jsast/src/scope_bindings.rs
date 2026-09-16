//! Binding positions shared by resolver repairs. Assignment targets are references.
use std::collections::HashSet;
use swc_core::ecma::ast::*;
use swc_core::ecma::visit::{Visit, VisitWith};

#[derive(Default)]
pub(crate) struct DeclaredBindings(pub HashSet<Id>);

impl Visit for DeclaredBindings {
    // BindingIdent also represents assignment targets. Only declaration and
    // parameter positions bind names; a generated capture setter `C = value`
    // must not conceal a free reference to the class's inner TDZ binding.
    fn visit_binding_ident(&mut self, _: &BindingIdent) {}

    fn visit_var_declarator(&mut self, declaration: &VarDeclarator) {
        crate::analysis::binding_names(&declaration.name, &mut |id| {
            self.0.insert(id.to_id());
        });
        declaration.visit_children_with(self);
    }

    fn visit_function(&mut self, function: &Function) {
        for parameter in &function.params {
            crate::analysis::binding_names(&parameter.pat, &mut |id| {
                self.0.insert(id.to_id());
            });
        }
        function.visit_children_with(self);
    }

    fn visit_arrow_expr(&mut self, arrow: &ArrowExpr) {
        for parameter in &arrow.params {
            crate::analysis::binding_names(parameter, &mut |id| {
                self.0.insert(id.to_id());
            });
        }
        arrow.visit_children_with(self);
    }

    fn visit_constructor(&mut self, constructor: &Constructor) {
        for parameter in &constructor.params {
            if let ParamOrTsParamProp::Param(parameter) = parameter {
                crate::analysis::binding_names(&parameter.pat, &mut |id| {
                    self.0.insert(id.to_id());
                });
            }
        }
        constructor.visit_children_with(self);
    }

    fn visit_catch_clause(&mut self, catch: &CatchClause) {
        if let Some(parameter) = &catch.param {
            crate::analysis::binding_names(parameter, &mut |id| {
                self.0.insert(id.to_id());
            });
        }
        catch.visit_children_with(self);
    }

    fn visit_fn_decl(&mut self, function: &FnDecl) {
        self.0.insert(function.ident.to_id());
        function.function.visit_with(self);
    }

    fn visit_fn_expr(&mut self, function: &FnExpr) {
        if let Some(ident) = &function.ident {
            self.0.insert(ident.to_id());
        }
        function.function.visit_with(self);
    }

    fn visit_class_decl(&mut self, class: &ClassDecl) {
        self.0.insert(class.ident.to_id());
        class.class.visit_with(self);
    }

    fn visit_class_expr(&mut self, class: &ClassExpr) {
        if let Some(ident) = &class.ident {
            self.0.insert(ident.to_id());
        }
        class.class.visit_with(self);
    }
}
