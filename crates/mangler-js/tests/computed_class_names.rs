//! Key conversion precedes class evaluation; inferred names precede static initialization.
use mangler_config::{ConfigFlags, Intensity, ResolvedConfig};
use mangler_jsast::ParseOpts;
use mangler_testkit::cross_engine::{Engine, chrome_path, evaluate_many, node_path};

#[test]
fn computed_class_names_preserve_key_conversion_and_static_initialization() {
    let cases = [
        "function pay(){let log=[];let key={[Symbol.toPrimitive](hint){log.push(hint);return 'payment'}};let object={[key]:class extends (log.push('heritage'),Object){static observed=(log.push(this.name),this.name)}};return JSON.stringify([log,object.payment.name,object.payment.observed])}globalThis.__out=pay();",
        "function pay(){let result=[];for(let key of [Symbol('payment'),Symbol(),Symbol(''),Symbol.iterator]){let o={[key]:class{static observed=this.name}};result.push([o[key].name,o[key].observed])}return JSON.stringify(result)}globalThis.__out=pay();",
        "function pay(){let result=[];for(let key of ['__proto__','constructor','\\ud800','\\udfff',42,1n]){let o={[key]:(class{static observed=this.name})};result.push([o[key].name,o[key].observed,Object.hasOwn(o,key)])}return JSON.stringify(result)}globalThis.__out=pay();",
        "function pay(){let log=[];let key={[Symbol.toPrimitive](){log.push('key');throw 7}};try{let o={[key]:class extends (log.push('heritage'),Object){static value=log.push('static')}}}catch(e){log.push(e)}return JSON.stringify(log)}globalThis.__out=pay();",
        "function pay(){let key='outer',log=[];let o={[key]:class{static first=(key='changed',this.name);static nested=({[key]:class{static observed=this.name}})[key]}};return JSON.stringify([o.outer.name,o.outer.first,o.outer.nested.name,o.outer.nested.observed,key])}globalThis.__out=pay();",
        "function pay(){let n=0;let key={toString(){n++;return 'key'}};let o={[key]:class Explicit{static observed=this.name}};return JSON.stringify([n,o.key.name,o.key.observed])}globalThis.__out=pay();",
        "async function pay(){let log=[];let key={toString(){log.push('key');return 'async'}};let o={[await Promise.resolve(key)]:class{static observed=this.name}};return JSON.stringify([log,o.async.name,o.async.observed])}pay().then(value=>globalThis.__out=value);",
        "function* pay(){let key=yield 'key';let o={[key]:class{static observed=this.name}};return [o[key].name,o[key].observed]}let iterator=pay();let first=iterator.next();globalThis.__out=JSON.stringify([first,iterator.next('generator')]);",
        "function pay(){let log=[],descriptor=Object.getOwnPropertyDescriptor(Symbol.prototype,'description');try{Object.defineProperty(Symbol.prototype,'description',{configurable:true,get(){log.push('description');return 'wrong'}});let key=Symbol('right');let o={[key]:class{static observed=this.name}};return JSON.stringify([log,o[key].name,o[key].observed])}finally{Object.defineProperty(Symbol.prototype,'description',descriptor)}}globalThis.__out=pay();",
        "function pay(){let key='initial';let o={[key]:class{static before=this.name;static name='overwritten';static after=this.name}};return JSON.stringify([o.initial.before,o.initial.name,o.initial.after])}globalThis.__out=pay();",
        "function pay(){let o={__proto__:class{static observed=this.name},['__proto__']:class{static observed=this.name}};let proto=Object.getPrototypeOf(o);return JSON.stringify([proto.name,proto.observed,o.__proto__.name,o.__proto__.observed,Object.hasOwn(o,'__proto__')])}globalThis.__out=pay();",
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
        "computed class names require a JavaScript engine"
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
                        format!("/tmp/mangler-computed-class-name-{case}-{variant}.js"),
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
