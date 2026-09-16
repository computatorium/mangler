//! Test-only parsing helpers for the VM compiler unit tests.

use swc_core::common::FileName;
use swc_core::common::SourceMap;
use swc_core::common::sync::Lrc;
use swc_core::ecma::ast::*;
use swc_core::ecma::parser::{EsSyntax, Parser, StringInput, Syntax, lexer::Lexer};

/// Parses `src` as the body of `function _() { ... }` and returns its block.
pub fn parse_fn_body(src: &str) -> FunctionBody {
    let cm: Lrc<SourceMap> = Default::default();
    let wrapped = format!("function _() {{ {src} }}");
    let fm = cm.new_source_file(Lrc::new(FileName::Custom("t.js".into())), wrapped);
    let lexer = Lexer::new(
        Syntax::Es(EsSyntax::default()),
        EsVersion::EsNext,
        StringInput::from(&*fm),
        None,
    );
    let mut parser = Parser::new_from(lexer);
    let program = parser.parse_program().unwrap();
    let first_stmt: Stmt = match program {
        Program::Module(m) => match m.body.into_iter().next().unwrap() {
            ModuleItem::Stmt(s) => s,
            _ => panic!("expected statement"),
        },
        Program::Script(s) => s.body.into_iter().next().unwrap(),
    };
    match first_stmt {
        Stmt::Decl(Decl::Fn(fd)) => fd.function.body.unwrap(),
        _ => panic!("expected function decl"),
    }
}

/// Parses `src` (a function EXPRESSION like `function(a,b){...}`) and returns
/// its params + body.
pub fn parse_fn_with_params(src: &str) -> (Vec<Param>, FunctionBody) {
    let cm: Lrc<SourceMap> = Default::default();
    let wrapped = format!("var __f = ({src});");
    let fm = cm.new_source_file(Lrc::new(FileName::Custom("t.js".into())), wrapped);
    let lexer = Lexer::new(
        Syntax::Es(EsSyntax::default()),
        EsVersion::EsNext,
        StringInput::from(&*fm),
        None,
    );
    let mut parser = Parser::new_from(lexer);
    let program = parser.parse_program().unwrap();
    let first_stmt: Stmt = match program {
        Program::Module(m) => match m.body.into_iter().next().unwrap() {
            ModuleItem::Stmt(s) => s,
            _ => panic!("expected statement"),
        },
        Program::Script(s) => s.body.into_iter().next().unwrap(),
    };
    let init = match first_stmt {
        Stmt::Decl(Decl::Var(v)) => v.decls.into_iter().next().unwrap().init.unwrap(),
        _ => panic!("expected var decl"),
    };
    let func = match *init {
        Expr::Paren(p) => match *p.expr {
            Expr::Fn(fe) => fe.function,
            other => panic!("expected fn expr, got {other:?}"),
        },
        Expr::Fn(fe) => fe.function,
        other => panic!("expected fn expr, got {other:?}"),
    };
    (func.params, func.body.unwrap())
}
