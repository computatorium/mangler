//! Optimization must preserve lexical initialization and observable host calls.
use mangler_config::{ConfigFlags, Intensity, KeepNames, ResolvedConfig};
use mangler_jsast::ParseOpts;

fn check(source: &str) {
    for preset in [
        Intensity::Minify,
        Intensity::Low,
        Intensity::Medium,
        Intensity::High,
        Intensity::Max,
    ] {
        let config = ResolvedConfig::try_from(ConfigFlags {
            preset: Some(preset),
            seed: Some(42),
            keep_names: Some(KeepNames(vec!["*".into()])),
            ..Default::default()
        })
        .unwrap();
        let (output, _) = mangler_js::process(source, &ParseOpts::default(), &config).unwrap();
        mangler_testkit::assert_behaviorally_equal(source, &output);
    }
}

#[test]
fn switch_bindings_keep_temporal_dead_zone() {
    check(
        "function pay(){switch(1){case 1:return (function(){return arguments.length})(1,2);case 2:let arguments}}globalThis.__out=pay();",
    );
    check(
        "function pay(){try{switch(1){case 1:return typeof x;case 2:let x=3}}catch(e){return e.name}}globalThis.__out=pay();",
    );
    check(
        "let x='outer';function pay(){try{switch(1){case 1:return (()=>x)();case 2:const x=3}}catch(e){return e.name}}globalThis.__out=pay();",
    );
    check(
        "let x=1;function pay(){try{switch(x){case x:return 'bad';case 2:let x=3}}catch(e){return e.name}}globalThis.__out=pay();",
    );
    check(
        "function pay(){switch(1){case 1:return ((x)=>x)(4);case 2:let x=3}}globalThis.__out=pay();",
    );
    check(
        "function pay(){switch(1){case 1:{let x=7;return x}case 2:let x=3}}globalThis.__out=pay();",
    );
    check(
        "function pay(){try{switch(1){case 1:x=4;return x;case 2:let x=3}}catch(e){return e.name}}globalThis.__out=pay();",
    );
}

#[test]
fn dead_object_assignment_retains_key_coercion() {
    check(
        "let log=[];let k={[Symbol.toPrimitive](){log.push('key');return 'x'}};function pay(){let o={};o[k]=(log.push('rhs'),7);return log}globalThis.__out=JSON.stringify(pay());",
    );
}

#[test]
fn for_of_destructuring_elisions_advance_and_close_iterators() {
    let source = r#"
function pay(){
  let log=[],value;
  function iterable(throwAt,closeError){let count=0;return {
    [Symbol.iterator](){return this},
    next(){log.push('next'+count);if(count++===throwAt)throw 'step';return {
      done:false,get value(){log.push('value');return 7}
    }},
    return(){log.push('close');if(closeError)throw 'close-error';return {}}
  }}
  for([,,] of [iterable(-1,false)]){}
  for([value,,] of [iterable(-1,false)]){}
  try{for([,,] of [iterable(1,true)]){}}catch(error){log.push(error)}
  try{for([,,] of [iterable(-1,true)]){}}catch(error){log.push(error)}
  let count=0;function* values(){try{count++;yield;count++;yield;count++}finally{log.push('finally')}}
  for([/* , */ ,// , ]
  ,] of [values()]){}
  return [value,count,log]
}
globalThis.__out=JSON.stringify(pay());
"#;
    let mut programs = vec![source.to_string()];
    for preset in [Intensity::Minify, Intensity::High, Intensity::Max] {
        for virtualized in [false, true] {
            let config = ResolvedConfig::try_from(ConfigFlags {
                preset: Some(preset),
                seed: Some(42),
                virtualize: virtualized.then(|| "pay".into()),
                require_virtualized: virtualized.then(|| "pay".into()),
                ..Default::default()
            })
            .unwrap();
            let (output, _) = mangler_js::process(source, &ParseOpts::default(), &config).unwrap();
            programs.push(output);
        }
    }
    // QuickJS reads iterator result.value for holes. Test262 and both V8 hosts
    // require elisions to advance without reading the value getter.
    use mangler_testkit::cross_engine::{Engine, chrome_path, evaluate_many, node_path};
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
            assert_eq!(value, &values[0], "{} elision case {index}", engine.name());
        }
    }
}

#[test]
fn binding_free_assignments_keep_iteration_and_object_requirements() {
    check(
        r#"
function pay(){
  let log=[];
  function iterator(fail){return {
    [Symbol.iterator](){log.push('open');return this},
    next(){log.push('next');return {done:false,value:7}},
    return(){log.push('close');if(fail)throw 'close-error';return {}}
  }}
  let source=iterator(false);
  log.push(([]=source)===source);
  log.push(([,,]=source)===source);
  try{([]=iterator(true))}catch(error){log.push(error)}
  try{({}=null)}catch(error){log.push(error.name)}
  try{({}=undefined)}catch(error){log.push(error.name)}
  let object={};log.push(({}=object)===object);
  return log
}
globalThis.__out=JSON.stringify(pay());
"#,
    );
}

#[test]
fn mutable_static_methods_are_not_constant_folded() {
    check(
        "let original=String.fromCharCode;String.fromCharCode=()=> 'patched';function pay(){return String.fromCharCode(65)}try{globalThis.__out=pay()}finally{String.fromCharCode=original}",
    );
    check(
        "let String={fromCharCode(){return 'local'}};function pay(){return String.fromCharCode(65)}globalThis.__out=pay();",
    );
}

#[test]
fn inlining_keeps_distinct_bindings_with_reserved_names() {
    check(
        "function decode(u){var out='';for(var i=0;i<u.length;i++)out+=u[i];return out}function pay(){var out=['n','e','x','t'];return decode(out)}globalThis.__out=pay();",
    );
}

#[test]
fn deleting_declared_bindings_retains_reference_semantics() {
    check(
        "var value=4;function pay(){return [delete value,value]}globalThis.__out=JSON.stringify(pay());",
    );
}

#[test]
fn explicit_function_and_class_names_survive_optimization() {
    check(
        "function pay(){return [class Foo {}?.name,(function Named(){}).name]}globalThis.__out=JSON.stringify(pay());",
    );
    check(
        "function pay(){class Inner{static label=this.name}function Local(){}return [Inner.name,Inner.label,Local.name]}globalThis.__out=JSON.stringify(pay());",
    );
}

#[test]
fn reflection_survives_default_naming_in_every_preset() {
    let source = "function pay(){function Local(){}class Inner{static label=this.name}let arrow=()=>1;let inferred=function(){};return [Local.name,Inner.name,Inner.label,arrow.name,inferred.name]}globalThis.__out=JSON.stringify(pay());";
    for preset in [
        Intensity::Minify,
        Intensity::Low,
        Intensity::Medium,
        Intensity::High,
        Intensity::Max,
    ] {
        for virtualized in [false, true] {
            let config = ResolvedConfig::try_from(ConfigFlags {
                preset: Some(preset),
                seed: Some(42),
                virtualize: virtualized.then(|| "pay".into()),
                require_virtualized: virtualized.then(|| "pay".into()),
                ..Default::default()
            })
            .unwrap();
            let (output, _) = mangler_js::process(source, &ParseOpts::default(), &config).unwrap();
            mangler_testkit::assert_behaviorally_equal(source, &output);
        }
    }
}

#[test]
fn native_factories_keep_source_optimization_safeguards() {
    let source = "let log=[];let k={[Symbol.toPrimitive](){log.push('key');return 'x'}};function pay(){function keep(){let o={};o[k]=(log.push('rhs'),7);return log}return keep()}globalThis.__out=JSON.stringify(pay());";
    for preset in [Intensity::Minify, Intensity::High, Intensity::Max] {
        let config = ResolvedConfig::try_from(ConfigFlags {
            preset: Some(preset),
            virtualize: Some("pay".into()),
            virtualize_exclude: Some("keep".into()),
            require_virtualized: Some("pay".into()),
            seed: Some(42),
            ..Default::default()
        })
        .unwrap();
        let (output, _) = mangler_js::process(source, &ParseOpts::default(), &config).unwrap();
        mangler_testkit::assert_behaviorally_equal(source, &output);
    }
}

#[test]
fn native_factories_keep_inferred_callable_names() {
    let source = "function pay(){function keep(){let arrow=()=>1;let inferred=(function(){});let {fallback=()=>2}={};return [arrow.name,inferred.name,fallback.name]}return keep()}globalThis.__out=JSON.stringify(pay());";
    for preset in [Intensity::Minify, Intensity::High, Intensity::Max] {
        let config = ResolvedConfig::try_from(ConfigFlags {
            preset: Some(preset),
            virtualize: Some("pay".into()),
            virtualize_exclude: Some("keep".into()),
            require_virtualized: Some("pay".into()),
            seed: Some(42),
            ..Default::default()
        })
        .unwrap();
        let (output, _) = mangler_js::process(source, &ParseOpts::default(), &config).unwrap();
        mangler_testkit::assert_behaviorally_equal(source, &output);
    }
}
