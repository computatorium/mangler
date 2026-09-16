//! Private field key mangling and grouped definitions retain source names.
use mangler_config::{ConfigFlags, Intensity, ResolvedConfig};
use mangler_jsast::ParseOpts;
use mangler_testkit::cross_engine::{Engine, chrome_path, evaluate_many, node_path};

#[test]
fn private_field_initializers_keep_original_named_evaluation() {
    let mut programs = Vec::new();
    for source in [
        r###"class C{#arrow=()=>1;#fn=function(){};#cls=class{static observed=this.name};get(){return [this.#arrow.name,this.#fn.name,this.#cls.name,this.#cls.observed]}}globalThis.__out=new C().get()"###,
        r###"class C{static #arrow=()=>1;static #fn=function(){};static #cls=class{static observed=this.name};static get(){return [this.#arrow.name,this.#fn.name,this.#cls.name,this.#cls.observed]}}globalThis.__out=C.get()"###,
        r###"class C{#fn=(function(){});#cls=(class{static observed=this.name});get(){return[this.#fn.name,this.#cls.name,this.#cls.observed]}}globalThis.__out=new C().get()"###,
        r###"class C{#fn=true?()=>1:null;#cls=true?class{static observed=this.name}:null;get(){return[this.#fn.name,this.#cls.name,this.#cls.observed]}}globalThis.__out=new C().get()"###,
    ] {
        let source = format!("{source};globalThis.__out=JSON.stringify(globalThis.__out)");
        programs.push(source.clone());
        for preset in [Intensity::Minify, Intensity::High] {
            for whole in [false, true] {
                let config = ResolvedConfig::try_from(ConfigFlags {
                    preset: Some(preset),
                    virtualize_program: whole,
                    seed: Some(42),
                    ..Default::default()
                })
                .unwrap();
                programs.push(
                    mangler_js::process(&source, &ParseOpts::default(), &config)
                        .unwrap()
                        .0,
                );
            }
        }
    }
    let sources: Vec<_> = programs.iter().map(String::as_str).collect();
    for engine in [
        Engine::Node(node_path().expect("Node required")),
        Engine::Chrome(chrome_path().expect("Chrome required")),
    ] {
        let results = evaluate_many(&engine, &sources).unwrap();
        for (case, group) in results.chunks_exact(5).enumerate() {
            for (variant, result) in group.iter().enumerate().skip(1) {
                assert_eq!(
                    result, &group[0],
                    "{engine:?} case {case} variant {variant}"
                );
            }
        }
    }
}
