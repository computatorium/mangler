//! Suspension declarations are lexical in blocks, including sloppy source.
//! Once their bodies are lowered to ordinary functions, retaining declaration
//! syntax would incorrectly activate Annex B's outer variable binding.
use swc_core::common::{DUMMY_SP, SyntaxContext};
use swc_core::ecma::ast::*;
use swc_core::ecma::visit::{VisitMut, VisitMutWith};

pub(super) fn lower(program: &mut Program) {
    program.visit_mut_with(&mut Lower);
}

fn declaration(statement: &Stmt) -> bool {
    matches!(statement, Stmt::Decl(Decl::Fn(function))
        if function.function.is_async || function.function.is_generator)
}

fn take(statement: &mut Stmt) -> Stmt {
    let Stmt::Decl(Decl::Fn(function)) =
        std::mem::replace(statement, Stmt::Empty(EmptyStmt { span: DUMMY_SP }))
    else {
        unreachable!()
    };
    let span = function.function.span;
    let name = function.ident.sym.to_string();
    let value = Box::new(Expr::Fn(FnExpr {
        ident: None,
        function: function.function,
    }));
    let value = mangler_jsast::assignment_target::named_value(&name, value);
    Stmt::Decl(Decl::Var(Box::new(VarDecl {
        span,
        ctxt: SyntaxContext::empty(),
        kind: VarDeclKind::Let,
        declare: false,
        decls: vec![VarDeclarator {
            span,
            name: Pat::Ident(function.ident.into()),
            // Declaration self-references resolve the mutable surrounding
            // binding; a named expression would introduce an immutable one.
            init: Some(value),
            definite: false,
        }],
    })))
}

struct Lower;
impl VisitMut for Lower {
    fn visit_mut_bin_expr(&mut self, expression: &mut BinExpr) {
        mangler_jsast::deep::walk_binary_mut(expression, self);
    }
    fn visit_mut_block_stmt(&mut self, block: &mut BlockStmt) {
        block.visit_mut_children_with(self);
        let mut declarations = Vec::new();
        for statement in &mut block.stmts {
            if declaration(statement) {
                declarations.push(take(statement));
            }
        }
        block.stmts.splice(..0, declarations);
    }
}

#[cfg(test)]
mod tests {
    use mangler_core::Language;
    use mangler_jsast::{Js, ParseOpts};
    use mangler_testkit::cross_engine::{Engine, evaluate_many, node_path};
    #[test]
    fn suspension_declarations_keep_block_scope_hoisting_and_names() {
        let engine =
            Engine::Node(node_path().expect("Node required for Annex B binding semantics"));
        for source in [
            "function f(){{function* g(){}}return typeof g}globalThis.__out=f()",
            "function f(){{async function g(){}}return typeof g}globalThis.__out=f()",
            "function f(){{async function* g(){}}return typeof g}globalThis.__out=f()",
            "function f(){{function g(){}}return typeof g}globalThis.__out=f()",
            "function f(){let x;{x=g().next().value;function* g(){yield 3}}return x}globalThis.__out=f()",
            "function f(){{function* g(){}return g.name}}globalThis.__out=f()",
            "function f(){{function* g(){g=7;yield g}let original=g;return[original().next().value,g]}}globalThis.__out=f()",
            "function* f(){{yield g().next().value;function* g(){yield 4}}return typeof g}globalThis.__out=Array.from(f())",
            "function f(){let a=[];for(let i=0;i<3;i++){function* g(){yield i}a.push(g)}return a.map(g=>g().next().value)}globalThis.__out=f()",
            "function f(){let a=[];outer:for(let i=0;i<3;i++){switch(i){case 1:continue outer;default:function* g(){yield i};a.push(g().next().value)}}return a}globalThis.__out=f()",
            "function f(){let old;{old=g;function* g(){yield x}let x=8}return old().next().value}globalThis.__out=f()",
            "function f(){var a=[];{function* g(){yield 7};a.push(eval(\"g().next().value\"))}a.push(typeof g);return a}globalThis.__out=f()",
        ] {
            let source = format!("{source};globalThis.__out=JSON.stringify(globalThis.__out)");
            let mut ast = Js.parse(&source, &ParseOpts::default()).unwrap();
            let selected = super::super::suspension_candidates(ast.program())
                .into_keys()
                .collect();
            let lowered = super::super::lower_with_lexicals(ast.program_mut(), &selected);
            mangler_jsast::directives::insert_program_statements(
                ast.program_mut(),
                lowered.helpers,
            );
            let output = Js.print(&ast);
            let values = evaluate_many(&engine, &[&source, &output]).unwrap();
            assert_eq!(values[0], values[1], "{source}\n{output}");
        }
    }
}
