//! Native class factories capture enclosing private operations, preserving local brands.
use mangler_config::{ConfigFlags, Intensity, ResolvedConfig};
use mangler_jsast::ParseOpts;
use mangler_testkit::cross_engine::{Engine, chrome_path, evaluate_many, node_path};

#[test]
fn nested_classes_capture_enclosing_private_operations() {
    let cases = [
        "async function pay(){class Outer{#x=17;async f(){let self=this;await 0;return class Inner{g(){return self.#x}}}}let I=await new Outer().f();return new I().g()}pay().then(value=>globalThis.__out=JSON.stringify(value));",
        "function pay(){class Outer{#x=18;*f(){let self=this;yield 0;return class Inner{g(){return self.#x}}}}let iterator=new Outer().f();iterator.next();return new(iterator.next().value)().g()}globalThis.__out=JSON.stringify(pay());",
        "function pay(){class Outer{#x=42;f(){var self=this;return class Inner{g(){return self.#x}}}}return new(new Outer().f())().g()}globalThis.__out=JSON.stringify(pay());",
        "function pay(){class Outer{#x=42;f(){let self=this;class Inner{g(){return self.#x}}return Inner}}return new(new Outer().f())().g()}globalThis.__out=JSON.stringify(pay());",
        "function pay(){class Outer{#x=42;f(){let self=this,I=class{static observed=this.name;g(){return self.#x}};return I}}let I=new Outer().f();return [new I().g(),I.name,I.observed]}globalThis.__out=JSON.stringify(pay());",
        "function pay(){class Outer{#x=42;f(){let self=this;let {I=class{g(){return self.#x}}}={};return I}}return new(new Outer().f())().g()}globalThis.__out=JSON.stringify(pay());",
        "function pay(){class Outer{#x=42;f(){let self=this;let [I=class{g(){return self.#x}}]=[];return I}}return new(new Outer().f())().g()}globalThis.__out=JSON.stringify(pay());",
        "function pay(){class Outer{#x=42;f(){let self=this,I;I=class{static observed=this.name;g(){return self.#x}};return I}}let I=new Outer().f();return [new I().g(),I.name,I.observed]}globalThis.__out=JSON.stringify(pay());",
        "function pay(){class Outer{#x=42;f(){let self=this;return {I:class{static observed=this.name;g(){return self.#x}}}}}let {I}=new Outer().f();return [new I().g(),I.name,I.observed]}globalThis.__out=JSON.stringify(pay());",
        "function pay(){class Outer{#x=42;#y=7;f(){let self=this;return class Inner{#x=3;g(){let error;try{self.#x}catch(e){error=e.name}return [this.#x,self.#y,error]}}}}return new(new Outer().f())().g()}globalThis.__out=JSON.stringify(pay());",
        "function pay(){class Outer{#x=3;#m(a){return [this.#x,a]}f(){let self=this;return class Inner{g(){self.#x+=4;return [self.#x++,++self.#x,self.#m(8),#x in self]}}}}return new(new Outer().f())().g()}globalThis.__out=JSON.stringify(pay());",
        "function pay(){class Outer{#x=9;f(){let self=this;return class Inner{g(o){return [o?.#x,self?.#x]}}}}let I=new Outer().f();return new I().g(null)}globalThis.__out=JSON.stringify(pay());",
        "function pay(){class Outer{#x=7;f(){let first=this;return class Middle{#y=8;f(){let second=this;return class Inner{#z=9;g(){return [first.#x,second.#y,this.#z]}}}}}}let M=new Outer().f(),I=new M().f();return new I().g()}globalThis.__out=JSON.stringify(pay());",
        "function pay(){class Outer{#Base=class{value=4};f(){return class Inner extends this.#Base{#Base=9;g(){return [this.value,this.#Base]}}}}return new(new Outer().f())().g()}globalThis.__out=JSON.stringify(pay());",
        "function pay(){class Outer{static #x=11;static f(){return class Inner{static g(){return Outer.#x}}}}return Outer.f().g()}globalThis.__out=JSON.stringify(pay());",
        "function pay(){let log=[];class Outer{#v=2;get #x(){log.push('get');return this.#v}set #x(v){log.push(['set',v]);this.#v=v}f(){let self=this;return class Inner{g(){self.#x+=3;return [self.#x,log]}}}}return new(new Outer().f())().g()}globalThis.__out=JSON.stringify(pay());",
        "function pay(){class Outer{#key='payment';#x=9;f(){let self=this;return class Inner{[self.#key](){return self.#x}}}}return new(new Outer().f())().payment()}globalThis.__out=JSON.stringify(pay());",
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
        "nested private class captures require a JavaScript engine"
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
                        .unwrap_or_else(|error| {
                            panic!("source {source} preset {preset:?} whole {whole}: {error}")
                        })
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
                        format!("/tmp/mangler-nested-private-{case}-{variant}.js"),
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

mod external {
    use mangler_core::Language;
    use mangler_js::runtime_frontend::{intrinsic_snapshot_factory, prepare_eval_with_context};
    use mangler_jsast::{Js, ParseOpts};
    use mangler_vm::{CompileOptions, TableBuilder, VmNames};
    use swc_core::ecma::ast::*;

    pub(super) fn external_private_artifact(source: &str) -> String {
        let ast = Js
            .parse(
                &format!("class D extends A{{#x;constructor(){{{source}}}}}"),
                &ParseOpts::default(),
            )
            .unwrap();
        let Program::Script(script) = ast.into_program() else {
            unreachable!()
        };
        let Stmt::Decl(Decl::Class(class)) = &script.body[0] else {
            unreachable!()
        };
        let constructor = class
            .class
            .body
            .iter()
            .find_map(|member| match member {
                ClassMember::Constructor(constructor) => Some(constructor),
                _ => None,
            })
            .unwrap();
        let prepared = mangler_vm::eval::prepare_eval_body(
            constructor.body.as_ref().unwrap().stmts.clone(),
            true,
            mangler_vm::eval::SourceContext::Function,
        );
        let context = mangler_vm::eval::EvalClassContext {
            capsule_binding: "__cap".into(),
            private_names: vec!["x".into()],
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
        let private = mangler_vm::eval_class::provider(Some("x"), "__apply", "__key", false);
        format!(
            "const __apply=Reflect.apply,__key=Symbol.iterator;const __support=({})(({})());{imports}{}class A{{constructor(a,b){{this.a=a;this.b=b}}}}class D extends A{{#x=42;constructor(){{super();const __cap={{p:{{x:({private})}},s:({constructor}),t:()=>this,n:()=>new.target}};return {}(__table[{}][0],__table[{}][1],[],[{captures}],{},{},void 0,true,[],new.target);}}}}Promise.resolve(new D()).then(value=>globalThis.__out=JSON.stringify(value));",
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
}

#[test]
fn nested_classes_keep_external_eval_private_capabilities() {
    let cases = [
        "class C{get(o){return o.#x}};this.result=new C().get(this);this",
        "let C=class{#y=9;get(o){return [o.#x,this.#y]}};this.result=new C().get(this);this",
        "let C=class{#x=9;get(o){return this.#x}};this.result=new C().get(this);this",
        "let C=class{get(o){return #x in o}};this.result=new C().get(this);this",
    ];
    let mut programs = Vec::new();
    for source in cases {
        programs.push(format!("class A{{}}class D extends A{{#x=42;constructor(){{super();return eval({source:?})}}}}Promise.resolve(new D()).then(value=>globalThis.__out=JSON.stringify(value));"));
        programs.push(external::external_private_artifact(source));
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
        "nested private eval capture requires JavaScript engines"
    );
    let sources: Vec<_> = programs.iter().map(String::as_str).collect();
    for engine in engines {
        let results = evaluate_many(&engine, &sources).unwrap();
        for (case, group) in results.chunks_exact(2).enumerate() {
            assert_eq!(
                group[0],
                group[1],
                "{} external private case {case}",
                engine.name()
            );
        }
    }
}
