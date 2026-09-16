//! Class name writes are observable even when their result is unused.
use mangler_config::{ConfigFlags, Intensity, ResolvedConfig};
use mangler_jsast::ParseOpts;
use mangler_testkit::cross_engine::{Engine, chrome_path, evaluate_many, node_path};

fn check(source: &str) {
    let mut programs = vec![source.to_string()];
    for preset in [Intensity::Minify, Intensity::High] {
        for virtualized in [false, true] {
            let config = ResolvedConfig::try_from(ConfigFlags {
                preset: Some(preset),
                seed: Some(42),
                virtualize: virtualized.then(|| "*".into()),
                require_virtualized: virtualized.then(|| "*".into()),
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
        "class binding verification requires Node or Chrome"
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
fn class_expression_immutable_writes_and_outer_identity_survive_compression() {
    for operation in [
        "C=null",
        "C+=1",
        "C++",
        "[C]=[null]",
        "({value:C}={value:null})",
        "for(C of [null]){}",
        "(()=>{C=null})()",
    ] {
        check(&format!(
            "var C='outside';var cls=class C{{probe(){{return C}}modify(){{{operation};}}}};let error;try{{cls.prototype.modify()}}catch(e){{error=e.name}}globalThis.__out=JSON.stringify([error,cls.prototype.probe()===cls,C]);"
        ));
    }
}

#[test]
fn class_declaration_inner_name_remains_immutable_after_outer_reassignment() {
    check(
        "class C{probe(){return C}modify(){C=null}}var saved=C;C='outside';let error;try{saved.prototype.modify()}catch(e){error=e.name}globalThis.__out=JSON.stringify([error,saved.prototype.probe()===saved,C]);",
    );
    check(
        "var log=[];try{var cls=class C{static{C=(log.push('rhs'),null)}}}catch(e){log.push(e.name)}globalThis.__out=JSON.stringify(log);",
    );
    check(
        "var cls=class C{value=(C=null)};let error;try{new cls}catch(e){error=e.name}globalThis.__out=JSON.stringify(error);",
    );
}

#[test]
fn class_name_shadows_and_object_properties_remain_mutable() {
    check(
        "var cls=class C{parameter(C){C=3;return C}block(){let C=4;C++;return C}caught(){try{throw 5}catch(C){C++;return C}}property(){C.value=7;return C.value}nested(){return class C{static value=9}}};let o=cls.prototype;globalThis.__out=JSON.stringify([o.parameter(1),o.block(),o.caught(),o.property(),o.nested().value]);",
    );
}
