use mangler_core::Language;
use mangler_js::runtime_frontend::{intrinsic_snapshot_factory, prepare_eval_with_context};
use mangler_jsast::{Js, ParseOpts};
use mangler_vm::{CompileOptions, TableBuilder, VmNames};
use swc_core::ecma::ast::*;

fn artifact(source: &str) -> String {
    let ast = Js
        .parse(
            &format!("class D extends A{{constructor(){{{source}}}}}"),
            &ParseOpts::default(),
        )
        .unwrap();
    let Program::Script(script) = ast.into_program() else {
        unreachable!()
    };
    let Stmt::Decl(Decl::Class(class)) = &script.body[0] else {
        unreachable!()
    };
    let ClassMember::Constructor(constructor) = &class.class.body[0] else {
        unreachable!()
    };
    let prepared = mangler_vm::eval::prepare_eval_body(
        constructor.body.as_ref().unwrap().stmts.clone(),
        true,
        mangler_vm::eval::SourceContext::Function,
    );
    let context = mangler_vm::eval::EvalClassContext {
        capsule_binding: "__cap".into(),
        private_names: vec![],
        allow_super_property: true,
        allow_super_call: true,
        arguments_forbidden: false,
    };
    let prepared =
        prepare_eval_with_context(prepared, &Default::default(), Some(&context)).unwrap();
    let hidden = prepared.support.names.iter().cloned().collect();
    let compiled = mangler_vm::eval::compile_prepared_eval_body(
        prepared.body,
        CompileOptions {
            internal_bindings: Some(&hidden),
            eval_class_contexts: Some(&prepared.class_contexts),
            ..Default::default()
        },
    )
    .unwrap();
    let names = VmNames {
        lean_interp: "__vm".into(),
        eh_interp: "__vm_eh".into(),
        lean_interp_strict: "__vm_strict".into(),
        eh_interp_strict: "__vm_strict_eh".into(),
        table: "__table".into(),
        rc: "__construct".into(),
        sy: "__iterator".into(),
    };
    let mut table = TableBuilder::new(&mut mangler_core::Rng::for_pass(42, "async_super_eval"));
    let chunk = table.add_strict(compiled.compiled, compiled.strict);
    let table = table.finish(&names).unwrap();
    let captures = chunk
        .captures
        .iter()
        .map(|name| {
            format!("{{__proto__:null,get:()=>{name},set:v=>{name}=v,type:()=>typeof {name}}}")
        })
        .collect::<Vec<_>>()
        .join(",");
    let imports = prepared
        .support
        .names
        .iter()
        .map(|name| format!("const {name}=__support[{name:?}];"))
        .collect::<String>();
    let constructor = mangler_vm::eval_class::provider(None, "__apply", "__key", true);
    format!(
        "const __apply=Reflect.apply,__key=Symbol.iterator;const __support=({})(({})());{imports}{}class A{{constructor(a,b){{this.a=a;this.b=b}}}}class D extends A{{constructor(){{const __cap={{s:({constructor}),t:()=>this,n:()=>new.target}};return {}(__table[{}][0],__table[{}][1],[],[{captures}],{},{},void 0,true,[],new.target);}}}}Promise.resolve(new D()).then(value=>globalThis.__out=JSON.stringify(value));",
        prepared.support.factory,
        intrinsic_snapshot_factory(),
        swc_core::ecma::codegen::to_code(&Program::Script(Script {
            body: table.prologue,
            ..Default::default()
        })),
        names.interp_for(chunk.needs_eh, chunk.is_strict),
        chunk.index,
        chunk.index,
        chunk.cap_start,
        chunk.pcount
    )
}

#[test]
fn async_super_arrows_keep_lazy_lexical_capabilities_in_static_and_eval_bytecode() {
    use mangler_config::{ConfigFlags, Intensity, ResolvedConfig};
    use mangler_testkit::cross_engine::{Engine, chrome_path, evaluate_many, node_path};
    let cases = [
        "(async()=>super(1,2))()",
        "(async()=>{super(1,2);this.result=7;return this})()",
        "let log=[];(async()=>{try{this}catch(e){log.push(e.name)}super(1,2);this.result=log;return this})()",
        "(async()=>{await 0;super(1,2);return this})()",
        "(async()=> (async()=>super(1,2))())()",
        "(async()=>{let read=()=>this;super(1,2);this.result=read()===this;return this})()",
        "(async(a=1,b=2)=>super(a,b))()",
        "const init=async()=>super(1,2);init()",
        "(async()=>{let own=(function(){return this.v}).call({v:9});super(1,2);this.result=own;return this})()",
        "(async(x=this)=>super())().catch(e=>({error:e.name}))",
        "(async()=>{super(1,2);try{super(3,4)}catch(e){this.result=e.name}return this})()",
        "(async()=>{super(1,2);this.result=super.hasOwnProperty('a');return this})()",
        "const key=Symbol.iterator,original=Array.prototype[key];(async()=>{Array.prototype[key]=function(){return original.call([7,8])};try{super(1,2);return this}finally{Array.prototype[key]=original}})()",
        "const key=Symbol.iterator,original=Array.prototype[key];(async()=>{Array.prototype[key]=function(){return original.call([7,8])};try{super(...[1,2]);return this}finally{Array.prototype[key]=original}})()",
    ];
    let mut programs = Vec::new();
    let mut comparisons = Vec::new();
    for (index, source) in cases.iter().enumerate() {
        let native = programs.len();
        programs.push(format!("class A{{constructor(a,b){{this.a=a;this.b=b}}}}class D extends A{{constructor(){{return eval({source:?})}}}}Promise.resolve(new D()).then(value=>globalThis.__out=JSON.stringify(value));"));
        comparisons.push((native, programs.len(), format!("eval {index}")));
        programs.push(artifact(source));
    }
    for index in [0, 1, 3, 4, 9, 11] {
        let source = format!(
            "function pay(){{class A{{constructor(a,b){{this.a=a;this.b=b}}}}class D extends A{{constructor(){{return {};}}}}return new D()}}Promise.resolve(pay()).then(value=>globalThis.__out=JSON.stringify(value));",
            cases[index]
        );
        let native = programs.len();
        programs.push(source.clone());
        for preset in [Intensity::Minify, Intensity::High] {
            for whole in [false, true] {
                let config = ResolvedConfig::try_from(ConfigFlags {
                    preset: Some(preset),
                    seed: Some(42),
                    virtualize: (!whole).then(|| "pay".into()),
                    require_virtualized: (!whole).then(|| "pay".into()),
                    virtualize_program: whole,
                    ..Default::default()
                })
                .unwrap();
                comparisons.push((
                    native,
                    programs.len(),
                    format!("static {index} {preset:?} whole={whole}"),
                ));
                programs.push(
                    mangler_js::process(&source, &ParseOpts::default(), &config)
                        .unwrap()
                        .0,
                );
            }
        }
    }
    for source in [
        "'use strict';async function pay(a){return await(async()=>{await 0;return [this===undefined,a,arguments[0],new.target]})()}pay(7).then(value=>globalThis.__out=JSON.stringify(value));",
        "async function pay(){let f=async()=>{await 0;return this.value};return f()}pay.call({value:9}).then(value=>globalThis.__out=JSON.stringify(value));",
        "function* pay(){return async()=>{await 0;return [this.value,new.target]}}pay.call({value:6}).next().value().then(value=>globalThis.__out=JSON.stringify(value));",
    ] {
        let native = programs.len();
        programs.push(source.into());
        let config = ResolvedConfig::try_from(ConfigFlags {
            preset: Some(Intensity::Minify),
            seed: Some(42),
            virtualize: Some("pay".into()),
            require_virtualized: Some("pay".into()),
            ..Default::default()
        })
        .unwrap();
        comparisons.push((native, programs.len(), "ordinary suspension owner".into()));
        programs.push(
            mangler_js::process(source, &ParseOpts::default(), &config)
                .unwrap()
                .0,
        );
    }
    let engines: Vec<_> = [
        node_path().map(Engine::Node),
        chrome_path().map(Engine::Chrome),
    ]
    .into_iter()
    .flatten()
    .collect();
    assert!(
        !engines.is_empty(),
        "async super semantics require a real JavaScript engine"
    );
    let sources: Vec<_> = programs.iter().map(String::as_str).collect();
    let mut failures = Vec::new();
    for engine in engines {
        let results = evaluate_many(&engine, &sources).unwrap();
        for (native, actual, label) in &comparisons {
            if results[*native] != results[*actual] {
                std::fs::write(
                    format!("/tmp/mangler-async-super-regression-{actual}.js"),
                    &programs[*actual],
                )
                .unwrap();
                failures.push(format!(
                    "{} {label}: expected {:?}, got {:?}",
                    engine.name(),
                    results[*native],
                    results[*actual]
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
