//! Transplant the authoritative native generator kernel into a module
//! declaration. The native declaration owns call-time parameters and the host
//! request queue; its driver runs the same compiled events as closure kernels.
use super::*;

pub(super) fn restore(
    function: &mut Function,
    depth: usize,
    is_async: bool,
    cfg: &FileConfig,
    intrinsics: &mut Vec<Stmt>,
) {
    if function.is_generator {
        return;
    }
    let body = function
        .body
        .as_mut()
        .expect("compiled generator has a body");
    let Some(Stmt::Return(returned)) = body.stmts.pop() else {
        unreachable!("compiled generator ends with its state producer")
    };
    let iterator = returned.arg.expect("compiled entry produces an iterator");
    let source = super::super::suspension::generator_kernel(depth, is_async);
    let mut kernel = Js
        .parse(&source, &ParseOpts::default())
        .expect("native generator kernel parses")
        .into_program();
    kernel.visit_mut_with(&mut GeneratedSpans);
    let error = cfg.fresh_name();
    struct Names<'a> {
        cfg: &'a FileConfig,
        names: std::collections::HashMap<String, String>,
    }
    impl VisitMut for Names<'_> {
        fn visit_mut_ident(&mut self, identifier: &mut Ident) {
            identifier.sym = self
                .names
                .entry(identifier.sym.to_string())
                .or_insert_with(|| self.cfg.fresh_name())
                .as_str()
                .into();
        }
        fn visit_mut_bin_expr(&mut self, expression: &mut BinExpr) {
            mangler_jsast::deep::walk_binary_mut(expression, self);
        }
    }
    let mut names = Names {
        cfg,
        names: std::collections::HashMap::from([("TypeError".into(), error.clone())]),
    };
    kernel.visit_mut_with(&mut names);
    let driver = names
        .names
        .get("driver")
        .expect("kernel binds its driver")
        .clone();
    let Program::Script(mut script) = kernel else {
        unreachable!()
    };
    let Stmt::Decl(Decl::Fn(kernel)) = script.body.remove(0) else {
        unreachable!()
    };
    let mut statements = kernel.function.body.expect("kernel body").stmts;
    let directives = statements
        .iter()
        .take_while(|statement| mangler_jsast::directives::is_directive(statement))
        .count();
    statements.drain(..directives);
    intrinsics.push(mangler_jsast::build::var_decl(
        VarDeclKind::Var,
        &error,
        mangler_jsast::build::ident_expr("TypeError"),
    ));
    body.stmts.push(mangler_jsast::build::var_decl(
        VarDeclKind::Var,
        &driver,
        *iterator,
    ));
    body.stmts.extend(statements);
    function.is_async = is_async;
    function.is_generator = true;
}
