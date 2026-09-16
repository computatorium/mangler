//! Abrupt exits from finally discard only that finally's saved completion.
use mangler_config::{ConfigFlags, Intensity, ResolvedConfig};
use mangler_jsast::ParseOpts;
use mangler_testkit::cross_engine::{Engine, chrome_path, evaluate_many, node_path};

#[test]
fn nested_finally_local_control_flow_preserves_enclosing_completion() {
    let engines = [
        Engine::Node(node_path().expect("Node required for finally completion validation")),
        Engine::Chrome(chrome_path().expect("Chrome required for finally completion validation")),
    ];
    let bodies = [
        "try{return 42}finally{do try{return 43}finally{break}while(0)}",
        "try{return 42}finally{L:try{return 43}finally{break L}}",
        "try{return 42}finally{do try{return 43}finally{continue}while(0)}",
        "try{return 41}finally{try{return 42}finally{do try{return 43}finally{break}while(0)}}",
        "try{throw 42}finally{L:try{return 43}finally{break L}}",
        "try{throw 0}catch(e){return 42}finally{L:try{return 43}finally{break L}}",
        "try{return 42}finally{try{try{return 43}finally{throw 9}}catch(e){}}",
        "try{return 42}finally{do{break}while(0)}",
        "L:try{return 42}finally{break L}return 43",
        "try{return 42}finally{return 43}",
        "try{return 42}finally{throw 43}",
        "try{return 42}finally{let i=0;L:while(i++<2){try{try{return 43}finally{continue L}}finally{globalThis.trace.push(i)}}}",
        "try{return 42}finally{L:for(let x of { [Symbol.iterator](){return {next(){return {value:1,done:false}},return(){globalThis.trace.push('closed');return {done:true}}}}}){try{return 43}finally{break L}}}",
        "try{throw 1}finally{let [x]={[Symbol.iterator](){return {next(){return {value:0,done:false}},return(){throw 2}}}}}",
        "try{throw 1}finally{let [x]={[Symbol.iterator](){return {next(){return {value:0,done:false}},get return(){throw 2}}}}}",
        "try{try{throw 1}finally{let [x]={[Symbol.iterator](){return {next(){return {value:0,done:false}},return(){return 2}}}}}}catch(e){return e.name}",
        "try{return 42}finally{try{let [x]={[Symbol.iterator](){return {next(){return {value:0,done:false}},return(){throw 2}}}}}catch(e){globalThis.trace.push(e)}}",
        "try{throw 42}finally{try{let [x]={[Symbol.iterator](){return {next(){return {value:0,done:false}},return(){throw 2}}}}}catch(e){globalThis.trace.push(e)}}",
        "try{throw 42}finally{let [x]={[Symbol.iterator](){return {next(){return {done:true}},return(){throw 2}}}}}",
    ];
    for preset in [Intensity::Minify, Intensity::High] {
        let mut programs = Vec::new();
        let mut labels = Vec::new();
        for seed in [3, 42, 99] {
            for strict in [false, true] {
                for body in bodies {
                    let source = format!(
                        "{}globalThis.trace=[];function pay(){{{body}}}try{{globalThis.__out=JSON.stringify(['return',pay(),trace])}}catch(e){{globalThis.__out=JSON.stringify(['throw',e,trace])}}",
                        if strict { "'use strict';" } else { "" }
                    );
                    let config = ResolvedConfig::try_from(ConfigFlags {
                        preset: Some(preset),
                        virtualize: Some("pay".into()),
                        require_virtualized: Some("pay".into()),
                        seed: Some(seed),
                        ..Default::default()
                    })
                    .unwrap();
                    let (output, _) =
                        mangler_js::process(&source, &ParseOpts::default(), &config).unwrap();
                    programs.extend([source, output]);
                    labels.push(format!("seed={seed} strict={strict}: {body}"));
                }
            }
        }
        let sources: Vec<_> = programs.iter().map(String::as_str).collect();
        for engine in &engines {
            let observed = evaluate_many(engine, &sources).unwrap();
            for (label, values) in labels.iter().zip(observed.chunks_exact(2)) {
                assert_eq!(values[0], values[1], "{} {preset:?} {label}", engine.name());
            }
        }
    }
}
