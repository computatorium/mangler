//! Native generator delegation preserves result identity and completion state.
use super::*;

fn output(source: &str, whole: bool) -> String {
    let cfg = FileConfig::new(
        ResolvedConfig::try_from(ConfigFlags {
            preset: Some(Intensity::Minify),
            seed: Some(917),
            virtualize: (!whole).then(|| "pay".into()),
            require_virtualized: Some(if whole { "*" } else { "pay" }.into()),
            virtualize_program: whole,
            ..Default::default()
        })
        .unwrap(),
        917,
        reserved_idents(source),
    );
    let mut ast = Js.parse(source, &ParseOpts::default()).unwrap();
    let mut bus = ArtifactBus::new();
    let pass = VirtualizePass;
    bus.enter_pass(pass.id(), pass.reads(), pass.writes());
    let mut rng = Rng::for_pass(cfg.seed(), pass.id());
    pass.run(
        &mut ast,
        &cfg,
        &mut rng,
        &mut bus,
        &mut mangler_core::Notes::default(),
    )
    .unwrap();
    assert!(
        bus.contains::<VmTableArtifact>(),
        "required generator emits bytecode"
    );
    Js.print(&ast)
}

#[test]
fn sync_native_kernel_preserves_delegation_and_pending_completions() {
    use mangler_testkit::cross_engine::{Engine, chrome_path, evaluate_many, node_path};
    let cases = [
        (
            "delegate_getter_order",
            r###"let log='';let count=5;const it={next(){log+='n';return{get done(){log+='d';return count--===0},get value(){log+='v';return 42}}},[Symbol.iterator](){log+='i';return this}};function* pay(){return yield* it}let g=pay();for(let i=0;i<6;i++)g.next();globalThis.__out=log;"###,
        ),
        (
            "delegate_result_identity",
            r###"const result={get done(){return false},get value(){throw 1}};const it={next(){return result},[Symbol.iterator](){return this}};function* pay(){yield* it}let g=pay();let r=g.next();globalThis.__out=[r===result,Object.getOwnPropertyDescriptor(r,'done').get===Object.getOwnPropertyDescriptor(result,'done').get];"###,
        ),
        (
            "delegate_return_identity",
            r###"let reads=[];const first={done:false,value:1};const returned={get done(){reads.push('done');return false},get value(){reads.push('value');return 2}};let it={next(){return first},return(v){reads.push(v);return returned},[Symbol.iterator](){return this}};function* pay(){yield* it}let g=pay();g.next();let r=g.return(17);globalThis.__out=[r===returned,reads];"###,
        ),
        (
            "delegate_throw_identity",
            r###"let reads=[];const first={done:false,value:1};const thrown={get done(){reads.push('done');return false},get value(){reads.push('value');return 2}};let it={next(){return first},throw(v){reads.push(v);return thrown},[Symbol.iterator](){return this}};function* pay(){yield* it}let g=pay();g.next();let r=g.throw(17);globalThis.__out=[r===thrown,reads];"###,
        ),
        (
            "delegate_return_done",
            r###"let log=[];const it={next(v){log.push(['next',arguments.length,v]);return{done:false,value:1}},return(v){log.push(['return',v]);return{get done(){log.push('done');return true},get value(){log.push('value');return v+1}}},[Symbol.iterator](){return this}};function* pay(){try{yield* it;log.push('after')}finally{log.push('finally');yield 2}}let g=pay();globalThis.__out=[g.next(),g.return(17),g.next(),log];"###,
        ),
        (
            "delegate_missing_throw",
            r###"let log=[];const it={next(){return{done:false,value:1}},get return(){log.push('getreturn');return function(){log.push([this===it,arguments.length]);return {}}},[Symbol.iterator](){return this}};function* pay(){try{yield* it}catch(e){yield e instanceof TypeError}}let g=pay();globalThis.__out=[g.next(),g.throw(4),g.next(),log];"###,
        ),
        (
            "delegate_receiver_and_next_cache",
            r###"let log=[];let i=0;const it={get next(){log.push(['getnext',this===it]);return function(v){log.push(['next',this===it,v]);return{done:i++>1,value:i}}},get [Symbol.iterator](){log.push(['getiterator',this===it]);return function(){log.push(['iterator',this===it]);return this}}};function* pay(){return yield* it}let g=pay();globalThis.__out=[g.next(9),g.next(4),g.next(5),log];"###,
        ),
        (
            "ownkeys_native_brand",
            r###"function* pay(){yield 1;return 2}let it=pay();let next=Object.getPrototypeOf(Object.getPrototypeOf((function*(){})())).next;globalThis.__out=[Reflect.ownKeys(it).length,next.call(it),next.call(it),Object.prototype.toString.call(it)];"###,
        ),
        (
            "empty_generator",
            r###"function* pay(){}globalThis.__out=[pay().next(),pay().return(3)];"###,
        ),
        (
            "finally_nested_returns",
            r###"function* pay(){try{yield 1}finally{try{yield 2}finally{yield 3}}}let g=pay();globalThis.__out=[g.next(),g.return(17),g.return(18),g.next()];"###,
        ),
        (
            "finally_return_cancellation",
            r###"function* pay(){try{return 42}finally{do try{return 43}finally{break}while(0)}}globalThis.__out=pay().next();"###,
        ),
        (
            "finally_loop_continue",
            r###"function* pay(){for(let i=0;i<2;i++){try{yield i}finally{if(i===0)continue;yield 4}}return 7}let g=pay();globalThis.__out=[g.next(),g.return(8),g.return(9),g.next()];"###,
        ),
        (
            "loop_helper_delegation",
            r###"function* pay(){let fs=[];for(let i=0;i<3;i++){fs.push(()=>i);try{yield* [i,i+1]}finally{yield 10+i}}yield fs.map(f=>f())}let g=pay();globalThis.__out=[g.next(),g.return(7),g.next(),g.next()];"###,
        ),
        (
            "pattern_helper_delegation",
            r###"function* pay(){let [x=yield* [2]]= [undefined];yield x}let g=pay();globalThis.__out=[g.next(),g.next(3),g.next()];"###,
        ),
        (
            "async_unaffected",
            r###"async function pay(){let x=await 2;return x+1}pay().then(v=>globalThis.__out=v);"###,
        ),
        (
            "async_generator_unaffected",
            r###"async function* pay(){yield await 2;return yield* [3,4]}(async()=>{let g=pay();globalThis.__out=[await g.next(),await g.next(),await g.next(),await g.next()]})();"###,
        ),
        (
            "default_calltime",
            r###"let log=[];function* pay(a=(log.push('default'),1)){log.push('body');yield a}let g=pay();let before=log.slice();globalThis.__out=[before,g.next(),log];"###,
        ),
        (
            "iterator_mutation",
            r###"function* pay(){yield* [1,2]}const prev=Array.prototype[Symbol.iterator];Array.prototype[Symbol.iterator]=function*(){yield 9};let g=pay();let a=g.next(),b=g.next();Array.prototype[Symbol.iterator]=prev;globalThis.__out=[a,b];"###,
        ),
        (
            "nested_generator",
            r###"function* pay(){function* inner(){yield* [1,2];return 3}return yield* inner()}globalThis.__out=Array.from(pay());"###,
        ),
        (
            "class_generator",
            r###"class C{*pay(){yield* [1,2]}}globalThis.__out=Array.from(new C().pay());"###,
        ),
        (
            "loop_helper_nested_returns",
            r###"function* pay(){let fs=[];for(let i=0;i<2;i++){fs.push(()=>i);try{yield i}finally{try{yield i+10}finally{yield i+20}}}}let g=pay();globalThis.__out=[g.next(),g.return(7),g.return(8),g.next()];"###,
        ),
        (
            "loop_helper_cancel_return",
            r###"function* pay(){let fs=[];for(let i=0;i<2;i++){fs.push(()=>i);try{yield i}finally{yield i+10;if(i===0)continue}}return 3}let g=pay();globalThis.__out=[g.next(),g.return(7),g.next(),g.next(),g.next()];"###,
        ),
        (
            "loop_helper_throw_during_finally",
            r###"function* pay(){let fs=[];for(let i=0;i<2;i++){fs.push(()=>i);try{yield i}finally{try{yield i+10}catch(e){yield e}}}}let g=pay();globalThis.__out=[g.next(),g.return(7),g.throw(8),g.next()];"###,
        ),
        (
            "async_loop_helper_return",
            r###"async function* pay(){let fs=[];for(let i=0;i<2;i++){fs.push(()=>i);try{yield* [i,i+1]}finally{yield 10+i}}} (async()=>{let g=pay();globalThis.__out=[await g.next(),await g.return(7),await g.next(),await g.next()]})();"###,
        ),
        (
            "generator_thenable_return",
            r###"let log=[];const value={then(){log.push('then')}};function* pay(){return value}let r=pay().next();globalThis.__out=[r.done,r.value===value,log];"###,
        ),
        (
            "generator_delegation_thenable",
            r###"let log=[];const value={then(){log.push('then')}};function* pay(){yield* [value]}let r=pay().next();globalThis.__out=[r.done,r.value===value,log];"###,
        ),
        (
            "delegate_next_getter_throw",
            r###"let log=[];let iter={get next(){throw 1},[Symbol.iterator](){return this}};function* pay(){try{yield* iter}catch(e){yield e}finally{log.push('finally')}}let g=pay();globalThis.__out=[g.next(),g.next(),log];"###,
        ),
        (
            "delegate_no_return",
            r###"function* pay(){try{yield*{next(){return{value:1,done:false}},[Symbol.iterator](){return this}}}finally{yield 9}}let g=pay();globalThis.__out=[g.next(),g.return(7),g.next()];"###,
        ),
        (
            "return_before_start",
            r###"let log=[];function* pay(){try{log.push('start');yield 1}finally{log.push('finally')}}let g=pay();globalThis.__out=[g.return(3),g.next(),log];"###,
        ),
        (
            "generator_reentrancy",
            r###"let g;function* pay(){try{g.next()}catch(e){yield e.name}}g=pay();globalThis.__out=[g.next(),g.next()];"###,
        ),
        (
            "nested_source_delegation_reads",
            r###"let log=[];let count=0;const it={next(){return{get done(){log.push('done');return count++>3},get value(){log.push('value');return 7}}},[Symbol.iterator](){log.push('iterator');return this}};function* inner(){return yield* it}function* pay(){return yield* inner()}let g=pay();let a=g.next(),b=g.next(),c=g.next();globalThis.__out=[log,a===b,b===c];"###,
        ),
        (
            "delegate_return_false_resumes_normally",
            r###"let log=[];let n=0;const it={next(v){log.push(['next',v]);return{done:n++>0,value:1}},return(v){log.push(['return',v]);return{done:false,value:2}},[Symbol.iterator](){return this}};function* pay(){try{return yield* it}finally{log.push('finally');yield 3}}let g=pay();globalThis.__out=[g.next(),g.return(4),g.next(5),g.next(6),log];"###,
        ),
        (
            "delegate_throw_done_return_override",
            r###"let log=[];const it={next(){return{done:false,value:1}},throw(v){return{done:true,value:v+1}},[Symbol.iterator](){return this}};function* pay(){try{return yield* it}finally{yield 3;return 4}}let g=pay();globalThis.__out=[g.next(),g.throw(9),g.next(),log];"###,
        ),
        (
            "delegate_return_result_getter_reentrancy",
            r###"let g,log=[];const it={next(){return{done:false,value:1}},return(){return{get done(){try{g.next()}catch(e){log.push(e.name)}return true},get value(){log.push('value');return 5}}},[Symbol.iterator](){return this}};function* pay(){try{yield* it}finally{log.push('finally')}}g=pay();globalThis.__out=[g.next(),g.return(7),log];"###,
        ),
        (
            "pattern_delegate_return_closes",
            r###"let log=[];const it={next(){log.push('next');return{done:false,value:undefined}},return(){log.push('close');return{}},[Symbol.iterator](){return this}};function* pay(){try{let [x=yield* [1,2]]=it;yield x}finally{log.push('finally');yield 9}}let g=pay();globalThis.__out=[g.next(),g.return(7),g.next(),log];"###,
        ),
        (
            "pattern_delegate_throw_closes",
            r###"let log=[];const it={next(){log.push('next');return{done:false,value:undefined}},return(){log.push('close');throw 4},[Symbol.iterator](){return this}};function* pay(){try{let [x=yield* [1,2]]=it;yield x}catch(e){yield e instanceof TypeError?'TypeError':e}finally{log.push('finally')}}let g=pay();globalThis.__out=[g.next(),g.throw(7),g.next(),log];"###,
        ),
        (
            "loop_nested_helpers_return",
            r###"function* pay(){let f=[];for(let i=0;i<2;i++){f.push(()=>i);try{for(let j=0;j<2;j++){f.push(()=>j);try{yield [i,j]}finally{yield 10+j}}}finally{yield 20+i}}return 30}let g=pay();globalThis.__out=[g.next(),g.return(7),g.next(),g.next(),g.next()];"###,
        ),
        (
            "loop_nested_helpers_replace_returns",
            r###"function* pay(){let f=[];for(let i=0;i<2;i++){f.push(()=>i);try{for(let j=0;j<2;j++){f.push(()=>j);try{yield [i,j]}finally{yield 10+j}}}finally{yield 20+i}}return 30}let g=pay();globalThis.__out=[g.next(),g.return(7),g.return(8),g.return(9),g.next()];"###,
        ),
        (
            "loop_nested_helpers_cancel_return",
            r###"function* pay(){let f=[];outer:for(let i=0;i<2;i++){f.push(()=>i);try{for(let j=0;j<2;j++){f.push(()=>j);try{yield [i,j]}finally{yield 10+j;if(i===0)continue outer}}}finally{yield 20+i}}return 30}let g=pay();globalThis.__out=[g.next(),g.return(7),g.next(),g.next(),g.next(),g.next(),g.next()];"###,
        ),
        (
            "finally_delegate_return_value",
            r###"let log=[];const it={next(){return{done:false,value:2}},return(v){log.push(v);return{done:true,value:v+1}},[Symbol.iterator](){return this}};function* pay(){try{yield 1}finally{yield* it}}let g=pay();globalThis.__out=[g.next(),g.return(7),g.return(8),log];"###,
        ),
        (
            "finally_inner_return_getter_throw",
            r###"const it={next(){return{done:false,value:2}},get return(){throw 9},[Symbol.iterator](){return this}};function* pay(){try{yield 1}finally{try{yield* it}catch(e){yield e}}}let g=pay();globalThis.__out=[g.next(),g.return(7),g.return(8),g.next()];"###,
        ),
        (
            "async_loop_nested_helpers_return",
            r###"async function* pay(){let f=[];for(let i=0;i<2;i++){f.push(()=>i);try{for(let j=0;j<2;j++){f.push(()=>j);try{yield [i,j]}finally{await 0;yield 10+j}}}finally{await 0;yield 20+i}}return 30}(async()=>{let g=pay();globalThis.__out=[await g.next(),await g.return(7),await g.next(),await g.next(),await g.next()]})();"###,
        ),
        (
            "async_queue_nested_return",
            r###"async function* pay(){let f=[];for(let i=0;i<2;i++){f.push(()=>i);try{yield i}finally{try{await 0;yield 10+i}finally{await 0;yield 20+i}}}}let g=pay();Promise.all([g.next(),g.return(7),g.throw(8).catch(e=>({error:e})),g.return(9),g.next()]).then(v=>globalThis.__out=v);"###,
        ),
        (
            "async_finally_delegate_return_value",
            r###"let log=[];const it={async next(){return{done:false,value:2}},async return(v){log.push(v);return{done:true,value:v+1}},[Symbol.asyncIterator](){return this}};async function* pay(){try{yield 1}finally{yield* it}}(async()=>{let g=pay();globalThis.__out=[await g.next(),await g.return(7),await g.return(8),log]})();"###,
        ),
        (
            "public_and_private_class_generator_protocol",
            r###"function pay(){class C{*values(){yield 3}*#values(){yield 7}private(){return this.#values}}let c=new C,f=c.private(),pub=c.values(),priv=f.call(c);let n=Object.getPrototypeOf(Object.getPrototypeOf((function*(){})())).next;return [n.call(pub),n.call(priv),Object.getPrototypeOf(pub)===C.prototype.values.prototype,Object.getPrototypeOf(priv)===f.prototype,Reflect.ownKeys(pub),Reflect.ownKeys(priv)]}globalThis.__out=pay();"###,
        ),
    ];
    let mut programs = Vec::new();
    for (_, source) in &cases {
        for source in [
            source.to_string(),
            output(source, false),
            output(source, true),
        ] {
            programs.push(format!(
                "{source};setTimeout(()=>{{globalThis.__out=JSON.stringify(globalThis.__out)}},0);"
            ));
        }
    }
    let sources: Vec<_> = programs.iter().map(String::as_str).collect();
    let mut engines = vec![Engine::Node(
        node_path().expect("Node generator protocol evaluator"),
    )];
    engines.extend(chrome_path().map(Engine::Chrome));
    for engine in engines {
        let results = evaluate_many(&engine, &sources).unwrap();
        for (index, group) in results.chunks_exact(3).enumerate() {
            assert_eq!(
                group[0],
                group[1],
                "{} named {}",
                engine.name(),
                cases[index].0
            );
            assert_eq!(
                group[0],
                group[2],
                "{} whole {}",
                engine.name(),
                cases[index].0
            );
        }
    }
}
