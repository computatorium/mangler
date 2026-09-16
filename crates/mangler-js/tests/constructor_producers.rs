//! Native bind producers share registration with their selected protected consumers.
use mangler_config::{ConfigFlags, Intensity, ResolvedConfig};
use mangler_jsast::ParseOpts;
use mangler_testkit::cross_engine::{Engine, chrome_path, evaluate_many, node_path};

fn check(source: &str) {
    let mut programs = vec![source.to_owned()];
    for preset in [Intensity::Minify, Intensity::High] {
        for whole in [false, true] {
            let config = ResolvedConfig::try_from(ConfigFlags {
                preset: Some(preset),
                seed: Some(42),
                virtualize: (!whole).then(|| "pay".into()),
                virtualize_program: whole,
                require_virtualized: Some("pay".into()),
                ..Default::default()
            })
            .unwrap();
            let (output, _) = mangler_js::process(source, &ParseOpts::default(), &config).unwrap();
            programs.push(output);
        }
    }
    let engines: Vec<_> = [
        node_path().map(Engine::Node),
        chrome_path().map(Engine::Chrome),
    ]
    .into_iter()
    .flatten()
    .collect();
    assert!(
        !engines.is_empty(),
        "constructor producer checks require Node or Chrome"
    );
    let sources: Vec<_> = programs.iter().map(String::as_str).collect();
    for engine in engines {
        let values = evaluate_many(&engine, &sources).unwrap();
        for (index, value) in values.iter().enumerate().skip(1) {
            assert_eq!(
                value,
                &values[0],
                "{} variant {index}: {source}",
                engine.name()
            );
        }
    }
}

#[test]
fn native_bound_constructors_reach_selected_consumers() {
    for (producer, invoke) in [
        ("const make=Function.bind(null);", "make('return 7')()"),
        (
            "const make=Function.prototype.bind.call(Function,null);",
            "make('return 7')()",
        ),
        (
            "const make=Function.prototype.bind.apply(Function,[null]);",
            "make('return 7')()",
        ),
        (
            "const make=Reflect.apply(Function.prototype.bind,Function,[null]);",
            "make('return 7')()",
        ),
        (
            "const bind=Function.prototype.call.bind(Function.prototype.bind);const make=bind(Function,null);",
            "new make('return 7')()",
        ),
        (
            "const bind=Function.prototype.apply.bind(Function.prototype.bind);const make=bind(Function,[null]);",
            "make('return 7')()",
        ),
        (
            "const bind=Reflect.apply.bind(null,Function.prototype.bind);const make=bind(Function,[null]);",
            "make('return 7')()",
        ),
        (
            "const make=Function.prototype.call.bind(Function);",
            "make(null,'return 7')()",
        ),
        (
            "const make=Function.prototype.apply.bind(Function);",
            "make(null,['return 7'])()",
        ),
        (
            "const first=Function.bind(null);const make=first.bind(null);",
            "make('return 7')()",
        ),
        (
            "const make=Function.bind(null,'a');",
            "new make('return a')(7)",
        ),
        (
            "function produce(){return Function.bind(null)}const make=produce();",
            "make('return 7')()",
        ),
    ] {
        check(&format!(
            "{producer}function pay(){{return {invoke}}}globalThis.__out=JSON.stringify([pay(),make.name,make.length,Object.hasOwn(make,'prototype')]);"
        ));
    }
}

#[test]
fn native_producer_arguments_keep_lexical_scope_and_order() {
    check(
        r#"
var events=[];
Object.defineProperty(Function,'bind',{configurable:true,get(){events.push('get');return Function.prototype.bind}});
function produce(){const make=Function['bind']((events.push('argument'),eval('var local=7;null')));events.push(local);return make}
const make=produce();
function pay(){return make('return 7')()}
globalThis.__out=JSON.stringify([pay(),events]);
"#,
    );
    check(
        r#"
var events=[];
Function.bind=function(value){events.push(this===Function,value);return function(){return function(){return 7}}};
const make=Function.bind((events.push('argument'),3));
function pay(){return make('return 7')()}
globalThis.__out=JSON.stringify([pay(),events]);
"#,
    );
}

#[test]
fn native_bound_suspension_constructors_preserve_kind() {
    for (constructor, invocation) in [
        (
            "Object.getPrototypeOf(async function(){}).constructor",
            "make('return 7')()",
        ),
        (
            "Object.getPrototypeOf(function*(){}).constructor",
            "make('yield 7')().next().value",
        ),
        (
            "Object.getPrototypeOf(async function*(){}).constructor",
            "make('yield 7')().next().then(result=>result.value)",
        ),
    ] {
        check(&format!(
            "const make={constructor}.bind(null);function pay(){{return {invocation}}}Promise.resolve(pay()).then(value=>globalThis.__out=JSON.stringify(value));"
        ));
    }
}

#[test]
fn native_bind_adapters_evaluate_references_and_arraylike_arguments_once() {
    check(
        r#"
var events=[];
Object.defineProperty(Function.prototype.bind,'call',{configurable:true,get(){events.push('get');return Function.prototype.call}});
const make=Function.prototype.bind.call((events.push('target'),Function),(events.push('receiver'),null));
function pay(){return make('return 7')()}
globalThis.__out=JSON.stringify([pay(),events]);
"#,
    );
    for producer in [
        "Function.prototype.bind.apply((events.push('target'),Function),(events.push('args'),args))",
        "Reflect.apply((events.push('bind'),Function.prototype.bind),(events.push('target'),Function),(events.push('args'),args))",
    ] {
        check(&format!(
            r#"
var events=[];
const args={{get length(){{events.push('length');return 2}},get 0(){{events.push('0');return null}},get 1(){{events.push('1');return 'a'}}}};
const make={producer};
function pay(){{return make('return a')(7)}}
globalThis.__out=JSON.stringify([pay(),events]);
"#
        ));
    }
}

#[test]
fn native_direct_eval_keeps_lexical_scope_when_alias_analysis_includes_an_adapter() {
    check(
        r#"
const binder=Function.prototype.call.bind(Function.prototype.bind);
function produce(){var local=7;var eval=globalThis.chooseBinder?binder:globalThis.eval;return eval('local')}
function pay(){return Function('return 7')()}
globalThis.__out=JSON.stringify([produce(),pay()]);
"#,
    );
}

#[test]
fn optional_native_bind_producers_reach_selected_consumers() {
    for producer in [
        "Function?.bind(null)",
        "Function.bind?.(null)",
        "Function?.bind?.(null)",
        "Function?.bind(null).bind(null)",
        "(Function?.bind)(null)",
        "(Function?.bind)?.(null)",
        "Function.prototype.bind.call?.(Function,null)",
        "Reflect?.apply?.(Function.prototype.bind,Function,[null])",
    ] {
        check(&format!(
            "const make={producer};function pay(){{return make('return 7')()}}globalThis.__out=JSON.stringify([pay(),make.name,make.length]);"
        ));
    }
}

#[test]
fn optional_native_suspension_constructors_preserve_kind() {
    for (constructor, invocation) in [
        (
            "Object.getPrototypeOf(async function(){}).constructor",
            "make('return 7')()",
        ),
        (
            "Object.getPrototypeOf(function*(){}).constructor",
            "make('yield 7')().next().value",
        ),
        (
            "Object.getPrototypeOf(async function*(){}).constructor",
            "make('yield 7')().next().then(result=>result.value)",
        ),
    ] {
        check(&format!(
            "const make={constructor}?.bind?.(null);function pay(){{return {invocation}}}Promise.resolve(pay()).then(value=>globalThis.__out=JSON.stringify(value));"
        ));
    }
}

#[test]
fn optional_native_producers_keep_short_circuit_regions_and_operand_order() {
    check(
        r#"
var events=[];const C=globalThis.useFunction?Function:null;
const make=C?.[(events.push('key'),'bind')]?.((events.push('argument'),null));
try{(C?.bind)((events.push('grouped-argument'),null))}catch(error){events.push(error.name)}
function pay(){return Function('return 7')()}
globalThis.__out=JSON.stringify([make,events,pay()]);
"#,
    );
    check(
        r#"
var events=[];const C={next:globalThis.useFunction?Function:undefined};
try{C?.next.bind(events.push('argument'))}catch(error){events.push(error.name)}
Object.defineProperty(Function,'bind',{get(){events.push('get');return Function.prototype.bind}});
const make=Function?.[(events.push('key'),'bind')]?.((events.push('argument'),null));
function pay(){return make('return 7')()}
globalThis.__out=JSON.stringify([pay(),events]);
"#,
    );
    check(
        r#"
function produce(){const make=Function.bind?.(eval('var local=7;null'));return [make,local]}
const [make,local]=produce();function pay(){return make('return 7')()}
globalThis.__out=JSON.stringify([local,pay()]);
"#,
    );
}

#[test]
fn optional_native_producers_preserve_delete_and_template_references() {
    check(
        r#"
var events=[];const target={get x(){events.push('get');return 3}};
Function.bind=function(){events.push('call');return target};
const removed=delete Function?.bind(null).x;
Object.defineProperty(target,'locked',{value:3});
const sloppy=delete Function?.bind(null).locked;
function strictDelete(){'use strict';try{return delete Function?.bind(null).locked}catch(error){return error.name}}
const strict=strictDelete();function pay(){return Function('return 7')()}
globalThis.__out=JSON.stringify([removed,sloppy,strict,events,pay()]);
"#,
    );
    check(
        r#"
var events=[],prior;const target={tag(strings,value){events.push(this===target,value,prior?prior===strings:true);prior=strings;return 7}};
Function.bind=function(){events.push('call');return target};
function produce(){return (Function?.bind(null).tag)`x${(events.push('value'),3)}`}
const first=produce(),second=produce();function pay(){return Function('return 7')()}
globalThis.__out=JSON.stringify([first,second,events,pay()]);
"#,
    );
}

#[test]
fn optional_native_producer_roots_keep_anonymous_names_and_suspension() {
    check(
        r#"
var events=[];const make=(function(){events.push(arguments.callee.name);return Function})()?.bind(null);
let seen;const other=(class{static{seen=this.name}}).constructor?.bind(null);
function pay(){return make('return 7')()}
globalThis.__out=JSON.stringify([pay(),events,seen,other.name]);
"#,
    );
    check(
        r#"
function* produce(){return Function?.[(yield 'key','bind')]?.(yield 'argument')}
const generator=produce(),first=generator.next(),second=generator.next(null),third=generator.next(null),make=third.value;
function pay(){return make('return 7')()}
globalThis.__out=JSON.stringify([first,second,third.done,pay()]);
"#,
    );
    check(
        r#"
async function produce(){return Function.bind?.(await null)}
function pay(make){if(arguments.length>1)Function('return 0');return make('return 7')()}
produce().then(make=>globalThis.__out=JSON.stringify(pay(make)));
"#,
    );
}
