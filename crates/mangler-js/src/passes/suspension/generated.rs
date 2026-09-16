//! Give compiler-created bindings a source-inaccessible name before hygiene.
//!
//! The VM's dynamic lookup metadata is keyed by printed names. A generated
//! `_state` in one function must not hide a source `_state` in another function,
//! so generated binding identities receive names absent from the entire input.
use super::*;

#[derive(Default)]
struct Bindings {
    used: HashSet<String>,
    generated: HashSet<Id>,
    source: HashSet<Id>,
}
impl Bindings {
    fn binding(&mut self, ident: &Ident) {
        if ident.span.is_dummy() {
            self.generated.insert(ident.to_id());
        } else {
            self.source.insert(ident.to_id());
        }
    }
}
impl Visit for Bindings {
    fn visit_bin_expr(&mut self, expression: &BinExpr) {
        mangler_jsast::deep::walk_binary(expression, self);
    }
    fn visit_ident(&mut self, ident: &Ident) {
        self.used.insert(ident.sym.to_string());
    }
    // BindingIdent also represents assignment targets. Only declaration owners
    // establish a binding; renaming a generated assignment to implicit arguments
    // would otherwise invent a nonexistent local.
    fn visit_var_declarator(&mut self, declaration: &VarDeclarator) {
        mangler_jsast::analysis::binding_names(&declaration.name, &mut |id| self.binding(id));
        declaration.visit_children_with(self);
    }
    fn visit_param(&mut self, parameter: &Param) {
        mangler_jsast::analysis::binding_names(&parameter.pat, &mut |id| self.binding(id));
        parameter.visit_children_with(self);
    }
    fn visit_arrow_expr(&mut self, arrow: &ArrowExpr) {
        for parameter in &arrow.params {
            mangler_jsast::analysis::binding_names(parameter, &mut |id| self.binding(id));
        }
        arrow.visit_children_with(self);
    }
    fn visit_catch_clause(&mut self, clause: &CatchClause) {
        if let Some(parameter) = &clause.param {
            mangler_jsast::analysis::binding_names(parameter, &mut |id| self.binding(id));
        }
        clause.visit_children_with(self);
    }
    fn visit_import_decl(&mut self, declaration: &ImportDecl) {
        for specifier in &declaration.specifiers {
            self.binding(match specifier {
                ImportSpecifier::Named(specifier) => &specifier.local,
                ImportSpecifier::Default(specifier) => &specifier.local,
                ImportSpecifier::Namespace(specifier) => &specifier.local,
            });
        }
        declaration.visit_children_with(self);
    }
    fn visit_fn_decl(&mut self, declaration: &FnDecl) {
        self.binding(&declaration.ident);
        declaration.visit_children_with(self);
    }
    fn visit_fn_expr(&mut self, expression: &FnExpr) {
        if let Some(ident) = &expression.ident {
            self.binding(ident);
        }
        expression.visit_children_with(self);
    }
    fn visit_class_decl(&mut self, declaration: &ClassDecl) {
        self.binding(&declaration.ident);
        declaration.visit_children_with(self);
    }
    fn visit_class_expr(&mut self, expression: &ClassExpr) {
        if let Some(ident) = &expression.ident {
            self.binding(ident);
        }
        expression.visit_children_with(self);
    }
}

/// Existing generated bindings may be named by artifacts owned by earlier passes.
/// Only this lowering's new identities can be renamed without invalidating them.
pub(super) fn existing_bindings(program: &Program) -> HashSet<Id> {
    let mut bindings = Bindings::default();
    program.visit_with(&mut bindings);
    bindings.generated.extend(bindings.source);
    bindings.generated
}

pub(super) fn isolate(program: &mut Program, existing: &HashSet<Id>) -> HashSet<String> {
    let mut bindings = Bindings::default();
    program.visit_with(&mut bindings);
    let mut identities: Vec<_> = bindings
        .generated
        .difference(&bindings.source)
        .filter(|identity| !existing.contains(*identity))
        .cloned()
        .collect();
    identities.sort_by(|left, right| {
        left.0
            .cmp(&right.0)
            .then(left.1.as_u32().cmp(&right.1.as_u32()))
    });
    let mut replacements = HashMap::new();
    let mut names = HashSet::new();
    let mut next = 0u32;
    for identity in identities {
        let name = loop {
            let name = format!("_mangler_internal_{next}");
            next += 1;
            if bindings.used.insert(name.clone()) {
                break name;
            }
        };
        names.insert(name.clone());
        replacements.insert(identity, name);
    }
    struct Rename(HashMap<Id, String>);
    impl VisitMut for Rename {
        fn visit_mut_bin_expr(&mut self, expression: &mut BinExpr) {
            mangler_jsast::deep::walk_binary_mut(expression, self);
        }
        fn visit_mut_prop(&mut self, property: &mut Prop) {
            if let Prop::Shorthand(ident) = property
                && let Some(name) = self.0.get(&ident.to_id())
            {
                let key = PropName::Ident(IdentName::new(ident.sym.clone(), ident.span));
                let mut value = ident.clone();
                value.sym = name.as_str().into();
                *property = Prop::KeyValue(KeyValueProp {
                    key,
                    value: Box::new(Expr::Ident(value)),
                });
                return;
            }
            property.visit_mut_children_with(self);
        }
        fn visit_mut_ident(&mut self, ident: &mut Ident) {
            if let Some(name) = self.0.get(&ident.to_id()) {
                ident.sym = name.as_str().into();
            }
        }
    }
    program.visit_mut_with(&mut Rename(replacements));
    names
}

/// Called only on standalone support fragments before they meet source nodes.
pub(super) fn certify_helpers(statements: &mut [Stmt]) {
    let span = mangler_jsast::span::protocol_helper_span();
    for statement in statements {
        match statement {
            Stmt::Decl(Decl::Fn(function)) => function.function.span = span,
            Stmt::Decl(Decl::Var(declaration)) => declaration.span = span,
            Stmt::Expr(expression) => expression.span = span,
            _ => panic!("protocol helper fragment must contain declarations or initialization"),
        }
    }
}

pub(super) fn take_helpers(program: &mut Program) -> Vec<Stmt> {
    use swc_core::common::Spanned;
    let certified =
        |statement: &Stmt| mangler_jsast::span::is_protocol_helper_span(statement.span());
    match program {
        Program::Script(script) => script
            .body
            .extract_if(.., |statement| certified(statement))
            .collect(),
        Program::Module(module) => module
            .body
            .extract_if(
                ..,
                |item| matches!(item, ModuleItem::Stmt(statement) if certified(statement)),
            )
            .map(|item| {
                let ModuleItem::Stmt(statement) = item else {
                    unreachable!()
                };
                statement
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mangler_jsast::build;

    #[test]
    fn assignment_targets_do_not_invent_generated_bindings() {
        use mangler_core::Language;
        use mangler_jsast::{Js, ParseOpts};
        let mut ast = Js
            .parse(
                "function source(value){arguments=value;external=value;return arguments}",
                &ParseOpts::default(),
            )
            .unwrap();
        let existing = existing_bindings(ast.program());
        ast.program_mut()
            .visit_mut_with(&mut mangler_jsast::span::GeneratedSpans);
        assert!(isolate(ast.program_mut(), &existing).is_empty());
        let output = Js.print(&ast);
        assert!(output.contains("arguments=value"));
        assert!(output.contains("external=value"));
    }

    #[test]
    fn only_certified_support_leaves_the_source_program() {
        use mangler_core::Language;
        use mangler_jsast::{Js, ParseOpts};
        let mut ast = Js
            .parse(
                "function protocol(){return Object.create(null)}function _pay(){return source()}",
                &ParseOpts::default(),
            )
            .unwrap();
        ast.program_mut()
            .visit_mut_with(&mut mangler_jsast::span::GeneratedSpans);
        let Program::Script(script) = ast.program_mut() else {
            unreachable!()
        };
        certify_helpers(&mut script.body[..1]);
        let helpers = take_helpers(ast.program_mut());
        assert_eq!(helpers.len(), 1);
        let Program::Script(script) = ast.program() else {
            unreachable!()
        };
        assert_eq!(script.body.len(), 1);
        assert!(
            matches!(&script.body[0], Stmt::Decl(Decl::Fn(function)) if function.ident.sym == *"_pay")
        );
    }

    #[test]
    fn preserves_prior_generated_binding_identity_and_renames_new_helpers() {
        let mut program = Program::Script(Script {
            body: vec![build::var_decl(
                VarDeclKind::Var,
                "decoder_core",
                build::ident_expr("original"),
            )],
            ..Default::default()
        });
        let existing = existing_bindings(&program);
        let Program::Script(script) = &mut program else {
            unreachable!()
        };
        script.body.push(build::var_decl(
            VarDeclKind::Var,
            "state_helper",
            build::ident_expr("decoder_core"),
        ));
        script
            .body
            .push(build::expr_stmt(build::ident_expr("state_helper")));
        let internals = isolate(&mut program, &existing);
        assert_eq!(internals, HashSet::from(["_mangler_internal_0".into()]));
        let Program::Script(script) = &program else {
            unreachable!()
        };
        let Stmt::Decl(Decl::Var(prior)) = &script.body[0] else {
            panic!("prior decoder declaration missing")
        };
        assert!(
            matches!(&prior.decls[0].name, Pat::Ident(binding) if binding.id.sym == "decoder_core")
        );
        let Stmt::Decl(Decl::Var(helper)) = &script.body[1] else {
            panic!("new helper declaration missing")
        };
        assert!(
            matches!(&helper.decls[0].name, Pat::Ident(binding) if binding.id.sym == "_mangler_internal_0")
        );
        assert!(
            matches!(helper.decls[0].init.as_deref(), Some(Expr::Ident(binding)) if binding.sym == "decoder_core")
        );
        assert!(
            matches!(&script.body[2], Stmt::Expr(ExprStmt { expr, .. }) if matches!(&**expr, Expr::Ident(binding) if binding.sym == "_mangler_internal_0"))
        );
    }
}
