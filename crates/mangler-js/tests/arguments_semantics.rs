//! Arguments must retain native mapped bindings, identity, and descriptors.
use mangler_config::{ConfigFlags, Intensity, ResolvedConfig};
use mangler_jsast::ParseOpts;
use mangler_testkit::eval::{CaptureMode, assert_behaviorally_equal_with};

fn assert_protected(source: &str) {
    for seed in [3, 19] {
        let cfg = ResolvedConfig::try_from(ConfigFlags {
            preset: Some(Intensity::Minify),
            seed: Some(seed),
            virtualize: Some("f".into()),
            require_virtualized: Some("f".into()),
            ..Default::default()
        }).unwrap();
        let (output, notes) = mangler_js::process(source, &ParseOpts::default(), &cfg)
            .unwrap_or_else(|error| panic!("must protect {source}: {error}"));
        assert!(notes.iter().any(|note| note.message == "f: virtualized"), "{notes:?}");
        assert_behaviorally_equal_with(source, &output, &CaptureMode::sink());
    }
}

#[test]
fn mapped_arguments_follow_native_parameter_map() {
    for source in [
        "function f(a){a=7;return arguments[0]} globalThis.__out=String(f(1));",
        "function f(a){arguments[0]=7;return a} globalThis.__out=String(f(1));",
        "function f(a){delete arguments[0];a=7;return [a,arguments[0],0 in arguments]} globalThis.__out=JSON.stringify(f(1));",
        "function f(a){Object.defineProperty(arguments,'0',{value:5,writable:false});a=7;return [a,arguments[0]]} globalThis.__out=JSON.stringify(f(1));",
        "function f(a){Object.defineProperty(arguments,'0',{get(){return 8}});a=7;return [a,arguments[0]]} globalThis.__out=JSON.stringify(f(1));",
        "function f(a){Object.freeze(arguments);a=7;return [a,arguments[0]]} globalThis.__out=JSON.stringify(f(1));",
        "function f(a){a=7;return [a,arguments.length,arguments[0]]} globalThis.__out=JSON.stringify(f());",
        "function f(a,a){a=7;return [arguments[0],arguments[1],a]} globalThis.__out=JSON.stringify(f(1,2));",
        "function f(a){var a; a=7;return arguments[0]} globalThis.__out=String(f(1));",
        "function f(a){var arguments; a=7;return arguments[0]} globalThis.__out=String(f(1));",
        "function f(a){var arguments={0:3}; a=7;return arguments[0]} globalThis.__out=String(f(1));",
    ] { assert_protected(source); }
}

#[test]
fn unmapped_arguments_and_reflection_preserve_native_behavior() {
    for source in [
        "function f(a){'use strict';a=7;return arguments[0]} globalThis.__out=String(f(1));",
        "function f(a=3){a=7;return arguments[0]} globalThis.__out=String(f(1));",
        "function f({a}){a=7;return arguments[0].a} globalThis.__out=String(f({a:1}));",
        "function f(...a){a[0]=7;return arguments[0]} globalThis.__out=String(f(1));",
        "function f(){return [arguments.callee===f,Array.isArray(arguments),Object.prototype.toString.call(arguments)]} globalThis.__out=JSON.stringify(f());",
        "function f(){'use strict';try{return arguments.callee}catch(e){return e.name}} globalThis.__out=String(f());",
        "function f(){'use strict';return typeof Object.getOwnPropertyDescriptor(arguments,'callee').get} globalThis.__out=String(f());",
        "function f(arguments){arguments[0]=7;return arguments[0]} globalThis.__out=String(f([1]));",
    ] { assert_protected(source); }
}
