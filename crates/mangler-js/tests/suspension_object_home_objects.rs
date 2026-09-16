//! Class naming must not force native method home objects through object splitting.
use mangler_config::{ConfigFlags, Intensity, ResolvedConfig};
use mangler_jsast::ParseOpts;
use mangler_testkit::cross_engine::{Engine, chrome_path, evaluate_many, node_path};

#[test]
fn suspended_objects_keep_super_home_objects_and_static_class_names() {
    let cases = [
        "async function pay(){let log=[];let k={[Symbol.toPrimitive](){log.push('key');return 'x'}};let o={__proto__:{x:4},async m(){return super[k]+=(log.push('rhs'),await Promise.resolve(3))}};return [await o.m(),o.x,log]}pay().then(x=>globalThis.__out=JSON.stringify(x));",
        "function pay(){let log=[];let k={[Symbol.toPrimitive](){log.push('key');return 'x'}};let o={__proto__:{x:4},*m(){return super[k]+=yield 3}};let i=o.m();return [i.next(),i.next(7),o.x,log]}globalThis.__out=JSON.stringify(pay());",
        "async function pay(){let o={__proto__:{x:4},Named:class{static observed=this.name},async m(){return super.x}};return [await o.m(),o.Named.name,o.Named.observed]}pay().then(x=>globalThis.__out=JSON.stringify(x));",
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
        "suspended object home objects require a JavaScript engine"
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
                        format!("/tmp/mangler-suspended-object-home-{case}-{variant}.js"),
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
