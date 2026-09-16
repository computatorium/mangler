//! Class execution contexts do not belong to the enclosing suspension state machine.
use mangler_config::{ConfigFlags, Intensity, ResolvedConfig};
use mangler_jsast::ParseOpts;
use mangler_testkit::cross_engine::{Engine, chrome_path, evaluate_many, node_path};

#[test]
fn suspended_classes_keep_constructor_and_static_block_boundaries() {
    let cases = [
        "async function f(){class C{constructor(){return ()=>7}}return new C()()}f().then(v=>globalThis.__out=JSON.stringify(v));",
        "function* f(){yield 0;class C{constructor(){return ()=>7}}return new C()()}let i=f();i.next();globalThis.__out=JSON.stringify(i.next().value);",
        "async function f(){await 0;class C{constructor(){return {x:7}}}return new C().x}f().then(v=>globalThis.__out=JSON.stringify(v));",
        "async function f(){await 0;class C{constructor(){return;}}return new C() instanceof C}f().then(v=>globalThis.__out=JSON.stringify(v));",
        "async function f(){await 0;class C{static {var value=7;for(let i=0;i<2;i++){if(i)break;value++}this.value=value}static read(){return this.value}}return C.read()}f().then(v=>globalThis.__out=JSON.stringify(v));",
        "async function f(){await 0;let log=[];try{class C{static {try{log.push(1);throw 7}finally{log.push(2)}}}}catch(e){log.push(e)}return log}f().then(v=>globalThis.__out=JSON.stringify(v));",
        "async function f(){await 0;class C{constructor(){class D{constructor(){return {x:9}}}return new D()}}return new C().x}f().then(v=>globalThis.__out=JSON.stringify(v));",
        "async function f(){await 0;class C{constructor(){return function*(){yield 1;return 2}}}let i=new C()();return [i.next(),i.next()]}f().then(v=>globalThis.__out=JSON.stringify(v));",
        "async function f(){await 0;class C{static {this.run=async()=>{await 0;return 11}}}return await C.run()}f().then(v=>globalThis.__out=JSON.stringify(v));",
        "async function f(){class C{constructor(){return async()=>new.target===C}}return new C()()}f().then(v=>globalThis.__out=JSON.stringify(v));",
        "async function f(){return 1;class C{constructor(){return 2}}}f().then(v=>globalThis.__out=JSON.stringify(v));",
        "async function f(){return 1;let o={x:class{static{this.a=2}}}}f().then(v=>globalThis.__out=JSON.stringify(v));",
        "async function f(){let k=await Promise.resolve('x');class C extends(await Promise.resolve(Object)){constructor(){super();return {[k]:7}}}return new C().x}f().then(v=>globalThis.__out=JSON.stringify(v));",
    ];
    let engines: Vec<_> = [
        node_path().map(Engine::Node),
        chrome_path().map(Engine::Chrome),
    ]
    .into_iter()
    .flatten()
    .collect();
    assert!(
        !engines.is_empty(),
        "class suspension boundaries require a JavaScript engine"
    );
    let mut programs = Vec::new();
    for source in &cases {
        programs.push(source.to_string());
        for preset in [Intensity::Minify, Intensity::High] {
            for whole in [false, true] {
                let config = ResolvedConfig::try_from(ConfigFlags {
                    preset: Some(preset),
                    seed: Some(23),
                    virtualize: (!whole).then(|| "f".into()),
                    require_virtualized: (!whole).then(|| "f".into()),
                    virtualize_program: whole,
                    ..Default::default()
                })
                .unwrap();
                programs.push(
                    mangler_js::process(source, &ParseOpts::default(), &config)
                        .unwrap()
                        .0,
                );
            }
        }
    }
    let sources: Vec<_> = programs.iter().map(String::as_str).collect();
    let mut failures = Vec::new();
    for engine in &engines {
        let results = evaluate_many(engine, &sources).unwrap();
        for (case, group) in results.chunks_exact(5).enumerate() {
            for (variant, result) in group.iter().enumerate().skip(1) {
                if result != &group[0] {
                    std::fs::write(
                        format!("/tmp/mangler-class-boundary-{case}-{variant}.js"),
                        &programs[case * 5 + variant],
                    )
                    .unwrap();
                    failures.push(format!(
                        "{} case {case} variant {variant}: expected {:?}, got {:?}: {}",
                        engine.name(),
                        group[0],
                        result,
                        cases[case]
                    ));
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
