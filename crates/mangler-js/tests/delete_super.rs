//! Deleting a SuperReference evaluates its raw key, then throws ReferenceError.
use mangler_config::{ConfigFlags, Intensity, ResolvedConfig};
use mangler_jsast::ParseOpts;
use mangler_testkit::cross_engine::{Engine, chrome_path, evaluate_many, node_path};

#[test]
fn delete_super_keeps_reference_errors_without_property_access_or_coercion() {
    check_cases(&[
        "function pay(){class B{}class C extends B{constructor(){super();delete super.x}}try{new C}catch(e){return e.name}}globalThis.__out=pay();",
        "function pay(){class C{static m(){delete super.x}}Object.setPrototypeOf(C,null);try{C.m()}catch(e){return e.name}}globalThis.__out=pay();",
        "function pay(){let log=[];class B{}class C extends B{constructor(){try{delete super[(log.push('key'),{toString(){log.push('coerce')}})]}catch(e){log.push(e.name)}super()}}new C;return JSON.stringify(log)}globalThis.__out=pay();",
        "function pay(){let log=[];class B{}class C extends B{constructor(){try{delete super[(()=>{log.push('key');throw 'key-error'})()]}catch(e){log.push(String(e))}super()}}new C;return JSON.stringify(log)}globalThis.__out=pay();",
        "function pay(){let log=[];const key={[Symbol.toPrimitive](){log.push('coerce');throw Error('key')}};let o={__proto__:new Proxy({},{get(){log.push('get');throw Error('get')}}),m(){return delete ((super[(log.push('key'),key)]))}};try{o.m()}catch(e){log.push(e.name)}return JSON.stringify(log)}globalThis.__out=pay();",
        "function pay(){let log=[];class B{get x(){log.push('get');return 1}}class C extends B{m(){return (()=>delete (super.x))()}}try{new C().m()}catch(e){log.push(e.name)}return JSON.stringify(log)}globalThis.__out=pay();",
        "function pay(){let log=[];let o={__proto__:{get x(){log.push('get');return {y:1}}},m(){return [delete (0,super.x),delete super.x.y]}};return JSON.stringify([o.m(),log])}globalThis.__out=pay();",
        "function pay(){let ReferenceError=function(){throw Error('shadow')};let o={m(){return delete super.x}};try{o.m()}catch(e){return e.name}}globalThis.__out=pay();",
    ]);
}

#[test]
fn delete_super_preserves_suspended_keys_and_lexical_arrow_contexts() {
    check_cases(&[
        "function pay(){let log=[];let o={*m(){try{delete super[(log.push('key'),yield 1)]}catch(e){log.push(e.name)}}};let i=o.m();let first=i.next();let second=i.next({toString(){log.push('coerce')}});return JSON.stringify([first,second,log])}globalThis.__out=pay();",
        "async function pay(){let log=[];class B{}class C extends B{async m(){try{delete super[await (log.push('key'),Promise.resolve({toString(){log.push('coerce')}}))]}catch(e){log.push(e.name)}}}await new C().m();return JSON.stringify(log)}pay().then(v=>globalThis.__out=v);",
        "async function pay(){let log=[];let o={async*m(){try{delete super[await (log.push('key'),1)]}catch(e){yield e.name}}};let i=o.m();return JSON.stringify([await i.next(),await i.next(),log])}pay().then(v=>globalThis.__out=v);",
        "async function pay(){let log=[];class C{m(){return async()=>{try{delete super[await(log.push('key'),1)]}catch(e){log.push(e.name)}}}}await new C().m()();return JSON.stringify(log)}pay().then(v=>globalThis.__out=v);",
        "function pay(){let log=[];class C{*m(){try{delete super[yield 1]}finally{log.push('finally')}}}let i=new C().m();let first=i.next();let end=i.return(7);return JSON.stringify([first,end,log])}globalThis.__out=pay();",
    ]);
}

#[test]
fn delete_super_in_eval_uses_the_existing_lexical_capability() {
    check_cases(&[
        "function pay(){class C{m(){try{return eval('delete super.x')}catch(e){return e.name}}}return new C().m()}globalThis.__out=pay();",
        "function pay(){let log=[],key={toString(){log.push('coerce')}};let o={m(){try{return eval('delete (super[(log.push(1),key)])')}catch(e){log.push(e.name)}}};o.m();return JSON.stringify(log)}globalThis.__out=pay();",
    ]);
}

fn check_cases(cases: &[&str]) {
    let mut programs = Vec::new();
    for source in cases {
        for whole in [false, true] {
            let config = ResolvedConfig::try_from(ConfigFlags {
                preset: Some(Intensity::Minify),
                seed: Some(42),
                virtualize: (!whole).then(|| "pay".into()),
                virtualize_program: whole,
                require_virtualized: Some("pay".into()),
                ..Default::default()
            })
            .unwrap();
            let (output, _) =
                mangler_js::runner::process(source, &ParseOpts::default(), &config).unwrap();
            programs.push((*source).to_owned());
            programs.push(output);
        }
    }
    let mut engines = vec![Engine::Node(
        node_path().expect("Node is required for delete-super semantics"),
    )];
    engines.extend(chrome_path().map(Engine::Chrome));
    let sources: Vec<_> = programs.iter().map(String::as_str).collect();
    for engine in engines {
        let values = evaluate_many(&engine, &sources).unwrap();
        for (index, pair) in values.chunks_exact(2).enumerate() {
            assert_eq!(
                pair[0]["outcome"][0],
                "value",
                "invalid native fixture: {}",
                sources[index * 2]
            );
            assert_eq!(
                pair[0],
                pair[1],
                "{}: {}",
                engine.name(),
                sources[index * 2]
            );
        }
    }
}
