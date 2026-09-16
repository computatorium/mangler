//! Parentheses preserve references but suppress identifier NamedEvaluation.
use mangler_config::{ConfigFlags, Intensity, ResolvedConfig};
use mangler_jsast::ParseOpts;
use mangler_testkit::cross_engine::{Engine, chrome_path, evaluate_many, node_path};

#[test]
fn parenthesized_targets_keep_reference_operations_and_scope() {
    check_cases(&[
        "function pay(){let x;(x)=1;((x))+=2;let a=(x)++;let b=++(x);return JSON.stringify([x,a,b])}globalThis.__out=pay();".into(),
        "function pay(){let x=0,o={x:1};with(o){(x)=3;(x)+=2;}return JSON.stringify([x,o.x])}globalThis.__out=pay();".into(),
        "function pay(){let events=[],value=1;let o={get x(){events.push('get');return value},set x(v){events.push('set');value=v}};function key(){events.push('key');return 'x'}function rhs(){events.push('rhs');return 3}(o[key()])+=rhs();let old=(o[key()])++;return JSON.stringify([events,value,old])}globalThis.__out=pay();".into(),
        "function pay(){const x=1;let log=[];try{(x)=2}catch(e){log.push(e.name)}try{(missing)=3}catch(e){log.push(e.name)}return JSON.stringify(log)}globalThis.__out=pay();".replace("function pay(){", "function pay(){'use strict';"),
        "function pay(){let o=null,log=[];try{(o.x)=(log.push('rhs'),3)}catch(e){log.push(e.name)}return JSON.stringify(log)}globalThis.__out=pay();".into(),
    ]);
}

#[test]
fn assignment_names_follow_original_identifier_syntax() {
    let mut cases = Vec::new();
    for operator in ["=", "&&=", "||=", "??="] {
        for value in [
            "function(){}",
            "(()=>{})",
            "(class{static observed=this.name})",
        ] {
            let initial = if operator == "&&=" { "true" } else { "null" };
            cases.push(format!("function pay(){{let bare={initial},grouped={initial},o={{x:{initial}}};bare{operator}{value};(grouped){operator}{value};(o.x){operator}{value};return JSON.stringify([bare.name,grouped.name,o.x.name,bare.observed,grouped.observed,o.x.observed])}}globalThis.__out=pay();"));
        }
    }
    cases.push("function pay(){let x;(x)=function Explicit(){};let a=x.name;(x)=class Explicit{static observed=this.name};return JSON.stringify([a,x.name,x.observed])}globalThis.__out=pay();".into());
    cases.push("function pay(){let count=0,x=1;(x)||=(count++,function(){});return count}globalThis.__out=pay();".into());
    cases.push("function pay(){let named=()=>{};return named.name}globalThis.__out=pay();".into());
    cases.push("function pay(){for(let named=class{static seen=this.name};;)return JSON.stringify([named.name,named.seen])}globalThis.__out=pay();".into());
    check_cases(&cases);
}

#[test]
fn parenthesized_private_and_super_targets_use_existing_lexical_bridges() {
    check_cases(&[
        "function pay(){class A{#x=1;run(){(this.#x)+=2;let before=(this.#x)++;let after=++(this.#x);return[before,after,this.#x]}}return JSON.stringify(new A().run())}globalThis.__out=pay();".into(),
        "function pay(){class A{#x=null;run(){(this.#x)??=class{static observed=this.name};return[this.#x.name,this.#x.observed]}}return JSON.stringify(new A().run())}globalThis.__out=pay();".into(),
        "function pay(){let events=[];class A{get x(){events.push('get');return this.value||0}set x(v){events.push('set');this.value=v}}class B extends A{run(){(super.x)=1;(super.x)+=2;let before=(super.x)++;let after=++(super.x);return[before,after,this.value,events]}}return JSON.stringify(new B().run())}globalThis.__out=pay();".into(),
        "function pay(){class A{get x(){return null}set x(v){this.saved=v}}class B extends A{run(){(super.x)??=class{static observed=this.name};return[this.saved.name,this.saved.observed]}}return JSON.stringify(new B().run())}globalThis.__out=pay();".into(),
        "function pay(){let events=[];function target(){events.push('call');return 0}try{((target()))=(events.push('rhs'),1)}catch(e){events.push(e.name)}return JSON.stringify(events)}globalThis.__out=pay();".into(),
    ]);
}

#[test]
fn parenthesized_targets_survive_suspension_lowering() {
    check_cases(&[
        "async function pay(){let x;(x)=await Promise.resolve(2);(x)+=await Promise.resolve(3);return x}pay().then(v=>globalThis.__out=v);".into(),
        "function* pay(){let x;(x)=yield 2;(x)+=yield 3;return x}let g=pay();globalThis.__out=JSON.stringify([g.next(),g.next(4),g.next(5)]);".into(),
        "async function pay(){let x;(x)=function(){};await 0;let n=x.name;(x)=class{static seen=this.name};return JSON.stringify([n,x.name,x.seen])}pay().then(v=>globalThis.__out=v);".into(),
        "function* pay(){let x;(x)=class{static seen=this.name};yield x.name;return x.seen}let g=pay();globalThis.__out=JSON.stringify([g.next(),g.next()]);".into(),
    ]);
}

fn check_cases(cases: &[String]) {
    let mut programs = Vec::new();
    for source in cases {
        programs.push(source.clone());
        for preset in [Intensity::Minify, Intensity::High] {
            for mode in 0..3 {
                let config = ResolvedConfig::try_from(ConfigFlags {
                    preset: Some(preset),
                    seed: Some(23),
                    virtualize: (mode == 1).then(|| "pay".into()),
                    require_virtualized: (mode == 1).then(|| "pay".into()),
                    virtualize_program: mode == 2,
                    ..Default::default()
                })
                .unwrap();
                let (output, _) = mangler_js::process(source, &ParseOpts::default(), &config)
                    .unwrap_or_else(|error| panic!("{source}: {error}"));
                programs.push(output);
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
        for (case, group) in values.chunks_exact(7).enumerate() {
            for (variant, value) in group[1..].iter().enumerate() {
                if group[0] != *value {
                    std::fs::write(
                        format!("/tmp/mangler-parenthesized36-{case}-{variant}.js"),
                        &programs[case * 7 + variant + 1],
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
