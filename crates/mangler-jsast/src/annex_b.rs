//! Annex B.3.3 treats a sloppy if-branch function as a one-statement block.
//! SWC retains the declaration AST but reports a recoverable DeclNotAllowed.
//! Accept only the exact ordinary-function positions permitted by that grammar.
use std::collections::HashSet;

use swc_core::common::{BytePos, Span, Spanned, SyntaxContext};
use swc_core::ecma::ast::*;
use swc_core::ecma::parser::error::{Error, SyntaxError};
use swc_core::ecma::visit::{VisitMut, VisitMutWith};

impl crate::Js {
    pub fn repair_annex_b(program: &mut Program, inherited_strict: bool, errors: &mut Vec<Error>) {
        let mut repair = Branches {
            strict: inherited_strict,
            accepted: HashSet::new(),
            rejected: Vec::new(),
        };
        program.visit_mut_with(&mut repair);
        errors.retain(|error| {
            !matches!(error.kind(), SyntaxError::DeclNotAllowed)
                || !repair.accepted.contains(&error.span().lo)
        });
        errors.extend(
            repair
                .rejected
                .into_iter()
                .map(|span| Error::new(span, SyntaxError::DeclNotAllowed)),
        );
    }
}

struct Branches {
    strict: bool,
    accepted: HashSet<BytePos>,
    rejected: Vec<Span>,
}

fn labelled_function(statement: &Stmt) -> Option<Span> {
    let mut statement = statement;
    while let Stmt::Labeled(label) = statement {
        statement = &label.body;
    }
    match statement {
        Stmt::Decl(Decl::Fn(function)) => Some(function.function.span),
        _ => None,
    }
}

impl Branches {
    fn branch(&mut self, statement: &mut Box<Stmt>) {
        let Stmt::Decl(Decl::Fn(function)) = statement.as_ref() else {
            if let Some(span) = labelled_function(statement) {
                self.rejected.push(span);
            }
            return;
        };
        if self.strict || function.function.is_async || function.function.is_generator {
            self.rejected.push(function.function.span);
            return;
        }
        let span = function.function.span;
        self.accepted.insert(span.lo);
        let declaration = std::mem::replace(statement.as_mut(), Stmt::Empty(EmptyStmt { span }));
        **statement = Stmt::Block(BlockStmt {
            span,
            ctxt: SyntaxContext::empty(),
            stmts: vec![declaration],
        });
    }
}

impl VisitMut for Branches {
    fn visit_mut_stmt(&mut self, statement: &mut Stmt) {
        let body = match statement {
            Stmt::While(node) => Some(&node.body),
            Stmt::DoWhile(node) => Some(&node.body),
            Stmt::For(node) => Some(&node.body),
            Stmt::ForIn(node) => Some(&node.body),
            Stmt::ForOf(node) => Some(&node.body),
            Stmt::With(node) => Some(&node.body),
            _ => None,
        };
        if let Some(span) = body.and_then(|body| labelled_function(body)) {
            self.rejected.push(span);
        }
        statement.visit_mut_children_with(self);
    }
    fn visit_mut_script(&mut self, script: &mut Script) {
        self.strict |= crate::directives::has_use_strict(&script.body);
        script.visit_mut_children_with(self);
    }
    fn visit_mut_module(&mut self, module: &mut Module) {
        self.strict = true;
        module.visit_mut_children_with(self);
    }
    fn visit_mut_function(&mut self, function: &mut Function) {
        let previous = self.strict;
        self.strict |= function
            .body
            .as_ref()
            .is_some_and(|body| crate::directives::has_use_strict(&body.stmts));
        function.visit_mut_children_with(self);
        self.strict = previous;
    }
    fn visit_mut_arrow_expr(&mut self, arrow: &mut ArrowExpr) {
        let previous = self.strict;
        if let ArrowFunctionBody::FunctionBody(body) = arrow.body.as_ref() {
            self.strict |= crate::directives::has_use_strict(&body.stmts);
        }
        arrow.visit_mut_children_with(self);
        self.strict = previous;
    }
    fn visit_mut_class(&mut self, class: &mut Class) {
        let previous = self.strict;
        self.strict = true;
        class.visit_mut_children_with(self);
        self.strict = previous;
    }
    fn visit_mut_if_stmt(&mut self, statement: &mut IfStmt) {
        statement.visit_mut_children_with(self);
        self.branch(&mut statement.cons);
        if let Some(alternate) = &mut statement.alt {
            self.branch(alternate);
        }
    }
    fn visit_mut_bin_expr(&mut self, expression: &mut BinExpr) {
        crate::deep::walk_binary_mut(expression, self);
    }
}

#[cfg(test)]
mod tests {
    use crate::{Js, ParseOpts};
    use mangler_core::Language;

    #[test]
    fn legacy_call_targets_parse_and_reprint_without_losing_the_call() {
        use swc_core::ecma::ast::{AssignTarget, Expr, Program, SimpleAssignTarget, Stmt};

        for source in [
            "f()=rhs();",
            "(f())=rhs();",
            "((f()))+=rhs();",
            "f()()**=rhs();",
            "(f?.())()=rhs();",
            "async()=rhs();",
            "++f(); --f(); f()++; f()--;",
            "for(f() in object); for(f() of iterable);",
            "for((async()) in object); for((f()) of iterable);",
            "async function consume(){for await(f() of iterable);}",
        ] {
            let ast = Js.parse(source, &ParseOpts::default()).expect(source);
            let printed = Js.print(&ast);
            Js.parse(&printed, &ParseOpts::default()).expect(&printed);
        }
        for operator in [
            "=", "*=", "/=", "%=", "+=", "-=", "<<=", ">>=", ">>>=", "&=", "^=", "|=", "**=",
        ] {
            let source = format!("f(){operator}rhs();");
            let ast = Js.parse(&source, &ParseOpts::default()).expect(&source);
            let Program::Script(script) = ast.program() else {
                panic!("{source}")
            };
            let Stmt::Expr(statement) = &script.body[0] else {
                panic!("{source}")
            };
            let Expr::Assign(assignment) = statement.expr.as_ref() else {
                panic!("{source}")
            };
            let AssignTarget::Simple(SimpleAssignTarget::Paren(target)) = &assignment.left else {
                panic!("{source}")
            };
            assert!(matches!(target.expr.as_ref(), Expr::Call(_)), "{source}");
        }
    }

    #[test]
    fn legacy_call_loop_heads_retain_convertible_parenthesized_targets() {
        use swc_core::ecma::ast::{AssignTarget, Expr, ForHead, Pat};
        use swc_core::ecma::visit::{Visit, VisitWith};

        struct Heads(usize);
        impl Visit for Heads {
            fn visit_for_head(&mut self, head: &ForHead) {
                let ForHead::Pat(pattern) = head else {
                    panic!("expected assignment head")
                };
                let Pat::Expr(expression) = pattern.as_ref() else {
                    panic!("expected expression target")
                };
                let Expr::Paren(paren) = expression.as_ref() else {
                    panic!("call target must retain a Paren for suspension lowering")
                };
                let mut inner = paren.expr.as_ref();
                while let Expr::Paren(paren) = inner {
                    inner = paren.expr.as_ref();
                }
                assert!(matches!(inner, Expr::Call(_)));
                assert!(AssignTarget::try_from(pattern.as_ref().clone()).is_ok());
                self.0 += 1;
            }
        }
        for source in [
            "for(f() in object);",
            "for((f()) of iterable);",
            "async function consume(){for await(f() of iterable);}",
            "function* consume(){for(f() of iterable)yield 1;}",
            "async function* consume(){for await((f()) of iterable)yield 1;}",
        ] {
            let ast = Js.parse(source, &ParseOpts::default()).expect(source);
            let mut heads = Heads(0);
            ast.program().visit_with(&mut heads);
            assert_eq!(heads.0, 1, "{source}");
        }
    }

    #[test]
    fn legacy_call_targets_remain_errors_in_strict_modules_and_typescript() {
        for source in [
            "f()=rhs();",
            "f()+=rhs();",
            "++f();",
            "f()--;",
            "for(f() in object);",
            "for(f() of iterable);",
        ] {
            for restricted in [
                format!("'use strict';{source}"),
                format!("function outer(){{'use strict';{source}}}"),
                format!("class C{{m(){{{source}}}}}"),
                format!("{source}export{{}}"),
                format!("function outer(){{{source}}}export{{}}"),
            ] {
                assert!(
                    Js.parse(&restricted, &ParseOpts::default()).is_err(),
                    "{restricted}"
                );
            }
            for opts in [
                ParseOpts {
                    module: true,
                    ..Default::default()
                },
                ParseOpts {
                    typescript: true,
                    ..Default::default()
                },
            ] {
                assert!(Js.parse(source, &opts).is_err(), "{source}, {opts:?}");
            }
        }
    }

    #[test]
    fn legacy_call_allowance_does_not_expand_other_assignment_grammar() {
        for source in [
            "f()&&=rhs();",
            "f()||=rhs();",
            "f()??=rhs();",
            "f?.()=rhs();",
            "f?.()++;",
            "++f?.();",
            "for(f?.() of iterable);",
            "for(f?.() in object);",
            "new f()=rhs();",
            "tag``=rhs();",
            "import('x')=rhs();",
            "(0,f())=rhs();",
            "++(0,f());",
            "[f()]=iterable;",
            "[f()=rhs()]=iterable;",
            "[...f()]=iterable;",
            "[(f())]=iterable;",
            "({x:f()}=object);",
            "({[key()]:f()}=object);",
            "({...f()}=object);",
            "({x:f()=rhs()}=object);",
            "for([f()] of iterable);",
            "for({x:f()} in object);",
            "const [f()]=iterable;",
            "function consume(f()){}",
        ] {
            assert!(Js.parse(source, &ParseOpts::default()).is_err(), "{source}");
        }
    }

    #[test]
    fn escaped_keywords_are_identifier_names_in_property_positions() {
        for keyword in [
            "break",
            "case",
            "catch",
            "class",
            "const",
            "continue",
            "debugger",
            "default",
            "delete",
            "do",
            "else",
            "enum",
            "export",
            "extends",
            "false",
            "finally",
            "for",
            "function",
            "if",
            "import",
            "in",
            "instanceof",
            "new",
            "null",
            "return",
            "super",
            "switch",
            "this",
            "throw",
            "true",
            "try",
            "typeof",
            "var",
            "void",
            "while",
            "with",
            "yield",
            "await",
            "let",
            "static",
            "implements",
            "interface",
            "package",
            "private",
            "protected",
            "public",
        ] {
            let escaped = format!("\\u{:04x}{}", keyword.as_bytes()[0], &keyword[1..]);
            for source in [
                format!("class C{{{escaped}(){{return 42}}}}new C().{escaped}();"),
                format!("class C{{static {escaped}(){{return 42}}}}C.{escaped}();"),
                format!("class C{{get {escaped}(){{return 42}}set {escaped}(v){{}}}}"),
                format!("class C{{#{escaped}=42;get(){{return this.#{escaped};}}}}"),
                format!("let o={{{escaped}:42}};o.{escaped};o?.{escaped};"),
                format!("let o={{{escaped}(){{return 42}}}};o.{escaped}();"),
                format!("let {{{escaped}:value}}=object;"),
                format!("let value=42;export{{value as {escaped}}};"),
            ] {
                let ast = Js.parse(&source, &ParseOpts::default()).expect(&source);
                Js.parse(&Js.print(&ast), &ParseOpts::default())
                    .expect(&source);
            }
        }
        for source in [
            r"class C{voi\u0064(){return 42}}",
            r"class C{whil\u0065(){return 42}}",
            r"class C{wit\u0068(){return 42}}",
            r"class C{\u{76}oid(){return 42}}",
            r"class C{\u0077hile():number{return 42}}",
        ] {
            let options = ParseOpts {
                typescript: source.contains(":number"),
                ..Default::default()
            };
            assert!(Js.parse(source, &options).is_ok(), "{source}");
        }
    }

    #[test]
    fn escaped_keyword_identifier_and_keyword_uses_remain_errors() {
        for keyword in [
            "break",
            "case",
            "catch",
            "class",
            "const",
            "continue",
            "debugger",
            "default",
            "delete",
            "do",
            "else",
            "enum",
            "export",
            "extends",
            "false",
            "finally",
            "for",
            "function",
            "if",
            "import",
            "in",
            "instanceof",
            "new",
            "null",
            "return",
            "super",
            "switch",
            "this",
            "throw",
            "true",
            "try",
            "typeof",
            "var",
            "void",
            "while",
            "with",
            "yield",
            "let",
            "static",
            "implements",
            "interface",
            "package",
            "private",
            "protected",
            "public",
        ] {
            let escaped = format!("\\u{:04x}{}", keyword.as_bytes()[0], &keyword[1..]);
            for source in [
                format!("'use strict';var {escaped}=1;"),
                format!("'use strict';function f({escaped}){{}}"),
                format!("'use strict';({{{escaped}}});"),
            ] {
                assert!(
                    Js.parse(&source, &ParseOpts::default()).is_err(),
                    "{source}"
                );
            }
        }
        for source in [
            r"\u0069f(true){}",
            r"\u0077hile(false){}",
            r"\u0076ar x=1;",
            r"function f(){\u0072eturn 1}",
            r"\u0074rue;",
            r"\u006eull;",
            r"class C{st\u0061tic x(){}}",
            r"class C{st\u0061tic{}}",
            r"async function f(){\u0061wait 1}",
            r"function* f(){\u0079ield 1}",
            r"async function f(){let \u0061wait=1}",
            r"class C{static{let \u0061wait=1}}",
        ] {
            assert!(Js.parse(source, &ParseOpts::default()).is_err(), "{source}");
        }
        // An ordinary escaped identifier retains its normal binding behavior.
        assert!(Js.parse(r"var \u0061=1;a;", &ParseOpts::default()).is_ok());
    }

    #[test]
    fn escaped_names_are_decoded_and_reserved_checks_cover_delayed_contexts() {
        for (source, expected) in [
            (r"class C{voi\u0064(){}}", "void()"),
            (r"class C{whil\u0065(){}}", "while()"),
            (r"class C{wit\u0068(){}}", "with()"),
            (r"class C{st\u0061tic(){}}", "static()"),
            (r"class C{g\u0065t(){}}", "get()"),
            (r"class C{as\u0079nc(){}}", "async()"),
        ] {
            let ast = Js.parse(source, &ParseOpts::default()).expect(source);
            let printed = Js.print(&ast);
            assert!(printed.contains(expected), "{source}: {printed}");
        }
        for source in [
            r"'use strict';var br\u0065ak=1;",
            r"'use strict';({st\u0061tic});",
            r"function f(\u0079ield){'use strict';}",
            r"var \u0061wait=0;export{};",
            r"import{default as \u0062reak}from'x';",
            r"async function f(){let aw\u0061it=0;}",
        ] {
            assert!(Js.parse(source, &ParseOpts::default()).is_err(), "{source}");
        }
        assert!(
            Js.parse(r"'use strict';var aw\u0061it=0;", &ParseOpts::default())
                .is_ok()
        );
    }

    #[test]
    fn body_directives_validate_all_parameter_binding_spellings() {
        for name in [
            "yield",
            "let",
            "static",
            "implements",
            "interface",
            "package",
            "private",
            "protected",
            "public",
            "eval",
            "arguments",
        ] {
            for spelling in [
                name.to_owned(),
                format!("\\u{:04x}{}", name.as_bytes()[0], &name[1..]),
            ] {
                for source in [
                    format!("function f({spelling}){{'use strict';}}"),
                    format!("(function({spelling}){{'use strict';}});"),
                    format!("async function f({spelling}){{'use strict';}}"),
                    format!("function* f({spelling}){{'use strict';}}"),
                    format!("async function* f({spelling}){{'use strict';}}"),
                    format!("({{m({spelling}){{'use strict';}}}});"),
                    format!("class C{{m({spelling}){{}}}}"),
                    format!("({spelling})=>{{'use strict';}};"),
                    format!("{spelling}=>{{'use strict';}};"),
                    format!("async ({spelling})=>{{'use strict';}};"),
                    format!("async {spelling}=>{{'use strict';}};"),
                ] {
                    assert!(
                        Js.parse(&source, &ParseOpts::default()).is_err(),
                        "{source}"
                    );
                }
                for source in [
                    format!("function f({spelling}){{return {spelling}}}"),
                    format!("({spelling})=>{spelling};"),
                    format!("({{m({spelling}){{return {spelling}}}}});"),
                ] {
                    assert!(Js.parse(&source, &ParseOpts::default()).is_ok(), "{source}");
                }
            }
        }
        for source in [
            "function f(x){'use strict';}function g(eval){return eval;}",
            "function f(yield){'use\\x20strict';return yield;}",
            "'use strict';function f(await){return await;}",
            "async function outer(){return function(await){return await;};}",
            "function* outer(){return function(yield){return yield;};}",
        ] {
            assert!(Js.parse(source, &ParseOpts::default()).is_ok(), "{source}");
        }
    }

    #[test]
    fn non_simple_parameters_and_own_async_grammar_remain_restricted() {
        for parameters in ["x=1", "{x}", "[x]", "...x"] {
            for source in [
                format!("function f({parameters}){{'use strict';}}"),
                format!("({parameters})=>{{'use strict';}};"),
                format!("async ({parameters})=>{{'use strict';}};"),
                format!("class C{{m({parameters}){{'use strict';}}}}"),
            ] {
                assert!(
                    Js.parse(&source, &ParseOpts::default()).is_err(),
                    "{source}"
                );
            }
        }
        for source in [
            "async function f(await){}",
            r"async function f(\u0061wait){}",
            "async await=>{};",
            r"async \u0061wait=>{};",
            "async ({await})=>{};",
            "async ([await])=>{};",
            "function* f(yield){}",
            r"function* f(\u0079ield){}",
            "class C{m({eval}){}}",
            r"class C{m({\u0065val}){}}",
            "'use strict';({arguments})=>{};",
        ] {
            assert!(Js.parse(source, &ParseOpts::default()).is_err(), "{source}");
        }
    }

    #[test]
    fn body_directives_validate_function_binding_names() {
        for name in [
            "yield",
            "let",
            "static",
            "implements",
            "interface",
            "package",
            "private",
            "protected",
            "public",
            "eval",
            "arguments",
        ] {
            for spelling in [
                name.to_owned(),
                format!("\\u{:04x}{}", name.as_bytes()[0], &name[1..]),
            ] {
                for source in [
                    format!("function {spelling}(){{'use strict';}}"),
                    format!("(function {spelling}(){{'use strict';}});"),
                    format!("async function {spelling}(){{'use strict';}}"),
                    format!("function* {spelling}(){{'use strict';}}"),
                    format!("async function* {spelling}(){{'use strict';}}"),
                    format!("(async function {spelling}(){{'use strict';}});"),
                    format!("(function* {spelling}(){{'use strict';}});"),
                ] {
                    assert!(
                        Js.parse(&source, &ParseOpts::default()).is_err(),
                        "{source}"
                    );
                }
                for source in [
                    format!("function {spelling}(){{}}"),
                    format!("(function {spelling}(){{}});"),
                ] {
                    assert!(Js.parse(&source, &ParseOpts::default()).is_ok(), "{source}");
                }
            }
        }
        for source in [
            "'use strict';(function await(){});",
            r"'use strict';(function \u0061wait(){});",
            "function await(){'use strict';}",
            "(function await(){'use strict';});",
            "async function await(){'use strict';}",
            r"async function \u0061wait(){'use strict';}",
            "function* await(){'use strict';}",
            "function* yield(){}",
            "async function* await(){}",
            "class C{static{let f=function await(){'use strict';};}}",
            r"class C{static{let f=function \u0061wait(){'use strict';};}}",
            "class C{static{let f=function* await(){'use strict';};}}",
            "function eval(){'use\\x20strict';}",
            "function outer(){function good(){'use strict';}function eval(){}}",
        ] {
            assert!(Js.parse(source, &ParseOpts::default()).is_ok(), "{source}");
        }
        for source in [
            "(async function await(){});",
            r"(async function \u0061wait(){});",
            "(function* yield(){});",
            r"(function* \u0079ield(){});",
            "class C{static{function await(){}}}",
            r"class C{static{function \u0061wait(){}}}",
            "export default function eval(){'use strict';}",
        ] {
            assert!(Js.parse(source, &ParseOpts::default()).is_err(), "{source}");
        }
    }

    #[test]
    fn static_constructor_names_are_methods_and_accessors() {
        use swc_core::ecma::ast::{ClassMember, Decl, Program, Stmt};
        for (member, count) in [
            ("static constructor(){return 1}", 1),
            ("static 'constructor'(){return 1}", 1),
            (r"static c\u006fnstructor(){return 1}", 1),
            ("static ['constructor'](){return 1}", 1),
            ("static *constructor(){yield 1}", 1),
            ("static async constructor(){return 1}", 1),
            ("static async *constructor(){yield 1}", 1),
            (
                "static get constructor(){return 1}static set constructor(value){}",
                2,
            ),
            ("static constructor(){}static constructor(){}", 2),
        ] {
            let source = format!("class C{{{member}constructor(){{}}}}");
            let ast = Js.parse(&source, &ParseOpts::default()).expect(&source);
            let Program::Script(script) = ast.program() else {
                panic!("expected Script")
            };
            let Stmt::Decl(Decl::Class(class)) = &script.body[0] else {
                panic!("expected class")
            };
            assert_eq!(
                class
                    .class
                    .body
                    .iter()
                    .filter(|member| matches!(member, ClassMember::Constructor(_)))
                    .count(),
                1,
                "{source}"
            );
            assert_eq!(
                class
                    .class
                    .body
                    .iter()
                    .filter(
                        |member| matches!(member, ClassMember::Method(method) if method.is_static)
                    )
                    .count(),
                count,
                "{source}"
            );
            Js.parse(&Js.print(&ast), &ParseOpts::default())
                .expect(&source);
            let expression = format!("var C=class{{{member}constructor(){{}}}};");
            assert!(
                Js.parse(&expression, &ParseOpts::default()).is_ok(),
                "{expression}"
            );
        }
        for source in [
            "class C{constructor(){}}",
            "class C{'constructor'(){}}",
            "class C{['constructor'](){}constructor(){}}",
            "class C extends Object{constructor(){super()}static constructor(){return super.name}}",
            "class C{static get constructor(){return super.constructor}}",
        ] {
            assert!(Js.parse(source, &ParseOpts::default()).is_ok(), "{source}");
        }
        assert!(Js.parse(
            "class C{static constructor(value:number){return value}constructor(value:number){}}",
            &ParseOpts {typescript:true,..Default::default()},
        ).is_ok());
    }

    #[test]
    fn actual_constructors_retain_their_early_errors() {
        for source in [
            "class C{constructor(){}constructor(){}}",
            "class C{constructor(){}'constructor'(){}}",
            "class C{*constructor(){}}",
            "class C{async constructor(){}}",
            "class C{async *constructor(){}}",
            "class C{get constructor(){}}",
            "class C{set constructor(value){}}",
            "class C{#constructor(){}}",
            "class C{static #constructor(){}}",
            "class C{constructor=1}",
            "class C{static constructor=1}",
            "class C{static constructor(){super()}}",
            "class C extends Object{static constructor(){super()}}",
        ] {
            assert!(Js.parse(source, &ParseOpts::default()).is_err(), "{source}");
        }
    }

    #[test]
    fn nested_function_parameters_reset_static_block_await_context() {
        for source in [
            "var await=0;var fromParam,fromBody;class C{static{new(class{constructor(x=fromParam=await){fromBody=await}})}}",
            "var await=0;class C{static{class D{constructor(await){}}}}",
            "var await=0;class C{static{function f(x=await){return await}f();}}",
            "var await=0;class C{static{function f(await){return await}f(0);}}",
            "var await=0;class C{static{(function(x=await){return await})();}}",
            "var await=0;class C{static{class D{m(x=await){return await}}new D().m();}}",
            "var await=0;class C{static{class D{static m({x=await}={}){return x}}D.m();}}",
            "var await=0;class C{static{class D{m(await){return await}}}}",
            "var await=0;class C{static{({m([x=await]=[]){return x}}).m();}}",
            "var await=0;class C{static{({set x(v=await){}}).x=undefined;}}",
            "var await=0;class C{static{function* f(x=await){return await}f().next();}}",
            "var await=0;class C{static{class D{*m(x=await){return x}}new D().m().next();}}",
            "var await=0;class C{static{class D{constructor(x=(y=await)=>y){x()}}new D();}}",
            "var await=0;class C{static{function f(x=(y=await)=>y){return x()}f();}}",
            "var await=0;class C{static{let f=(x=function(y=await){return y})=>x();f();}}",
            // Arrow parameter grammar inherits the enclosing restriction, but
            // its function body starts a new Await grammar context.
            "var await=0;class C{static{let f=()=>await;f();}}",
            "class C{static{let f=async()=>await 1;f();}}",
            "class C{static{async function f(x=1){return await x}f();}}",
            "class C{static{class D{async m(x=1){return await x}}new D().m();}}",
            "var await=0;async function outer(){class C{static{class D{constructor(x=await){}}new D();}}}outer();",
        ] {
            let ast = Js.parse(source, &ParseOpts::default()).expect(source);
            Js.parse(&Js.print(&ast), &ParseOpts::default())
                .expect(source);
        }
    }

    #[test]
    fn static_block_and_async_parameter_await_restrictions_remain_errors() {
        for source in [
            "class C{static{await;}}",
            "class C{static{await 1;}}",
            "class C{static{var await=0;}}",
            "class C{static{let await=0;}}",
            "class C{static{let f=(x=await)=>x;}}",
            "class C{static{let f=await=>1;}}",
            "class C{static{let f=(await)=>1;}}",
            "class C{static{let f=({x=await})=>x;}}",
            "class C{static{let f=async(x=await)=>x;}}",
            "class C{static{let f=async(x=await 1)=>x;}}",
            "class C{static{async function f(await){}}}",
            "class C{static{async function f(x=await){}}}",
            "class C{static{async function f(x=await 1){}}}",
            "class C{static{class D{async m(x=await 1){}}}}",
            "class C{static{function* f(x=yield 1){}}}",
            "class C{static{class D{*m(x=yield 1){}}}}",
            "class C{static{function f(x=await 1){}}}",
            "class C{static{class D{constructor(x=await 1){}}}}",
            "class C{static{class D{constructor(x=yield 1){}}}}",
            // A nested parameter guard must restore the outer static context.
            "class C{static{function f(x=0){}await;}}",
            "class C{static{class D{constructor(x=0){}}await;}}",
        ] {
            assert!(Js.parse(source, &ParseOpts::default()).is_err(), "{source}");
        }
    }

    #[test]
    fn for_in_initializers_ignore_enclosing_expression_in_grammar() {
        for source in [
            "(function(){for(var a=0 in {});})();",
            "(function(){let n=0;for(var a=++n in {});})();",
            "(function(){let n=0;for(var a=(++n,-1) in {});})();",
            "const f=()=>{for(var a=0 in {});};",
            "const o={m(){for(var a=0 in {});}};",
            "for(var a=('key' in {})?1:0;;)break;",
        ] {
            let ast = Js.parse(source, &ParseOpts::default()).expect(source);
            Js.parse(&Js.print(&ast), &ParseOpts::default())
                .expect(source);
        }
        for source in [
            "(function(){'use strict';for(var a=0 in {});})();",
            "(function(){for(var a=0 of []);})();",
        ] {
            assert!(Js.parse(source, &ParseOpts::default()).is_err(), "{source}");
        }
    }

    #[test]
    fn sloppy_if_functions_parse_and_reprint_as_blocks() {
        for source in [
            "if(true) function f(){}",
            "if(true) function f(){} else function g(){}",
            "if(false) {} else function f(){}",
            "if(false) {} else if(true) function f(){}",
            "function outer(){if(true) function f(){'use strict';}}",
            "(()=>{if(true) function f(){}})()",
            "({m(){if(true) function f(){}}})",
            "'use strict ignored';if(true) function f(){}",
        ] {
            let program = Js.parse(source, &ParseOpts::default()).expect(source);
            let printed = Js.print(&program);
            Js.parse(&printed, &ParseOpts::default()).expect(&printed);
        }
    }

    #[test]
    fn invalid_branch_declarations_remain_errors() {
        for source in [
            "'use strict';if(true) function f(){}",
            "function outer(){'use strict';if(true) function f(){}}",
            "(()=>{'use strict';if(true) function f(){}})()",
            "class C{m(){if(true) function f(){}}}",
            "if(true) function* f(){}",
            "if(true) async function f(){}",
            "if(true) class C{}",
            "while(false) function f(){}",
            "while(false) async function f(){}",
            "if(true) label:function f(){}",
            "for(;;) label:function f(){}",
            "if(true) function f(){};export{}",
        ] {
            assert!(Js.parse(source, &ParseOpts::default()).is_err(), "{source}");
        }
        assert!(
            Js.parse(
                "if(true) function f(){}",
                &ParseOpts {
                    module: true,
                    ..Default::default()
                }
            )
            .is_err()
        );
    }

    #[test]
    fn super_call_grammar_follows_derived_constructor_and_arrow_scopes() {
        for typescript in [false, true] {
            let opts = ParseOpts {
                typescript,
                ..Default::default()
            };
            for source in [
                "class C extends B{constructor(){super()}}",
                "class C extends null{constructor(){super()}}",
                "class C extends B{constructor(value=super()){}}",
                "class C extends B{constructor(value=()=>super()){}}",
                "class C extends B{constructor(){(()=>super())()}}",
                "class C extends B{constructor(){(async()=>super())()}}",
                "class C extends B{constructor(){({[super()](){}})}}",
                "class C extends B{constructor(){class D{[super()](){}}}}",
                "class C extends B{constructor(){class D{[super()]=1}}}",
                "class C extends B{constructor(){class D{static [super()]=1}}}",
                "class C extends B{constructor(){class D extends super(){}}}",
                "class C extends B{constructor(){function f(){}super()}}",
                "class C extends B{constructor(){({method(){}});super()}}",
                "class C extends B{constructor(){class D{field=1;static{}}super()}}",
                "class C{method(){return super.method()}static constructor(){return super.x}}",
                "({method(){return super.x},get value(){return super.x},set value(v){super.x=v}})",
            ] {
                let ast = Js.parse(source, &opts).expect(source);
                Js.parse(&Js.print(&ast), &opts).unwrap();
            }
            for source in [
                "super();",
                "()=>super();",
                "function f(){super()}",
                "function f(value=super()){}",
                "async function f(){super()}",
                "function* f(){super()}",
                "({method(){super()}})",
                "({get value(){super()}})",
                "({set value(v){super()}})",
                "class C{constructor(){super()}}",
                "class C{constructor(value=super()){}}",
                "class C extends B{static constructor(){super()}}",
                "class C extends B{method(){super()}}",
                "class C extends B{get value(){super()}}",
                "class C extends B{set value(v){super()}}",
                "class C extends B{field=super()}",
                "class C extends B{static field=super()}",
                "class C extends B{static{super()}}",
                "class C extends B{constructor(){function f(){super()}}}",
                "class C extends B{constructor(){function f(value=super()){}}}",
                "class C extends B{constructor(){({method(){super()}})}}",
                "class C extends B{constructor(){({method(value=super()){}})}}",
                "class C extends B{constructor(){({get value(){super()}})}}",
                "class C extends B{constructor(){class D{constructor(){super()}}}}",
                "class C extends B{constructor(){class D{field=super()}}}",
                "class C extends B{constructor(){class D{field=()=>super()}}}",
                "class C extends B{constructor(){class D{static field=()=>super()}}}",
                "class C extends B{constructor(){class D{static{super()}}}}",
                "class C extends B{constructor(){class D{static{(()=>super())()}}}}",
                "class C extends B{constructor(){super?.()}}",
            ] {
                assert!(
                    Js.parse(source, &opts).is_err(),
                    "accepted TS={typescript}: {source}"
                );
            }
        }
    }

    #[test]
    fn external_super_call_capability_is_explicit_and_lexically_scoped() {
        use swc_core::common::{FileName, SourceMap, sync::Lrc};
        use swc_core::ecma::ast::EsVersion;
        use swc_core::ecma::parser::{EsSyntax, Lexer, Parser, StringInput, Syntax};
        for (allowed, source, valid) in [
            (false, "super()", false),
            (true, "super()", true),
            (true, "()=>super()", true),
            (true, "async()=>super()", true),
            (true, "function nested(){super()}", false),
            (true, "({method(){super()}})", false),
            (true, "class C{constructor(){super()}}", false),
            (true, "class C{field=()=>super()}", false),
            (true, "class C{static{super()}}", false),
            (true, "class C{[super()](){}}", true),
            (true, "class C{};super()", true),
        ] {
            let cm: Lrc<SourceMap> = Default::default();
            let file = cm.new_source_file(
                Lrc::new(FileName::Custom("eval.js".into())),
                source.to_owned(),
            );
            let lexer = Lexer::new(
                Syntax::Es(EsSyntax::default()),
                EsVersion::EsNext,
                StringInput::from(&*file),
                None,
            );
            let mut parser = Parser::new_from(lexer);
            parser.set_allow_super_call(allowed);
            let parsed = parser.parse_program();
            let accepted = parsed.is_ok() && parser.take_errors().is_empty();
            assert_eq!(accepted, valid, "external={allowed}: {source}");
        }
    }

    #[test]
    fn resource_for_initializers_retain_resource_scope_and_loop_labels() {
        use swc_core::ecma::ast::{Decl, Program, Stmt};
        for source in [
            "for(using resource=null;;)break;",
            "for(using resource=null;false;);",
            "for(using resource=null,other=null;test();update())body();",
            "for(using of=null;false;);",
            "for(using using=null;false;);",
            "for(using r=(value in object);false;);",
            "function f(){for(using r=null;;)return r}",
            "outer:inner:for(using r=null;;){continue outer;}",
            "outer:for(using r=null;;){inner:for(using s=null;;){continue outer;}}",
            "label:{using r=null;for(;;){break label}}",
            "async function f(){for(await using r=null;;)break}",
            "async function f(){outer:inner:for(await using r=null;false;){continue inner}}",
            "function* f(){for(using r=yield 1;;){yield r;break}}",
            "async function* f(){for(await using r=yield 1;;){yield r;break}}",
            // Existing expression and for-of interpretations remain intact.
            "for(using=0;using<1;using++);for(using of iterable);for(using;;);",
            "for(using [r]=value;false;);",
            r"for(\u0075sing r=null;false;);",
            "for(using resource of iterable);",
            "async function f(){for(await using of of []){}}",
            "async function f(){for await(await using of of []){}}",
            "for(using of of[0,1,2]);",
        ] {
            let ast = Js.parse(source, &ParseOpts::default()).expect(source);
            let printed = Js.print(&ast);
            Js.parse(&printed, &ParseOpts::default()).expect(&printed);
        }
        let source = "outer:inner:for(using r=null,s=null;test();update()){continue outer}";
        let ast = Js.parse(source, &ParseOpts::default()).unwrap();
        let Program::Script(script) = ast.program() else {
            panic!("expected Script")
        };
        let Stmt::Block(block) = &script.body[0] else {
            panic!("resource loop scope")
        };
        assert_eq!(block.stmts.len(), 2);
        let Stmt::Decl(Decl::Using(resource)) = &block.stmts[0] else {
            panic!("genuine UsingDecl")
        };
        assert_eq!(resource.decls.len(), 2);
        assert!(!resource.is_await);
        let Stmt::Labeled(outer) = &block.stmts[1] else {
            panic!("outer loop label")
        };
        assert_eq!(outer.label.sym, "outer");
        let Stmt::Labeled(inner) = outer.body.as_ref() else {
            panic!("inner loop label")
        };
        assert_eq!(inner.label.sym, "inner");
        let Stmt::For(loop_) = inner.body.as_ref() else {
            panic!("labels must target iteration")
        };
        assert!(loop_.init.is_none());
        assert!(loop_.test.is_some());
        assert!(loop_.update.is_some());
        assert_eq!(loop_.span, block.span);

        let opts = ParseOpts {
            module: true,
            ..Default::default()
        };
        let ast = Js.parse("for(await using r=null;false;);", &opts).unwrap();
        Js.parse(&Js.print(&ast), &opts).unwrap();
    }

    #[test]
    fn resource_for_initializers_keep_head_grammar_errors() {
        for source in [
            "for(using r;;);",
            "for(using r=null,;;);",
            "for(using r=null,s;false;);",
            "for(using {r}=value;false;);",
            "for(using let=null;false;);",
            "for(using r=value in object;false;);",
            "for(using r=()=>value in object;false;);",
            "for(using r=null,r=null;false;);",
            "for(using\nr=null;false;);",
            "function f(){for(await using r=null;false;);}",
            "async function f(){for(await\nusing r=null;false;);}",
            "async function f(){for(await using\nr=null;false;);}",
            "async function f(){for await(using r=null;false;);}",
            "for(using r=null;false){}",
            "for(;false){}",
            "for(let r=null;false){}",
            "for(using r=null in object){}",
            "async function f(){for(await using of in []){}}",
            "async function f(){for(await using\nof of []){}}",
            "async function f(){for(await\nusing of of []){}}",
        ] {
            assert!(
                Js.parse(source, &ParseOpts::default()).is_err(),
                "accepted {source}"
            );
        }
    }

    #[test]
    fn parameter_uniqueness_preserves_sloppy_simple_ordinary_functions() {
        for source in [
            "async function f(a,a){}",
            "function* f(a,a){}",
            "async function* f(a,a){}",
            "(async function(a,a){})",
            "(function*(a,a){})",
            "(async function*(a,a){})",
            "function f(a,a){}",
            "(function(a,a){})",
            "function f(a,a){ {\"use strict\";} }",
            "function f(a,a){\"use\\x20strict\";}",
            "function f(a,a){function g(a,b){\"use strict\";}}",
            "function f({x:a},{x:b}){}",
            "function f({[a]:b},a){}",
            "function f(a=function(a,a){},b){}",
            "function f(a=()=>a,b){}",
            "function f([a,...b],c){}",
            "function f({a:x,...rest},y){}",
            "({m(a,b){},set x({a,b}){}})",
            "(a,b)=>({a,b})",
            "async(a,b)=>({a,b})",
            "async function f(a,b){}",
            "function* f(a,b){}",
            "class C{constructor(a,b){}m(a,b){}static m(a,b){}}",
            "function outer(a,a){return ({method(x,y){}})}",
        ] {
            let ast = Js.parse(source, &ParseOpts::default()).expect(source);
            Js.parse(&Js.print(&ast), &ParseOpts::default()).unwrap();
        }
    }

    #[test]
    fn unique_parameter_grammars_reject_duplicate_bound_names() {
        for parameters in [
            "a,a",
            "a,...a",
            "a=0,a",
            "{a},a",
            "[a],{x:a}",
            "{a,a}",
            "a,{x:b,...a}",
        ] {
            for form in [
                "({method(P){}})",
                "(P)=>{}",
                "async(P)=>{}",
                "async function f(P){}",
                "function* f(P){}",
                "async function* f(P){}",
                "class C{constructor(P){}}",
                "class C{method(P){}}",
            ] {
                if parameters == "a,a"
                    && matches!(
                        form,
                        "async function f(P){}" | "function* f(P){}" | "async function* f(P){}"
                    )
                {
                    continue; // Their sloppy simple FormalParameters permit duplicates.
                }
                let source = form.replace('P', parameters);
                assert!(
                    Js.parse(&source, &ParseOpts::default()).is_err(),
                    "accepted {source}"
                );
            }
        }
        for source in [
            "function f(a,...a){}",
            "function f(a,a,...b){}",
            "function f({a},a){}",
            "function f(a=0,a){}",
            "function f({a,a}){}",
            "function f([a],{x:a}){}",
            "function f(a,a){\"use strict\"}",
            "\"use strict\";(function(a,a){})",
            "function f(a,\\u0061){\"use strict\"}",
            "({set value({a,a}){}})",
        ] {
            assert!(
                Js.parse(source, &ParseOpts::default()).is_err(),
                "accepted {source}"
            );
        }
        let many = (0..200)
            .map(|index| format!("[p{index}]"))
            .collect::<Vec<_>>()
            .join(",");
        let source = format!("function f({many},a,a){{}}");
        assert!(Js.parse(&source, &ParseOpts::default()).is_err());
        let opts = ParseOpts {
            typescript: true,
            ..Default::default()
        };
        for source in [
            "class C{constructor(public a,a){}}",
            "class C{constructor(public a=0,a){}}",
        ] {
            assert!(Js.parse(source, &opts).is_err(), "accepted {source}");
        }
        Js.parse("class C{constructor(public a,b){}}", &opts)
            .unwrap();
    }

    #[test]
    fn module_exported_names_are_unique_across_export_forms() {
        for module in [false, true] {
            let opts = ParseOpts {
                module,
                ..Default::default()
            };
            for source in [
                "export function f(){};export function* f(){};",
                "var a;export {a};export {a};",
                "var a,b;export {a as x,b as x};",
                "var a;export default a;export {a as default};",
                "var a;export {a as default};export default a;",
                "export default function(){};export default class{};",
                "var a;export {a as x};export * as x from \"dep\";",
                "export * as default from \"dep\";export default 1;",
                "export * as x from \"a\";export * as x from \"b\";",
                "export var a;export var a;",
                "export var {a}=source;export {a};",
                "export var [a]=source;export {a};",
                "export const {key:{a},...rest}=source;export {a};",
                "export const [first,...rest]=source;export {rest};",
                "export const {a=1}=source;export {a};",
                "const a=1,b=2;export {a as \"x\",b as x};",
                "const a=1,b=2;export {a as \"\",b as \"\"};",
                "const a=1,b=2;export {a as \"x\",b as \"\\u0078\"};",
                "const a=1,b=2;export {a as \"\\uD83D\\uDE00\",b as \"😀\"};",
                "export * as \"x\" from \"a\";export {v as x} from \"b\";",
                "export * as \"default\" from \"a\";export default 1;",
                "export {v as \"x\"} from \"a\";export {v as x} from \"b\";",
            ] {
                assert!(
                    Js.parse(source, &opts).is_err(),
                    "accepted explicit_module={module}: {source}"
                );
            }
        }
    }

    #[test]
    fn module_export_names_require_well_formed_unicode() {
        let opts = ParseOpts {
            module: true,
            ..Default::default()
        };
        for name in [r"\uD83D", r"\uDC00", r"a\uD83Db"] {
            for source in [
                format!("import {{ '{name}' as value }} from 'm';"),
                format!("const value=0;export {{ value as '{name}' }};"),
                format!("export {{ '{name}' }} from 'm';"),
                format!("export * as '{name}' from 'm';"),
            ] {
                assert!(Js.parse(&source, &opts).is_err(), "accepted {source}");
            }
        }
        for source in [
            r"import { '\uD83D\uDE00' as value } from 'm';",
            r"export { '\u{1F600}' as '\uD83D\uDE00' } from 'm';",
            r"export * as '\uD83D\uDE00' from 'm';",
            r"import value from 'm' with {'\uD83D': '\uDC00'};",
            r"export const value = '\uD83D';",
        ] {
            let ast = Js.parse(source, &opts).expect(source);
            Js.parse(&Js.print(&ast), &opts).unwrap();
        }
    }

    #[test]
    fn import_attributes_share_exact_grammar_across_import_and_export_forms() {
        let opts = ParseOpts {
            module: true,
            ..Default::default()
        };
        for head in [
            "import 'dep'",
            "import * as ns from 'dep'",
            "export * from 'dep'",
            "export {value} from 'dep'",
        ] {
            for attributes in [
                "{}",
                "{type:'json'}",
                "{'type':'json',}",
                "{type:'json',mode:'test'}",
            ] {
                for separator in [" ", "\n", "/*\n*/"] {
                    let source = format!("{head}{separator}with{separator}{attributes};");
                    let ast = Js.parse(&source, &opts).expect(&source);
                    Js.parse(&Js.print(&ast), &opts).unwrap();
                }
            }
            for attributes in [
                "{type:42}",
                "{type:true}",
                "{type:`json`}",
                "{type}",
                "{...value}",
                "{['type']:'json'}",
                "{1:'json'}",
                "{get type(){return 'json'}}",
                "{type:'json','type':'json'}",
                "{type:'json','\\u0074ype':'json'}",
            ] {
                let source = format!("{head} with {attributes};");
                assert!(Js.parse(&source, &opts).is_err(), "accepted {source}");
            }
        }
    }

    #[test]
    fn module_exports_keep_distinct_names_and_type_declaration_merging() {
        for module in [false, true] {
            let opts = ParseOpts {
                module,
                ..Default::default()
            };
            for source in [
                "export const a=1;export const b=2;",
                "var a,b;export {a as x,b as y};",
                "export {a as x} from \"dep\";export {a as y} from \"dep\";",
                "export * from \"a\";export * from \"b\";",
                "export * as x from \"a\";export * from \"b\";",
                "export default function f(){};export {f};",
                "export default class C{};export {C};",
                "export var {a}=source;var a;",
                "export const {key:a,...rest}=source;export {a as other};",
                "export const [a,,...rest]=source;export {rest as tail};",
                "const a=1,b=2;export {a as \"\",b as \"x\"};",
                "const a=1,b=2;export {a as \"__proto__\",b as \"constructor\"};",
                "export const {[key]:value=(()=>{let key;return key})()}=source;export {value as key};",
                "export const f=function(a,b){return a};export {f as other};",
            ] {
                let ast = Js.parse(source, &opts).expect(source);
                Js.parse(&Js.print(&ast), &opts).unwrap();
            }
        }
        let opts = ParseOpts {
            typescript: true,
            module: true,
            ..Default::default()
        };
        for source in [
            "export interface M{a:number}export interface M{b:number}",
            "export function f(a:number):number;export function f(a:any){return a}",
            "export namespace M{export const a=1}export namespace M{export const b=2}",
        ] {
            Js.parse(source, &opts).expect(source);
        }
    }
}
