//! Keep a named class's immutable inner binding across resolver and optimizer rewrites.
//!
//! SWC first resolves a class expression's superclass in its enclosing scope,
//! then visits it again inside the class scope. The first visit's marks survive,
//! incorrectly binding `class C extends C {}` to an outer `C`. Rebind references
//! free within the class to the inner class name, retaining all nested shadows.
//! The optimizer can also turn a declaration into `let C = class C {}` while
//! leaving body references marked for the mutable outer declaration.

use std::collections::HashSet;
use swc_core::ecma::ast::*;
use swc_core::ecma::visit::{Visit, VisitMut, VisitMutWith, VisitWith};

pub(crate) struct RepairClassHeritage;

impl VisitMut for RepairClassHeritage {
    fn visit_mut_class_expr(&mut self, class: &mut ClassExpr) {
        class.visit_mut_children_with(self);
        let Some(name) = &class.ident else {
            return;
        };
        let mut bindings = crate::scope_bindings::DeclaredBindings::default();
        class.class.visit_with(&mut bindings);
        class.class.visit_mut_with(&mut RebindHeritage {
            name,
            local: &bindings.0,
        });
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

/// Writes to the immutable class name can throw even when their value is unused.
/// Keep declaration identities in the test so parameters, block locals, catch
/// bindings and nested named classes with the same spelling remain unrelated.
pub(crate) fn inner_name_is_written(name: &Ident, class: &Class) -> bool {
    let mut locals = crate::scope_bindings::DeclaredBindings::default();
    class.visit_with(&mut locals);
    struct Targets<'a> {
        name: &'a Ident,
        locals: &'a HashSet<Id>,
        found: bool,
    }
    impl Visit for Targets<'_> {
        fn visit_ident(&mut self, ident: &Ident) {
            // Native source factories are recorded before the final resolver.
            // Empty contexts cannot distinguish shadows yet, so retain a
            // conservative safety bit rather than lose a possible throw.
            self.found |= ident.sym == self.name.sym
                && (self.name.ctxt == swc_core::common::SyntaxContext::empty()
                    || !self.locals.contains(&ident.to_id()));
        }
        fn visit_expr(&mut self, expression: &Expr) {
            match expression {
                Expr::Ident(ident) => self.visit_ident(ident),
                Expr::Paren(paren) => paren.expr.visit_with(self),
                Expr::TsAs(value) => value.expr.visit_with(self),
                Expr::TsTypeAssertion(value) => value.expr.visit_with(self),
                Expr::TsNonNull(value) => value.expr.visit_with(self),
                Expr::TsSatisfies(value) => value.expr.visit_with(self),
                Expr::TsInstantiation(value) => value.expr.visit_with(self),
                // Member bases and computed keys are reads, not binding writes.
                _ => {}
            }
        }
        fn visit_member_expr(&mut self, _: &MemberExpr) {}
        fn visit_super_prop_expr(&mut self, _: &SuperPropExpr) {}
        fn visit_assign_pat(&mut self, pattern: &AssignPat) {
            pattern.left.visit_with(self);
        }
        fn visit_assign_pat_prop(&mut self, property: &AssignPatProp) {
            property.key.visit_with(self);
        }
        fn visit_key_value_pat_prop(&mut self, property: &KeyValuePatProp) {
            property.value.visit_with(self);
        }
    }
    struct Writes<'a>(Targets<'a>);
    impl Visit for Writes<'_> {
        fn visit_bin_expr(&mut self, expression: &BinExpr) {
            crate::deep::walk_binary(expression, self);
        }
        fn visit_assign_expr(&mut self, assignment: &AssignExpr) {
            assignment.left.visit_with(&mut self.0);
            assignment.visit_children_with(self);
        }
        fn visit_update_expr(&mut self, update: &UpdateExpr) {
            update.arg.visit_with(&mut self.0);
            update.visit_children_with(self);
        }
        fn visit_for_head(&mut self, head: &ForHead) {
            if let ForHead::Pat(pattern) = head {
                pattern.visit_with(&mut self.0);
            }
            head.visit_children_with(self);
        }
    }
    let mut writes = Writes(Targets {
        name,
        locals: &locals.0,
        found: false,
    });
    class.visit_with(&mut writes);
    writes.0.found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Js, ParseOpts};
    use mangler_core::Language;
    use swc_core::ecma::visit::Visit;

    fn written(source: &str) -> bool {
        Js::with_globals(|| {
            let mut ast = Js.parse(source, &ParseOpts::default()).unwrap();
            Js::resolve(&mut ast);
            struct Find(Option<bool>);
            impl Visit for Find {
                fn visit_class_expr(&mut self, class: &ClassExpr) {
                    if self.0.is_none() {
                        self.0 = Some(inner_name_is_written(
                            class.ident.as_ref().unwrap(),
                            &class.class,
                        ));
                    }
                }
                fn visit_class_decl(&mut self, class: &ClassDecl) {
                    if self.0.is_none() {
                        self.0 = Some(inner_name_is_written(&class.ident, &class.class));
                    }
                }
            }
            let mut found = Find(None);
            ast.program().visit_with(&mut found);
            found.0.unwrap()
        })
    }

    #[test]
    fn immutable_name_write_detection_follows_assignment_positions() {
        for body in [
            "m(){C=null}",
            "m(){C+=1}",
            "m(){C++}",
            "m(){(C)=1}",
            "m(){[C]=[1]}",
            "m(){({value:C}={value:1})}",
            "m(){({C=1}={})}",
            "m(){for(C of [1]){}}",
            "m(){for(C in {}){}}",
            "m(){return ()=>{C=1}}",
            "static {C=1}",
            "field=(C=1)",
        ] {
            assert!(written(&format!("var holder=class C{{{body}}};")), "{body}");
            assert!(written(&format!("class C{{{body}}}")), "declaration {body}");
        }
    }

    #[test]
    fn immutable_name_write_detection_preserves_shadows_and_read_positions() {
        for body in [
            "m(){return C}",
            "m(C){C=1}",
            "m(){let C;C=1}",
            "m(){try{}catch(C){C=1}}",
            "m(){let other;({[C]:other}={})}",
            "m(){let other;[other=C]=[]}",
            "m(){C.value=1}",
            "m(){let other={};other[C]=1}",
            "m(){return class C{m(){C=1}}}",
            "m(){return function C(){C=1}}",
            "m(){for(let C of [1]){C=2}}",
        ] {
            assert!(
                !written(&format!("var holder=class C{{{body}}};")),
                "{body}"
            );
        }
    }

    #[test]
    fn class_body_repair_retains_inner_name_and_constructor_shadows() {
        Js::with_globals(|| {
            let mut ast = Js.parse(
                "let C=class C{static self=()=>C;m(){return C}constructor(C){this.value=C}shadow(C){return ()=>C}};",
                &ParseOpts::default(),
            ).unwrap();
            let (unresolved, _) = Js::resolve(&mut ast);
            let Program::Script(script) = ast.program_mut() else {
                unreachable!()
            };
            let Stmt::Decl(Decl::Var(variable)) = &mut script.body[0] else {
                unreachable!()
            };
            let Expr::Class(class) = &mut **variable.decls[0].init.as_mut().unwrap() else {
                unreachable!()
            };
            struct Names(Vec<Id>);
            impl Visit for Names {
                fn visit_ident(&mut self, id: &Ident) {
                    if id.sym == *"C" {
                        self.0.push(id.to_id());
                    }
                }
            }
            let mut expected = Names(Vec::new());
            class.class.visit_with(&mut expected);
            struct Corrupt {
                inner: swc_core::common::SyntaxContext,
                outer: swc_core::common::SyntaxContext,
            }
            impl VisitMut for Corrupt {
                fn visit_mut_ident(&mut self, id: &mut Ident) {
                    if id.sym == *"C" && id.ctxt == self.inner {
                        id.ctxt = self.outer;
                    }
                }
            }
            class.class.visit_mut_with(&mut Corrupt {
                inner: class.ident.as_ref().unwrap().ctxt,
                outer: swc_core::common::SyntaxContext::empty().apply_mark(unresolved),
            });
            class.visit_mut_with(&mut RepairClassHeritage);
            let mut actual = Names(Vec::new());
            class.class.visit_with(&mut actual);
            assert_eq!(expected.0, actual.0);
        });
    }
}
