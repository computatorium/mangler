//! Compression must retain writes and the original NamedEvaluation syntax.
use mangler_config::{ConfigFlags, Intensity, ResolvedConfig};
use mangler_jsast::ParseOpts;
use mangler_testkit::cross_engine::{Engine, chrome_path, evaluate_many, node_path};
fn check(source: &str) {
    let mut programs = vec![source.to_string()];
    for preset in [Intensity::Minify, Intensity::High] {
        for virtualized in [false, true] {
            let config = ResolvedConfig::try_from(ConfigFlags {
                preset: Some(preset),
                seed: Some(23),
                virtualize: virtualized.then(|| "pay".into()),
                require_virtualized: virtualized.then(|| "pay".into()),
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
        "assignment reference tests require Node or Chrome"
    );
    for engine in engines {
        let results = evaluate_many(&engine, &sources).unwrap();
        for (index, value) in results.iter().enumerate().skip(1) {
            assert_eq!(
                value,
                &results[0],
                "{} case {index}: {source}",
                engine.name()
            );
        }
    }
}
#[test]
fn const_writes_and_grouped_targets_keep_runtime_errors() {
    for target in ["x", "(x)"] {
        for operation in [
            format!("{target}=2"),
            format!("{target}++"),
            format!("++{target}"),
            format!("[{target}]=[2]"),
            format!("for({target} of [2]){{}}"),
        ] {
            check(&format!(
                "function pay(){{'use strict';const x=1;let log=[];try{{{operation}}}catch(e){{log.push(e.name)}}try{{(missing)=3}}catch(e){{log.push(e.name)}}return JSON.stringify(log)}}globalThis.__out=pay();"
            ));
        }
    }
}
#[test]
fn grouping_and_conditional_values_do_not_gain_inferred_class_names() {
    check(
        "function pay(){let x;(x)=(true?class{static observed=this.name}:null);let y=({['original']:true?class{static observed=this.name}:null})['original'];return JSON.stringify([x.name,x.observed,y.name,y.observed])}globalThis.__out=pay();",
    );
    check(
        "function pay(){let x;[(x)=class{static observed=this.name}]=[];return JSON.stringify([x.name,x.observed])}globalThis.__out=pay();",
    );
}
#[test]
fn computed_property_name_helper_preserves_utf16_before_static_initializers() {
    check(
        "function pay(){let x=({['original']:class{static observed=this.name}})['original'];let y=({['\\ud800']:class{static observed=this.name}})['\\ud800'];return JSON.stringify([x.name,x.observed,y.name,y.observed])}globalThis.__out=pay();",
    );
}

#[test]
fn immutable_assignments_in_return_and_throw_positions_keep_the_write() {
    for body in [
        "const x=1;return x=2",
        "const x=1;return (x)=2",
        "const x=1;throw x=2",
        "const x=1;return x+=2",
        "const x=1;return ++x",
    ] {
        check(&format!(
            "function pay(){{'use strict';{body}}}try{{globalThis.__out=String(pay())}}catch(e){{globalThis.__out=e.name}}"
        ));
    }
}

#[test]
fn immutable_assignment_rhs_retains_original_name_before_throwing() {
    for (target, operator, initial) in [
        ("x", "=", "null"),
        ("(x)", "=", "null"),
        ("x", "||=", "null"),
        ("(x)", "||=", "null"),
        ("x", "&&=", "true"),
        ("(x)", "&&=", "true"),
        ("x", "??=", "null"),
        ("(x)", "??=", "null"),
    ] {
        check(&format!(
            "function pay(){{'use strict';const x={initial};let log=[];try{{{target}{operator}class{{static{{log.push(this.name)}}}}}}catch(e){{log.push(e.name)}}return JSON.stringify(log)}}globalThis.__out=pay();"
        ));
    }
}
