//! Object environments resolve references at the same points as native JavaScript.
use mangler_config::{ConfigFlags, Intensity, ResolvedConfig};
use mangler_jsast::ParseOpts;
use mangler_testkit::eval::{CaptureMode, assert_behaviorally_equal_with};

fn assert_protected(source: &str) {
    assert_protected_expected(source, source);
}

fn assert_protected_expected(source: &str, expected: &str) {
    for seed in [11, 37] {
        let cfg = ResolvedConfig::try_from(ConfigFlags {
            preset: Some(Intensity::Minify), seed: Some(seed),
            virtualize: Some("f".into()), require_virtualized: Some("f".into()),
            ..Default::default()
        }).unwrap();
        let (output, notes) = mangler_js::process(source, &ParseOpts::default(), &cfg)
            .unwrap_or_else(|error| panic!("must protect {source}: {error}"));
        assert!(notes.iter().any(|note| note.message == "f: virtualized"), "{notes:?}");
        assert!(!output.contains("with("), "source object scope must lower to VM references");
        assert_behaviorally_equal_with(expected, &output, &CaptureMode::sink());
    }
}

#[test]
fn object_scope_reads_writes_and_reference_timing() {
    for source in [
        "function f(o,x){with(o){return x}} globalThis.__out=String(f({x:3},7));",
        "function f(o,x){with(o){x=9}return [o.x,x]} globalThis.__out=JSON.stringify(f({x:3},7));",
        "function f(o,x){with(o){x=(delete o.x,9)}return [o.x,x]} globalThis.__out=JSON.stringify(f({x:3},7));",
        "function f(o,x){with(o){x+=(delete o.x,9)}return [o.x,x]} globalThis.__out=JSON.stringify(f({x:3},7));",
        "function f(o,x){with(o){x=(o.x=20,9)}return [o.x,x]} globalThis.__out=JSON.stringify(f({},7));",
        "function f(o){with(o){return [x++,++x,x--,--x,x]}} globalThis.__out=JSON.stringify(f({x:2}));",
        "function f(o){with(o){return [typeof missing,delete missing,delete x,typeof x]}}globalThis.__out=JSON.stringify(f({x:4}));",
        "function f(o){with(o){var x=4}return [x,o.x]}globalThis.__out=JSON.stringify(f({x:1}));",
        "function f(o){with(o){let x=4;return [x,o.x]}}globalThis.__out=JSON.stringify(f({x:1}));",
        "function f(o){with(o){try{return x}catch(e){return e.name}let x=4}}globalThis.__out=f({x:1});",
        "function f(o){with(o){({a:x}={a:5});return x}}globalThis.__out=String(f({x:1}));",
        "function f(o){with(o){for(x of [2,3]){}return x}}globalThis.__out=String(f({x:1}));",
        "function f(o){with(o){for(x in {a:1,b:2}){}return x}}globalThis.__out=String(f({x:1}));",
        "function f(){try{with(null){return 1}}catch(e){return e.name}}globalThis.__out=f();",
        "function f(){with('abc'){return length}}globalThis.__out=String(f());",
    ] { assert_protected(source); }
}

#[test]
fn object_scope_closures_recheck_bindings_and_keep_receivers() {
    for source in [
        "function f(o,x){with(o){return ()=>x}}let o={x:3},g=f(o,7);let a=g();delete o.x;let b=g();o.x=9;globalThis.__out=JSON.stringify([a,b,g()]);",
        "function f(o,x){with(o){return function(){return x}}}let o={x:3},g=f(o,7);let a=g();delete o.x;let b=g();o.x=9;globalThis.__out=JSON.stringify([a,b,g()]);",
        "function f(o,x){with(o){return function(v){x=v;return x}}}let o={x:3},g=f(o,7);g(4);delete o.x;let b=g(8);o.x=9;globalThis.__out=JSON.stringify([b,g(10),o.x]);",
        "function f(o){with(o){return m()}}globalThis.__out=String(f({x:3,m(){return this.x}}));",
        "function f(o){with(o){return function(){return m()}}}let o={x:3,m(){return this.x}},g=f(o);globalThis.__out=String(g());",
        "function f(o){with(o){return m?.()}}globalThis.__out=String(f({x:3,m(){return this.x}}));",
        "function f(o){with(o){return m``}}globalThis.__out=String(f({x:3,m(){return this.x}}));",
        "function f(o){with(o){return function(){'use strict';try{x=4;return 'bad'}catch(e){return e.name}}}}let o={};Object.defineProperty(o,'x',{value:1,writable:false});globalThis.__out=f(o)();",
        "function f(o){with(o){return function(){try{x=4;return x}catch(e){return e.name}}}}let o={};Object.defineProperty(o,'x',{value:1,writable:false});globalThis.__out=String(f(o)());",
        "function f(o){with(o){let x=7;return ()=>x}}let o={x:3},g=f(o);o.x=9;globalThis.__out=String(g());",
        "function f(o){let out=[];for(let i=0;i<3;i++){with(o){out.push(()=>i)}}return out.map(g=>g())}globalThis.__out=JSON.stringify(f({}));",

        "function f(o,p,x){with(o){with(p){return ()=>x}}}let o={x:2},p={x:3},g=f(o,p,7);let a=g();delete p.x;let b=g();delete o.x;globalThis.__out=JSON.stringify([a,b,g()]);",
    ] { assert_protected(source); }
}

#[test]
fn unscopables_and_proxy_lookup_are_observable() {
    for source in [
        "function f(o,x){with(o){return x}}let o={x:3,[Symbol.unscopables]:{x:true}};globalThis.__out=String(f(o,7));",
        "function f(o,x){with(o){return ()=>x}}let o={x:3,[Symbol.unscopables]:{x:false}},g=f(o,7);let a=g();o[Symbol.unscopables].x=true;globalThis.__out=JSON.stringify([a,g()]);",
        "function f(o,x){with(o){return x}}let log=[],o=new Proxy({}, {has(t,k){log.push('has:'+String(k));return false},get(t,k){log.push('get:'+String(k));return {}}});let value=f(o,7);globalThis.__out=JSON.stringify([value,log]);",
    ] { assert_protected(source); }
}

#[test]
fn object_binding_read_checks_property_after_resolution() {
    // ECMA-262 9.1.1.2.6 performs another HasProperty at GetBindingValue.
    // Some engines skip that observable check, so the spec is the oracle.
    assert_protected_expected(
"function f(o,x){with(o){return x}}let log=[],o=new Proxy({x:3},{has(t,k){log.push('has:'+String(k));return Reflect.has(t,k)},get(t,k,r){log.push('get:'+String(k));return Reflect.get(t,k,r)}});let value=f(o,7);globalThis.__out=JSON.stringify([value,log]);",
        "globalThis.__out=JSON.stringify([3,['has:x','get:Symbol(Symbol.unscopables)','has:x','get:x']]);",
    );
    for directive in ["", "'use strict';"] {
        let source = format!("function f(o){{with(o){{return function(){{{directive}return x}}}}}}let o={{x:3,get [Symbol.unscopables](){{delete this.x;return undefined}}}};try{{globalThis.__out=String(f(o)())}}catch(e){{globalThis.__out=e.name}}");
        let expected = if directive.is_empty() { "undefined" } else { "ReferenceError" };
        assert_protected_expected(&source, &format!("globalThis.__out='{expected}';"));
    }
}

#[test]
fn explicitly_native_closures_keep_object_environment_receivers() {
    let source="function f(o){with(o){return function keep(){return [m(),m``]}}}let o={x:3,m(){return this.x}},g=f(o);globalThis.__out=JSON.stringify(g());";
    let cfg=ResolvedConfig::try_from(ConfigFlags{preset:Some(Intensity::Minify),seed:Some(17),virtualize:Some("f".into()),virtualize_exclude:Some("keep".into()),require_virtualized:Some("f".into()),..Default::default()}).unwrap();
    let (output,_)=mangler_js::process(source,&ParseOpts::default(),&cfg).unwrap();
    assert_behaviorally_equal_with(source,&output,&CaptureMode::sink());
}
