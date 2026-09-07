//! Execute exact production artifacts in explicitly configured Node and Chrome.
//! CI requires both; local runs opt in with absolute engine paths.

use mangler_config::{ConfigFlags, Intensity, ResolvedConfig, StringMode};
use mangler_jsast::ParseOpts;
use mangler_testkit::cross_engine::{Engine, chrome_path, evaluate_many, node_path};

const LEVELS: [Intensity; 5] = [
    Intensity::Minify,
    Intensity::Low,
    Intensity::Medium,
    Intensity::High,
    Intensity::Max,
];

fn transform(source: &str, mut flags: ConfigFlags) -> String {
    flags.seed = Some(42);
    let config = ResolvedConfig::try_from(flags).unwrap();
    let (output, notes) = mangler_js::process(source, &ParseOpts::default(), &config).unwrap();
    if config.passes.virtualize.required.is_some() {
        assert!(
            notes
                .iter()
                .any(|note| note.message.ends_with(": virtualized")),
            "required function must have positive VM coverage: {notes:?}"
        );
    }
    output
}

fn parity(engine: &Engine, pairs: &[(String, String, String)]) {
    let programs: Vec<&str> = pairs
        .iter()
        .flat_map(|(_, a, b)| [a.as_str(), b.as_str()])
        .collect();
    let values = evaluate_many(engine, &programs)
        .unwrap_or_else(|e| panic!("{} execution failed: {e}", engine.name()));
    for (i, (name, _, transformed)) in pairs.iter().enumerate() {
        assert_eq!(
            values[i * 2],
            values[i * 2 + 1],
            "{} diverged on {name}\n--- exact artifact ---\n{transformed}",
            engine.name()
        );
        assert_eq!(
            values[i * 2]["outcome"][0],
            "value",
            "original fixture {name} must execute successfully"
        );
        assert!(
            values[i * 2]["rejections"].as_array().unwrap().is_empty(),
            "original fixture {name} must settle without rejection"
        );
    }
}

#[test]
fn node_all_presets_execute_the_full_corpus() {
    let Some(node) = node_path() else {
        return;
    };
    let engine = Engine::Node(node);
    let corpus =
        mangler_testkit::corpus::enumerate(&mangler_testkit::corpus::default_corpus_dir()).unwrap();
    for level in LEVELS {
        let pairs: Vec<_> = corpus
            .iter()
            .map(|path| {
                let source = std::fs::read_to_string(path).unwrap();
                let output = transform(
                    &source,
                    ConfigFlags {
                        preset: Some(level),
                        ..Default::default()
                    },
                );
                (format!("{} / {level:?}", path.display()), source, output)
            })
            .collect();
        parity(&engine, &pairs);
    }
}

#[test]
fn chrome_loads_exact_artifacts_for_every_preset() {
    let Some(chrome) = chrome_path() else {
        return;
    };
    let engine = Engine::Chrome(chrome);
    let sources = [
        (
            "strict",
            "'use strict'; function f(){return this===undefined} globalThis.__out=f();",
        ),
        (
            "async",
            include_str!("../../../tests/corpus/async_await_promise.js"),
        ),
        (
            "dom",
            include_str!("../../../tests/corpus/dom_events_dispatch.js"),
        ),
        (
            "closures",
            include_str!("../../../tests/corpus/closures_shared_cell.js"),
        ),
    ];
    for level in LEVELS {
        let pairs: Vec<_> = sources
            .iter()
            .map(|(name, source)| {
                (
                    format!("{name}/{level:?}"),
                    source.to_string(),
                    transform(
                        source,
                        ConfigFlags {
                            preset: Some(level),
                            ..Default::default()
                        },
                    ),
                )
            })
            .collect();
        parity(&engine, &pairs);
    }
}

#[test]
fn exact_hardened_and_verified_artifacts_run_in_both_engines() {
    let engines = [
        node_path().map(Engine::Node),
        chrome_path().map(Engine::Chrome),
    ];
    if engines.iter().all(Option::is_none) {
        return;
    }
    let source = "'use strict'; function calculate(n){return 'answer:'+String(n*2)} globalThis.__out=calculate(21);";
    let mut pairs = Vec::new();
    for (name, flags) in [
        (
            "self-defending",
            ConfigFlags {
                self_defending: Some(true),
                ..Default::default()
            },
        ),
        (
            "debug-protection",
            ConfigFlags {
                debug_protection: Some(true),
                ..Default::default()
            },
        ),
        (
            "global-anchor",
            ConfigFlags {
                harden_global_anchor: true,
                ..Default::default()
            },
        ),
        (
            "self-coupled",
            ConfigFlags {
                self_coupled_key: Some(true),
                strings_in_vm: Some(true),
                ..Default::default()
            },
        ),
        (
            "exec-trace",
            ConfigFlags {
                exec_trace_key: Some(true),
                strings_in_vm: Some(true),
                ..Default::default()
            },
        ),
        (
            "decoder-vm",
            ConfigFlags {
                strings_in_vm: Some(true),
                ..Default::default()
            },
        ),
        (
            "bound-key",
            ConfigFlags {
                key_source: Some("42".into()),
                key_expected: Some("42".into()),
                ..Default::default()
            },
        ),
    ] {
        let flags = ConfigFlags {
            preset: Some(Intensity::Max),
            strings: Some(StringMode::Encrypt),
            ..flags
        };
        let plain = transform(source, flags.clone());
        let checked = transform(
            source,
            ConfigFlags {
                verify: true,
                ..flags
            },
        );
        assert_eq!(
            plain, checked,
            "--verify must check the exact {name} artifact without changing it"
        );
        pairs.push((name.to_string(), source.to_string(), checked));
    }
    for engine in engines.into_iter().flatten() {
        parity(&engine, &pairs);
    }
}

#[test]
fn named_and_whole_program_vm_artifacts_run_under_every_preset() {
    let engines = [
        node_path().map(Engine::Node),
        chrome_path().map(Engine::Chrome),
    ];
    if engines.iter().all(Option::is_none) {
        return;
    }
    let source = "'use strict'; const rate=3; function calculate(n=2){let x=n*rate;function add(y){return x+y}return add(1)} globalThis.__out=JSON.stringify([calculate(),calculate(7)]);";
    let mut pairs = Vec::new();
    for level in LEVELS {
        for whole in [false, true] {
            let flags = ConfigFlags {
                preset: Some(level),
                virtualize: (!whole).then(|| "calculate".into()),
                virtualize_program: whole,
                require_virtualized: Some("calculate".into()),
                ..Default::default()
            };
            pairs.push((
                format!("{level:?}/whole={whole}"),
                source.to_string(),
                transform(source, flags),
            ));
        }
    }
    for engine in engines.into_iter().flatten() {
        parity(&engine, &pairs);
    }
}
