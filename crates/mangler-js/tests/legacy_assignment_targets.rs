//! Annex B call targets retain their evaluation and abrupt-completion order.
use mangler_config::{ConfigFlags, Intensity, ResolvedConfig};
use mangler_jsast::ParseOpts;
use mangler_testkit::cross_engine::{Engine, chrome_path, evaluate_many, node_path};

#[test]
fn call_assignment_targets_throw_after_call_before_rhs_or_coercion() {
    let mut cases = Vec::new();
    for operation in [
        "target()=rhs()",
        "(target())=rhs()",
        "target()+=rhs()",
        "target()**=rhs()",
        "target()<<=rhs()",
        "target()++",
        "++target()",
        "target()--",
        "--target()",
        "holder.target()=rhs()",
        "(holder?.target)()=rhs()",
        "target()()=rhs()",
        "for(target() in {x:1})events.push('body')",
        "for(target() in {})events.push('body')",
        "for(target() of iterable)events.push('body')",
        "for(target() of [])events.push('body')",
    ] {
        cases.push(format!(
            "function pay(){{var events=[];function target(){{events.push(this===holder?'receiver':'call');return result}}var result=function(){{events.push('second');return 1}};result.valueOf=function(){{events.push('coerce');return 1}};function rhs(){{events.push('rhs');return 2}}var holder={{target:target}};var iterable={{[Symbol.iterator](){{events.push('iterator');return{{next(){{events.push('next');return{{done:false,get value(){{events.push('value');return 3}}}}}},return(){{events.push('close');throw Error('cleanup')}}}}}}}};try{{{operation}}}catch(e){{events.push(e.name)}}return JSON.stringify(events)}}globalThis.__out=pay();"
        ));
    }
    cases.push("function pay(){var events=[];function target(){events.push('call');throw 17}function rhs(){events.push('rhs')}try{target()=rhs()}catch(e){events.push(e)}return JSON.stringify(events)}globalThis.__out=pay();".into());
    cases.push("let ReferenceError=17;function pay(){function target(){return 1}try{target()=2}catch(e){return e.name+':'+ReferenceError}}globalThis.__out=pay();".into());
    check_cases(cases);
}

#[test]
fn async_iteration_call_targets_keep_value_and_close_order() {
    let mut cases = Vec::new();
    for protocol in ["iterator", "asyncIterator"] {
        let source = format!(
            "async function pay(){{var events=[];function target(){{events.push('call');return 1}}var iterable={{[Symbol.{protocol}](){{return{{next(){{events.push('next');return{{done:false,get value(){{events.push('value');return 3}}}}}},return(){{events.push('close');return{{done:true}}}}}}}}}};try{{for await(target() of iterable)events.push('body')}}catch(e){{events.push(e.name)}}return JSON.stringify(events)}}pay().then(value=>{{globalThis.__out=value}});"
        );
        cases.push(
            source
                .replace("async function pay", "async function* pay")
                .replace(
                    "pay().then(value=>{globalThis.__out=value})",
                    "pay().next().then(result=>{globalThis.__out=result.value})",
                ),
        );
        cases.push(source);
    }
    check_cases(cases);
}

#[test]
fn generator_iteration_assignment_targets_preserve_order_and_scope() {
    let mut cases = Vec::new();
    for operation in [
        "target()=rhs()",
        "for(target() of iterable);",
        "for(target() in {x:1});",
        "for(holder().x of iterable){let holder=0;break;}",
    ] {
        cases.push(format!(
            "function* pay(){{var events=[];function target(){{events.push('call');return 1}}function rhs(){{events.push('rhs');return 2}}function holder(){{events.push('reference');return{{set x(v){{events.push('set')}}}}}}var iterable={{[Symbol.iterator](){{return{{next(){{events.push('next');return{{done:false,get value(){{events.push('value');return 3}}}}}},return(){{events.push('close');return{{done:true}}}}}}}}}};try{{{operation}}}catch(e){{events.push(e.name)}}return JSON.stringify(events)}}globalThis.__out=pay().next().value;"
        ));
    }

    check_cases(cases);
}

fn check_cases(cases: Vec<String>) {
    let mut programs = Vec::new();
    for source in &cases {
        programs.push(source.clone());
        for preset in [Intensity::Minify, Intensity::High] {
            for seed in [5, 23] {
                for whole_program in [false, true] {
                    let config = ResolvedConfig::try_from(ConfigFlags {
                        preset: Some(preset),
                        seed: Some(seed),
                        virtualize_program: whole_program,
                        virtualize: (!whole_program).then(|| "pay".into()),
                        require_virtualized: (!whole_program).then(|| "pay".into()),
                        ..Default::default()
                    })
                    .unwrap();
                    let (output, _) = mangler_js::process(source, &ParseOpts::default(), &config)
                        .unwrap_or_else(|error| panic!("{source}: {error}"));
                    programs.push(output);
                }
            }
        }
    }
    let sources: Vec<_> = programs.iter().map(String::as_str).collect();
    for engine in [
        node_path().map(Engine::Node),
        chrome_path().map(Engine::Chrome),
    ]
    .into_iter()
    .flatten()
    {
        let values = evaluate_many(&engine, &sources).unwrap();
        for (case, group) in values.chunks_exact(9).enumerate() {
            for (variant, value) in group[1..].iter().enumerate() {
                if group[0] != *value {
                    std::fs::write(
                        "/tmp/mangler-legacy-target-failure.js",
                        &programs[case * 9 + variant + 1],
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

#[test]
fn generator_iteration_patterns_keep_lazy_values_and_lexical_targets() {
    let mut cases = vec![
        "function* pay(){let x=0;for(x of [3]){}return x}globalThis.__out=pay().next().value;".into(),
        "function* pay(){const x=0;try{for(x of [3]){}}catch(e){return e.name}}globalThis.__out=pay().next().value;".into(),
        "function* pay(){let x=0;let o={x:1};with(o){for(x of[4]){yield[x,o.x]}}return[x,o.x]}var g=pay();globalThis.__out=JSON.stringify([g.next(),g.next()]);".into(),
        "function* pay(){let x=0;for(x of[4]){let x=9;yield x}return x}var g=pay();globalThis.__out=JSON.stringify([g.next(),g.next()]);".into(),
    ];
    for resume in ["return('stop')", "next(7)"] {
        cases.push(format!(
            "let log=[];const inner={{[Symbol.iterator](){{log.push('inner-open');return this}},next(){{log.push('inner-next');return{{done:false,value:undefined}}}},return(){{log.push('inner-close');return{{}}}}}};const outer={{[Symbol.iterator](){{return this}},next(){{log.push('outer-next');return{{done:false,get value(){{log.push('outer-value');return inner}}}}}},return(){{log.push('outer-close');return{{}}}}}};function* pay(){{let a,b;for([a=yield 'default',b] of outer){{break}}return[a,b]}}const g=pay();let first=g.next();let before=log.slice();let last=g.{resume};globalThis.__out=JSON.stringify([first,before,last,log]);"
        ));
    }
    check_cases(cases);
}
