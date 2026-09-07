//! Native class invariants must survive protection of their eligible method bodies.
use mangler_config::{ConfigFlags, Intensity, ResolvedConfig};
use mangler_jsast::ParseOpts;
use mangler_testkit::cross_engine::{node_path, run_bounded};
use std::{process::Command, time::Duration};

fn transform(src: &str, class: bool, regex: bool) -> String {
    let cfg = ResolvedConfig::try_from(ConfigFlags {
        preset: Some(Intensity::Minify),
        seed: Some(7),
        virtualize_program: true,
        virtualize_desugar_class: class,
        virtualize_desugar_regex: regex,
        ..Default::default()
    })
    .unwrap();
    mangler_js::process(src, &ParseOpts::default(), &cfg)
        .unwrap()
        .0
}

fn node_output(src: &str) -> String {
    let mut command = Command::new(node_path().expect("test checked configured Node"));
    command.args(["-e", src]);
    let out = run_bounded(&mut command, Duration::from_secs(5))
        .expect("Node semantic probe must finish within five seconds");
    assert!(
        out.status.success(),
        "Node failed for {src}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn native_class_semantics_survive_method_virtualization() {
    if node_path().is_none() {
        return;
    }
    for src in [
        "let n=0; function base(){ n++; return class {}; } class C extends base(){} new C; console.log(n);",
        "class B{} class C extends B { constructor(_self){ super(); this.x=_self; } } console.log(new C(7).x);",
        "class C { static x=this; static getSelf=()=>this; } console.log(C.x===C,C.getSelf()===C);",
        "let calls=0; class B{set x(v){calls++;}} class C extends B{x=7;} let c=new C; console.log(c.x,calls,Object.keys(c));",
        "function B(){return {base:1};} class C extends B { x=7; constructor(){super();this.y=2;} } console.log(JSON.stringify(new C));",
        "class B{} class C extends B{} class D extends C{} console.log(new D instanceof D,new D instanceof C);",
        "class C{m(){return this;} } let f=new C().m; console.log(f()===undefined); try{new f;console.log('bad')}catch(e){console.log(e.name)}",
        "class C{} try{C();console.log('bad')}catch(e){console.log(e.name)} console.log(Object.getOwnPropertyDescriptor(C,'prototype').writable);",
        "try{console.log(C)}catch(e){console.log(e.name)} class C{}",
        "let x=3; class C{x=x;constructor(x){}} console.log(new C(7).x);",
        "class C {static x=()=>C; m(){return C;} } let old=C; C=7; console.log(old.x()===old,new old().m()===old);",
        "class C { ['__proto__']=7; } console.log(Object.getOwnPropertyDescriptor(new C,'__proto__').value);",
        "class C { 'a-b'=3; empty; f=()=>0; } const c=new C; console.log(c['a-b'],c.empty,c.f.name);",
        "class B{} class C extends B {constructor(){try{console.log(this)}catch(e){console.log(e.name)}super();}} new C;",
        "class B{} class C extends B {constructor(){super();return;}} console.log(new C instanceof C);",
        "class C extends null{} try{new C}catch(e){console.log(e.name)} console.log(Object.getPrototypeOf(C.prototype));",
        "function f(Object){class C{x=3;} return new C().x;} console.log(f(null));",
        "class C{constructor(){this.name=new.target.name}} class D extends C{} console.log(new D().name);",
        "let out=[]; class C{static x=(out.push('x'),1);static m(){return 2}static y=(out.push(this.m()),3)} console.log(out.join(','));",
        "class B{m(){return 3}} class C extends B{m(){return super.m()+1}} console.log(new C().m());",
        "let calls=[];let B=new Proxy(class {},{get(t,k,r){calls.push(String(k));return Reflect.get(t,k,r)}});class C extends B{} console.log(calls.join(','));",
        "class C{#x=3;m(){return this.#x}} console.log(new C().m());",
    ] {
        let out = transform(src, true, false);
        assert_eq!(node_output(src), node_output(&out), "{src} => {out}");
    }
}

#[test]
fn regex_shadowing_and_replacement_stay_native() {
    if node_path().is_none() {
        return;
    }
    for src in [
        "function f(RegExp){return /x/.test('x');} console.log(f(null));",
        "globalThis.RegExp=function(){throw 1;}; console.log(/x/.test('x'));",
        "globalThis['RegExp']=null; console.log(/x/.test('x'));",
    ] {
        let out = transform(src, false, true);
        assert_eq!(node_output(src), node_output(&out), "{src}");
    }
}

#[test]
fn class_method_protection_is_real_and_reported() {
    let src = "class Billing { charge(amount){return amount*3+1;} } console.log(new Billing().charge(7));";
    let cfg = ResolvedConfig::try_from(ConfigFlags {
        preset: Some(Intensity::Minify),
        seed: Some(7),
        virtualize_program: true,
        virtualize_desugar_class: true,
        require_virtualized: Some("charge".into()),
        ..Default::default()
    })
    .unwrap();
    let (out, notes) = mangler_js::process(src, &ParseOpts::default(), &cfg).unwrap();
    assert!(out.contains("class"), "native class envelope must remain");
    assert!(
        notes
            .iter()
            .any(|note| note.message == "charge: virtualized"),
        "{notes:?}"
    );
    if node_path().is_some() {
        assert_eq!(node_output(src), node_output(&out));
    }
}

#[test]
fn named_class_targets_keep_strict_and_sloppy_interpreters_distinct() {
    let src = "function sloppy(){return this===globalThis} class C{strict(){return this===undefined} f=function field(){return this===undefined}} let strict=C.prototype.strict,field=new C().f; console.log(sloppy(),strict(),field());";
    let cfg = ResolvedConfig::try_from(ConfigFlags {
        preset: Some(Intensity::Minify),
        seed: Some(11),
        virtualize: Some("*".into()),
        ..Default::default()
    })
    .unwrap();
    let (out, notes) = mangler_js::process(src, &ParseOpts::default(), &cfg).unwrap();
    for name in ["sloppy", "strict", "field"] {
        assert!(
            notes
                .iter()
                .any(|n| n.message == format!("{name}: virtualized")),
            "{notes:?}"
        );
    }
    if node_path().is_some() {
        assert_eq!(node_output(src), node_output(&out));
    }
}

#[test]
fn class_expression_heritage_retains_inner_name_tdz() {
    if node_path().is_none() {
        return;
    }
    for src in [
        "let C='outer'; try{let X=class C extends C{}}catch(e){console.log(e.name)}",
        "let C=class {};try{let X=class C extends (()=>C)(){}}catch(e){console.log(e.name)}",
        "let C=class {};try{let X=class C extends (function(C){return C})(C){}}catch(e){console.log(e.name)}",
        "let C=class {};let X=class C extends (function(C){return C})(class {}){};console.log(new X instanceof X);",
        "let C=class {};let X=class C extends (()=>{const C=class {};return C;})(){};console.log(new X instanceof X);",
        "let C=class {};let X=class C extends (class C {}){};console.log(new X instanceof X);",
    ] {
        let out = transform(src, true, false);
        assert_eq!(node_output(src), node_output(&out), "{src} => {out}");
    }
}

#[test]
fn methods_requiring_native_home_objects_cannot_claim_required_protection() {
    let src = "class B{charge(){return 1}} class C extends B{charge(){return super.charge()+1}}";
    let cfg = ResolvedConfig::try_from(ConfigFlags {
        preset: Some(Intensity::Minify),
        seed: Some(3),
        virtualize_program: true,
        virtualize_desugar_class: true,
        require_virtualized: Some("charge".into()),
        ..Default::default()
    })
    .unwrap();
    let error = mangler_js::process(src, &ParseOpts::default(), &cfg).unwrap_err();
    assert!(error.to_string().contains("charge: native"), "{error}");
}
