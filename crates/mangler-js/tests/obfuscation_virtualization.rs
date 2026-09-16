//! The full high/max pipeline must preserve decoder and VM initialization scopes.
use mangler_config::{ConfigFlags, Intensity, ResolvedConfig};
use mangler_jsast::ParseOpts;

#[test]
fn required_virtualization_survives_high_and_max_obfuscation() {
    let fixtures = [
        (
            "pay",
            "let before=pay;async function pay(x=7){return await x}before().then(x=>globalThis.__out=x);",
        ),
        (
            "pay",
            "async function* pay(){yield await 7}pay().next().then(x=>globalThis.__out=JSON.stringify(x));",
        ),
        (
            "pay",
            "function* pay(){yield 1;return 2}let g=pay();globalThis.__out=[g.next(),g.next()];",
        ),
        (
            "pay",
            "function* pay(){try{yield 1;yield 2}finally{globalThis.closed=true}}let g=pay();g.next();globalThis.__out=[g.return(7),closed];",
        ),
        (
            "pay",
            "class Account{pay(x){return x*7}}globalThis.__out=new Account().pay(2);",
        ),
        (
            "#pay",
            "class Account{#pay(x){return x*7}charge(x){return this.#pay(x)}}globalThis.__out=new Account().charge(2);",
        ),
        (
            "pay",
            "function pay(x=y,y=2){return x}try{globalThis.__out=pay()}catch(e){globalThis.__out=e.name}",
        ),
        (
            "pay",
            r"let tag=(s)=>[s[0],s.raw[0]];function pay(){return tag`\unicode`}globalThis.__out=pay();",
        ),
    ];
    for level in [Intensity::High, Intensity::Max] {
        for (target, source) in fixtures {
            let source = format!("{source};globalThis.__out=JSON.stringify(globalThis.__out);");
            let mut config = ResolvedConfig::try_from(ConfigFlags {
                preset: Some(level),
                virtualize: Some(target.into()),
                require_virtualized: Some(target.into()),
                seed: Some(42),
                ..Default::default()
            })
            .unwrap();
            config.engine.keep_names = vec!["*".into()];
            let (output, _) = mangler_js::process(&source, &ParseOpts::default(), &config).unwrap();
            mangler_testkit::eval::assert_behaviorally_equal_with(
                &source,
                &output,
                &mangler_testkit::eval::CaptureMode::sink(),
            );
        }
    }
}
