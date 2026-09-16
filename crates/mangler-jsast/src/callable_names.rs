//! Conservatively retain callable names unless every callable is used only at
//! direct call sites. This lets arithmetic helper declarations disappear without
//! changing names on function values passed to application code or reflection.
use std::collections::HashSet;
use swc_core::common::Mark;
use swc_core::ecma::ast::*;
use swc_core::ecma::visit::{Visit, VisitWith};

/// SWC's keep_fnames option covers explicit names but its variable inliner can
/// still erase NamedEvaluation: `let f=()=>1; f.name` becomes `(()=>1).name`.
/// Only bindings that can expose their function value need this extra guard.
pub(crate) fn inferred_observable(program: &Program, top_level: Mark) -> bool {
    inferred(InferredSource::Program(program, top_level))
}

/// Source expressions embedded in native factories are serialized and lose
/// original spans/resolver IDs. Retain the inferred-name guard conservatively
/// for those expressions, independently of their generated table container.
pub(crate) fn native_inferred(expression: &Expr) -> bool {
    inferred(InferredSource::Native(expression))
}

enum InferredSource<'a> {
    Program(&'a Program, Mark),
    Native(&'a Expr),
}

fn inferred(source: InferredSource<'_>) -> bool {
    fn anonymous(mut value: &Expr) -> bool {
        while let Expr::Paren(p) = value {
            value = &p.expr;
        }
        matches!(value, Expr::Arrow(_) | Expr::Fn(FnExpr { ident: None, .. }))
    }
    #[derive(Default)]
    struct Inferred(HashSet<Id>, bool);
    impl Visit for Inferred {
        fn visit_bin_expr(&mut self, e: &BinExpr) {
            crate::deep::walk_binary(e, self);
        }
        fn visit_var_declarator(&mut self, d: &VarDeclarator) {
            if (self.1 || !d.span.is_dummy())
                && let (Pat::Ident(id), Some(value)) = (&d.name, &d.init)
                && anonymous(value)
            {
                self.0.insert(id.to_id());
            }
            d.visit_children_with(self);
        }
        fn visit_assign_expr(&mut self, assignment: &AssignExpr) {
            if (self.1 || !assignment.span.is_dummy())
                && anonymous(&assignment.right)
                && let AssignTarget::Simple(SimpleAssignTarget::Ident(id)) = &assignment.left
            {
                self.0.insert(id.to_id());
            }
            assignment.visit_children_with(self);
        }
        fn visit_assign_pat(&mut self, pattern: &AssignPat) {
            if (self.1 || !pattern.span.is_dummy())
                && anonymous(&pattern.right)
                && let Pat::Ident(id) = &*pattern.left
            {
                self.0.insert(id.to_id());
            }
            pattern.visit_children_with(self);
        }
        fn visit_assign_pat_prop(&mut self, pattern: &AssignPatProp) {
            if (self.1 || !pattern.span.is_dummy())
                && pattern.value.as_ref().is_some_and(|value| anonymous(value))
            {
                self.0.insert(pattern.key.to_id());
            }
            pattern.visit_children_with(self);
        }
    }
    let mut inferred = Inferred(HashSet::new(), matches!(&source, InferredSource::Native(_)));
    match &source {
        InferredSource::Program(program, _) => program.visit_with(&mut inferred),
        InferredSource::Native(expression) => expression.visit_with(&mut inferred),
    }
    if inferred.0.is_empty() {
        return false;
    }
    let InferredSource::Program(program, top_level) = source else {
        return true;
    };
    if inferred.0.iter().any(|(_, ctxt)| ctxt.outer() == top_level) {
        return true;
    }
    struct References<'a> {
        inferred: &'a HashSet<Id>,
        escapes: bool,
    }
    impl Visit for References<'_> {
        fn visit_bin_expr(&mut self, e: &BinExpr) {
            crate::deep::walk_binary(e, self);
        }
        fn visit_binding_ident(&mut self, _: &BindingIdent) {}
        fn visit_ident(&mut self, id: &Ident) {
            self.escapes |= self.inferred.contains(&id.to_id());
        }
        fn visit_with_stmt(&mut self, _: &WithStmt) {
            self.escapes = true;
        }
        fn visit_call_expr(&mut self, call: &CallExpr) {
            self.escapes |= crate::analysis::scope::is_direct_eval_callee(&call.callee);
            if let Callee::Expr(callee) = &call.callee {
                let mut value = &**callee;
                while let Expr::Paren(p) = value {
                    value = &p.expr;
                }
                if matches!(value, Expr::Ident(id) if self.inferred.contains(&id.to_id())) {
                    call.args.visit_with(self);
                    return;
                }
            }
            call.visit_children_with(self);
        }
    }
    let mut references = References {
        inferred: &inferred.0,
        escapes: false,
    };
    program.visit_with(&mut references);
    references.escapes
}

pub(crate) fn observable(program: &Program, top_level: Mark) -> bool {
    struct Declarations(HashSet<Id>);
    impl Visit for Declarations {
        fn visit_bin_expr(&mut self, e: &BinExpr) {
            crate::deep::walk_binary(e, self);
        }
        fn visit_fn_decl(&mut self, f: &FnDecl) {
            self.0.insert(f.ident.to_id());
            f.function.visit_with(self);
        }
        fn visit_fn_expr(&mut self, f: &FnExpr) {
            if let Some(id) = &f.ident {
                self.0.insert(id.to_id());
            }
            f.function.visit_with(self);
        }
    }
    let mut declarations = Declarations(HashSet::new());
    program.visit_with(&mut declarations);
    struct Scan<'a> {
        functions: &'a HashSet<Id>,
        top_level: Mark,
        observable: bool,
    }
    fn unparen(mut expression: &Expr) -> &Expr {
        while let Expr::Paren(p) = expression {
            expression = &p.expr;
        }
        expression
    }
    impl Visit for Scan<'_> {
        fn visit_bin_expr(&mut self, e: &BinExpr) {
            crate::deep::walk_binary(e, self);
        }
        fn visit_fn_decl(&mut self, f: &FnDecl) {
            if f.ident.ctxt.outer() == self.top_level {
                self.observable = true;
            }
            f.function.visit_with(self);
        }
        fn visit_ident(&mut self, id: &Ident) {
            if self.functions.contains(&id.to_id()) || id.sym == *"arguments" {
                self.observable = true;
            }
        }
        fn visit_fn_expr(&mut self, _: &FnExpr) {
            self.observable = true;
        }
        fn visit_arrow_expr(&mut self, _: &ArrowExpr) {
            self.observable = true;
        }
        fn visit_class(&mut self, _: &Class) {
            self.observable = true;
        }
        fn visit_method_prop(&mut self, _: &MethodProp) {
            self.observable = true;
        }
        fn visit_getter_prop(&mut self, _: &GetterProp) {
            self.observable = true;
        }
        fn visit_setter_prop(&mut self, _: &SetterProp) {
            self.observable = true;
        }
        fn visit_with_stmt(&mut self, _: &WithStmt) {
            self.observable = true;
        }
        fn visit_member_prop(&mut self, prop: &MemberProp) {
            if matches!(prop, MemberProp::Ident(id) if id.sym == *"caller" || id.sym == *"callee") {
                self.observable = true;
            }
            prop.visit_children_with(self);
        }
        fn visit_call_expr(&mut self, call: &CallExpr) {
            if crate::analysis::scope::is_direct_eval_callee(&call.callee) {
                self.observable = true;
            }
            match &call.callee {
                Callee::Expr(callee) => match unparen(callee) {
                    Expr::Ident(id) if self.functions.contains(&id.to_id()) => {}
                    Expr::Fn(function) => function.function.visit_with(self),
                    Expr::Arrow(arrow) => arrow.visit_children_with(self),
                    _ => callee.visit_with(self),
                },
                _ => call.callee.visit_with(self),
            }
            call.args.visit_with(self);
        }
    }
    let mut scan = Scan {
        functions: &declarations.0,
        top_level,
        observable: false,
    };
    program.visit_with(&mut scan);
    scan.observable
}

#[cfg(test)]
mod tests {
    use crate::{Js, ParseOpts};
    use mangler_core::Language;
    #[test]
    fn inferred_names_are_guarded_only_when_function_values_escape() {
        Js::with_globals(|| {
            for (source, expected) in [
                ("function pay(){let f=()=>1;return f()}", false),
                ("function pay(){let f=()=>1;return f.name}", true),
                (
                    "function pay(){let f=(function(){});return consume(f)}",
                    true,
                ),
                ("function pay(){let {f=()=>1}={};return f.name}", true),
                ("function pay(){let f=()=>1;return eval('f.name')}", true),
            ] {
                let mut ast = Js.parse(source, &ParseOpts::default()).unwrap();
                let (_, top) = Js::resolve(&mut ast);
                assert_eq!(
                    super::inferred_observable(ast.program(), top),
                    expected,
                    "{source}"
                );
            }
        });
    }
    #[test]
    fn only_direct_local_callables_allow_name_elision() {
        Js::with_globals(|| {
            for (source, expected) in [
                (
                    "(function(){function sum(a,b){return a+b}out=sum(1,2)})()",
                    false,
                ),
                ("(function(){function sum(){}out=sum.name})()", true),
                ("(function(){function sum(){}out=sum})()", true),
                ("(function(){let f=function named(){};out=f.name})()", true),
                ("function sum(){}sum()", true),
                (
                    "(function(){function sum(){return arguments.callee.name}out=sum()})()",
                    true,
                ),
            ] {
                let mut ast = Js.parse(source, &ParseOpts::default()).unwrap();
                let (_, top) = Js::resolve(&mut ast);
                assert_eq!(super::observable(ast.program(), top), expected, "{source}");
            }
        });
    }
}
