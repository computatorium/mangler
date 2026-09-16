//! Differential tests for VM statements and lexical closures. Every case must
//! compile: an unsupported construct is a failure rather than a skipped probe.
mod support;

use mangler_core::Rng;
use mangler_testkit::eval::{CaptureMode, assert_behaviorally_equal_with};
use mangler_vm::{
    diversity::VmDiversity,
    table::{TableBuilder, VmNames},
};
use swc_core::{
    common::{FileName, SourceMap, sync::Lrc},
    ecma::{
        ast::*,
        codegen::{Config, Emitter, text_writer::JsWriter},
        parser::{EsSyntax, Parser, StringInput, Syntax, lexer::Lexer},
    },
};

fn differential(source: &str, call: &str) {
    differential_against(source, source, call);
}

fn differential_against(source: &str, native_source: &str, call: &str) {
    differential_engines(source, native_source, call, true);
}

fn differential_engines(source: &str, native_source: &str, call: &str, node: bool) {
    let cm: Lrc<SourceMap> = Default::default();
    let file = cm.new_source_file(
        Lrc::new(FileName::Custom("control.js".into())),
        format!("({source})"),
    );
    let mut parser = Parser::new_from(Lexer::new(
        Syntax::Es(EsSyntax::default()),
        EsVersion::EsNext,
        StringInput::from(&*file),
        None,
    ));
    let expression = parser.parse_expr().expect("parse");
    let Expr::Paren(expression) = *expression else {
        panic!("parentheses")
    };
    let Expr::Fn(function) = *expression.expr else {
        panic!("function")
    };
    let compiled = mangler_vm::compile_body_with_opts(
        &function.function.params,
        function.function.body.as_ref().unwrap(),
        mangler_vm::CompileOptions {
            live_captures: true,
            ..Default::default()
        },
    )
    .unwrap_or_else(|reason| panic!("{reason}: {source}"));
    assert!(
        compiled
            .consts
            .iter()
            .all(|c| !matches!(c, mangler_vm::chunk::Const::NativeFactory(_))),
        "native diversion"
    );
    for seed in [0, 37, 912] {
        let mut table =
            TableBuilder::with_diversity(VmDiversity::draw(&mut Rng::for_pass(seed, "vm")));
        let chunk = table.add(compiled.clone());
        let names = VmNames {
            lean_interp: "V".into(),
            eh_interp: "E".into(),
            lean_interp_strict: "Vs".into(),
            eh_interp_strict: "Es".into(),
            table: "T".into(),
            rc: "rc".into(),
            sy: "sy".into(),
        };
        let rendered = table.finish(&names).expect("render");
        let mut bytes = Vec::new();
        Emitter {
            cfg: Config::default().with_minify(true),
            cm: cm.clone(),
            comments: None,
            wr: JsWriter::new(cm.clone(), "", &mut bytes, None),
        }
        .emit_program(&Program::Script(Script {
            span: swc_core::common::DUMMY_SP,
            body: rendered.prologue,
            shebang: None,
        }))
        .expect("emit");
        let captures = format!(
            "[{}]",
            chunk
                .captures
                .iter()
                .map(|name| format!("{{get:()=>{name},set:v=>{name}=v,type:()=>typeof {name}}}"))
                .collect::<Vec<_>>()
                .join(",")
        );
        let entry = support::entry(
            &names.table,
            &chunk,
            "f",
            &captures,
            true,
            support::function_length(&function.function.params),
        );
        let vm = format!(
            "{};{entry};globalThis.__out=JSON.stringify({call});",
            String::from_utf8(bytes).unwrap()
        );
        let native = format!("var f=({native_source});globalThis.__out=JSON.stringify({call});");
        assert_behaviorally_equal_with(&native, &vm, &CaptureMode::sink());
        if !node {
            continue;
        }
        // Node supplies an independent engine and accepts chained loop labels
        // that the embedded QuickJS version rejects.
        let exact_native = format!("var f=({source});globalThis.__out=JSON.stringify({call});");
        let execute = |program: &str| {
            let output = std::process::Command::new("node")
                .arg("-e")
                .arg(format!(
                    "{program};process.stdout.write(String(globalThis.__out));"
                ))
                .output()
                .expect("Node is required for control-flow conformance tests");
            assert!(
                output.status.success(),
                "Node failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            output.stdout
        };
        assert_eq!(
            execute(&exact_native),
            execute(&vm),
            "Node divergence: {source}"
        );
    }
}

#[test]
fn nested_strict_closure_keeps_receiver_and_write_rules() {
    differential(
        "function(){function g(){'use strict';var o=Object.freeze({x:1});try{o.x=2;}catch(e){return [typeof this,e.name];}}return g();}",
        "f()",
    );
}

#[test]
fn arrows_capture_arguments_through_multiple_levels() {
    differential(
        "function(a,b){return (()=>(()=>[arguments[0],arguments[1],arguments.length]))()();}",
        "f(8,13,21)",
    );
    differential(
        "function(a){function g(b){return ()=>arguments[0];}return [(()=>arguments[0])(),g(34)()];}",
        "f(12)",
    );
}

#[test]
fn loop_member_targets_and_updates_evaluate_once() {
    differential(
        "function(){var o={x:0},sum=0,n=0;for(o.x of [2,5,9]){o.x++;sum+=o.x;}for(o[n++] in {a:1,b:2}){}return [sum,n,o];}",
        "f()",
    );
}

#[test]
fn catch_patterns_and_loop_closures_keep_distinct_bindings() {
    differential(
        "function(){var out=[];for(let [a,{b=7}] of [[1,{}],[2,{b:8}]]){try{throw {v:[a,b]};}catch({v:[x,y]}){out.push(()=>[x,y]);}}return [out[0](),out[1]()];}",
        "f()",
    );
}

#[test]
fn abrupt_loop_targets_run_finally_and_close() {
    differential(
        "function(){var log=[];outer:for(let x of [1,2,3]){try{for(let y of [4,5]){try{if(x===1)continue outer;if(y===4)break outer;}finally{log.push(y);}}}finally{log.push(x);}}return log;}",
        "f()",
    );
}

#[test]
fn debugger_is_stack_neutral() {
    differential("function(){debugger;return 9;}", "f()");
}

#[test]
fn closures_observe_late_initialization_and_block_shadowing() {
    differential("function(){var read=()=>x;var x=42;return read();}", "f()");
    differential(
        "function(){var x=1;function read(){if(true){let x=9;}return x;}x=7;return read();}",
        "f()",
    );
    differential(
        "function(){function first(){return second();}function second(){return 17;}return first();}",
        "f()",
    );
}

#[test]
fn var_loop_closures_share_the_final_binding() {
    differential(
        "function(){var out=[];for(var x of [1,2,3])out.push(()=>x);return [out[0](),out[1](),out[2]()];}",
        "f()",
    );
    differential(
        "function(){var out=[];for(var x in {a:1,b:2})out.push(()=>x);return [out[0](),out[1]()];}",
        "f()",
    );
}

#[test]
fn closures_preserve_inferred_and_explicit_names() {
    differential(
        "function(){var a=function(){},b=()=>1;function c(){}return [a.name,b.name,c.name,(function(){}).name];}",
        "f()",
    );
}

#[test]
fn chained_labels_share_a_loop_continue_target() {
    // QuickJS rejects a valid chain of labels on an iteration statement. Compare
    // against its equivalent single-label program; the Node audit uses the exact
    // original source as its independent baseline.
    differential_against(
        "function(){var out=[];a:b:for(let i=0;i<3;i++){try{out.push(i);continue a;}finally{out.push(9);}}return out;}",
        "function(){var out=[];a:for(let i=0;i<3;i++){try{out.push(i);continue a;}finally{out.push(9);}}return out;}",
        "f()",
    );
}

#[test]
fn iterator_step_failure_does_not_close() {
    differential(
        "function(){var log=[];var xs={[Symbol.iterator](){return {next(){throw 1;},return(){log.push('close');return {};}};}};try{for(const x of xs){}}catch(e){return [e,log];}}",
        "f()",
    );
}

#[test]
fn block_functions_hoist_locally_and_update_sloppy_alias_at_declaration() {
    differential(
        "function(){var out=[typeof g];{out.push(g());function g(){return 7;}}out.push(g());return out;}",
        "f()",
    );
    differential(
        "function(){var out=[];for(let x of [1,2]){out.push(g);function g(){return x;}}return [out[0](),out[1]()];}",
        "f()",
    );
    differential(
        "function(){function inner(){'use strict';{function local(){return 9;}if(local()!==9)throw 1;}return typeof local;}return inner();}",
        "f()",
    );
    differential(
        "function(){function g(){return g===original;}var original=g;g=function(){};return original();}",
        "f()",
    );
}

#[test]
fn iterator_close_completion_precedence() {
    differential(
        "function(){var xs={[Symbol.iterator](){return {next(){return {value:1,done:false};},return(){return 1;}};}};try{for(var x of xs){break;}}catch(e){return e.name;}}",
        "f()",
    );
    differential(
        "function(){var xs={[Symbol.iterator](){return {next(){return {value:1,done:false};},return(){throw 2;}};}};try{for(var x of xs){throw 1;}}catch(e){return e;}}",
        "f()",
    );
    differential(
        "function(){var xs={[Symbol.iterator](){return {next(){return {value:1,done:false};},return(){throw 2;}};}};try{for(var x of xs){break;}}catch(e){return e;}}",
        "f()",
    );
    differential(
        "function(){var log=[];var xs={[Symbol.iterator](){return {next(){return {get done(){throw 1;}};},return(){log.push('close');return {};}};}};try{for(var x of xs){}}catch(e){return [e,log];}}",
        "f()",
    );
    differential(
        "function(){var log=[];var xs={[Symbol.iterator](){return {next(){return {done:false,get value(){throw 1;}};},return(){log.push('close');return {};}};}};try{for(var x of xs){}}catch(e){return [e,log];}}",
        "f()",
    );
}

#[test]
fn nested_function_lengths_stop_at_first_default_or_rest() {
    differential(
        "function(){var a=(x,y=1,z)=>z,b=function(x,...rest){},c=({x},[y])=>x+y;return [a.length,b.length,c.length];}",
        "f()",
    );
}

#[test]
fn with_closures_keep_dynamic_object_scope_after_exit() {
    differential(
        "function(){var x=1,o={x:2},read;with(o){read=()=>x;}o.x=7;var a=read();delete o.x;return [a,read()];}",
        "f()",
    );
    differential(
        "function(){var x=1,o={x:2},read;with(o){read=()=>x;}o[Symbol.unscopables]={x:true};return read();}",
        "f()",
    );
    differential(
        "function(){var o={x:2,m(){return this.x;}},read;with(o){read=()=>m();}return read();}",
        "f()",
    );
}

#[test]
fn with_loop_heads_and_var_initializers_resolve_object_bindings() {
    differential(
        "function(){var x=1,o={x:2};with(o){for(x of [4,5]){}var x=9;}return [x,o.x];}",
        "f()",
    );
    differential(
        "function(){var x=1,o={x:2};with(o){for(var x in {a:1,b:2}){}}return [x,o.x];}",
        "f()",
    );
}

#[test]
fn with_initializer_resolves_reference_before_evaluating_rhs() {
    // ECMA-262 14.3.2.1 resolves the binding before evaluating the initializer.
    // QuickJS agrees; V8 re-resolves after the delete and writes the outer x.
    // This probe checks the specified behavior rather than V8's divergence.
    // https://tc39.es/ecma262/2026/multipage/ecmascript-language-statements-and-declarations.html#sec-variable-statement-runtime-semantics-evaluation
    let source = "function(){var x=1,o={x:2};with(o){var x=(delete o.x,9);}return [x,o.x];}";
    differential_engines(source, source, "f()", false);
}

#[test]
fn source_class_arrow_cannot_forge_structural_factory_marker() {
    let cm: Lrc<SourceMap> = Default::default();
    let file = cm.new_source_file(
        Lrc::new(FileName::Custom("unprepared-class.js".into())),
        "(function(){return ()=>class{m(){return 1}}})".to_owned(),
    );
    let mut parser = Parser::new_from(Lexer::new(
        Syntax::Es(EsSyntax::default()),
        EsVersion::EsNext,
        StringInput::from(&*file),
        None,
    ));
    let expression = parser.parse_expr().expect("parse");
    let Expr::Paren(expression) = *expression else {
        panic!("parentheses")
    };
    let Expr::Fn(function) = *expression.expr else {
        panic!("function")
    };
    // The ordinary expression-arrow adapter creates a DUMMY_SP block. Only the
    // explicit class preparation pass may mark a native construction factory.
    let result = mangler_vm::compile_body(
        &function.function.params,
        function.function.body.as_ref().unwrap(),
    );
    assert!(
        result.is_err(),
        "unprepared source class must not divert to native: {result:?}"
    );
}
