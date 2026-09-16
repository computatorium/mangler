//! Initialize generated module machinery on the first entry, including entries
//! through a cyclic import before this module starts evaluating. Source bindings
//! stay native: this bootstrap never initializes a source lexical declaration.
use super::FileConfig;
use mangler_jsast::{build, span::is_runtime_span};
use swc_core::ecma::ast::*;
use swc_core::ecma::visit::{VisitMut, VisitMutWith};

pub(super) fn install(
    ast: &mut mangler_jsast::lang::Ast,
    cfg: &FileConfig,
    runtime: &mut Vec<Stmt>,
) {
    if !cfg.runtime_frontend()
        && matches!(ast.program(), Program::Module(_))
        && prepare(
            ast.program_mut(),
            runtime,
            &cfg.fresh_name(),
            &cfg.fresh_name(),
        )
    {
        ast.register_runtime(runtime, None);
    }
}

/// Keep generated storage at module scope while hoisting its initializer as a
/// declaration. Unlike a `var` initializer, the declaration exists at linking.
/// A normal module evaluation still captures its intrinsics at the same point;
/// an early imported call performs that initialization exactly once.
fn prepare(program: &mut Program, runtime: &mut Vec<Stmt>, init: &str, ready: &str) -> bool {
    let Program::Module(module) = program else {
        return false;
    };
    let mut retained = Vec::with_capacity(module.body.len());
    for item in std::mem::take(&mut module.body) {
        match item {
            ModuleItem::Stmt(ref statement) if generated_bootstrap(statement) => {
                let ModuleItem::Stmt(statement) = item else {
                    unreachable!()
                };
                runtime.push(statement);
            }
            item => retained.push(item),
        }
    }
    module.body = retained;
    if runtime.is_empty() {
        return false;
    }
    let span = mangler_jsast::span::runtime_span();
    let mut initialize = Vec::new();
    let mut storage = Vec::new();
    for statement in std::mem::take(runtime) {
        match statement {
            Stmt::Decl(Decl::Var(mut declaration)) => {
                assert_eq!(
                    declaration.kind,
                    VarDeclKind::Var,
                    "runtime capsule uses hoisted storage"
                );
                for variable in &mut declaration.decls {
                    if let Some(value) = variable.init.take() {
                        let Pat::Ident(binding) = &variable.name else {
                            unreachable!("runtime capsule exports identifier bindings")
                        };
                        initialize.push(build::expr_stmt(Expr::Assign(AssignExpr {
                            span,
                            op: AssignOp::Assign,
                            left: SimpleAssignTarget::Ident(binding.id.clone().into()).into(),
                            right: value,
                        })));
                    }
                }
                storage.push(Stmt::Decl(Decl::Var(declaration)));
            }
            statement @ Stmt::Decl(Decl::Fn(_)) => storage.push(statement),
            statement => initialize.push(statement),
        }
    }
    let mut body = vec![Stmt::If(IfStmt {
        span,
        test: Box::new(build::ident_expr(ready)),
        cons: Box::new(Stmt::Return(ReturnStmt { span, arg: None })),
        alt: None,
    })];
    body.append(&mut initialize);
    body.push(build::expr_stmt(build::assign(
        ready,
        build::bool_lit(true),
    )));
    let bootstrap = Stmt::Decl(Decl::Fn(FnDecl {
        ident: Ident::new_no_ctxt(init.into(), span),
        declare: false,
        function: Box::new(Function {
            span,
            body: Some(FunctionBody { span, stmts: body }),
            ..Default::default()
        }),
    }));
    storage.push(Stmt::Decl(Decl::Var(Box::new(VarDecl {
        span,
        kind: VarDeclKind::Var,
        decls: vec![VarDeclarator {
            span,
            name: Pat::Ident(Ident::new_no_ctxt(ready.into(), span).into()),
            init: None,
            definite: false,
        }],
        ..Default::default()
    }))));
    storage.push(bootstrap);
    storage.push(entry(init));
    *runtime = storage;
    for item in &mut module.body {
        let function = match item {
            ModuleItem::Stmt(Stmt::Decl(Decl::Fn(function))) => Some(&mut function.function),
            ModuleItem::ModuleDecl(ModuleDecl::ExportDecl(ExportDecl {
                decl: Decl::Fn(function),
                ..
            })) => Some(&mut function.function),
            ModuleItem::ModuleDecl(ModuleDecl::ExportDefaultDecl(ExportDefaultDecl {
                decl: DefaultDecl::Fn(function),
                ..
            })) => Some(&mut function.function),
            _ => None,
        };
        if let Some(function) = function {
            // A default/computed parameter initializer runs before the body.
            // Excluded functions can still contain the protected string decoder.
            function.params.visit_mut_with(&mut ParameterEntry(init));
            if let Some(body) = &mut function.body {
                let directives = body
                    .stmts
                    .iter()
                    .take_while(|statement| mangler_jsast::directives::is_directive(statement))
                    .count();
                body.stmts.insert(directives, entry(init));
            }
        }
    }
    true
}

struct ParameterEntry<'a>(&'a str);
impl VisitMut for ParameterEntry<'_> {
    fn visit_mut_assign_pat(&mut self, pattern: &mut AssignPat) {
        pattern.left.visit_mut_with(self);
        if let Pat::Ident(binding) = &*pattern.left {
            name_default(&mut pattern.right, binding.id.sym.as_ref());
        }
        pattern.right.visit_mut_with(self);
    }

    fn visit_mut_object_pat_prop(&mut self, property: &mut ObjectPatProp) {
        if let ObjectPatProp::Assign(assign) = property {
            if let Some(value) = &mut assign.value {
                name_default(value, assign.key.id.sym.as_ref());
                value.visit_mut_with(self);
            }
        } else {
            property.visit_mut_children_with(self);
        }
    }

    fn visit_mut_expr(&mut self, expression: &mut Expr) {
        let original = std::mem::replace(
            expression,
            Expr::Invalid(Invalid {
                span: Default::default(),
            }),
        );
        let Stmt::Expr(initialize) = entry(self.0) else {
            unreachable!()
        };
        // Formal defaults and computed pattern keys require an AssignmentExpression;
        // raw codegen must remain valid before the pipeline's final fixer.
        *expression = Expr::Paren(ParenExpr {
            span: Default::default(),
            expr: Box::new(Expr::Seq(SeqExpr {
                span: Default::default(),
                exprs: vec![initialize.expr, Box::new(original)],
            })),
        });
    }
}

fn name_default(value: &mut Box<Expr>, name: &str) {
    if mangler_jsast::assignment_target::is_anonymous_definition(value) {
        let original = std::mem::replace(
            value,
            Box::new(Expr::Invalid(Invalid {
                span: Default::default(),
            })),
        );
        *value = mangler_jsast::assignment_target::named_value(name, original);
    }
}

fn entry(init: &str) -> Stmt {
    let mut expression = build::call(build::ident_expr(init), Vec::new());
    if let Expr::Call(call) = &mut expression {
        call.span = mangler_jsast::span::runtime_span();
    }
    build::expr_stmt(expression)
}

fn generated_bootstrap(statement: &Stmt) -> bool {
    match statement {
        Stmt::Decl(Decl::Var(declaration)) => {
            is_runtime_span(declaration.span)
                || (!declaration.decls.is_empty()
                    && declaration.decls.iter().all(|variable| {
                        variable.init.as_ref().is_some_and(|value| {
                            use swc_core::common::Spanned;
                            is_runtime_span(value.span())
                        })
                    }))
        }
        Stmt::Expr(expression) => {
            use swc_core::common::Spanned;
            is_runtime_span(expression.expr.span())
        }
        _ => false,
    }
}
