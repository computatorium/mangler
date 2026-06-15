//! Test-only helpers shared by the `cfg`, `emit`, `tdz` and `helpers` unit-test
//! modules.

#![cfg(test)]

use swc_core::common::sync::Lrc;
use swc_core::common::{FileName, SourceMap, DUMMY_SP};
use swc_core::ecma::ast::*;
use swc_core::ecma::codegen::{text_writer::JsWriter, Config as CodegenConfig, Emitter};
use swc_core::ecma::parser::{lexer::Lexer, EsSyntax, Parser, StringInput, Syntax};

/// Parses `src` as the body of `function __t() { ... }` and returns its block.
pub fn parse_body(src: &str) -> BlockStmt {
    let cm: Lrc<SourceMap> = Default::default();
    let wrapped = format!("function __t() {{ {src} }}");
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

/// A minimal [`FileConfig`](crate::config::FileConfig) for unit tests that need
/// `fresh_name()` (the TDZ helper / rewrite paths). Medium preset, seed 1.
pub fn test_support_cfg() -> crate::config::FileConfig {
    use mangler_config::{ConfigFlags, Intensity, ResolvedConfig};
    use std::collections::HashSet;
    let flags = ConfigFlags {
        preset: Some(Intensity::Medium),
        seed: Some(1),
        ..Default::default()
    };
    let resolved = ResolvedConfig::try_from(flags).expect("valid preset config");
    crate::config::FileConfig::new(resolved, 1, HashSet::new())
}

/// Codegens a [`BlockStmt`] to a string for structural assertions.
pub fn emit_block(block: &BlockStmt) -> String {
    let cm: Lrc<SourceMap> = Default::default();
    let mut buf = Vec::new();
    {
        let wr = JsWriter::new(cm.clone(), "\n", &mut buf, None);
        let mut emitter = Emitter {
            cfg: CodegenConfig::default(),
            cm: cm.clone(),
            comments: None,
            wr,
        };
        let script = Script {
            span: DUMMY_SP,
            body: vec![Stmt::Block(block.clone())],
            shebang: None,
        };
        emitter.emit_script(&script).unwrap();
    }
    String::from_utf8(buf).unwrap()
}
