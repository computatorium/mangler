use mangler_core::Rng;
use mangler_testkit::eval::{CaptureMode, assert_behaviorally_equal_with};
use mangler_vm::diversity::VmDiversity;
use mangler_vm::table::{TableBuilder, VmNames};
use swc_core::common::sync::Lrc;
use swc_core::common::{FileName, SourceMap};
use swc_core::ecma::ast::*;
use swc_core::ecma::codegen::{Config as CodegenConfig, Emitter, text_writer::JsWriter};
use swc_core::ecma::parser::{EsSyntax, Parser, StringInput, Syntax, lexer::Lexer};

/// Parse a function-expression source into `(name, params, body)`. `name` is the
/// function's own identifier (a named function expression like `function fac(){…}`),
/// or `None` for an anonymous one.
fn parse_fn_named(src: &str) -> (Option<String>, Vec<Param>, FunctionBody) {
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
    let stmt = match program {
        Program::Script(s) => s.body.into_iter().next().unwrap(),
        Program::Module(m) => match m.body.into_iter().next().unwrap() {
            ModuleItem::Stmt(s) => s,
            _ => panic!("stmt"),
        },
    };
    let init = match stmt {
        Stmt::Decl(Decl::Var(v)) => *v.decls.into_iter().next().unwrap().init.unwrap(),
        _ => panic!("var"),
    };
    let fe = match init {
        Expr::Paren(p) => match *p.expr {
            Expr::Fn(fe) => fe,
            _ => panic!("fn"),
        },
        Expr::Fn(fe) => fe,
        _ => panic!("fn"),
    };
    let name = fe.ident.map(|i| i.sym.to_string());
    (name, fe.function.params, fe.function.body.unwrap())
}

/// Render a slice of statements to minified JS source.
fn render(stmts: Vec<Stmt>) -> String {
    let cm: Lrc<SourceMap> = Default::default();
    let mut buf = Vec::new();
    {
        let wr = JsWriter::new(cm.clone(), "", &mut buf, None);
        let mut emitter = Emitter {
            cfg: CodegenConfig::default().with_minify(true),
            cm,
            comments: None,
            wr,
        };
        let program = Program::Script(Script {
            span: swc_core::common::DUMMY_SP,
            body: stmts,
            shebang: None,
        });
        emitter.emit_program(&program).unwrap();
    }
    String::from_utf8(buf).unwrap()
}

/// Build a complete program that defines `f` as a VM-virtualized thunk for the given
/// function-expression source, drawing diversification from `seed`. Returns `None` if
/// the body bails (unsupported construct) — a bail is never a miscompile, the caller
/// just skips that body. `pcount`/`caps` are threaded into the thunk's call.
fn virtualize(src: &str, seed: u64) -> Option<String> {
    let (own_name, params, body) = parse_fn_named(src);
    let compiled =
        mangler_vm::compile_body(&params, &body).unwrap_or_else(|reason| panic!("{reason}: {src}"));

    let div = VmDiversity::draw(&mut Rng::for_pass(seed, "vm"));
    let mut tb = TableBuilder::with_diversity(div);
    let chunk = tb.add(compiled);

    let names = VmNames {
        lean_interp: "Vv".into(),
        eh_interp: "Dd".into(),
        lean_interp_strict: "Vs".into(),
        eh_interp_strict: "Ds".into(),
        table: "Tt".into(),
        rc: "rcc".into(),
        sy: "syy".into(),
    };
    let vt = tb.finish(&names).expect("finish");

    // The thunk replaces the BODY of the (possibly named) function, keeping its own
    // name `f` (or the source's name) so a named-fn-expr self-reference capture
    // resolves to the thunk itself — exactly as the real pass replaces a function's
    // body in place. `function f(<params>){ return <interp>(T[i][0],T[i][1],arguments,[caps],capStart,pcount,this); }`
    let fn_name = own_name.as_deref().unwrap_or("f");
    let interp = if chunk.needs_eh {
        &names.eh_interp
    } else {
        &names.lean_interp
    };
    let caps = format!("[{}]", chunk.captures.join(","));
    let param_src: Vec<String> = (0..chunk.pcount).map(|i| format!("p{i}")).collect();
    let row = format!("{}[{}]", names.table, chunk.index);
    let start = chunk.cap_start;
    let count = chunk.pcount;
    let thunk = format!(
        "var {fn_name}={row}[5]?{row}[5](function(receiver,args,refs){{return {interp}({row}[0],{row}[1],args,{caps},{start},{count},receiver,false,refs);}}):function {fn_name}({}){{return {interp}({row}[0],{row}[1],arguments,{caps},{start},{count},this);}};var f={fn_name};",
        param_src.join(","),
    );

    let prologue = render(vt.prologue);
    Some(format!("{prologue}\n{thunk}"))
}

fn check(src: &str, args: &str) {
    for seed in [1, 7, 42] {
        let program = virtualize(src, seed).unwrap_or_else(|| panic!("binding bailed: {src}"));
        let original = format!("var f=({src});globalThis.__out=JSON.stringify(f({args}));");
        let transformed = format!("{program};globalThis.__out=JSON.stringify(f({args}));");
        assert_behaviorally_equal_with(&original, &transformed, &CaptureMode::sink());
    }
}

fn check_expected(source: &str, expected: &str) {
    for seed in [1, 7, 42] {
        let program = virtualize(source, seed).expect("binding source compiles");
        let transformed = format!("{program};globalThis.__out=JSON.stringify(f());");
        let expected = format!("globalThis.__out=JSON.stringify({expected});");
        assert_behaviorally_equal_with(&expected, &transformed, &CaptureMode::sink());
        if let Some(node) = mangler_testkit::cross_engine::node_path() {
            let original = format!("var f=({source});globalThis.__out=JSON.stringify(f());");
            let values = mangler_testkit::cross_engine::evaluate_many(
                &mangler_testkit::cross_engine::Engine::Node(node),
                &[&original, &transformed],
            )
            .expect("V8 evaluates both binding environments");
            assert_eq!(values[0], values[1]);
        }
    }
}

#[test]
fn ordered_parameter_initialization_and_rest_patterns() {
    check(
        "function([a=3], b=a, {c=b}={}, ...[d=7]){return [a,b,c,d]}",
        "[]",
    );
    check("function({a=2}={},b=a+1,c=b+1){return [a,b,c]}", "");
    check("function(...{length}){return length}", "1,2,3");
    check("function(a=1,b=()=>a){var a=2;return [a,b()]}", "");
    check("function(a=1,b=()=>a){var a; a=2;return b()}", "");
    check(
        "function(){var c=9;function f(a=c){var c=2;return a}return f()}",
        "",
    );
}

#[test]
fn parameter_tdz_and_default_closures() {
    check(
        "function(){function f(a=a){return a}try{return f()}catch(e){return e.name}}",
        "",
    );
    check(
        "function(){function f(a=b,b=2){return a}try{return f()}catch(e){return e.name}}",
        "",
    );
    check(
        "function(){function f([a=b,b=2]){return a}try{return f([])}catch(e){return e.name}}",
        "",
    );
    check("function(a=()=>b,b=3){return a()}", "");
}

#[test]
fn mapped_arguments_and_duplicate_parameters() {
    check(
        "function(a,a){a=3;return [a,arguments[0],arguments[1]]}",
        "1,2",
    );
    check("function(a,a){return [a,arguments[0],arguments[1]]}", "1");
    check("function(a){arguments[0]=4;return a}", "1");
    check("function(a){a=4;return arguments[0]}", "1");
    check(
        "function(a){delete arguments[0];a=4;return [a,arguments[0]]}",
        "1",
    );
    check(
        "function(a){Object.defineProperty(arguments,'0',{value:5,writable:false});a=7;return [a,arguments[0]]}",
        "1",
    );
    check("function(a){var arguments;return arguments[0]}", "3");
    check(
        "function(a=arguments[0]){return [a,arguments.length]}",
        "undefined,2",
    );
    check("function(a=1){a=4;return arguments[0]}", "2");
    check(
        "function(a){var f=()=>arguments;return [f()===arguments,f()[0]]}",
        "3",
    );
    check("function(a){var f=()=>a;arguments[0]=4;return f()}", "1");
}

#[test]
fn block_function_hoisting_and_annex_b() {
    check(
        "function(){var out=[];{out.push(f());function f(){return 3}}out.push(f());return out}",
        "",
    );
    check(
        "function(){var out=[];if(false){function f(){return 3}}return typeof f}",
        "",
    );
    check(
        "function(){var f=1;{function f(){return 3}}return typeof f}",
        "",
    );
    check(
        "function(){var f=1;{let f=2;{function f(){return 3}}}return f}",
        "",
    );
    check(
        "function(){function f(){return f===original}var original=f;f=()=>0;return original()}",
        "",
    );
}

#[test]
fn assignment_reaches_parameter_closure_without_var_redeclaration() {
    // QuickJS incorrectly creates a second binding here; ECMAScript and V8 share
    // the parameter binding when the body does not redeclare it.
    for (source, expected) in [
        ("function(a=1,b=()=>a){a=2;return b()}", "2"),
        (
            "function(a=()=>arguments){var arguments=3;return [a().length,arguments]}",
            "[0,3]",
        ),
    ] {
        check_expected(source, expected);
    }
}

#[test]
fn annex_b_respects_enclosing_lexical_declarations() {
    check(
        "function(){for(let f=0;f<1;f++){function f(){return 2}}return f===globalThis.f}",
        "",
    );
    check(
        "function(){for(let f of [0]){function f(){return 2}}return f===globalThis.f}",
        "",
    );
    check(
        "function(){switch(0){case 0:let f=1;{function f(){return 2}}}return f===globalThis.f}",
        "",
    );
}

#[test]
fn non_simple_arguments_have_restricted_callee() {
    check(
        "function(a=1){try{return arguments.callee}catch(e){return e.name}}",
        "",
    );
    check(
        "function(){function f(a=1){try{return arguments.callee}catch(e){return e.name}}return f()}",
        "",
    );
}

#[test]
fn duplicate_block_functions_share_the_last_hoisted_value() {
    // QuickJS copies the earlier declaration instead of the current block binding.
    check_expected(
        "function(){var out;block:{function g(){return 1}out=g();break block;function g(){return 2}}return [out,g()]}",
        "[2,2]",
    );
}

#[test]
fn computed_parameter_keys_cannot_initialize_other_bindings() {
    check(
        "function(){function g({[([b]=[1])]:a},b){}try{g({})}catch(e){return e.name}}",
        "",
    );
}

#[test]
fn mapped_arguments_preserve_native_brand_without_a_string_tag() {
    check(
        "function(a){return [Object.prototype.toString.call(arguments),arguments[Symbol.toStringTag]]}",
        "1",
    );
}
