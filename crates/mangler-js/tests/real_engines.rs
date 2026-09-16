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

#[test]
fn object_accessor_completion_length_and_parenthesized_names() {
    let fixtures = [
        (
            "invalid_template_decimal_escape",
            r"function pay(){return [(s=>[s[0],s.raw[0]])`\8`,(s=>[s[0],s.raw[0]])`\9`,(s=>[s[0],s.raw[0]])`\08`,(s=>[s[0],s.raw[0]])`\09`,(s=>[s[0],s.raw[0]])`\0`]}globalThis.__out=pay();",
        ),
        (
            "setter",
            "function pay(){let o={set x(v=4){return v}},s=Object.getOwnPropertyDescriptor(o,'x').set;let d=Object.getOwnPropertyDescriptor(s,'length');return [s.length,s(436),s(),s.name,d.writable,d.enumerable,d.configurable]}globalThis.__out=pay();",
        ),
        (
            "proto_accessor",
            "function pay(){let o={__proto__:null,['__proto__']:0,__proto__(){},get __proto__(){return 33},set __proto__(v){return 44}};let d=Object.getOwnPropertyDescriptor(o,'__proto__');return [Object.getPrototypeOf(o),d.get(),d.set(),d.get.name,d.set.name]}globalThis.__out=pay();",
        ),
        (
            "computed_cover",
            "function pay(){let key=Symbol('test262'),anonymous=Symbol(),o={[key]:(function(){}),[anonymous]:(function(){}),id:(()=>0),explicit:(function Named(){}),klass:(class NamedClass{}),sequence:(0,function(){})};return [o[key].name,o[anonymous].name,o.id.name,o.explicit.name,o.klass.name,o.sequence.name]}globalThis.__out=pay();",
        ),
    ];
    let mut pairs = Vec::new();
    for (name, source) in fixtures {
        for strict in [false, true] {
            for preset in [Intensity::Minify, Intensity::High] {
                let source = format!(
                    "{}{source};globalThis.__out=JSON.stringify(globalThis.__out);",
                    if strict { "'use strict';" } else { "" }
                );
                let output = transform(
                    &source,
                    ConfigFlags {
                        preset: Some(preset),
                        require_virtualized: Some("pay".into()),
                        ..Default::default()
                    },
                );
                pairs.push((format!("{name}/{strict}/{preset:?}"), source, output));
            }
        }
    }
    for engine in [
        node_path().map(Engine::Node),
        chrome_path().map(Engine::Chrome),
    ]
    .into_iter()
    .flatten()
    {
        parity(&engine, &pairs);
    }
}

#[test]
fn suspended_object_preserves_evaluation_order_descriptors_and_home() {
    let fixtures = [
        (
            "resource_init_names",
            "function* pay(){let arrow={['arrow']:()=>{}}['arrow'];let fn={['fn']:function(){}}['fn'];let gen={['gen']:function*(){}}['gen'];let cover={['cover']:(function(){})}['cover'];return [arrow.name,fn.name,gen.name,cover.name]}globalThis.__out=pay().next().value;",
        ),
        (
            "yield_accessors",
            "function* pay(){return {get [yield \"get\"](){return 7},set [yield \"set\"](v){return v}}}let i=pay(),a=i.next(),b=i.next(\"x\"),c=i.next(\"y\");globalThis.__out=[a,b,c.done,c.value&&Object.keys(c.value)]",
        ),
        (
            "ordered_descriptors",
            "function* pay(){let log=[],key={[Symbol.toPrimitive](){log.push('key');return 'x'}}, spread={get a(){log.push('spread');return 6}};let o={get [key](){return 1},[(log.push('data'), 'x')]:yield 2,set x(v){return v},...spread,[yield 3]:4};let d=Object.getOwnPropertyDescriptor(o,'x');return [log,Object.keys(o),d.get,d.set(9),d.set.name,Object.hasOwn(d.set,'prototype'),o.a]};let i=pay();globalThis.__out=[i.next(),i.next(7),i.next('end')];",
        ),
        (
            "home",
            "function* pay(){let base={get x(){return this.tag}},log=[];let o={__proto__:base,tag:yield 3,get [yield 4](){return super.x},[yield 5](v){return super.x+v}};return [o.answer,o.method(2),Object.getPrototypeOf(o)===base]};let i=pay();globalThis.__out=[i.next(),i.next(7),i.next('answer'),i.next('method')];",
        ),
        (
            "non_suspending_sideeffects",
            "function* pay(){let n=0,k={[Symbol.toPrimitive](){n++;return 'x'}};let o={get[k](){return 3},set[k](v){return v},[k]:7};return [n,o.x,Object.keys(o)]}globalThis.__out=pay().next();",
        ),
        (
            "anon_names",
            "function* pay(){let key=yield 1;let o={[key]:(function(){}),a:(()=>1),[yield 2]:(function Named(){})};return [o[key].name,o.a.name,o.b.name]}let i=pay();globalThis.__out=[i.next(),i.next(Symbol('foo')),i.next('b')];",
        ),
        (
            "static_yield",
            "function* pay(){let proto={x:3};let o={__proto__:yield 1,a:yield 2,get x(){return super.x}};return [o.a,o.x,Object.getPrototypeOf(o)===proto]}let i=pay();globalThis.__out=[i.next(),i.next({x:3}),i.next(4)];",
        ),
        (
            "loop",
            "function* pay(){let a=[];for(let i=0;i<3;i++)a.push({[yield i]:i,get x(){return this['k'+i]}});return a.map(o=>o.x)}let i=pay();globalThis.__out=[i.next(),i.next('k0'),i.next('k1'),i.next('k2')];",
        ),
    ];
    let mut pairs = Vec::new();
    for (name, source) in fixtures {
        for strict in [false, true] {
            for preset in [Intensity::Minify, Intensity::High] {
                let source = format!(
                    "{}{source};globalThis.__out=JSON.stringify(globalThis.__out);",
                    if strict { "'use strict';" } else { "" }
                );
                let output = transform(
                    &source,
                    ConfigFlags {
                        preset: Some(preset),
                        require_virtualized: Some("pay".into()),
                        ..Default::default()
                    },
                );
                pairs.push((format!("{name}/{strict}/{preset:?}"), source, output));
            }
        }
    }
    for engine in [
        node_path().map(Engine::Node),
        chrome_path().map(Engine::Chrome),
    ]
    .into_iter()
    .flatten()
    {
        parity(&engine, &pairs);
    }
}
