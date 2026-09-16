//! Intrinsics used by VM machinery must not resolve to source shadows.
use mangler_config::{ConfigFlags, Intensity, ResolvedConfig};
use mangler_jsast::ParseOpts;

fn check(source: &str) {
    for seed in [3, 42] {
        let config = ResolvedConfig::try_from(ConfigFlags {
            preset: Some(Intensity::Minify),
            virtualize: Some("pay".into()),
            require_virtualized: Some("pay".into()),
            seed: Some(seed),
            ..Default::default()
        })
        .unwrap();
        let (output, notes) = mangler_js::process(source, &ParseOpts::default(), &config).unwrap();
        assert!(notes.iter().any(|n| n.message == "pay: virtualized"));
        mangler_testkit::assert_behaviorally_equal(source, &output);
    }
}

#[test]
fn source_lexical_shadows_preserve_values_and_vm_operations() {
    for source in [
        "function Object(){return 19}function pay(){return [Object(),{x:7}.x]}globalThis.__out=JSON.stringify(pay());",
        "{let Object=3,globalThis=4;}function pay(){return {ok:7}.ok}globalThis.__out=pay();",
        "let Object=7;function pay(){let o={a:2};return Object+o.a}globalThis.__out=pay();",
        "let Reflect=7;function pay(){let f=(x)=>x+Reflect;return f(2)}globalThis.__out=pay();",
        "let Array=7;function pay(){let a=[1,2];return a.length+Array}globalThis.__out=pay();",
        "let String=7;function pay(){return 'hello'+String}globalThis.__out=pay();",
        "let Symbol=7;function pay(){let n=Symbol;for(let x of [1,2])n+=x;return n}globalThis.__out=pay();",
        "let RegExp=()=>{throw 1};function pay(){return /a/.test('a')}globalThis.__out=pay();",
        "let BigInt=()=>{throw 1};function pay(){return (12345678901234567890n+1n).toString()}globalThis.__out=pay();",
        "let TypeError=7;function pay(){const n=1;try{n=2}catch(e){return e.name+TypeError}}globalThis.__out=pay();",
    ] {
        check(source);
    }
}

#[test]
fn source_replacements_do_not_redirect_vm_primitives() {
    for source in [
        "let original=Object.defineProperty;function pay(){let a={x:3};return a.x}Object.defineProperty=()=>{throw 1};try{globalThis.__out=pay()}finally{Object.defineProperty=original}",
        "let original=Reflect.apply;function pay(){let f=(x)=>x+2;return f(3)}Reflect.apply=()=>{throw 1};try{globalThis.__out=pay()}finally{Reflect.apply=original}",
        "let original=String.fromCharCode;function pay(){return 'hello'}String.fromCharCode=()=>{throw 1};try{globalThis.__out=pay()}finally{String.fromCharCode=original}",
    ] {
        check(source);
    }
}

#[test]
fn hoisted_intrinsic_functions_use_only_required_recovery_paths() {
    for name in [
        "Reflect",
        "Array",
        "String",
        "Symbol",
        "TypeError",
        "ReferenceError",
        "Function",
        "RegExp",
        "BigInt",
        "Proxy",
        "WeakMap",
    ] {
        check(&format!(
            "function {name}(){{return 7}}function pay(){{return {name}()+2}}globalThis.__out=pay();"
        ));
    }
    check(
        "function Reflect(){return 7}function pay(){let C=function(x){this.value=x};let o=new C(9);return [Reflect(),o.value]}globalThis.__out=JSON.stringify(pay());",
    );
    check(
        "function Symbol(){return 7}function pay(){let out=[];for(let value of [1,2])out.push(value+Symbol());return out}globalThis.__out=JSON.stringify(pay());",
    );
}

#[test]
fn required_unrecoverable_intrinsic_paths_fail_explicitly() {
    let config = ResolvedConfig::try_from(ConfigFlags {
        preset: Some(Intensity::Minify),
        virtualize: Some("pay".into()),
        require_virtualized: Some("pay".into()),
        ..Default::default()
    })
    .unwrap();
    let source =
        "function WeakMap(){}function pay(){return {m(){return 1}}.m()}globalThis.__out=pay();";
    let error = mangler_js::process(source, &ParseOpts::default(), &config).unwrap_err();
    assert!(error.to_string().contains("unavailable WeakMap"), "{error}");
}

#[test]
fn decoder_helpers_are_shared_across_function_and_variable_skeletons() {
    use mangler_testkit::cross_engine::{Engine, evaluate_many, node_path};
    let engine = Engine::Node(node_path().expect("Node is required for shared decoder validation"));
    let source = "function pay(which){function describe(value){return ()=>\"payment:\"+value}try{if(which)throw Error(\"declined\");return describe(\"accepted\")()}catch(error){return error.message}}globalThis.__out=JSON.stringify([pay(0),pay(1)])";
    for seed in 0..8 {
        let config = ResolvedConfig::try_from(ConfigFlags {
            preset: Some(Intensity::Minify),
            virtualize: Some("pay".into()),
            require_virtualized: Some("pay".into()),
            seed: Some(seed),
            ..Default::default()
        })
        .unwrap();
        let (output, _) = mangler_js::process(source, &ParseOpts::default(), &config).unwrap();
        mangler_testkit::assert_behaviorally_equal(source, &output);
        let values = evaluate_many(&engine, &[source, &output]).unwrap();
        assert_eq!(
            values[0], values[1],
            "shared decoder semantics at seed {seed}"
        );
        assert_eq!(
            output.matches("8192===").count() + output.matches("===8192").count(),
            1,
            "seed {seed} duplicated the UTF-16 decoder"
        );
    }
}

#[test]
fn generated_descriptors_ignore_inherited_specification_fields() {
    for (field, value) in [
        ("value", "19"),
        ("writable", "true"),
        ("get", "function(){return 19}"),
        ("set", "function(value){}"),
        ("enumerable", "true"),
        ("configurable", "true"),
    ] {
        for body in [
            "let x=n;const f=()=>x;x=8;return f()",
            "var f=()=>n;n=8;return f()",
            "var o={get x(){return n},set x(v){n=v}};o.x=8;return o.x",
        ] {
            check(&format!(
                "function pay(n){{{body}}}var result;Object.prototype.{field}={value};try{{result=pay(7)}}finally{{delete Object.prototype.{field}}}globalThis.__out=result;"
            ));
        }
    }
}

#[test]
fn source_descriptor_inheritance_and_builtin_identities_remain_native() {
    for (field, value, body) in [
        (
            "value",
            "19",
            "var o={};Object.defineProperty(o,'x',{});return o.x",
        ),
        (
            "get",
            "function(){return 19}",
            "var o={};Object.defineProperty(o,'x',{});return o.x",
        ),
        (
            "set",
            "function(v){this.y=v}",
            "var o={};Object.defineProperty(o,'x',{});o.x=19;return o.y",
        ),
        (
            "writable",
            "true",
            "var o={};Object.defineProperty(o,'x',{value:7});o.x=19;return o.x",
        ),
    ] {
        check(&format!(
            "var nativeDefine=Object.defineProperty;function pay(){{{body}}}var result;Object.prototype.{field}={value};try{{result=pay()}}finally{{delete Object.prototype.{field}}}globalThis.__out=result===19&&Object.defineProperty===nativeDefine;"
        ));
    }
}

#[test]
fn runtime_intrinsic_snapshot_normalizes_generated_descriptor_inputs_and_results() {
    let factory = mangler_js::runtime_frontend::intrinsic_snapshot_factory();
    let source = format!(
        "var resolve=({factory})();var define=resolve(['Object','defineProperty']),describe=resolve(['Object','getOwnPropertyDescriptor']),bulk=resolve(['Object','defineProperties']),create=resolve(['Object','create']);var result;Object.prototype.value=19;try{{var o={{}};define(o,'x',{{get:function(){{return 7}}}});bulk(o,{{y:{{get:function(){{return 8}}}}}});var p=create(null,{{z:{{get:function(){{return 9}}}}}});result=o.x+o.y+p.z;}}finally{{delete Object.prototype.value}}Object.prototype.get=function(){{return 19}};try{{var d=describe({{x:7}},'x');result+=d.get===undefined&&Object.getPrototypeOf(d)===null?1:0;}}finally{{delete Object.prototype.get}}globalThis.__out=result;"
    );
    mangler_testkit::assert_behaviorally_equal("globalThis.__out=25", &source);
}

#[test]
fn private_descriptor_support_does_not_declare_source_names() {
    check(
        "let _manglerDescriptor0=19,_manglerDescriptor1=23;function pay(){let x=7;return x}globalThis.__out=pay()+_manglerDescriptor0+_manglerDescriptor1;",
    );
}
