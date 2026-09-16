//! Arrow wrappers forward arguments without re-entering source iteration.
use mangler_config::{ConfigFlags, Intensity, ResolvedConfig};
use mangler_jsast::ParseOpts;
use mangler_testkit::cross_engine::{Engine, chrome_path, evaluate_many, node_path};

#[test]
fn arrow_forwarding_avoids_user_iterators() {
    let cases = [
        "function pay(){const key=Symbol.iterator,descriptor=Object.getOwnPropertyDescriptor(Array.prototype,key);let log=[],out;try{Array.prototype[key]=function(){log.push('iterator');throw Error('unexpected iterator')};out=((...values)=>values.length)(1,2,3)}finally{Object.defineProperty(Array.prototype,key,descriptor)}return [out,log]}globalThis.__out=JSON.stringify(pay());",
        "function pay(){const key=Symbol.iterator,descriptor=Object.getOwnPropertyDescriptor(Array.prototype,key);let log=[],out;try{Array.prototype[key]=function(){log.push('iterator');throw Error('unexpected iterator')};let f=(a,b,...values)=>[a,b,values.length,values[0],values[1]];out=[f.length,f(1,2,3,4)]}finally{Object.defineProperty(Array.prototype,key,descriptor)}return [out,log]}globalThis.__out=JSON.stringify(pay());",
        "function pay(){const key=Symbol.iterator,descriptor=Object.getOwnPropertyDescriptor(Array.prototype,key);let log=[],out;try{Array.prototype[key]=function(){log.push('iterator');throw Error('unexpected iterator')};let f=(a,b=3,...values)=>[a,b,values.length,values[0]];out=[f.length,f(1,void 0,4)]}finally{Object.defineProperty(Array.prototype,key,descriptor)}return [out,log]}globalThis.__out=JSON.stringify(pay());",
        "function pay(){const key=Symbol.iterator,descriptor=Object.getOwnPropertyDescriptor(Array.prototype,key);let log=[],out;try{Array.prototype[key]=function(){log.push('iterator');throw Error('unexpected iterator')};let f=(a,b)=>[a,b];out=[f(),f(1),f(1,2,3)]}finally{Object.defineProperty(Array.prototype,key,descriptor)}return [out,log]}globalThis.__out=JSON.stringify(pay());",
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
        "arrow argument forwarding require a JavaScript engine"
    );
    let mut programs = Vec::new();
    for source in &cases {
        programs.push(source.to_string());
        for preset in [Intensity::Minify, Intensity::High] {
            for whole in [false, true] {
                let config = ResolvedConfig::try_from(ConfigFlags {
                    preset: Some(preset),
                    seed: Some(23),
                    virtualize: (!whole).then(|| "*".into()),
                    require_virtualized: (!whole).then(|| "*".into()),
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
                        format!("/tmp/mangler-arrow-forwarding-{case}-{variant}.js"),
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
