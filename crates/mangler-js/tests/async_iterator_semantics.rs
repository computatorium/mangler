//! The current async-from-sync close contract is verified in real engines.
use mangler_config::{ConfigFlags, Intensity, ResolvedConfig};
use mangler_jsast::ParseOpts;
use mangler_testkit::cross_engine::{Engine, chrome_path, evaluate_many, node_path};

#[test]
fn rejected_sync_values_close_once_and_preserve_the_original_error() {
    let source = r#"
async function pay(){
  let log=[];
  for(let mode of ['reject','done','getter','call','constructor','break']){
    let count=0;
    let iterator={
      [Symbol.iterator](){return this},
      next(){
        let value=mode==='break'?1:mode==='constructor'?Promise.resolve(1):Promise.reject('original');
        if(mode==='constructor')Object.defineProperty(value,'constructor',{get(){throw 'constructor'}});
        return {done:mode==='done',value};
      },
      get return(){
        log.push(mode+':get');
        if(mode==='getter')throw 'cleanup-getter';
        return function(){
          log.push(mode+':call:'+String(this===iterator)+':'+arguments.length);count++;
          if(mode==='call')throw 'cleanup-call';
          if(mode==='break')return {done:false,value:Promise.reject('close-value')};
          return 0;
        };
      }
    };
    try{for await(let value of iterator){break}}catch(error){log.push(mode+':error:'+error)}
    log.push(mode+':count:'+count);
  }
  return log;
}
pay().then(value=>globalThis.__out=JSON.stringify(value));
"#;
    assert_protected(source, source);
}

#[test]
fn abrupt_sync_results_reject_without_closing() {
    let source = r#"
async function pay(){
  let log=[];
  for(let mode of ['next','primitive','done-getter','value-getter','done-value-getter']){
    let count=0;
    let iterator={
      [Symbol.iterator](){return this},
      next(){
        if(mode==='next')throw mode;
        if(mode==='primitive')return 0;
        return {
          get done(){if(mode==='done-getter')throw mode;return mode==='done-value-getter'},
          get value(){throw mode}
        };
      },
      return(){count++;return {done:true}}
    };
    try{for await(let value of iterator){break}}
    catch(error){log.push(mode+':'+(error instanceof TypeError?'TypeError':error)+':'+count)}
  }
  return log;
}
pay().then(value=>globalThis.__out=JSON.stringify(value));
"#;
    // AsyncFromSyncIteratorContinuation steps 2-5 reject abrupt result getters
    // before the close-on-rejection continuation exists. Likewise, next rejects
    // abrupt calls and non-object results before entering that continuation.
    // V8 currently closes these paths too (builtins-async-iterator-gen.cc), so
    // this edge requires a specification oracle instead of native comparison.
    let expected = r#"globalThis.__out=JSON.stringify([
      'next:next:0','primitive:TypeError:0','done-getter:done-getter:0',
      'value-getter:value-getter:0','done-value-getter:done-value-getter:0'
    ]);"#;
    assert_protected(source, expected);
}

#[test]
fn async_iterator_close_validates_awaited_results_and_preserves_body_errors() {
    let source = r#"
async function pay(){
  let log=[];
  for(let mode of ['normal','null','noncallable','primitive','reject','getter','bodythrow']){
    let iterator={
      [Symbol.asyncIterator](){return this},
      next(){return {done:false,value:1}},
      get return(){
        log.push(mode+':get');
        if(mode==='getter')throw 'getter';
        if(mode==='null')return null;
        if(mode==='noncallable')return 3;
        return function(){
          log.push(mode+':call:'+arguments.length+':'+(this===iterator));
          if(mode==='primitive'||mode==='bodythrow')return 3;
          if(mode==='reject')return Promise.reject('close');
          return {get done(){throw 'done'},get value(){throw 'value'}};
        };
      }
    };
    try{for await(let value of iterator){if(mode==='bodythrow')throw 'body';break}}
    catch(error){log.push(mode+':'+(error instanceof TypeError?'TypeError':error))}
  }
  return log;
}
pay().then(value=>globalThis.__out=JSON.stringify(value));
"#;
    assert_protected(source, source);
}

fn assert_protected(source: &str, expected: &str) {
    let mut programs = vec![expected.to_string()];
    for preset in [Intensity::Minify, Intensity::High] {
        for seed in [5, 23] {
            let config = ResolvedConfig::try_from(ConfigFlags {
                preset: Some(preset),
                seed: Some(seed),
                virtualize: Some("pay".into()),
                require_virtualized: Some("pay".into()),
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
    let sources: Vec<&str> = programs.iter().map(String::as_str).collect();
    for engine in [
        node_path().map(Engine::Node),
        chrome_path().map(Engine::Chrome),
    ]
    .into_iter()
    .flatten()
    {
        let values = evaluate_many(&engine, &sources).unwrap();
        for (index, value) in values.iter().enumerate().skip(1) {
            assert_eq!(value, &values[0], "{} close case {index}", engine.name());
        }
    }
}
