//! Native lexical initialization remains at the source statement boundary.
use mangler_config::{ConfigFlags, Intensity, ResolvedConfig};
use mangler_jsast::ParseOpts;
use mangler_testkit::cross_engine::{Engine, chrome_path, evaluate_many, node_path};

#[test]
fn compression_preserves_lexical_initialization_and_iteration_heads() {
    let engines = [
        Engine::Node(node_path().expect("Node required for lexical TDZ validation")),
        Engine::Chrome(chrome_path().expect("Chrome required for lexical TDZ validation")),
    ];
    let fixtures = [
        "let out;try{(()=>x)()}catch(e){out=e.name}let x;globalThis.__out=out;",
        "let out;try{(()=>x)()}catch(e){out=e.name}let x=undefined;globalThis.__out=out;",
        "let out;try{(()=>x)()}catch(e){out=e.name}let x=void 0;globalThis.__out=out;",
        "let out;try{(()=>x)()}catch(e){out=e.name}let x=1?undefined:2;globalThis.__out=out;",
        "let out;try{(()=>{x=3})()}catch(e){out=e.name}let x;globalThis.__out=out;",
        "let out;try{(()=>{for({x}of[{}]){}})()}catch(e){out=e.name}let x;globalThis.__out=out;",
        "let out;try{(()=>{for([x=y]of[[]]){}})()}catch(e){out=e.name}let y;globalThis.__out=out;",
        "let out;try{(()=>{for([...x]of[[]]){}})()}catch(e){out=e.name}let x;globalThis.__out=out;",
        "let out=[];for(let key in {a:1})out.push(key);for(let value of [2])out.push(value);for(let index;index===undefined;index=3)out.push(index===undefined);globalThis.__out=JSON.stringify(out);",
        "let named=function(){};let arrow=()=>1;let [other]=[3];globalThis.__out=JSON.stringify([named.name,arrow.name,other]);",
        "let out;try{let {}=void 0}catch(e){out=e.name}globalThis.__out=out;",
        "let out;try{let []=void 0}catch(e){out=e.name}globalThis.__out=out;",
        "let observed=[];let Payment=class{static nameAtInitialization=observed.push(this.name)};globalThis.__out=JSON.stringify([Payment.name,observed]);",
        "let observed=[];let Payment=(class{static {observed.push(this.name)}});globalThis.__out=JSON.stringify([Payment.name,observed]);",
        "let observed=[];let Conditional=true?class{static {observed.push(this.name)}}:null;globalThis.__out=JSON.stringify([Conditional.name,observed]);",
        "let conditional=true?function(){}:null;let sequence=(0,function(){});globalThis.__out=JSON.stringify([conditional.name,sequence.name]);",
        "let conditional=true?()=>1:null;let sequence=(0,()=>1);globalThis.__out=JSON.stringify([conditional.name,sequence.name]);",
        "let explicit=function original(){};let renamed=class Original{static seen=this.name};globalThis.__out=JSON.stringify([explicit.name,renamed.name,renamed.seen]);",
    ];
    for preset in [Intensity::Minify, Intensity::High] {
        for whole_program in [false, true] {
            for strict in [false, true] {
                let mut programs = Vec::new();
                for fixture in fixtures {
                    let source = format!("{}{fixture}", if strict { "'use strict';" } else { "" });
                    let config = ResolvedConfig::try_from(ConfigFlags {
                        preset: Some(preset),
                        virtualize_program: whole_program,
                        seed: Some(42),
                        ..Default::default()
                    })
                    .unwrap();
                    let (output, _) =
                        mangler_js::process(&source, &ParseOpts::default(), &config).unwrap();
                    assert!(!output.contains("mangler_lexical_initializer"));
                    programs.extend([source, output]);
                }
                let sources: Vec<_> = programs.iter().map(String::as_str).collect();
                for engine in &engines {
                    let observed = evaluate_many(engine, &sources).unwrap();
                    for (fixture, values) in fixtures.iter().zip(observed.chunks_exact(2)) {
                        assert_eq!(
                            values[0],
                            values[1],
                            "{} preset={preset:?} whole={whole_program} strict={strict}: {fixture}",
                            engine.name()
                        );
                    }
                }
            }
        }
    }
}
