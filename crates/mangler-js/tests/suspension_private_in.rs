//! Private brand syntax must not become an evaluated value across suspension.
use mangler_config::{ConfigFlags, Intensity, ResolvedConfig};
use mangler_jsast::ParseOpts;
use mangler_testkit::cross_engine::{Engine, chrome_path, evaluate_many, node_path};

#[test]
fn private_brand_checks_keep_suspended_rhs_and_lexical_names() {
    let cases = [
        "async function pay(){class C{#x;static async check(v,flag){return flag && (#x in await v)}static async bits(v){return 2 | ((#x in await v) & 1)}}return [await C.check(new C(),true),await C.check({},true),await C.check(1,false),await C.bits(new C())]}pay().then(x=>globalThis.__out=JSON.stringify(x));",
        "function pay(){class C{#x;static *check(v){return false || (#x in (yield v))}}let a=C.check(1);return [a.next(),a.next(new C())]}globalThis.__out=JSON.stringify(pay());",
        "async function pay(){class C{#field;static async check(value){return #field in await value}}return [await C.check(new C()),await C.check({}),await C.check(1).catch(e=>e.name)]}pay().then(x=>globalThis.__out=JSON.stringify(x));",
        "function pay(){class C{#field;static *check(value){return #field in (yield value)}}let a=C.check(1),b=C.check(2),c=C.check(3);let out=[a.next(),a.next(new C()),b.next(),b.next({}),c.next()];try{c.next(null)}catch(e){out.push(e.name)}return out}globalThis.__out=JSON.stringify(pay());",
        "async function pay(){let log=[];class C{#x;static async check(value){return #x in (log.push('before'),await value,log.push('after'),new C())}}return [await C.check(0),log]}pay().then(x=>globalThis.__out=JSON.stringify(x));",
        "async function pay(){class A{#x;static async check(a){class B{#x;static async check(b){return #x in await b}}return [#x in await a,await B.check(a),await B.check(new B())]}}return A.check(new A())}pay().then(x=>globalThis.__out=JSON.stringify(x));",
        "async function pay(){class C{#x;static async check(){return #x in await Promise.reject('source rejection')}}return C.check().catch(e=>e)}pay().then(x=>globalThis.__out=JSON.stringify(x));",
        "async function pay(){class C{#x;static async check(){return 7;return #x in await 1}static async other(v){return [typeof await v,#x in await v]}}return [await C.check(),await C.other(new C())]}pay().then(x=>globalThis.__out=JSON.stringify(x));",
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
        "suspended private brand checks require a JavaScript engine"
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
                        format!("/tmp/mangler-private-in-suspension-{case}-{variant}.js"),
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
