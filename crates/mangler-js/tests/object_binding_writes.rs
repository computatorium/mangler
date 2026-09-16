//! Object Environment SetMutableBinding must recheck existence after RHS/GetValue.
use mangler_config::{ConfigFlags, Intensity, ResolvedConfig};
use mangler_jsast::ParseOpts;
use mangler_testkit::cross_engine::{Engine, chrome_path, evaluate_many, node_path};

#[test]
fn object_binding_writes_recheck_removed_properties() {
    // Spec oracle: PutValue retains the resolved environment across RHS/getter
    // effects. V8 can instead redirect a sloppy simple write after RHS deletion.
    // https://tc39.es/ecma262/#sec-object-environment-records-setmutablebinding-n-v-s
    let mut cases = Vec::new();
    let mut expected = Vec::new();
    for strict in [false, true] {
        for operation in [
            "x=rhs()",
            "x+=rhs()",
            "x&&=rhs()",
            "x||=rhs()",
            "x??=rhs()",
            "x++",
            "++x",
            "x--",
            "--x",
        ] {
            let initial = if operation.contains("||=") || operation.contains("??=") {
                "null"
            } else {
                "2"
            };
            let directive = if strict { "'use strict';" } else { "" };
            let result = if strict {
                "\"ReferenceError\""
            } else {
                match operation {
                    "x+=rhs()" => "5",
                    "x++" | "x--" => "2",
                    "--x" => "1",
                    _ => "3",
                }
            };
            let events = if operation == "x=rhs()" {
                "[\"rhs\"]"
            } else if operation.contains("rhs") {
                "[\"get\",\"rhs\"]"
            } else {
                "[\"get\"]"
            };
            expected.push(format!("[{result},{events},{}]", !strict));
            cases.push(format!("function pay(){{let events=[],scope={{get x(){{events.push('get');delete this.x;return {initial}}}}};function rhs(){{events.push('rhs');delete scope.x;return 3}}let run;with(scope){{run=function(){{{directive}return {operation}}}}}let result;try{{result=run()}}catch(e){{result=e.name}}return JSON.stringify([result,events,Object.hasOwn(scope,'x')])}}globalThis.__out=pay();"));
        }
    }
    // The captured environment survives suspension; inherited properties count.
    cases.push("function pay(){let parent={x:7},o=Object.create(parent),run;Object.defineProperty(o,'x',{configurable:true,get(){delete this.x;return 2}});with(o){run=function(){'use strict';return x+=3}}return JSON.stringify([run(),o.x,parent.x])}globalThis.__out=pay();".into());
    expected.push("[5,5,7]".into());
    let expected: Vec<_> = expected.iter().map(String::as_str).collect();
    check(&cases, Some(&expected));
}

#[test]
fn async_object_binding_writes_retain_strictness() {
    let cases = vec!["async function pay(){let o={get x(){delete this.x;return 2}},run;with(o){run=async function(){'use strict';await 0;try{x+=3}catch(e){return e.name}}}return JSON.stringify([await run(),'x'in o])}pay().then(v=>globalThis.__out=v);".into()];
    check(&cases, Some(&["[\"ReferenceError\",false]"]));
}

#[test]
fn object_binding_set_observes_has_before_set() {
    // Normative oracle: ECMA-262 9.1.1.2.5 calls HasProperty even for sloppy
    // writes. V8 26 currently omits this second has trap, so native equivalence
    // is not an oracle for these proxy observations.
    // https://tc39.es/ecma262/#sec-object-environment-records-setmutablebinding-n-v-s
    let mut cases = Vec::new();
    for strict in [false, true] {
        let directive = if strict { "'use strict';" } else { "" };
        for abrupt in [false, true] {
            cases.push(format!("function pay(){{let log=[],count=0,o=new Proxy({{x:1}},{{has(t,k){{if(k==='x'){{log.push('has');if(++count===2&&{abrupt})throw 'stop'}}return Reflect.has(t,k)}},set(t,k,v,r){{log.push('set');return Reflect.set(t,k,v,r)}}}}),run;with(o){{run=function(){{{directive}x=3}}}}try{{run()}}catch(e){{log.push(e)}}return JSON.stringify(log)}}globalThis.__out=pay();"));
        }
    }
    check(
        &cases,
        Some(&[
            "[\"has\",\"has\",\"set\"]",
            "[\"has\",\"has\",\"stop\"]",
            "[\"has\",\"has\",\"set\"]",
            "[\"has\",\"has\",\"stop\"]",
        ]),
    );
}

fn check(cases: &[String], expected: Option<&[&str]>) {
    let mut programs = Vec::new();
    for (case, source) in cases.iter().enumerate() {
        programs.push(expected.map_or_else(
            || source.clone(),
            |values| format!("globalThis.__out={:?};", values[case]),
        ));
        for preset in [Intensity::Minify, Intensity::High] {
            for whole in [false, true] {
                let config = ResolvedConfig::try_from(ConfigFlags {
                    preset: Some(preset),
                    seed: Some(23),
                    virtualize: (!whole).then(|| "pay".into()),
                    require_virtualized: (!whole).then(|| "pay".into()),
                    virtualize_program: whole,
                    ..Default::default()
                })
                .unwrap();
                programs.push(
                    mangler_js::process(source, &ParseOpts::default(), &config)
                        .unwrap_or_else(|error| panic!("{source}: {error}"))
                        .0,
                );
            }
        }
    }
    let sources: Vec<_> = programs.iter().map(String::as_str).collect();
    let engines: Vec<_> = [
        node_path().map(Engine::Node),
        chrome_path().map(Engine::Chrome),
    ]
    .into_iter()
    .flatten()
    .collect();
    assert!(!engines.is_empty(), "a real JavaScript engine is required");
    for engine in engines {
        let values = evaluate_many(&engine, &sources).unwrap();
        for (case, group) in values.chunks_exact(5).enumerate() {
            for (variant, value) in group[1..].iter().enumerate() {
                if group[0] != *value {
                    std::fs::write(
                        "/tmp/mangler-object-binding36-failure.js",
                        &programs[case * 5 + variant + 1],
                    )
                    .unwrap();
                }
                assert_eq!(
                    group[0],
                    *value,
                    "{} case {case} variant {variant}: {}",
                    engine.name(),
                    cases[case]
                );
            }
        }
    }
}
