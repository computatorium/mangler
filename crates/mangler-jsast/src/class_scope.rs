//! Correct the upstream resolver's double visit of named-class heritage.
//!
//! SWC first resolves a class expression's superclass in its enclosing scope,
//! then visits it again inside the class scope. The first visit's marks survive,
//! incorrectly binding `class C extends C {}` to an outer `C`. Rebind references
//! free within heritage to the inner class name, retaining all nested shadows.

use std::collections::HashSet;
use swc_core::ecma::ast::*;
use swc_core::ecma::visit::{Visit, VisitMut, VisitMutWith, VisitWith};

pub(crate) struct RepairClassHeritage;

impl VisitMut for RepairClassHeritage {
    fn visit_mut_class_expr(&mut self, class: &mut ClassExpr) {
        class.visit_mut_children_with(self);
        let (Some(name), Some(heritage)) = (&class.ident, &mut class.class.super_class) else {
            return;
        };
        let mut bindings = HeritageBindings::default();
        heritage.visit_with(&mut bindings);
        heritage.visit_mut_with(&mut RebindHeritage {
            name,
            local: &bindings.0,
        });
    }
}

#[derive(Default)]
struct HeritageBindings(HashSet<Id>);

impl Visit for HeritageBindings {
    fn visit_binding_ident(&mut self, ident: &BindingIdent) {
        self.0.insert(ident.id.to_id());
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

struct RebindHeritage<'a> {
    name: &'a Ident,
    local: &'a HashSet<Id>,
}

impl VisitMut for RebindHeritage<'_> {
    fn visit_mut_ident(&mut self, ident: &mut Ident) {
        if ident.sym == self.name.sym && !self.local.contains(&ident.to_id()) {
            ident.ctxt = self.name.ctxt;
        }
    }

    fn visit_mut_labeled_stmt(&mut self, stmt: &mut LabeledStmt) {
        stmt.body.visit_mut_with(self);
    }
    fn visit_mut_break_stmt(&mut self, _: &mut BreakStmt) {}
    fn visit_mut_continue_stmt(&mut self, _: &mut ContinueStmt) {}
}
