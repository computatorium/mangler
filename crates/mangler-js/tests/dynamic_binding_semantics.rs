//! Scope-aware operations must execute before a capture's ordinary getter.
use mangler_config::{ConfigFlags, Intensity, ResolvedConfig};
use mangler_jsast::ParseOpts;
use mangler_testkit::eval::{CaptureMode, assert_behaviorally_equal_with};

fn protected_source(source: &str, seed: u64) -> String {
    let cfg = ResolvedConfig::try_from(ConfigFlags {
        preset: Some(Intensity::Minify),
        seed: Some(seed),
        virtualize: Some("f".into()),
        require_virtualized: Some("f".into()),
        ..Default::default()
    })
    .unwrap();
    let (output, notes) = mangler_js::process(source, &ParseOpts::default(), &cfg)
        .unwrap_or_else(|error| panic!("must protect {source}: {error}"));
    assert!(
        notes.iter().any(|note| note.message == "f: virtualized"),
        "{notes:?}"
    );
    output
}

fn assert_protected(source: &str) {
    for seed in [5, 23] {
        assert_behaviorally_equal_with(
            source,
            &protected_source(source, seed),
            &CaptureMode::sink(),
        );
    }
}

#[test]
fn typeof_preserves_missing_binding_and_tdz_distinction() {
    for source in [
        "function f(){return typeof __missing_binding_1837} globalThis.__out=f();",
        "function f(){return typeof ((__missing_binding_1837))} globalThis.__out=f();",
        "let value=4;function f(){return typeof value} globalThis.__out=f();",
        "function f(){try{return typeof value}catch(e){return e.name}} globalThis.__out=f();let value=4;",
        "function f(){function g(){return typeof __missing_binding_1837}return g()} globalThis.__out=f();",
        "function f(){try{return __missing_binding_1837}catch(e){return e.name+':'+typeof __missing_binding_1837}} globalThis.__out=f();",
        "function f(){return [typeof value,value]} let value=4;globalThis.__out=JSON.stringify(f());",
    ] {
        assert_protected(source);
    }
}

#[test]
fn delete_uses_source_binding_reference_without_reading_value() {
    for source in [
        "function f(){return delete __missing_binding_1837} globalThis.__out=String(f());",
        "var value=4;function f(){return [delete value,value]} globalThis.__out=JSON.stringify(f());",
        "let value=4;function f(){return [delete value,value]} globalThis.__out=JSON.stringify(f());",
        "globalThis.__removable=4;function f(){return [delete __removable,typeof __removable]}globalThis.__out=JSON.stringify(f());",
        "Object.defineProperty(globalThis,'__readtrap',{configurable:true,get(){throw 1}});function f(){return delete __readtrap}globalThis.__out=String(f());",
        "function f(){function g(){return delete __missing_binding_1837}return g()}globalThis.__out=String(f());",
    ] {
        assert_protected(source);
    }
}

#[test]
fn annex_b_if_functions_preserve_block_and_variable_bindings() {
    let mut programs = Vec::new();
    for source in [
        "function f(){var before=typeof g;if(true)function g(){return 7}return [before,g()]}globalThis.__out=JSON.stringify(f());",
        "function f(){if(false)function g(){return 1}else function g(){return 2}return g()}globalThis.__out=f();",
        "function f(){let g=3;if(true)function g(){return 1}return g}globalThis.__out=f();",
        "function f(g){if(true)function g(){return 1}return g}globalThis.__out=f(3);",
        "function f(){try{throw {}}catch({g}){if(true)function g(){return 1}}try{return g}catch(e){return e.name}}globalThis.__out=f();",
        "function f(){var g;if(true)function g(){return 1}var first=g;if(false)function g(){return 2}return first===g}globalThis.__out=f();",
    ] {
        programs.push(source.to_string());
        programs.extend([5, 23].map(|seed| protected_source(source, seed)));
    }
    // QuickJS's unwrapped if/else function declarations do not implement the
    // web-legacy binding behavior exercised here. Compare actual browser hosts.
    use mangler_testkit::cross_engine::{Engine, chrome_path, evaluate_many, node_path};
    let sources: Vec<&str> = programs.iter().map(String::as_str).collect();
    for engine in [
        node_path().map(Engine::Node),
        chrome_path().map(Engine::Chrome),
    ]
    .into_iter()
    .flatten()
    {
        let values = evaluate_many(&engine, &sources).unwrap();
        for (index, group) in values.chunks_exact(3).enumerate() {
            assert_eq!(
                group[0],
                group[1],
                "{} Annex B case {index}, seed 5",
                engine.name()
            );
            assert_eq!(
                group[0],
                group[2],
                "{} Annex B case {index}, seed 23",
                engine.name()
            );
        }
    }
}

#[test]
fn whole_program_annex_b_respects_lexical_barriers() {
    let mut programs = Vec::new();
    for source in [
        "let f=123;{function f(){}}globalThis.__out=f;".to_string(),
        "class f{static value=123}{function f(){}}globalThis.__out=f.value;".to_string(),
    ].into_iter().chain([
        "{let f=123;{function f(){}}}",
        "{using f=null;{function f(){}}}",
        "for(using f of [null]){function f(){}}",
        "for(let f=0;f<1;f++){function f(){}}",
        "for(let f of [1]){function f(){}}",
        "for(let f in {x:1}){function f(){}}",
        "switch(1){case 1:let f;{function f(){}}}",
        "try{throw {}}catch({f}){{function f(){}}}",
    ].map(|body| format!("var result=[];try{{f;result.push('bound')}}catch(e){{result.push(e.name)}}{body}try{{f;result.push('bound')}}catch(e){{result.push(e.name)}}globalThis.__out=JSON.stringify(result);"))) {
        programs.push(source.clone());
        for seed in [5, 23] {
            let cfg = ResolvedConfig::try_from(ConfigFlags {
                preset: Some(Intensity::Minify), seed: Some(seed), virtualize_program: true,
                ..Default::default()
            }).unwrap();
            programs.push(mangler_js::process(&source, &ParseOpts::default(), &cfg).unwrap().0);
        }
    }
    use mangler_testkit::cross_engine::{Engine, chrome_path, evaluate_many, node_path};
    let sources: Vec<&str> = programs.iter().map(String::as_str).collect();
    for engine in [
        node_path().map(Engine::Node),
        chrome_path().map(Engine::Chrome),
    ]
    .into_iter()
    .flatten()
    {
        let values = evaluate_many(&engine, &sources).unwrap();
        for (index, group) in values.chunks_exact(3).enumerate() {
            assert_eq!(
                group[0],
                group[1],
                "{} global case {index}, seed 5",
                engine.name()
            );
            assert_eq!(
                group[0],
                group[2],
                "{} global case {index}, seed 23",
                engine.name()
            );
        }
    }
}

#[test]
fn lexical_suspension_declarations_and_legacy_loop_initializers_keep_scope() {
    let mut programs = Vec::new();
    for body in [
        "{async function g(){}}return typeof g",
        "{function* g(){}}return typeof g",
        "{async function* g(){}}return typeof g",
        "let g=3;{async function g(){}}return g",
        "var before;{before=g;async function g(){return g=7}g()}return [typeof before,typeof g]",
        "switch(1){case 1:async function g(){}}return typeof g",
        "switch(1){case 1:function* g(){}}return typeof g",
        "var n=0;for(var g=++n in {});return [g,n]",
    ] {
        let source = format!("var f=(function(){{{body}}});globalThis.__out=JSON.stringify(f());");
        programs.push(source.clone());
        for preset in [Intensity::Minify, Intensity::High] {
            for virtualized in [false, true] {
                let cfg = ResolvedConfig::try_from(ConfigFlags {
                    preset: Some(preset),
                    seed: Some(23),
                    virtualize: virtualized.then(|| "f".into()),
                    require_virtualized: virtualized.then(|| "f".into()),
                    ..Default::default()
                })
                .unwrap();
                programs.push(
                    mangler_js::process(&source, &ParseOpts::default(), &cfg)
                        .unwrap_or_else(|error| {
                            panic!("{preset:?}, vm={virtualized}, {source}: {error}")
                        })
                        .0,
                );
            }
        }
    }
    use mangler_testkit::cross_engine::{Engine, chrome_path, evaluate_many, node_path};
    let sources: Vec<&str> = programs.iter().map(String::as_str).collect();
    for engine in [
        node_path().map(Engine::Node),
        chrome_path().map(Engine::Chrome),
    ]
    .into_iter()
    .flatten()
    {
        let values = evaluate_many(&engine, &sources).unwrap();
        for (index, group) in values.chunks_exact(5).enumerate() {
            for actual in &group[1..] {
                assert_eq!(
                    actual,
                    &group[0],
                    "{} lexical callable case {index}",
                    engine.name()
                );
            }
        }
    }
}
