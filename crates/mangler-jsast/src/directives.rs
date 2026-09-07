//! Preserve directive prologues when inserting generated statements.

use swc_core::ecma::ast::{Expr, ExprStmt, Lit, ModuleItem, Program, Stmt};

pub fn is_directive(stmt: &Stmt) -> bool {
    matches!(stmt, Stmt::Expr(ExprStmt { expr, .. }) if matches!(&**expr, Expr::Lit(Lit::Str(_))))
}

pub fn leading_directive_count(stmts: &[Stmt]) -> usize {
    stmts.iter().take_while(|stmt| is_directive(stmt)).count()
}

/// Insert after all directives, retaining script/module scope and the shebang.
pub fn insert_program_statements(program: &mut Program, statements: Vec<Stmt>) {
    match program {
        Program::Script(script) => {
            let at = leading_directive_count(&script.body);
            script.body.splice(at..at, statements);
        }
        Program::Module(module) => {
            let at = module
                .body
                .iter()
                .take_while(|item| matches!(item, ModuleItem::Stmt(stmt) if is_directive(stmt)))
                .count();
            module
                .body
                .splice(at..at, statements.into_iter().map(ModuleItem::Stmt));
        }
    }
}
