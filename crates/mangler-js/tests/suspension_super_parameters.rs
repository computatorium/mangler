//! Async method defaults retain their source home object and receiver.
use mangler_config::{ConfigFlags, Intensity, ResolvedConfig};
use mangler_jsast::ParseOpts;
use mangler_testkit::cross_engine::{Engine, chrome_path, evaluate_many, node_path};

#[test]
fn async_method_parameters_keep_source_super_capabilities() {
    let cases = [
        "async function pay(){let sup={method(){return 'sup'}},child={async method(x=super.method()){return await x}};Object.setPrototypeOf(child,sup);return child.method()}pay().then(x=>globalThis.__out=JSON.stringify(x));",
        "async function pay(){class B{method(){return this.value}}class C extends B{value=7;async method(x=super.method()){return await x}}return new C().method()}pay().then(x=>globalThis.__out=JSON.stringify(x));",
        "async function pay(){let log=[],key={[Symbol.toPrimitive](){log.push('key');return 'm'}},sup={m(a){log.push(this.value);return a+this.value}},child={value:4,async method(a=3,x=super[key](a)){return await x}};Object.setPrototypeOf(child,sup);return [await child.method(),log]}pay().then(x=>globalThis.__out=JSON.stringify(x));",
        "async function pay(){let log=[],sup={get n(){log.push('get');return 2},set n(v){log.push(['set',v,this===child])}},child={async method(x=super.n+=(log.push('rhs'),3)){return await x}};Object.setPrototypeOf(child,sup);return [await child.method(),log]}pay().then(x=>globalThis.__out=JSON.stringify(x));",
        "async function pay(){let sup={n:4},child={async method(x=super.n++){return [await x,this.n]}};Object.setPrototypeOf(child,sup);return child.method()}pay().then(x=>globalThis.__out=JSON.stringify(x));",
        "async function pay(){let sup={m(){return this.value}},child={value:9,async method(x=super.m?.(),y=super.absent?.()){return [await x,y]}};Object.setPrototypeOf(child,sup);return child.method()}pay().then(x=>globalThis.__out=JSON.stringify(x));",
        "async function pay(){let sup={tag(t,v){return [this.value,t[0],v]}},child={value:9,async method(x=super.tag`a${3}`){return await x}};Object.setPrototypeOf(child,sup);return child.method()}pay().then(x=>globalThis.__out=JSON.stringify(x));",
        "async function pay(){let sup={n:5},child={async method([x=super.n]=[]){return await x}};Object.setPrototypeOf(child,sup);return child.method()}pay().then(x=>globalThis.__out=JSON.stringify(x));",
        "async function pay(){class B{static m(){return this.value}}class C extends B{static value=8;static async m(x=super.m()){return await x}}return C.m()}pay().then(x=>globalThis.__out=JSON.stringify(x));",
        "async function pay(){class B{m(){return this.value}}class C extends B{value=6;async #m(x=super.m()){return await x}call(){return this.#m()}}return new C().call()}pay().then(x=>globalThis.__out=JSON.stringify(x));",
        "async function pay(){let sup={m(){return this.value}},child={value:12,async method(fn=()=>super.m()){await 0;return fn}};Object.setPrototypeOf(child,sup);return (await child.method())()}pay().then(x=>globalThis.__out=JSON.stringify(x));",
        "async function pay(){let sup={m(){return 2}},child={async *method(x=super.m()){yield x;return x+1}};Object.setPrototypeOf(child,sup);let iterator=child.method();return [await iterator.next(),await iterator.next()]}pay().then(x=>globalThis.__out=JSON.stringify(x));",
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
        "async method super parameters require a JavaScript engine"
    );
    let mut programs = Vec::new();
    for source in &cases {
        programs.push(source.to_string());
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
                        .unwrap_or_else(|error| {
                            panic!("source {source} preset {preset:?} whole {whole}: {error}")
                        })
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
                        format!("/tmp/mangler-super-parameter-{case}-{variant}.js"),
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
