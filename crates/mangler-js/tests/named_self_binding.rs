//! Named expression self bindings ignore sloppy writes and reject strict writes.
use mangler_config::{ConfigFlags, Intensity, ResolvedConfig};
use mangler_jsast::ParseOpts;
use mangler_testkit::cross_engine::{Engine, chrome_path, evaluate_many, node_path};

fn assert_protected(source: &str, expected: &str) {
    let mut programs = vec![format!("globalThis.__out={expected:?};"), source.into()];
    for preset in [Intensity::Minify, Intensity::High] {
        for seed in [3, 42] {
            let config = ResolvedConfig::try_from(ConfigFlags {
                preset: Some(preset),
                seed: Some(seed),
                virtualize: Some("Self".into()),
                require_virtualized: Some("Self".into()),
                ..Default::default()
            })
            .unwrap();
            let (output, notes) =
                mangler_js::process(source, &ParseOpts::default(), &config).unwrap();
            assert!(
                notes.iter().any(|note| note.message == "Self: virtualized"),
                "{notes:?}"
            );
            programs.push(output);
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
        "named self binding verification requires Node or Chrome"
    );
    for engine in engines {
        let values = evaluate_many(&engine, &sources).unwrap();
        for (index, value) in values.iter().enumerate().skip(1) {
            assert_eq!(
                value,
                &values[0],
                "{} case {index}: {source}",
                engine.name()
            );
        }
    }
}

#[test]
fn async_self_writes_keep_identity_and_evaluate_operands() {
    assert_protected(
        r#"
var log=[];
var callable=async function Self(){
  var result=(Self=(log.push('rhs'),7));
  log.push(result,Self===callable);
  (()=>{Self=9})();
  log.push(Self===callable);
  var old=Self++;
  log.push(typeof old,Self===callable);
  try{(()=>{'use strict';Self=(log.push('strict-rhs'),11)})()}catch(error){log.push(error.name)}
  const constant=1;
  try{constant=2}catch(error){log.push(error.name)}
  return log;
};
callable().then(value=>globalThis.__out=JSON.stringify(value),error=>globalThis.__out=error.name);
"#,
        "[\"rhs\",7,true,true,\"number\",true,\"strict-rhs\",\"TypeError\",\"TypeError\"]",
    );
}

#[test]
fn self_parameter_defaults_and_shadowing_use_distinct_bindings() {
    for (source, expected) in [
        (
            "var callable=async function Self(value=(Self=3)){return [value,Self===callable]};callable().then(value=>globalThis.__out=JSON.stringify(value));",
            "[3,true]",
        ),
        (
            "var callable=async function Self(Self){Self=3;return Self};callable(1).then(value=>globalThis.__out=JSON.stringify(value));",
            "3",
        ),
        (
            "var callable=async function Self(){var Self=1;Self=3;return Self};callable().then(value=>globalThis.__out=JSON.stringify(value));",
            "3",
        ),
        (
            "var callable=async function Self(){let result;{let Self=1;Self=3;result=Self}return [result,Self===callable]};callable().then(value=>globalThis.__out=JSON.stringify(value));",
            "[3,true]",
        ),
    ] {
        assert_protected(source, expected);
    }
}

#[test]
fn self_binding_is_available_to_eval_without_static_self_reads() {
    for (source, expected) in [
        (
            "var callable=async function Self(){return eval('Self=3;Self')===callable};callable().then(value=>globalThis.__out=JSON.stringify(value),error=>globalThis.__out=error.name);",
            "true",
        ),
        (
            "var callable=async function Self(){return (()=>eval('Self=3;Self'))()===callable};callable().then(value=>globalThis.__out=JSON.stringify(value),error=>globalThis.__out=error.name);",
            "true",
        ),
        (
            "var callable=async function Self(){try{return eval('\"use strict\";Self=3')}catch(error){return error.name}};callable().then(value=>globalThis.__out=JSON.stringify(value));",
            "\"TypeError\"",
        ),
        (
            "var callable=async function Self(){return [eval('var Self=3;Self'),Self]};callable().then(value=>globalThis.__out=JSON.stringify(value));",
            "[3,3]",
        ),
    ] {
        assert_protected(source, expected);
    }
}

#[test]
fn strict_and_generator_self_bindings_follow_the_same_policy() {
    for (source, expected) in [
        (
            "var callable=async function Self(){'use strict';try{Self=1}catch(error){return error.name}};callable().then(value=>globalThis.__out=JSON.stringify(value));",
            "\"TypeError\"",
        ),
        (
            "var callable=async function Self(){'use strict';try{(()=>{Self=1})()}catch(error){return error.name}};callable().then(value=>globalThis.__out=JSON.stringify(value));",
            "\"TypeError\"",
        ),
        (
            "var callable=function* Self(){Self=1;yield Self===callable};globalThis.__out=JSON.stringify(Array.from(callable()));",
            "[true]",
        ),
        (
            "var callable=async function* Self(){Self=1;yield Self===callable};callable().next().then(value=>globalThis.__out=JSON.stringify(value.value));",
            "true",
        ),
        (
            "var callable=function Self(){Self=1;return Self===callable};globalThis.__out=JSON.stringify(callable());",
            "true",
        ),
    ] {
        assert_protected(source, expected);
    }
}
