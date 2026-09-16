//! Preserve directive prologues when inserting generated statements.

use swc_core::ecma::ast::{Expr, ExprStmt, Lit, ModuleItem, Program, Stmt};

pub fn is_directive(stmt: &Stmt) -> bool {
    matches!(stmt, Stmt::Expr(ExprStmt { expr, .. }) if matches!(&**expr, Expr::Lit(Lit::Str(_))))
}

/// A Use Strict Directive contains no escapes or line continuation. The cooked
/// string value alone cannot distinguish it from a generic directive.
pub fn has_use_strict(stmts: &[Stmt]) -> bool {
    stmts
        .iter()
        .take_while(|stmt| is_directive(stmt))
        .any(|stmt| {
            let Stmt::Expr(expression) = stmt else {
                return false;
            };
            let Expr::Lit(Lit::Str(value)) = &*expression.expr else {
                return false;
            };
            value.value.as_str() == Some("use strict")
                && value
                    .raw
                    .as_ref()
                    .is_none_or(|raw| matches!(raw.as_ref(), "\"use strict\"" | "'use strict'"))
        })
}

pub fn leading_directive_count(stmts: &[Stmt]) -> usize {
    stmts.iter().take_while(|stmt| is_directive(stmt)).count()
}

/// Preserve compiler initialization before transformations whose generated
/// expressions may depend on the runtime initialized by that entry call.
pub fn leading_initialization_count(stmts: &[Stmt]) -> usize {
    use swc_core::common::Spanned;
    let directives = leading_directive_count(stmts);
    directives + stmts[directives..].iter().take_while(|statement| {
        matches!(statement, Stmt::Expr(expression) if crate::span::is_runtime_span(expression.expr.span()))
    }).count()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Js, ParseOpts};
    use mangler_core::Language;

    #[test]
    fn strict_directive_requires_unescaped_source_spelling() {
        for (source, expected) in [
            ("'use strict';", true),
            ("\"use strict\";", true),
            ("'other';'use strict';", true),
            (r"'use\x20strict';", false),
            (r"'use\u0020strict';", false),
            ("'use \\\nstrict';", false),
            (";'use strict';", false),
            ("('use strict');", false),
            (r"'use\x20strict';'use strict';", true),
        ] {
            let Program::Script(script) = Js
                .parse(source, &ParseOpts::default())
                .unwrap()
                .into_program()
            else {
                panic!()
            };
            assert_eq!(has_use_strict(&script.body), expected, "{source}");
        }
    }
}
