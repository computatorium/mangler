//! C-style resource heads share one disposal scope across all loop iterations.
use mangler_config::{ConfigFlags, Intensity, ResolvedConfig};
use mangler_jsast::ParseOpts;
use mangler_testkit::cross_engine::{Engine, chrome_path, evaluate_many, node_path};

#[test]
fn resource_for_scope_labels_and_disposal_order() {
    let source = r#"
function pay(){
  let log=[],i=0,resource='outer',captured;
  function make(name){log.push('init:'+name);return {[Symbol.dispose](){log.push('dispose:'+name)}}}
  outer:inner:for(using resource=make('first'),second=make('second');i<2;i++){
    captured=()=>resource;
    using body=make('body'+i);
    log.push('body:'+i);
    if(i===0){for(let j=0;j<1;j++)continue outer;}
    break inner;
  }
  log.push(resource,typeof captured(),i);
  for(using empty=make('empty');false;)log.push('unreachable');
  return log;
}
globalThis.__out=JSON.stringify(pay());
"#;
    assert_protected(
        source,
        r#"["init:first","init:second","init:body0","body:0","dispose:body0","init:body1","body:1","dispose:body1","dispose:second","dispose:first","outer","object",1,"init:empty","dispose:empty"]"#,
    );
}

#[test]
fn resource_for_abrupt_initializers_and_loop_exits() {
    let source = r#"
function pay(){
 let all=[];
 for(let mode of ['init','test','update','body','return','assign','tdz']){
  let log=[],i=0;
  function make(n){log.push('init:'+n);return {[Symbol.dispose](){log.push('dispose:'+n)}}}
  function fail(){throw mode}
  function run(){
   if(mode==='tdz'){for(using r=r;;){}return}
   for(using r=make('r'),s=mode==='init'?fail():make('s');mode==='test'?fail():i<1;mode==='update'?fail():i++){
    log.push('body');
    if(mode==='body')throw mode;
    if(mode==='return')return 7;
    if(mode==='assign')r=null;
   }
  }
  try{log.push('result:'+run())}catch(e){log.push(e instanceof TypeError?'TypeError':e instanceof ReferenceError?'ReferenceError':e)}
  all.push([mode,log]);
 }
 return all;
}
globalThis.__out=JSON.stringify(pay());
"#;
    assert_protected(
        source,
        r#"[["init",["init:r","dispose:r","init"]],["test",["init:r","init:s","dispose:s","dispose:r","test"]],["update",["init:r","init:s","body","dispose:s","dispose:r","update"]],["body",["init:r","init:s","body","dispose:s","dispose:r","body"]],["return",["init:r","init:s","body","dispose:s","dispose:r","result:7"]],["assign",["init:r","init:s","body","dispose:s","dispose:r","TypeError"]],["tdz",["ReferenceError"]]]"#,
    );
}

#[test]
fn resource_for_disposal_suppression_and_async_order() {
    let source = r#"
async function pay(){
 let log=[];
 try{
  for(using r={[Symbol.dispose](){log.push('dispose:r');throw 'r'}},s={[Symbol.dispose](){log.push('dispose:s');throw 's'}};;){throw 'body'}
 }catch(e){log.push(e.error,e.suppressed.error,e.suppressed.suppressed)}
 let i=0;
 outer:inner:for(await using r={async [Symbol.asyncDispose](){log.push('start:r');await 0;log.push('end:r')}},s={[Symbol.dispose](){log.push('dispose:s')}};i<2;i++){
  log.push('body:'+i);
  if(i===0)continue outer;
  break inner;
 }
 log.push('after');return log;
}
pay().then(value=>globalThis.__out=JSON.stringify(value));
"#;
    assert_protected(
        source,
        r#"["dispose:s","dispose:r","r","s","body","body:0","body:1","dispose:s","start:r","end:r","after"]"#,
    );
}

#[test]
fn resource_for_generator_lifetime_and_labelled_outer_continue() {
    let source = r#"
function pay(){
 let log=[];
 function* sequence(){for(using r={[Symbol.dispose](){log.push('dispose:generator')}};;){yield 1;yield 2}}
 let iterator=sequence();log.push(iterator.next().value);log.push(iterator.return(9).value);
 let i=0;
 outer:for(;i<2;i++){
  inner:for(using r={[Symbol.dispose](){log.push('dispose:'+i)}};;){log.push('body:'+i);continue outer}
 }
 return log;
}
globalThis.__out=JSON.stringify(pay());
"#;
    assert_protected(
        source,
        r#"[1,"dispose:generator",9,"body:0","dispose:0","body:1","dispose:1"]"#,
    );
}

fn assert_protected(source: &str, expected_json: &str) {
    // ForLoopEvaluation (14.7.4.2) uses an empty perIterationLets list for
    // resource bindings and calls DisposeResources exactly once on loop exit.
    // Node 26 currently re-disposes synchronous C-style heads, so its original
    // source execution is not the oracle for these explicit event traces.
    // https://tc39.es/ecma262/#sec-runtime-semantics-forloopevaluation
    let mut programs = vec![format!("globalThis.__out=JSON.stringify({expected_json});")];
    for preset in [Intensity::Minify, Intensity::High] {
        for mode in 0..3 {
            let config = ResolvedConfig::try_from(ConfigFlags {
                preset: Some(preset),
                seed: Some(23),
                virtualize: (mode == 1).then(|| "pay".into()),
                require_virtualized: (mode == 1).then(|| "pay".into()),
                virtualize_program: mode == 2,
                ..Default::default()
            })
            .unwrap();
            programs.push(
                mangler_js::process(source, &ParseOpts::default(), &config)
                    .unwrap()
                    .0,
            );
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
    assert!(
        !engines.is_empty(),
        "resource loops require a JavaScript engine"
    );
    for engine in engines {
        let values = evaluate_many(&engine, &sources).unwrap();
        for (variant, value) in values.iter().enumerate().skip(1) {
            assert_eq!(
                value,
                &values[0],
                "{} variant {variant}: {source}",
                engine.name()
            );
        }
    }
}
