//! Real Node parity for property construction, references, and iterator effects.

mod support;

use mangler_core::Rng;
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
/// function-expression source, drawing diversification from `seed`.
fn virtualize(src: &str, seed: u64) -> Option<String> {
    let (own_name, params, body) = parse_fn_named(src);
    let compiled = mangler_vm::compile_body(&params, &body)
        .unwrap_or_else(|reason| panic!("compile failed: {reason}: {src}"));

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

fn node(source: &str) -> String {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut child = Command::new("node")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Node is required for VM semantic verification");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(source.as_bytes())
        .unwrap();
    let result = child.wait_with_output().unwrap();
    assert!(
        result.status.success(),
        "Node failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8(result.stdout).unwrap()
}

fn parity(source: &str, args: &str) {
    let expected = node(&format!(
        "var f=({source});console.log(JSON.stringify(f({args})));"
    ));
    for seed in [0, 19, 127] {
        let compiled = virtualize(source, seed).expect("supported compilation");
        let observed = node(&format!(
            "{compiled}\nconsole.log(JSON.stringify(f({args})));"
        ));
        assert_eq!(observed, expected, "seed={seed}: {source}");
    }
}

#[test]
fn descriptors_methods_and_literal_prototypes() {
    parity(
        r#"function(){
        var n=0;
        var o={get x(){return n;},set x(v){n=v;},m(a){return this.x+a;},__proto__:{base:9}};
        o.x=4;
        var d=Object.getOwnPropertyDescriptor(o,'x');
        return [o.m(3),o.base,!!d.get,!!d.set,d.enumerable,d.configurable,Object.keys(o)];
    }"#,
        "",
    );
    parity(
        r#"function(){
        var __proto__=7;
        var o={__proto__,['__proto__']:8,...JSON.parse('{"__proto__":9}')};
        return [Object.getPrototypeOf(o)===Object.prototype,o.__proto__,Object.keys(o)];
    }"#,
        "",
    );
    parity(
        r#"function(){
        var o={get x(){return 1},set x(v){},x:3};
        var a={x:1,get x(){return 4}};
        var b={get x(){return 4},...{x:8}};
        return [o.x,Object.getOwnPropertyDescriptor(o,'x').writable,a.x,b.x];
    }"#,
        "",
    );
}

#[test]
fn computed_destructure_keys_and_reference_order() {
    parity(
        r#"function(){
        var log=[],sym=Symbol('k');
        var key={[Symbol.toPrimitive](){log.push('key');return sym;}};
        var source={[sym]:7,keep:9};
        var {[key]:picked,...rest}=source;
        return [picked,rest.keep,Reflect.ownKeys(rest).length,log];
    }"#,
        "",
    );
    parity(
        r#"function(){
        var log=[],target={};
        var source={get x(){log.push('get');return 5;}};
        ({x:target[(log.push('ref'),'p')]}=source);
        return [log,target.p];
    }"#,
        "",
    );
    parity(
        r#"function(){
        var log=[],target={};
        var source={[Symbol.iterator](){return {next(){log.push('next');return {value:8,done:false};},return(){log.push('close');return {};}};}};
        [target[(log.push('ref'),'p')]]=source;
        return [log,target.p];
    }"#,
        "",
    );
}

#[test]
fn logical_and_compound_member_assignment() {
    parity(
        r#"function(){
        var log=[],n=1;
        var o={get p(){log.push('get');return n;},set p(v){log.push('set');n=v;}};
        function key(){log.push('key');return 'p';}
        var a=(o[key()]+=2),b=(o[key()]||=9),c=(o[key()]&&=4);
        o.p=null;
        var d=(o[key()]??=6),e=(o[key()]??=8);
        return [a,b,c,d,e,n,log];
    }"#,
        "",
    );
    parity(
        "function(){var a=0,b=2,c=null;return [a||=3,b&&=4,c??=5,a,b,c];}",
        "",
    );
}

#[test]
fn iterator_exhaustion_and_abrupt_completion() {
    parity(
        r#"function(){
        var n=0;
        var source={[Symbol.iterator](){return {next(){n++;return {done:true};},return(){n+=100;return {};}};}};
        var [a,...rest]=source;
        return [n,a,rest];
    }"#,
        "",
    );
    parity(
        r#"function(){
        var log=[];
        var source={[Symbol.iterator](){return {next(){return {done:false,value:undefined};},get return(){log.push('close');throw 'close-error';}};}};
        try {var [a=(function(){throw 'binding-error';})()]=source;} catch(e){log.push(e);}
        return log;
    }"#,
        "",
    );
    parity(
        r#"function(){
        var log=[];
        var source={[Symbol.iterator](){return {next(){throw 'step-error';},return(){log.push('close');return {};}};}};
        try {var [a]=source;} catch(e){log.push(e);}
        return log;
    }"#,
        "",
    );
    parity(
        r#"function(){
        var log=[];
        var source={[Symbol.iterator](){return {next(){return {get value(){throw 'value-error';},done:false};},return(){log.push('close');return {};}};}};
        try {var [a]=source;} catch(e){log.push(e);}
        return log;
    }"#,
        "",
    );
}

#[test]
fn method_names_and_nonconstructibility() {
    parity(
        r#"function(){
        var symbol=Symbol('pay'),o={m(a){return a},get x(){return 1},set x(v){},[symbol]:function(a,b){return a+b;}};
        var getter=Object.getOwnPropertyDescriptor(o,'x').get;
        var setter=Object.getOwnPropertyDescriptor(o,'x').set;
        var ctor=[];
        for(var fn of [o.m,getter,setter]){try {new fn();ctor.push(true);}catch(e){ctor.push(e instanceof TypeError);}}
        return [o.m.name,o.m.length,getter.name,getter.length,setter.name,setter.length,o[symbol].name,o[symbol].length,ctor,o.m.hasOwnProperty('prototype')];
    }"#,
        "",
    );
}

#[test]
fn assignment_property_key_coercion_timing() {
    parity(
        r#"function(){
        var log=[],k={[Symbol.toPrimitive](){log.push('key');return 'p';}},o={};
        o[k]=(log.push('rhs'),7);
        o[k]+=(log.push('rhs'),2);
        ({p:o[k]}={get p(){log.push('get');return 9;}});
        return [log,o.p];
    }"#,
        "",
    );
    parity(
        r#"function(){var o={x:4,m(){return this.x;}};return [(o.m)(),((o.m))()];}"#,
        "",
    );
}

#[test]
fn parenthesized_optional_method_reference() {
    parity(
        r#"function(){
        var log=[],o={x:5,m(a){return this.x+a;}};
        var a=(o?.m)(2);
        try {(null?.m)(log.push('arg'));}catch(e){log.push(e instanceof TypeError);}
        return [a,log];
    }"#,
        "",
    );
}

#[test]
fn object_super_tracks_home_and_receiver() {
    parity(
        r#"function(){
        var p={get x(){return this.n+1},set x(v){this.n=v-1},m(v){return this.n+v}};
        var o={__proto__:p,n:4,get x(){return super.x},set x(v){super.x=v},m(v){return super.m(v)},arrow(){return ()=>super.x}};
        var first=o.m(3),arrow=o.arrow();
        o.x=10;
        var other={n:20,m:o.m};
        var values=[first,o.x,o.n,other.m(2),arrow()];
        Object.setPrototypeOf(o,{get x(){return this.n+100},m(v){return this.n-v}});
        return [values,o.x,o.m(3),arrow()];
    }"#,
        "",
    );
    parity(
        r#"function(){
        var list=[];
        for(var i=0;i<3;i++)list.push({__proto__:{x:i},m(){return super.x}});
        var unrelated={x:99};
        return [list[0].m(),list[1].m(),list[2].m(),unrelated.x];
    }"#,
        "",
    );
}

#[test]
fn dynamic_scope_references_are_resolved_once() {
    parity(
        r#"function(){
        var x=1,m=function(){return -1},o={x:4,m(){return this.x}},log=[];
        with(o){
            log.push(m());
            x+=(delete o.x,3);
            log.push(x);
            ({v:x}={get v(){delete o.x;return 8;}});
            log.push(x);
        }
        return [x,o.x,log];
    }"#,
        "",
    );
    parity(
        r#"function(){
        var x=2,o={x:5},log=[];
        with(o){
            log.push(({x}).x);
            x ||= 9;
            [x]=[7];
        }
        return [x,o.x,log];
    }"#,
        "",
    );
}

#[test]
fn anonymous_assignment_names_and_method_arguments() {
    parity(
        r#"function(){
        var a,b,o={};
        a=function(){};
        b ||= ()=>0;
        o.m=function(){};
        var {c=()=>1}= {};
        var [d=function(){}]=[];
        var p={m(){return arguments.callee===p.m},get x(){return arguments.callee===Object.getOwnPropertyDescriptor(p,'x').get}};
        return [a.name,b.name,o.m.name,c.name,d.name,p.m(),p.x];
    }"#,
        "",
    );
}

#[test]
fn strict_method_receivers() {
    parity(
        r#"function(){
        var o={m(){'use strict';return [typeof this,this===null,this===undefined];},loose(){return [typeof this,this===globalThis]}};
        return [o.m.call(7),o.m.call(null),o.m.call(undefined),o.loose.call(7),o.loose.call(null)];
    }"#,
        "",
    );
}

#[test]
fn array_spread_and_rest_ignore_array_push_overrides() {
    parity(
        r#"function(){
        var push=Array.prototype.push;
        Array.prototype.push=function(){throw 'overridden push';};
        try {
            var a=[1,...[2,3]], [first,...rest]=a;
            return [first,rest,...a];
        } finally {Array.prototype.push=push;}
    }"#,
        "",
    );
}

#[test]
fn super_parentheses_and_sloppy_failed_set() {
    parity(
        r#"function(){
        var p=Object.freeze({x:1,m(v){return this.n+v}});
        var o={__proto__:p,n:3,m(){super.x=2;return [this.x,(super.m)(4),(super.m)?.(5)]},strict(){'use strict';super.x=2;}};
        var value=o.m();
        try{o.strict();}catch(e){value.push(e instanceof TypeError);}
        return value;
    }"#,
        "",
    );
}

#[test]
fn super_assignment_uses_host_reference_semantics() {
    for op in [
        "=", "+=", "-=", "*=", "/=", "%=", "**=", "<<=", ">>=", ">>>=", "|=", "^=", "&=", "&&=",
        "||=", "??=",
    ] {
        parity(
            &format!(
                r#"function(){{
            var log=[],key={{[Symbol.toPrimitive](){{log.push('key');return 'x'}}}};
            var o={{__proto__:{{x:7}},m(){{var v=(super[key] {op} (log.push('rhs'),2));return [v,this.x,log]}}}};
            return o.m();
        }}"#
            ),
            "",
        );
    }
    parity(
        r#"function(){
        var log=[],key={[Symbol.toPrimitive](){log.push('key');return 'x'}};
        var o={__proto__:{x:7},m(){var a=super[key]++,b=++super[key],c=super[key]--,d=--super[key];return [a,b,c,d,this.x,log]}};
        return o.m();
    }"#,
        "",
    );
}

#[test]
fn super_in_method_parameter_initializers() {
    parity(
        r#"function(){
        var o={__proto__:{value:8},m(x=super.value){return x}};
        return [o.m(),o.m(3)];
    }"#,
        "",
    );
}
