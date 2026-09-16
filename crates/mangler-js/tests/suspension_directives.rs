//! Directive prologues retain lexical strictness across async/generator lowering.
use mangler_config::{ConfigFlags, Intensity, ResolvedConfig};
use mangler_jsast::ParseOpts;
use mangler_testkit::cross_engine::{Engine, chrome_path, evaluate_many, node_path};

#[test]
fn suspension_preserves_strict_writes_receivers_and_escaped_directives() {
    let cases = [
        "async function pay(){let o={get x(){delete this.x;return 2}},run;with(o){run=async function(){'use strict';await 0;try{x+=3}catch(e){return e.name}}}return JSON.stringify([await run(),'x'in o])}pay().then(v=>globalThis.__out=v);",
        "async function pay(){'use strict';await 0;return [this===undefined,(()=>this)()===undefined]}pay().then(v=>globalThis.__out=JSON.stringify(v));",
        "function* pay(){'use strict';yield 0;let o=Object.freeze({x:1});try{o.x=2;return ['ok',this===undefined]}catch(e){return [e.name,this===undefined]}}let it=pay();it.next();globalThis.__out=JSON.stringify(it.next().value);",
        "async function* pay(){'use strict';yield 0;let o=Object.freeze({x:1});try{o.x=2;return ['ok',this===undefined]}catch(e){return [e.name,this===undefined]}}let it=pay();it.next().then(()=>it.next()).then(v=>globalThis.__out=JSON.stringify(v.value));",
        "function* pay(){'use\\x20strict';yield 0;let o=Object.freeze({x:1});try{o.x=2;return ['ok',this===undefined]}catch(e){return [e.name,this===undefined]}}let it=pay();it.next();globalThis.__out=JSON.stringify(it.next().value);",
        "function pay(){'use strict';return (async function(){await 0;return this===undefined})()}pay().then(v=>globalThis.__out=String(v));",
        "function pay(){return Promise.all([(async function(){'use strict';await 0;return this===undefined})(),(async function(){'use\\x20strict';await 0;return this===undefined})()])}pay().then(v=>globalThis.__out=JSON.stringify(v));",
        "async function pay(){let o=Object.freeze({x:1});return (async()=>{'use strict';await 0;try{o.x=2;return 'ok'}catch(e){return e.name}})()}pay().then(v=>globalThis.__out=v);",
    ];
    let mut programs = Vec::new();
    for source in cases {
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
                let (output, notes) =
                    mangler_js::process(source, &ParseOpts::default(), &config).unwrap();
                assert!(
                    whole || notes.iter().any(|note| note.message == "pay: virtualized"),
                    "{notes:?}"
                );
                programs.extend([source.to_string(), output]);
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
    assert!(!engines.is_empty(), "a real engine is required");
    for engine in engines {
        let results = evaluate_many(&engine, &sources).unwrap();
        for (index, pair) in results.chunks_exact(2).enumerate() {
            assert_eq!(
                pair[0],
                pair[1],
                "{} case {} variant {}: {}",
                engine.name(),
                index / 4,
                index % 4,
                sources[index * 2]
            );
        }
    }
}
