use mangler_jsast::ParseOpts;
use mangler_testkit::cross_engine::{Engine, evaluate_many, node_path};

fn equivalent(sources: &[&str]) {
    let node = node_path().expect("Node is required for resource disposal differential tests");
    for source in sources {
        for seed in [7, 42] {
            let config = mangler_config::ResolvedConfig::try_from(mangler_config::ConfigFlags {
                preset: Some(mangler_config::Intensity::Minify),
                seed: Some(seed),
                virtualize: Some("pay".into()),
                require_virtualized: Some("pay".into()),
                ..Default::default()
            })
            .unwrap();
            let (output, _) =
                crate::runner::process(source, &ParseOpts::default(), &config).unwrap();
            let results = evaluate_many(&Engine::Node(node.clone()), &[source, &output]).unwrap();
            assert_eq!(results[0], results[1], "seed {seed}: {source}\n{output}");
        }
    }
}

#[test]
fn resources_close_in_reverse_order_and_cache_methods() {
    equivalent(&[
        "let log=[];function pay(){const first={[Symbol.dispose](){log.push(this.tag)},tag:'first'};{using a=first;first[Symbol.dispose]=()=>log.push('changed');using b={[Symbol.dispose](){log.push('second')}};log.push('body');}return log}globalThis.__out=JSON.stringify(pay());",
        "let log=[];function pay(){using a={[Symbol.dispose](){log.push('closed')}};return 7}let result=pay();globalThis.__out=JSON.stringify([result,log]);",
        "let log=[];function pay(){using a={[Symbol.dispose](){log.push('closed')}};using b=1;}try{pay()}catch(e){log.push(e.name)}globalThis.__out=JSON.stringify(log);",
    ]);
}

#[test]
fn resources_preserve_throw_and_suppressed_error_structure() {
    equivalent(&[
        "function pay(){using a={[Symbol.dispose](){throw 'a'}};using b={[Symbol.dispose](){throw 'b'}};throw 'body'}try{pay()}catch(e){globalThis.__out=JSON.stringify([e.name,e.error,e.suppressed.error,e.suppressed.suppressed,e instanceof SuppressedError,Object.keys(e)])}",
        "function pay(){using a={[Symbol.dispose](){throw 7}};throw undefined}try{pay()}catch(e){globalThis.__out=JSON.stringify([e.error,e.suppressed===undefined,Object.getOwnPropertyDescriptor(e,'error').enumerable])}",
    ]);
}

#[test]
fn resources_close_each_iteration_before_iterator_close() {
    equivalent(&[
        "let log=[];function pay(){const values=[1,2,3].map(n=>({n,[Symbol.dispose](){log.push('d'+n)}}));for(using item of values){log.push(item.n);if(item.n===1)continue;break}return log}globalThis.__out=JSON.stringify(pay());",
        "let log=[];function pay(){const source={[Symbol.iterator](){return this},next(){return {done:false,value:{[Symbol.dispose](){log.push('dispose')}}}},return(){log.push('iterator');return {done:true}}};for(using item of source){log.push('body');break}}pay();globalThis.__out=JSON.stringify(log);",
    ]);
}

#[test]
fn resource_protocol_names_are_hidden_from_with_objects() {
    equivalent(&[
        "function pay(){let log=[];let scope=new Proxy({},{has(t,k){log.push(String(k));return true},get(){return 99}});with(scope){using resource=null;}return log}globalThis.__out=JSON.stringify(pay());",
        "function pay(){let log=[];let value={[Symbol.dispose](){log.push('disposed')}};let scope=new Proxy({},{has(t,k){log.push(String(k));return k!=='value'},get(){return 99}});with(scope){using resource=value;}return log}globalThis.__out=JSON.stringify(pay());",
    ]);
}

#[test]
fn resources_preserve_case_blocks_and_function_bindings() {
    equivalent(&[
        "let log=[];function pay(){switch(0){case 0:{using a={[Symbol.dispose](){log.push('a')}};log.push('zero');}case 1:{using b={[Symbol.dispose](){log.push('b')}};log.push('one')}}}pay();globalThis.__out=JSON.stringify(log);",
        "function pay(){'use strict';const before=f();using a={value:7,[Symbol.dispose](){}};function f(){return this===undefined}return [before,f(),(()=>a.value)()]}globalThis.__out=JSON.stringify(pay());",
        "let log=[];function pay(){using resource=class{static{log.push(this.name);try{resource}catch(e){log.push(e.name)}Object.defineProperty(this,'name',{value:'changed'});}static [Symbol.dispose](){log.push(this.name)}};}pay();globalThis.__out=JSON.stringify(log);",
        "function pay(undefined){using a={[Symbol.dispose](){}};return undefined}globalThis.__out=pay(7);",
        "function pay(f=()=>7){using a={[Symbol.dispose](){}};return f()}globalThis.__out=JSON.stringify(pay());",
        "let log=[];Function.prototype[Symbol.dispose]=function(){log.push(this.name)};function pay(){using resource=function(){};}pay();delete Function.prototype[Symbol.dispose];globalThis.__out=JSON.stringify(log);",
    ]);
}

#[test]
fn async_resources_preserve_await_order_and_sync_fallback() {
    equivalent(&[
        "let log=[];async function pay(){await using a={[Symbol.asyncDispose](){log.push('dispose');return Promise.resolve().then(()=>log.push('resolved'))}};log.push('body');return 7}pay().then(x=>log.push(x));Promise.resolve().then(()=>log.push('tick1')).then(()=>log.push('tick2')).then(()=>log.push('tick3'));globalThis.__trace=log;globalThis.__out='trace';",
        "let log=[];async function pay(){await using a=null;await using b={[Symbol.asyncDispose](){return Promise.reject(7)}};}pay().catch(x=>log.push(x));Promise.resolve().then(()=>log.push('tick1')).then(()=>log.push('tick2')).then(()=>log.push('tick3'));globalThis.__trace=log;globalThis.__out='trace';",
        "let log=[];async function pay(){using a={[Symbol.dispose](){log.push('sync')}};await using b=null;log.push('body')}pay().then(()=>log.push('done'));Promise.resolve().then(()=>log.push('tick'));globalThis.__trace=log;globalThis.__out='trace';",
        "let log=[];async function pay(){await using a={[Symbol.asyncDispose]:null,[Symbol.dispose](){log.push('dispose');return {then(){log.push('bad')}}}};log.push('body')}pay().then(()=>log.push('done'));globalThis.__trace=log;globalThis.__out='trace';",
    ]);
}

#[test]
fn resources_close_suspended_generators_and_async_iterations() {
    equivalent(&[
        "let log=[];function* pay(){using a={[Symbol.dispose](){log.push('dispose')}};yield 7;log.push('unreachable')}let it=pay();log.push(it.next().value);log.push(it.return(9).value);globalThis.__out=JSON.stringify(log);",
        "let log=[];async function* pay(){await using a={[Symbol.asyncDispose](){log.push('dispose');return Promise.resolve().then(()=>log.push('closed'))}};yield 7;}const it=pay();(async()=>{log.push((await it.next()).value);log.push((await it.return(9)).value)})();globalThis.__trace=log;globalThis.__out='trace';",
        "let log=[];async function pay(){for(await using value of [1,2].map(x=>({x,[Symbol.asyncDispose](){log.push('d'+x)}}))){log.push(value.x);if(value.x===1)continue;break}return 'done'}pay().then(x=>log.push(x));globalThis.__trace=log;globalThis.__out='trace';",
    ]);
}

#[test]
fn module_resource_envelopes_preserve_lexical_exports_and_abrupt_cleanup() {
    use mangler_testkit::cross_engine::run_bounded;
    use std::{process::Command, time::Duration};
    let node = node_path().expect("Node is required for resource module tests");
    let observe = |source: &str| {
        let runner = format!(
            "try{{const namespace=await import('data:text/javascript,'+encodeURIComponent({}));if('balance' in namespace)globalThis.__out.push(namespace.balance);}}catch(e){{globalThis.__out.push(e.name)}}console.log(JSON.stringify(globalThis.__out));",
            serde_json::to_string(source).unwrap()
        );
        let result = run_bounded(
            Command::new(&node).args(["--input-type=module", "--eval", &runner]),
            Duration::from_secs(30),
        )
        .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        String::from_utf8(result.stdout).unwrap()
    };
    for source in [
        "globalThis.__out=[];using resource={[Symbol.dispose](){globalThis.__out.push('disposed')}};try{x}catch(e){globalThis.__out.push(e.name)}export const x=7;try{x=8}catch(e){globalThis.__out.push(e.name)}globalThis.__out.push(x);",
        "globalThis.__out=[];export let balance=7;using resource={[Symbol.dispose](){balance++;globalThis.__out.push('disposed')}};globalThis.__out.push(balance);",
        "globalThis.__out=[];using resource={[Symbol.dispose](){globalThis.__out.push('disposed')}};export const {x}=null;",
        "globalThis.__out=[];await using resource=await Promise.resolve({[Symbol.asyncDispose](){globalThis.__out.push('dispose');return Promise.resolve().then(()=>globalThis.__out.push('closed'))}});export const x=7;globalThis.__out.push(x);",
    ] {
        let config = mangler_config::ResolvedConfig::try_from(mangler_config::ConfigFlags {
            preset: Some(mangler_config::Intensity::Minify),
            seed: Some(42),
            virtualize_program: true,
            require_virtualized: Some("*".into()),
            ..Default::default()
        })
        .unwrap();
        let (output, _) = crate::runner::process(
            source,
            &ParseOpts {
                module: true,
                ..Default::default()
            },
            &config,
        )
        .unwrap();
        assert_eq!(observe(source), observe(&output), "{source}\n{output}");
    }
}

#[test]
fn resource_protocol_survives_obfuscation_passes() {
    let source = "let log=[];function pay(){using item={[Symbol.dispose](){log.push('closed')}};log.push('paid');return log}globalThis.__out=JSON.stringify(pay());";
    let node = node_path().expect("Node is required for resource pass integration tests");
    for preset in [
        mangler_config::Intensity::High,
        mangler_config::Intensity::Max,
    ] {
        let config = mangler_config::ResolvedConfig::try_from(mangler_config::ConfigFlags {
            preset: Some(preset),
            seed: Some(42),
            virtualize: Some("pay".into()),
            require_virtualized: Some("pay".into()),
            ..Default::default()
        })
        .unwrap();
        let (output, _) = crate::runner::process(source, &ParseOpts::default(), &config).unwrap();
        let results = evaluate_many(&Engine::Node(node.clone()), &[source, &output]).unwrap();
        assert_eq!(results[0], results[1], "{preset:?}\n{output}");
    }
}

#[test]
fn resource_loop_heads_preserve_rhs_tdz_and_iteration_bindings() {
    equivalent(&[
        "function pay(){let x={[Symbol.dispose](){}};try{for(using x of [x]){}}catch(e){return e.name}return 'missing'}globalThis.__out=pay();",
        "async function pay(){let x={[Symbol.asyncDispose](){}};try{for(await using x of [x]){}}catch(e){return e.name}return 'missing'}pay().then(v=>globalThis.__out=v);",
        "function pay(){let f;for(using x of (f=()=>x,[])){}try{f()}catch(e){return e.name}return 'missing'}globalThis.__out=pay();",
        "function pay(){let f;for(using x of (f=()=>x,[{[Symbol.dispose](){}}])){}try{f()}catch(e){return e.name}return 'missing'}globalThis.__out=pay();",
        "function pay(){let result;for(using x of [{get [Symbol.dispose](){try{x}catch(e){result=e.name}return ()=>{}}}]){}return result}globalThis.__out=pay();",
        "function pay(){let values=[];for(using x of [1,2].map(value=>({value,[Symbol.dispose](){}}))){values.push(()=>x.value)}return values.map(f=>f()).join(',')}globalThis.__out=pay();",
        "function pay(){'use strict';let x={[Symbol.dispose](){}};try{for(using x of [x]){}}catch(e){return e.name}return 'missing'}globalThis.__out=pay();",
        "async function pay(){'use strict';let x={[Symbol.asyncDispose](){}};try{for(await using x of [x]){}}catch(e){return e.name}return 'missing'}pay().then(v=>globalThis.__out=v);",
        "function pay(){'use strict';let f;for(using x of (f=()=>x,[])){}try{f()}catch(e){return e.name}return 'missing'}globalThis.__out=pay();",
        "function pay(){'use strict';let f;for(using x of (f=()=>x,[{[Symbol.dispose](){}}])){}try{f()}catch(e){return e.name}return 'missing'}globalThis.__out=pay();",
        "function pay(){'use strict';let result;for(using x of [{get [Symbol.dispose](){try{x}catch(e){result=e.name}return ()=>{}}}]){}return result}globalThis.__out=pay();",
        "function pay(){'use strict';let values=[];for(using x of [1,2].map(value=>({value,[Symbol.dispose](){}}))){values.push(()=>x.value)}return values.map(f=>f()).join(',')}globalThis.__out=pay();",
    ]);
}
