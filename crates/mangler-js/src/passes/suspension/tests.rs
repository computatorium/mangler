use super::*;
use mangler_core::Language;
use mangler_jsast::{Js, ParseOpts};
fn lowered(source: &str) -> String {
    let mut ast = Js.parse(source, &ParseOpts::default()).unwrap();
    let selected = suspension_candidates(ast.program()).into_keys().collect();
    let lowered = lower_with_lexicals(ast.program_mut(), &selected);
    mangler_jsast::directives::insert_program_statements(ast.program_mut(), lowered.helpers);
    struct SourceShapes<'a>(&'a HashSet<u32>);
    impl Visit for SourceShapes<'_> {
        fn visit_function(&mut self, function: &Function) {
            if self.0.contains(&function.span.lo.0) {
                assert!(!function.is_async && !function.is_generator);
            }
            function.visit_children_with(self);
        }
        fn visit_arrow_expr(&mut self, arrow: &ArrowExpr) {
            if self.0.contains(&arrow.span.lo.0) {
                assert!(!arrow.is_async && !arrow.is_generator);
            }
            arrow.visit_children_with(self);
        }
    }
    ast.program().visit_with(&mut SourceShapes(&selected));
    Js.print(&ast)
}
fn equivalent(source: &str) {
    let output = lowered(source);
    // Native helpers own the protocol; source callables have become explicit
    // state transitions, also checked through required bytecode below.
    mangler_testkit::eval::assert_behaviorally_equal_with(
        source,
        &output,
        &mangler_testkit::CaptureMode::Sink("JSON.stringify(globalThis.__out)".into()),
    );
}
#[test]
fn generator_call_assignment_iterator_value_is_read_before_target() {
    use mangler_testkit::cross_engine::{Engine, evaluate_many, node_path};
    let source = "var events=[];function target(){events.push('call')}var iterable={[Symbol.iterator](){return{next(){events.push('next');return{done:false,get value(){events.push('value');return 3}}},return(){events.push('close');return{}}}}};function* pay(){try{for(target() of iterable);}catch(e){events.push(e.name)}}pay().next();globalThis.__out=JSON.stringify(events);";
    let output = lowered(source);
    let engine =
        Engine::Node(node_path().expect("Node required for Annex B runtime target semantics"));
    let values = evaluate_many(&engine, &[source, &output]).unwrap();
    if values[0] != values[1] {
        std::fs::write("/tmp/mangler-lowered-call-target.js", &output).unwrap();
    }
    assert_eq!(values[0], values[1]);
}
#[test]
fn generator_resume_throw_return_and_finally() {
    equivalent(
        r#"
    function* g() { try { let x = yield 1; yield x + 2; } catch(e) { yield e.message; } finally { yield 9; } return 8; }
    let a = g(), b = g();
    globalThis.__out = [a.next(), a.next(4), a.return(7), a.next(), b.next(), b.throw(new Error('x')), b.next(), b.next()];
    "#,
    );
}

#[test]
fn generator_lexical_tdz_and_const_survive_suspension() {
    equivalent(
        r#"
    function* g(){try{yield typeof x}catch(e){yield e.name}let x=1;yield x;const y=2;yield y;try{y=3}catch(e){yield e.name}}
    globalThis.__out=Array.from(g());
    "#,
    );
}
#[test]
fn delegation_loops_and_lexical_capture() {
    equivalent(
        r#"
    function* g() { let fs = []; for (let [x,y] of [[1,2],[3,4]]) { fs.push(() => x+y); yield x+y; } yield* fs.map(f => f()); return 9; }
    globalThis.__out = Array.from(g());
    "#,
    );
}
#[test]
fn async_await_rejection_and_finally() {
    equivalent(
        r#"
    let log = []; async function g(x) { try { log.push('start'); let y = await x; log.push(y); throw new Error('x'); } catch(e) { log.push(e.message); return 5; } finally { log.push('finally'); } }
    g(Promise.resolve(3)).then(x => log.push(x)); log.push('sync'); globalThis.__out = log;
    "#,
    );
}

#[test]
fn async_arrow_retains_constructor_new_target() {
    equivalent(
        r#"
    function Pay(){let f=async()=>new.target===Pay;return f()}
    new Pay().then(value=>globalThis.__out=value);
    "#,
    );
}
#[test]
fn async_generator_delegation_and_for_await_close() {
    let source = r#"
    let log = []; async function* g() { try { yield await Promise.resolve(1); yield* [2,3]; } finally { log.push('closed'); } }
    async function main() { for await (let x of g()) { log.push(x); if(x===2) break; } log.push('done'); }
    main(); globalThis.__out = log;
    "#;
    let output = lowered(source);
    // QuickJS currently resumes this native for-await break before the
    // delegate's finally completes. ECMAScript and V8 await IteratorClose.
    mangler_testkit::eval::assert_behaviorally_equal_with(
        "globalThis.__out=[1,2,'closed','done'];",
        &output,
        &mangler_testkit::CaptureMode::Sink("JSON.stringify(globalThis.__out)".into()),
    );
    use mangler_testkit::cross_engine::{Engine, evaluate_many, node_path};
    if let Some(node) = node_path() {
        let original = format!("{source};globalThis.__trace=log;globalThis.__out=true;");
        let transformed = format!("{output};globalThis.__trace=log;globalThis.__out=true;");
        let results = evaluate_many(&Engine::Node(node), &[&original, &transformed]).unwrap();
        assert_eq!(results[0], results[1]);
    }
}
#[test]
fn helpers_do_not_capture_user_names() {
    equivalent(
        r#"
    let _ts_generator = 17, _async_to_generator = 18, _state = 19;
    async function g() { return await _ts_generator + _async_to_generator + _state; }
    g().then(x => globalThis.__out = x);
    "#,
    );
}

#[test]
fn suspension_inside_modern_expressions() {
    equivalent(
        r#"
    function* g() {
        let nil = null, x = 0;
        let skipped = nil?.[yield 'bad'];
        x ||= yield 3;
        let o = { a: yield 4, ...(yield 5), [yield 6]: yield 7 };
        return [skipped, x, o, `value:${yield 8}`];
    }
    let it = g(); globalThis.__out = [it.next(), it.next(30), it.next(40), it.next({b:50}), it.next('c'), it.next(70), it.next(80)];
    "#,
    );
}

#[test]
fn delegation_validates_iterator_protocol() {
    equivalent(
        r#"
    let log = [];
    function* g(it) { try { return yield* it; } catch(e) { return e.name; } }
    let it = {[Symbol.iterator](){return this}, next(){return {value:1,done:false}}, return(){log.push('close');return {done:true}}};
    let a=g(it); log.push(a.next(),a.throw(3));
    let bad = {[Symbol.iterator](){return this},next(){return 7}};
    log.push(g(bad).next());
    let reads=0, n=0, cached = {[Symbol.iterator](){return this}, get next(){reads++;return function(){return {done:n++>0,value:2}}}};
    let b=g(cached);log.push(b.next(),b.next(),reads);
    globalThis.__out=log;
    "#,
    );
}

#[test]
fn delegation_rejects_noniterable_arraylikes() {
    equivalent(
        r#"
    function* g(){try{yield* {0:1,length:1};return 'bad'}catch(e){return e.name}}
    globalThis.__out=g().next();
    "#,
    );
}

#[test]
fn async_iterator_acquires_next_once_and_closes_once() {
    equivalent(
        r#"
    let log=[], n=0;
    let iterable={[Symbol.asyncIterator](){return this},get next(){log.push('next');return function(){return Promise.resolve({done:n++>1,value:n})}},get return(){log.push('return');return function(){log.push('close');return Promise.resolve({done:true})}}};
    async function g(){for await(let x of iterable){log.push(x);if(x===2)break}}
    g();globalThis.__out=log;
    "#,
    );
}

#[test]
fn for_await_rejects_nonobject_results() {
    equivalent(
        r#"
    let log=[];async function g(){try{for await(let x of {[Symbol.asyncIterator](){return this},next(){return Promise.resolve(1)}}){log.push('bad');break}}catch(e){log.push(e.name)}}g();globalThis.__out=log;
    "#,
    );
}

#[test]
fn async_generator_throw_return_and_queue() {
    equivalent(
        r#"
    let log=[];
    async function* g(){try{yield 1;yield 2}catch(e){yield e}finally{yield 9}return 8}
    async function main(){let i=g();let a=i.next(),b=i.throw(3),c=i.return(7),d=i.next();log.push(await a,await b,await c,await d)}
    main();globalThis.__out=log;
    "#,
    );
}

#[test]
fn async_generator_return_awaits_finally_without_replacing_completion() {
    equivalent(
        r#"
    let log=[];async function* g(){try{yield 1}finally{log.push('finally');await Promise.resolve(2);log.push('after-await')}}
    async function main(){let i=g();log.push(await i.next(),await i.return(7));}main();globalThis.__out=log;
    "#,
    );
}

#[test]
fn async_generator_return_awaits_value_before_resuming_body() {
    equivalent(
        r#"
    let log=[];async function* g(){try{yield 1}catch(e){log.push('caught:'+e);yield 3}finally{log.push('finally')}}
    async function main(){for(let fail of [false,true]){let i=g();await i.next();let value={then(ok,no){log.push('then');fail?no('bad'):ok(7)}};log.push(await i.return(value),await i.next())}}main();globalThis.__out=log;
    "#,
    );
}

#[test]
fn async_delegation_defers_method_access_and_rejects_bad_results() {
    equivalent(
        r#"
    let log=[],n=0;
    let source={[Symbol.asyncIterator](){return this},next(){return Promise.resolve({value:++n,done:false})},get throw(){log.push('throw-get');return undefined},get return(){log.push('return-get');return function(){log.push('close');return Promise.resolve({done:true})}}};
    async function* g(value){try{return yield* value}catch(e){return e.name}}
    async function main(){let i=g(source);log.push(await i.next());log.push('resumed');log.push(await i.throw(3));let bad={[Symbol.asyncIterator](){return this},next(){return Promise.resolve(1)}};log.push(await g(bad).next())}
    main();globalThis.__out=log;
    "#,
    );
}

#[test]
fn async_generator_promise_turn_order() {
    let source = r#"
    let log=[];
    async function* pay(){log.push('body');yield 1;log.push('after');return 2}
    let i=pay();log.push('call');i.next().then(r=>log.push('first:'+r.value));
    Promise.resolve().then(()=>log.push('tick1')).then(()=>log.push('tick2')).then(()=>log.push('tick3'));
    i.next().then(r=>log.push('second:'+r.value));globalThis.__out=log;
    "#;
    equivalent(source);
    let config = mangler_config::ResolvedConfig::try_from(mangler_config::ConfigFlags {
        preset: Some(mangler_config::Intensity::Minify),
        seed: Some(23),
        virtualize: Some("pay".into()),
        require_virtualized: Some("pay".into()),
        ..Default::default()
    })
    .unwrap();
    let nested = source.replace(
        "async function* pay(){log.push('body');yield 1;log.push('after');return 2}",
        "function pay(){return async function*(){log.push('body');yield 1;log.push('after');return 2}}",
    ).replace("let i=pay();", "let i=pay()();");
    for source in [source, nested.as_str()] {
        let (output, _) = crate::runner::process(source, &ParseOpts::default(), &config).unwrap();
        mangler_testkit::eval::assert_behaviorally_equal_with(
            source,
            &output,
            &mangler_testkit::CaptureMode::Sink("JSON.stringify(globalThis.__out)".into()),
        );
    }
}

#[test]
fn tagged_template_site_survives_multiple_generator_instances() {
    equivalent(
        r#"
    let first;
    function tag(strings,value){let same=first===undefined||first===strings;first=strings;return [same,strings.raw[0],value]}
    function* g(){return tag`before\n${yield 1}after`}
    let a=g(),b=g();a.next();b.next();globalThis.__out=[a.next(3),b.next(4)];
    "#,
    );
}

#[test]
fn unselected_suspension_functions_keep_native_shape() {
    let source = r#"
    async function selected(){return await 3}
    async function untouched(){return await 4}
    function* untouchedGenerator(){yield 5}
    globalThis.__out=[Object.getPrototypeOf(untouched)===Object.getPrototypeOf(async function(){}),Object.getPrototypeOf(untouchedGenerator)===Object.getPrototypeOf(function*(){}),untouchedGenerator().next().value];
    "#;
    let mut ast = Js.parse(source, &ParseOpts::default()).unwrap();
    let span = match ast.program() {
        Program::Script(script) => match &script.body[0] {
            Stmt::Decl(Decl::Fn(f)) => f.function.span.lo.0,
            _ => unreachable!(),
        },
        _ => unreachable!(),
    };
    let lowered = lower_with_lexicals(ast.program_mut(), &HashSet::from([span]));
    assert_eq!(lowered.suspensions.len(), 1);
    mangler_jsast::directives::insert_program_statements(ast.program_mut(), lowered.helpers);
    mangler_testkit::eval::assert_behaviorally_equal_with(
        source,
        &Js.print(&ast),
        &mangler_testkit::CaptureMode::Sink("JSON.stringify(globalThis.__out)".into()),
    );
}

#[test]
fn required_nested_suspension_preserves_callable_reflection() {
    let source = r#"
    let count=0;
    function pay(){
        function* g(a=++count){yield a}
        async function a(x){return await x}
        async function* ag(a=++count){yield a}
        return [g,a,ag];
    }
    let f=pay(), out=[];
    let prototypes=[Object.getPrototypeOf(function*(){}),Object.getPrototypeOf(async function(){}),Object.getPrototypeOf(async function*(){})];
    for(let i=0;i<f.length;i++){
        let fn=f[i], constructible;
        try{Reflect.construct(Object,[],fn);constructible=true}catch(e){constructible=false}
        out.push(Object.getPrototypeOf(fn)===prototypes[i],Object.prototype.toString.call(fn),Object.hasOwn(fn,'prototype'),constructible,fn.length);
    }
    let iterator=f[0]();out.push(count,Object.getPrototypeOf(iterator)===f[0].prototype,Object.prototype.toString.call(iterator),iterator.next().value);
    let ai=f[2]();out.push(count,Object.getPrototypeOf(ai)===f[2].prototype,Object.prototype.toString.call(ai));
    ai.next().then(result=>{out.push(result.value);globalThis.__out=JSON.stringify(out)});
    "#;
    let config = mangler_config::ResolvedConfig::try_from(mangler_config::ConfigFlags {
        preset: Some(mangler_config::Intensity::Minify),
        seed: Some(23),
        virtualize: Some("pay".into()),
        require_virtualized: Some("pay".into()),
        ..Default::default()
    })
    .unwrap();
    let (output, _) = crate::runner::process(source, &ParseOpts::default(), &config).unwrap();
    mangler_testkit::assert_behaviorally_equal(source, &output);
}

#[test]
fn generator_arguments_remain_mapped_across_resumes() {
    let source = r#"
    function pay(){function* g(a){yield a;arguments[0]=7;yield a;a=9;return arguments[0]}let i=g(2);return [i.next(),i.next(),i.next()]}
    globalThis.__out=JSON.stringify(pay());
    "#;
    let config = mangler_config::ResolvedConfig::try_from(mangler_config::ConfigFlags {
        preset: Some(mangler_config::Intensity::Minify),
        seed: Some(23),
        virtualize: Some("pay".into()),
        require_virtualized: Some("pay".into()),
        ..Default::default()
    })
    .unwrap();
    let (output, _) = crate::runner::process(source, &ParseOpts::default(), &config).unwrap();
    mangler_testkit::assert_behaviorally_equal(source, &output);
}

#[test]
fn required_root_suspension_preserves_callable_reflection_and_hoisting() {
    for definition in [
        "async function pay(a){return await a}",
        "function* pay(a){yield a}",
        "async function* pay(a){yield a}",
    ] {
        let prototype = if definition.starts_with("async function*") {
            "async function*(){}"
        } else if definition.starts_with("async") {
            "async function(){}"
        } else {
            "function*(){}"
        };
        let source = format!(
            "let before=pay;{definition};let constructible;try{{Reflect.construct(Object,[],pay);constructible=true}}catch(e){{constructible=false}}globalThis.__out=JSON.stringify([before===pay,Object.getPrototypeOf(pay)===Object.getPrototypeOf({prototype}),Object.prototype.toString.call(pay),Object.hasOwn(pay,'prototype'),constructible,pay.length]);"
        );
        let config = mangler_config::ResolvedConfig::try_from(mangler_config::ConfigFlags {
            preset: Some(mangler_config::Intensity::Minify),
            seed: Some(23),
            virtualize: Some("pay".into()),
            require_virtualized: Some("pay".into()),
            ..Default::default()
        })
        .unwrap();
        let (output, _) = crate::runner::process(&source, &ParseOpts::default(), &config).unwrap();
        mangler_testkit::assert_behaviorally_equal(&source, &output);
    }
}

#[test]
fn suspension_preserves_super_assignment_reference() {
    for (source, expected) in [
        (
            "async function pay(){let log=[];let k={[Symbol.toPrimitive](){log.push('key');return 'x'}};let o={__proto__:{x:4},async m(){return super[k]+=(log.push('rhs'),await Promise.resolve(3))}};return [await o.m(),o.x,log]}pay().then(x=>globalThis.__out=JSON.stringify(x));",
            "globalThis.__out=JSON.stringify([7,7,['key','rhs','key']]);",
        ),
        (
            "function pay(){let log=[];let k={[Symbol.toPrimitive](){log.push('key');return 'x'}};let o={__proto__:{x:4},*m(){return super[k]+=yield 3}};let i=o.m();return [i.next(),i.next(7),o.x,log]}globalThis.__out=JSON.stringify(pay());",
            "globalThis.__out=JSON.stringify([{value:3,done:false},{value:11,done:true},11,['key','key']]);",
        ),
    ] {
        let config = mangler_config::ResolvedConfig::try_from(mangler_config::ConfigFlags {
            preset: Some(mangler_config::Intensity::Minify),
            seed: Some(23),
            virtualize: Some("pay".into()),
            require_virtualized: Some("pay".into()),
            ..Default::default()
        })
        .unwrap();
        let (output, _) = crate::runner::process(source, &ParseOpts::default(), &config).unwrap();
        // QuickJS and V8 differ on super compound-assignment key coercion.
        // Check V8's actual behavior and retain its expected trace locally.
        mangler_testkit::assert_behaviorally_equal(expected, &output);
        use mangler_testkit::cross_engine::{Engine, evaluate_many, node_path};
        if let Some(node) = node_path() {
            let results = evaluate_many(&Engine::Node(node), &[source, &output]).unwrap();
            assert_eq!(results[0], results[1]);
        }
    }
}

#[test]
fn state_machine_bodies_are_required_bytecode() {
    use mangler_config::{ConfigFlags, Intensity, ResolvedConfig};
    for source in [
        "function* pay(x){try{let a=yield x;yield* [a,a+1]}finally{yield 9}return 8} let i=pay(3);globalThis.__out=[i.next(),i.next(4),i.return(7),i.next()];",
        "async function pay(x){try{return await x+2}finally{globalThis.__out.push('finally')}}globalThis.__out=[];pay(Promise.resolve(3)).then(x=>globalThis.__out.push(x));",
        "async function* pay(){try{yield await Promise.resolve(2);yield* [3,4]}finally{globalThis.__out.push('closed')}}async function collect(){for await(let x of pay()){globalThis.__out.push(x);if(x===3)break}}globalThis.__out=[];collect();",
    ] {
        let config = ResolvedConfig::try_from(ConfigFlags {
            preset: Some(Intensity::Minify),
            seed: Some(17),
            virtualize: Some("*".into()),
            require_virtualized: Some("*".into()),
            ..Default::default()
        })
        .unwrap();
        let (output, _) = crate::runner::process(source, &ParseOpts::default(), &config).unwrap();
        mangler_testkit::eval::assert_behaviorally_equal_with(
            source,
            &output,
            &mangler_testkit::CaptureMode::Sink("JSON.stringify(globalThis.__out)".into()),
        );
    }
}

#[test]
fn await_uses_intrinsic_promise_after_application_replacements() {
    equivalent(
        r#"let OriginalPromise=Promise;let log=[];Promise.resolve=()=>{throw Error('resolve override')};async function g(){return await 7}g().then(x=>log.push(x));globalThis.Promise=function(){throw Error('Promise override')};g().then(x=>log.push(x));globalThis.__out=log;"#,
    );
}
#[test]
fn source_promise_calls_remain_observable() {
    equivalent(
        r#"let log=[];Promise.resolve=()=>{throw Error('source resolve')};async function g(){try{await Promise.resolve(7)}catch(e){log.push(e.message)}}g();globalThis.__out=log;"#,
    );
}

#[test]
fn async_parameter_errors_reject_and_arrows_keep_async_constructor() {
    use mangler_config::{ConfigFlags, Intensity, ResolvedConfig};
    for source in [
        "async function pay(x=(()=>{throw Error('default')})()){return x}let log=[];try{pay().then(()=>log.push('bad'),e=>log.push(e.message));log.push('returned')}catch(e){log.push('sync')}globalThis.__out=log;",
        "function pay(){return async(x=(()=>{throw Error('default')})())=>x}let log=[];try{pay()().then(()=>log.push('bad'),e=>log.push(e.message));log.push('returned')}catch(e){log.push('sync')}globalThis.__out=log;",
        "const pay=async()=>{};let C=pay.constructor;globalThis.__out=[];new C('return await 7')().then(x=>globalThis.__out.push(C.name,Object.prototype.toString.call(pay),x));",
    ] {
        let config = ResolvedConfig::try_from(ConfigFlags {
            preset: Some(Intensity::Minify),
            seed: Some(29),
            virtualize: Some("*".into()),
            require_virtualized: Some("*".into()),
            ..Default::default()
        })
        .unwrap();
        let (output, _) = crate::runner::process(source, &ParseOpts::default(), &config).unwrap();
        mangler_testkit::eval::assert_behaviorally_equal_with(
            source,
            &output,
            &mangler_testkit::CaptureMode::Sink("JSON.stringify(globalThis.__out)".into()),
        );
    }
}

#[test]
fn await_observes_promise_constructor_once_and_ignores_species() {
    equivalent(
        r#"let log=[];let p=Promise.resolve(1);Object.defineProperty(p,'constructor',{get(){log.push('constructor');return Promise}});async function pay(){log.push(await p)}pay();globalThis.__out=log;"#,
    );
    equivalent(
        r#"Object.defineProperty(Promise,Symbol.species,{get(){throw Error('species')}});let log=[];async function pay(){log.push(await 7)}pay();globalThis.__out=log;"#,
    );
    equivalent(
        r#"Promise.prototype.then=()=>{throw Error('then override')};let log=[];async function pay(){log.push(await 7)}pay();globalThis.__out=log;"#,
    );
}

#[test]
fn async_generator_implicit_and_explicit_return_jobs() {
    equivalent(
        r#"let log=[];async function* a(){}async function* b(){return}async function* c(){return undefined}async function* d(){return void 0}Promise.resolve().then(()=>log.push('tick1')).then(()=>log.push('tick2'));a().next().then(()=>log.push('a'));b().next().then(()=>log.push('b'));c().next().then(()=>log.push('c'));d().next().then(()=>log.push('d'));globalThis.__out=log;"#,
    );
}
#[test]
fn async_delegate_reads_result_done_and_value_once() {
    equivalent(
        r#"let log=[];let n=0;let source={[Symbol.asyncIterator](){return this},next(){return {done:false,value:1}},get return(){log.push('return');return function(value){return {get done(){log.push('done');return ++n>1},get value(){log.push('value');return value}}}}};async function* g(){yield* source}let i=g();i.next().then(()=>i.return(3)).then(x=>{log.push(x);return i.return(4)}).then(x=>log.push(x));globalThis.__out=log;"#,
    );
}
#[test]
fn async_callable_arguments_callee_is_public_shell() {
    let source = "async function pay(){return arguments.callee===pay}globalThis.__out=[];pay().then(x=>globalThis.__out.push(x));";
    let config = mangler_config::ResolvedConfig::try_from(mangler_config::ConfigFlags {
        preset: Some(mangler_config::Intensity::Minify),
        seed: Some(23),
        virtualize: Some("*".into()),
        require_virtualized: Some("*".into()),
        ..Default::default()
    })
    .unwrap();
    let (output, _) = crate::runner::process(source, &ParseOpts::default(), &config).unwrap();
    mangler_testkit::eval::assert_behaviorally_equal_with(
        source,
        &output,
        &mangler_testkit::CaptureMode::Sink("JSON.stringify(globalThis.__out)".into()),
    );
}

#[test]
fn async_delegate_missing_return_awaits_value_without_result_job() {
    equivalent(
        r#"let log=[];let source={[Symbol.asyncIterator](){return this},next(){return {done:false}},get return(){log.push('return')}};async function* g(){log.push('start');yield* source;log.push('unreachable')}Promise.resolve().then(()=>log.push('tick1')).then(()=>log.push('tick2')).then(()=>log.push('tick3'));let i=g();i.next();i.return({get then(){log.push('then')}});globalThis.__out=log;"#,
    );
}

#[test]
fn async_delegate_native_promise_result_has_no_adoption_delay() {
    equivalent(
        r#"let log=[];let n=0;let source={[Symbol.asyncIterator](){return this},next(){return Promise.resolve({value:++n,done:n>1})}};async function* g(){yield* source}let i=g();i.next().then(x=>log.push(x));i.next().then(x=>log.push(x));Promise.resolve().then(()=>log.push('tick1')).then(()=>log.push('tick2')).then(()=>log.push('tick3')).then(()=>log.push('tick4'));globalThis.__out=log;"#,
    );
}

#[test]
fn iterator_calls_ignore_overridden_call_and_apply_properties() {
    equivalent(
        r#"let log=[];let n=0;let next=function(){return {value:++n,done:n>1}};next.call=next.apply=()=>{throw Error('intercept')};let method=function(){return {next}};method.call=()=>{throw Error('method intercept')};function* g(){yield* {[Symbol.iterator]:method}}globalThis.__out=Array.from(g());"#,
    );
    equivalent(
        r#"let n=0;let next=function(){return {value:++n,done:n>1}};next.call=next.apply=()=>{throw Error('intercept')};let method=function(){return {next}};method.call=()=>{throw Error('method intercept')};let log=[];async function g(){for await(let value of {[Symbol.asyncIterator]:method})log.push(value)}g();globalThis.__out=log;"#,
    );
}

#[test]
fn suspended_calls_ignore_callable_own_apply_property() {
    equivalent(
        r#"function* g(){function f(x){return x+1}f.apply=()=>99;return f(yield 3)}let i=g();globalThis.__out=[i.next(),i.next(7)];"#,
    );
    equivalent(
        r#"function* g(){function f(x){return x+1}f.apply=function(receiver,args){return args[0]+10};return f.apply(null,[yield 3])}let i=g();globalThis.__out=[i.next(),i.next(7)];"#,
    );
}

#[test]
fn suspended_eval_keeps_reference_receiver_and_source_environment() {
    use mangler_testkit::cross_engine::{Engine, evaluate_many, node_path};
    let node = node_path().expect("Node is required for suspended eval differential tests");
    for source in [
        "function* pay(){let x=7;return eval(yield 'code')}let i=pay();globalThis.__out=JSON.stringify([i.next(),i.next('x')]);",
        "function* pay(){let x=7,eval=globalThis.eval;yield ()=>eval=()=>99;return eval(yield 'code')}let i=pay();let change=i.next().value;let step=i.next();change();globalThis.__out=JSON.stringify([step,i.next('x')]);",
        "function* pay(){let object={marker:4,eval:function(s){return this.marker+s}};with(object){return eval(yield 'code')}}let i=pay();globalThis.__out=JSON.stringify([i.next(),i.next(3)]);",
        "function* pay(){let x=7;return eval(...(yield 'args'))}let i=pay();globalThis.__out=JSON.stringify([i.next(),i.next(['x'])]);",
    ] {
        let config = mangler_config::ResolvedConfig::try_from(mangler_config::ConfigFlags {
            preset: Some(mangler_config::Intensity::Minify),
            seed: Some(23),
            virtualize: Some("pay".into()),
            require_virtualized: Some("pay".into()),
            ..Default::default()
        })
        .unwrap();
        let (output, _) = crate::runner::process(source, &ParseOpts::default(), &config).unwrap();
        let results = evaluate_many(&Engine::Node(node.clone()), &[source, &output]).unwrap();
        assert_eq!(results[0], results[1], "{source}");
    }
}

#[test]
fn high_preset_promise_discovery_does_not_leave_orphan_rejection() {
    use mangler_testkit::cross_engine::{Engine, evaluate_many, node_path};
    let node = node_path().expect("Node is required for Promise initialization ordering");
    let source = "async function pay(){return 7}pay().then(value=>globalThis.__out=value);";
    let config = mangler_config::ResolvedConfig::try_from(mangler_config::ConfigFlags {
        preset: Some(mangler_config::Intensity::High),
        seed: Some(42),
        virtualize: Some("pay".into()),
        require_virtualized: Some("pay".into()),
        ..Default::default()
    })
    .unwrap();
    let (output, _) = crate::runner::process(source, &ParseOpts::default(), &config).unwrap();
    let results = evaluate_many(&Engine::Node(node), &[source, &output]).unwrap();
    assert_eq!(results[0], results[1]);
}

#[test]
fn native_async_generator_kernel_preserves_brands_and_pending_completions() {
    use mangler_testkit::cross_engine::{Engine, evaluate_many, node_path};
    let node = node_path().expect("Node is required for native async-generator protocol tests");
    for source in [
        "async function* pay(){try{yield 1}finally{try{yield await 2}finally{yield 3}}}let i=pay(),log=[];i.next().then(x=>log.push(x));i.return(7).then(x=>log.push(x));i.return(8).then(x=>log.push(x));i.next().then(x=>log.push(x));Object.defineProperty(globalThis,'__out',{get:()=>JSON.stringify(log)});",
        "async function* pay(){try{yield 1}finally{let n=0;while(n++<2){try{yield 2}finally{continue}}}}let i=pay(),log=[];i.next().then(x=>log.push(x));i.return(7).then(x=>log.push(x));i.return(8).then(x=>log.push(x));i.next().then(x=>log.push(x));Object.defineProperty(globalThis,'__out',{get:()=>JSON.stringify(log)});",
        "async function* pay(){let fs=[];try{yield 1}finally{for(let i=0;i<2;i++){fs.push(()=>i);try{yield 2}finally{continue}}}}let i=pay(),log=[];i.next().then(x=>log.push(x));i.return(7).then(x=>log.push(x));i.return(8).then(x=>log.push(x));i.next().then(x=>log.push(x));Object.defineProperty(globalThis,'__out',{get:()=>JSON.stringify(log)});",
        "async function* pay(){yield 1}let i=pay(),proto=Object.getPrototypeOf(Object.getPrototypeOf((async function*(){})())),log=[Reflect.ownKeys(i).map(String),Object.prototype.toString.call(i),Object.getPrototypeOf(i)===pay.prototype];proto.next.call(i).then(x=>log.push(x));proto.throw.call(i,'external').catch(e=>log.push(e));proto.return.call(i,7).then(x=>log.push(x));Object.defineProperty(globalThis,'__out',{get:()=>JSON.stringify(log)});",
    ] {
        let config = mangler_config::ResolvedConfig::try_from(mangler_config::ConfigFlags {
            preset: Some(mangler_config::Intensity::Minify),
            seed: Some(23),
            virtualize: Some("pay".into()),
            require_virtualized: Some("pay".into()),
            ..Default::default()
        })
        .unwrap();
        let (output, _) = crate::runner::process(source, &ParseOpts::default(), &config).unwrap();
        let results = evaluate_many(&Engine::Node(node.clone()), &[source, &output]).unwrap();
        assert_eq!(results[0], results[1], "{source}");
    }
}

#[test]
fn delegated_iterator_accessors_keep_their_call_time_receiver() {
    use mangler_testkit::cross_engine::{Engine, chrome_path, evaluate_many, node_path};
    let mut programs = Vec::new();
    for strict in ["", "'use strict';"] {
        for symbol in ["iterator", "asyncIterator"] {
            let source = format!(
                "{strict}let seen=[];const object={{get [Symbol.{symbol}](){{seen.push(this===object);return function(){{seen.push(this===object);return{{next(){{return{{done:true,value:1}}}}}}}}}}}};async function* pay(){{yield* object}}pay().next().then(()=>globalThis.__out=JSON.stringify(seen));"
            );
            let config = mangler_config::ResolvedConfig::try_from(mangler_config::ConfigFlags {
                preset: Some(mangler_config::Intensity::Minify),
                seed: Some(42),
                virtualize_program: true,
                require_virtualized: Some("*".into()),
                ..Default::default()
            })
            .unwrap();
            let (output, _) =
                crate::runner::process(&source, &ParseOpts::default(), &config).unwrap();
            programs.extend([source, output]);
        }
    }
    let mut engines = vec![Engine::Node(
        node_path().expect("Node is required for iterator receiver tests"),
    )];
    engines.extend(chrome_path().map(Engine::Chrome));
    let programs: Vec<_> = programs.iter().map(String::as_str).collect();
    for engine in engines {
        let results = evaluate_many(&engine, &programs).unwrap();
        for (index, pair) in results.chunks_exact(2).enumerate() {
            assert_eq!(
                pair[0],
                pair[1],
                "{}: {}",
                engine.name(),
                programs[index * 2]
            );
        }
    }
}

#[test]
fn runtime_generator_templates_share_one_site_per_compilation() {
    use mangler_testkit::cross_engine::{Engine, chrome_path, evaluate_many, node_path};
    let mut engines = vec![Engine::Node(
        node_path().expect("Node template-site evaluator"),
    )];
    if let Some(path) = chrome_path() {
        engines.push(Engine::Chrome(path));
    }
    for source in [
        r#"(function*(){for(let i=0;i<2;i++)yield tag`x`})"#,
        r#"(function*(){for(let i=0;i<2;i++)yield tag`before\n${yield i}after`})"#,
        r#"(function*(){for(let i=0;i<2;i++)yield obj.tag`x${yield (events.push('sub'),0)}y`})"#,
        r#"(function*(){yield tag`x`;yield tag`x`})"#,
        r#"(function*(){for(let i=0;i<2;i++)yield tag`bad\unicode`})"#,
        r#"(function*(){for(let i=0;i<2;i++)yield tag`\uD800`})"#,
        r#"(async function*(){for(let i=0;i<2;i++)yield tag`x${await i}y`})"#,
    ] {
        let prepared = crate::runtime_frontend::prepare_function(source).unwrap();
        let mut transformed = format!(
            "const intrinsics=({})();function make(){{const support=({})(intrinsics);",
            crate::runtime_frontend::intrinsic_snapshot_factory(),
            prepared.support.factory
        );
        for name in prepared.support.names {
            transformed.push_str(&format!(
                "const {name}=support[{}];",
                serde_json::to_string(&name).unwrap()
            ));
        }
        transformed.push_str(&format!(
            "return {};}}",
            swc_core::ecma::codegen::to_code(&prepared.initializer)
        ));
        let native = format!(
            "function make(){{return eval({});}}",
            serde_json::to_string(source).unwrap()
        );
        let driver = r#"
            const events=[],tag=s=>s,obj={get tag(){events.push('get');return function(s){events.push(this===obj?'this':'wrong');return s}}};
            async function collect(it){let values=[];for(;;){let step=await it.next(3);if(step.done)return values;if(Array.isArray(step.value))values.push(step.value)}}
            (async()=>{const a=make(),b=make(),x=await collect(a()),y=await collect(a()),z=await collect(b());return [x[0]===x[1],x[0]===y[0],x[0]!==z[0],Object.isFrozen(x[0]),Object.isFrozen(x[0].raw),x[0],x[0].raw,events]})().then(value=>globalThis.__out=JSON.stringify(value));
        "#;
        let native = native + driver;
        let transformed = transformed + driver;
        for engine in &engines {
            let values = evaluate_many(engine, &[&native, &transformed]).unwrap();
            assert_eq!(values[0], values[1], "{engine:?}: {source}");
        }
    }
}

#[test]
fn switch_suspension_declarations_keep_the_caseblock_environment() {
    use mangler_config::{ConfigFlags, Intensity, ResolvedConfig};
    use mangler_testkit::cross_engine::{Engine, evaluate_many, node_path};
    let engine = Engine::Node(node_path().expect("Node required for switch declaration scope"));
    let config = ResolvedConfig::try_from(ConfigFlags {
        preset: Some(Intensity::Minify),
        seed: Some(42),
        virtualize: Some("*".into()),
        require_virtualized: Some("*".into()),
        ..Default::default()
    })
    .unwrap();
    for source in [
        "var g=3;function f(){let x;switch(eval(\"g\")){case 3:x=g().next().value;break;default:function* g(){yield 9}}return[x,typeof g]}globalThis.__out=f()",
        "function f(){let x;switch(9){case g().next().value:x=g.name;break;default:function* g(){yield 9}}return[x,typeof g]}globalThis.__out=f()",
        "function f(){var g=3;let read;switch((read=()=>g,1)){default:function* g(){};return[read(),g.name]}}globalThis.__out=f()",
        "function f(){let old;switch(1){case 1:let x=4;old=g;break;default:function* g(){yield x}}return old().next().value}globalThis.__out=f()",
        "function f(){let events=[];switch(1){case g():break;default:let x=4;function g(){try{return x}catch(e){events.push(e.name);return 3}}}return events}globalThis.__out=f()",
        "function f(){switch(1){case 1:let x=4;return g().next().value;default:function* g(){yield h()}function h(){return x}}}globalThis.__out=f()",
        "var events=[];function f(){switch((events.push(\"discriminant\"),1)){case(events.push(\"a\"),3):break;default:function* g(){yield 1};events.push(g().next().value);case(events.push(\"b\"),2):events.push(\"tail\")}}f();globalThis.__out=events",
        "function f(){switch(1){default:function* g(){};var x=4}return[x,typeof g]}globalThis.__out=f()",
        "function f(){switch(1){default:function* g(){yield 4};let x=5;return eval(\"[g().next().value,x]\")}}globalThis.__out=f()",
        "function f(){switch(1){default:function* g(){yield 4};function h(g1){return[g1,g().next().value]};return[h(9),g.name]}}globalThis.__out=f()",
        "function f(){switch(1){default:function* g(){yield 4};let x=5;return()=>[g().next().value,x]}}globalThis.__out=f()()",
        "function f(){let a=[];outer:for(let i=0;i<3;i++){switch(i){case 1:continue outer;default:function* g(){yield i};a.push(g().next().value)}}return a}globalThis.__out=f()",
    ] {
        let source = format!("{source};globalThis.__out=JSON.stringify(globalThis.__out)");
        let lowered = lowered(&source);
        let (protected, _) =
            crate::runner::process(&source, &ParseOpts::default(), &config).unwrap();
        let values = evaluate_many(&engine, &[&source, &lowered, &protected]).unwrap();
        assert_eq!(values[0], values[1], "lowered: {source}");
        assert_eq!(values[0], values[2], "protected: {source}");
    }
}
