mod support;

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
    let compiled = mangler_vm::compile_body(&params, &body).ok()?;

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

    let fn_name = own_name.as_deref().unwrap_or("f");
    let captures = format!("[{}]", chunk.captures.join(","));
    let thunk = support::entry(
        &names.table,
        &chunk,
        fn_name,
        &captures,
        false,
        support::function_length(&params),
    );

    let prologue = render(vt.prologue);
    Some(format!("{prologue}\n{thunk}"))
}

fn check(src: &str, args: &str) {
    for seed in [1, 7, 42] {
        let program = virtualize(src, seed).unwrap_or_else(|| panic!("expression bailed: {src}"));
        let original = format!("var f=({src});globalThis.__out=JSON.stringify(f({args}));");
        let transformed = format!("{program};globalThis.__out=JSON.stringify(f({args}));");
        assert_behaviorally_equal_with(&original, &transformed, &CaptureMode::sink());
    }
}

#[test]
fn optional_method_calls_and_spreads() {
    check(
        "function(o){return o.m?.(2)}",
        "{v:3,m:function(x){return this.v+x}}",
    );
    check("function(o){var n=0;var r=o.m?.(n++);return [r,n]}", "{}");
    check(
        "function(o){var n=0;var r=o?.m?.(...[n++]);return [r,n]}",
        "null",
    );
    check(
        "function(o){return o?.m?.(...[2,3])}",
        "{v:4,m:function(a,b){return this.v+a+b}}",
    );
    check(
        "function(f){return f?.(...[2,3])}",
        "function(a,b){return a+b}",
    );
    check("function(f){return f?.(...[2,3])}", "null");
}

#[test]
fn sparse_arrays_preserve_absent_properties() {
    check(
        "function(){var a=[,1,,];return [a.length,0 in a,1 in a,2 in a]}",
        "",
    );
    check(
        "function(){var a=[,...[1,,3],,4,,];return [a.length,Object.keys(a),a]}",
        "",
    );
}

#[test]
fn literals_and_property_keys() {
    check(
        "function(){return [(999999999999999999999999n+1n).toString(),typeof 1n]}",
        "",
    );
    check(
        "function(){var a=/a/g,b=/a/g;return [a!==b,a.test('aa'),a.lastIndex,b.lastIndex]}",
        "",
    );
    check(
        "function(){var a='\\ud800';return [a.length,a.charCodeAt(0),{'\\udfff':3}['\\udfff']]}",
        "",
    );
    check(
        "function(){var a={1.5:2,1e30:3,999999999999999999999n:4};return Object.keys(a)}",
        "",
    );
}

#[test]
fn update_results_and_reference_order() {
    check("function(x){return [x++,x,--x,x--,x]}", "'4'");
    check("function(x){return [Object.is(x++,-0),x]}", "-0");
    check(
        "function(x){return [(x++).toString(),x.toString(),(++x).toString()]}",
        "4n",
    );
    check(
        "function(o){return [o.x++,++o.x,o.x--,--o.x,o.x]}",
        "{x:'4'}",
    );
    check(
        "function(o){var i=0;var n=o[i++]++;return [n,i,o[0]]}",
        "{0:2}",
    );
}

#[test]
fn delete_values_and_optional_references() {
    check("function(o){return [delete o?.x,o]}", "null");
    check("function(o){return [delete o?.x,o]}", "{x:2}");
    check("function(o){return [delete o?.x.y,o]}", "null");
    check("function(o){return [delete o?.x.y,o]}", "{x:{y:2}}");
    check("function(x){return [delete x,delete (x+1),x]}", "3");
}

#[test]
fn spread_construction_ignores_shadowed_reflect() {
    check(
        "function(C,Reflect){return new C(...[2,3]).x}",
        "function(a,b){this.x=a+b},null",
    );
}

#[test]
fn template_utf16_and_coercion() {
    check(r"function(){return `\ud800`.charCodeAt(0)}", "");
    check(
        r"function(tag){return tag`\ud800`}",
        "function(s){return [s[0].charCodeAt(0),s.raw[0],Object.isFrozen(s)]}",
    );
    check(
        "function(x){try{return `${x}`}catch(e){return e.name}}",
        "Symbol('x')",
    );
}

#[test]
fn optional_getter_and_parentheses_receivers() {
    check(
        "function(o){return (o.m)?.(2)}",
        "{x:3,m:function(v){return this.x+v}}",
    );
    check(
        "function(o){return o.m?.(o.x=5)}",
        "{x:3,get m(){this.x=4;return function(v){return [this.x,v]}}}",
    );
    check("function(o){return delete o?.m()}", "null");
    check(
        "function(o){return delete o?.m()}",
        "{m:function(){return 2}}",
    );
}

#[test]
fn update_coercion_order() {
    check(
        "function(o,k){var n=o[k]++;return [n,o.x,o.y]}",
        "{x:2,y:8},{n:0,[Symbol.toPrimitive](){return this.n++?'y':'x'}}",
    );
    check(
        "function(o){return [o.x++,o.x]}",
        "{x:{[Symbol.toPrimitive](){return 2n}}}",
    );
}

#[test]
fn regular_expression_literals_are_fresh_each_evaluation() {
    check(
        "function(){var a=[];for(var i=0;i<2;i++){a[i]=/a/g}a[0].exec('aa');return [a[0]!==a[1],a[0].lastIndex,a[1].lastIndex]}",
        "",
    );
}

#[test]
fn with_reads_updates_typeof_and_delete() {
    check(
        "function(o){var x=1;with(o){var a=[x,typeof x,x++,x];a[4]=delete x;a[5]=x;return a}}",
        "{x:4}",
    );
    check(
        "function(o){var x=1,absent;with(o){return [x,typeof absent,delete absent]}}",
        "{x:4,[Symbol.unscopables]:{x:true}}",
    );
}

#[test]
fn with_optional_calls_and_tags_keep_receiver() {
    check(
        "function(o){var m;with(o){return m?.(...[2])}}",
        "{x:4,m:function(v){return this.x+v}}",
    );
    check("function(o){var m;with(o){return m?.(...[2])}}", "{m:null}");
    check(
        "function(o){var tag;with(o){return tag`x`}}",
        "{x:4,tag:function(s){return [this.x,s[0]]}}",
    );
}

#[test]
fn generated_operator_chains_have_bounded_compiler_stack() {
    for count in [500, 1_000, 5_000] {
        let sum = std::iter::repeat_n("x", count)
            .collect::<Vec<_>>()
            .join("+");
        check(&format!("function(x){{return {sum}}}"), "1");
    }
}

#[test]
fn iterative_short_circuit_preserves_side_effects() {
    check(
        "function(x){var n=0;var a=(x&&(n++,0))||(n++,4);var b=(x??(n++,2))??(n++,3);return [a,b,n]}",
        "null",
    );
    check(
        "function(x){var n=0;return [(n++,n+=2,n),x&&(n++,1)&&(n++,2),n]}",
        "true",
    );
}

#[test]
fn grouped_anonymous_functions_keep_inferred_names() {
    check(
        "function(){var f=((function(){})),g=((()=>1));return [f.name,g.name]}",
        "",
    );
}

#[test]
fn nullish_base_throws_before_property_key_coercion() {
    check(
        "function(k,log){try{null[k]}catch(e){}try{delete null[k]}catch(e){}return log}",
        "{[Symbol.toPrimitive](){throw 'coerced'}},[]",
    );
    check(
        "function(k){try{null[k]}catch(e){return e.name}}",
        "{[Symbol.toPrimitive](){throw 'coerced'}}",
    );
    check(
        "function(k){try{delete null[k]}catch(e){return e.name}}",
        "{[Symbol.toPrimitive](){throw 'coerced'}}",
    );
}

#[test]
fn new_target_distinguishes_calls_construction_and_lexical_arrows() {
    check(
        "function(){function C(){this.target=new.target;this.arrow=()=>new.target}var c=new C;return [c.target===C,c.arrow()===C,(function(){return new.target})()===undefined]}",
        "",
    );
    check(
        "function(){function C(){this.target=new.target}function D(){}var c=Reflect.construct(C,[],D);return [c.target===D,Object.getPrototypeOf(c)===D.prototype]}",
        "",
    );
}
