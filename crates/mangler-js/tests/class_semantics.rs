//! Native class invariants must survive protection of their eligible method bodies.
use mangler_config::{ConfigFlags, Intensity, ResolvedConfig};
use mangler_jsast::ParseOpts;
use mangler_testkit::cross_engine::{node_path, run_node_source};
use std::time::Duration;

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
    let out = run_node_source(
        &node_path().expect("test checked configured Node"),
        src,
        Duration::from_secs(5),
    )
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
        "class C {constructor(C){this.x=C}m(C){return C}static immutable(){try{C=1}catch(e){return e.name}}}let old=C;C=7;console.log(new old(3).x,new old().m(4),old.immutable());",
        "class C {static f(){let C=3;return ()=>C}static nested(){return class C{static self(){return C}}}}let old=C;C=7;let inner=old.nested();console.log(old.f()(),inner.self()===inner);",
        "let order=[];let key={[Symbol.toPrimitive](){order.push('key');return 'm'}};class C extends (order.push('base'),class {}){[key](){return 3}static x=(order.push('field'),4)}console.log(order.join(','),new C().m());",
        "function make(){this.key='x';class C{[this.key]=3}return new C}console.log(new make().x);",
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
fn class_initializer_eval_has_implicit_function_context() {
    if node_path().is_none() {
        return;
    }
    let source = "let log=[];class C{x=eval('new.target');f=()=>eval('new.target');static x=eval('new.target');static{log.push(eval('new.target')===undefined)}}let c=new C;log.push(c.x===undefined,c.f()===undefined,C.x===undefined);try{class D{[eval('new.target')](){}}}catch(e){log.push(e.name)}console.log(JSON.stringify(log));";
    let config = ResolvedConfig::try_from(ConfigFlags {
        preset: Some(Intensity::Minify),
        seed: Some(7),
        virtualize: Some("*".into()),
        require_virtualized: Some("*".into()),
        ..Default::default()
    })
    .unwrap();
    let output = mangler_js::process(source, &ParseOpts::default(), &config)
        .unwrap()
        .0;
    assert_eq!(node_output(source), node_output(&output));
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
fn suspension_methods_keep_calltime_parameters_identity_and_private_brands() {
    check_suspension_methods(false);
}

#[test]
fn suspension_methods_compile_default_eval_in_bytecode() {
    check_suspension_methods(true);
}

fn check_suspension_methods(default_eval: bool) {
    if node_path().is_none() {
        return;
    }
    for (source, target) in [
        (
            "let log=[];class C{async pay(a=(log.push('default'),eval('3'))){return [a,arguments.length]}}let c=new C,p=c.pay();log.push('call');console.log(C.prototype.pay.name,C.prototype.pay.length,C.prototype.pay.constructor.name,Object.hasOwn(C.prototype.pay,'prototype'));p.then(v=>console.log(JSON.stringify([log,v])));",
            "pay",
        ),
        (
            "class C{async pay(a=(()=>{throw Error('default')})()){return a}}let sync=false,p;try{p=new C().pay()}catch(e){sync=true}console.log(sync);p.catch(e=>console.log(e.message));",
            "pay",
        ),
        (
            "let log=[];class C{*pay(a=(log.push('default'),eval('3'))){log.push('body');yield a;return 7}}let f=C.prototype.pay,it=new C().pay();console.log(JSON.stringify(log),f.name,f.length,f.constructor.name,Object.getPrototypeOf(it)===f.prototype);console.log(JSON.stringify(it.next()),JSON.stringify(log),JSON.stringify(it.next()));let p={};f.prototype=p;console.log(Object.getPrototypeOf(new C().pay(4))===p);",
            "pay",
        ),
        (
            "let log=[];class C{async *pay(a=(log.push('default'),eval('3'))){log.push('body');yield a}}let f=C.prototype.pay,it=new C().pay();console.log(JSON.stringify(log),f.name,f.length,f.constructor.name,Object.getPrototypeOf(it)===f.prototype,Object.keys(it).length);it.next().then(v=>console.log(JSON.stringify(v),JSON.stringify(log)));",
            "pay",
        ),
        (
            "class C{async #pay(a=eval('3')){return [a,this]}get(){return this.#pay}replace(){try{this.#pay=0}catch(e){return e.name}}has(x){return #pay in x}}let a=new C,b=new C,f=a.get();console.log(f===b.get(),f.name,f.length,f.constructor.name,a.replace(),a.has(b),a.has({}));f.call(7).then(v=>console.log(JSON.stringify(v)));",
            "#pay",
        ),
        (
            "class B{m(){return this.value}}class C extends B{value=7;*#pay(a=eval('3')){yield super.m()+a}get(){return this.#pay}}let a=new C,b=new C,f=a.get();console.log(f===b.get(),f.name,f.constructor.name,JSON.stringify(f.call(a).next()));",
            "#pay",
        ),
        (
            "class C{static async #pay(a=eval('3')){return [a,this]}static get(){return this.#pay}static has(value){return #pay in value}}class D extends C{}let f=C.get();console.log(f===C.get(),f.name,f.constructor.name,C.has(C),C.has(D));try{C.get.call(D)}catch(e){console.log(e.name)}f.call(7).then(v=>console.log(JSON.stringify(v)));",
            "#pay",
        ),
        (
            "let log=[],key={toString(){log.push('key');return 'pay'}};class C{a(){}async [key](a=3){return a}get x(){return 1}set x(v){}b(){}static view=[Object.getOwnPropertyNames(this.prototype),Object.getOwnPropertySymbols(this.prototype).length]}console.log(JSON.stringify([log,C.view,Object.getOwnPropertyDescriptor(C.prototype,'x').get.name,Object.getOwnPropertyDescriptor(C.prototype,'x').set.name]));new C().pay().then(v=>console.log(v));",
            "<computed>",
        ),
    ] {
        let source = if default_eval {
            source.to_string()
        } else {
            source.replace("eval('3')", "3")
        };
        let config = ResolvedConfig::try_from(ConfigFlags {
            preset: Some(Intensity::Minify),
            seed: Some(7),
            virtualize: Some(target.into()),
            require_virtualized: Some(target.into()),
            ..Default::default()
        })
        .unwrap();
        let (output, _) = mangler_js::process(&source, &ParseOpts::default(), &config).unwrap();
        assert_eq!(
            node_output(&source),
            node_output(&output),
            "{source} => {output}"
        );
    }
}

#[test]
fn nested_accessors_keep_own_receiver_and_outer_computed_keys() {
    if node_path().is_none() {
        return;
    }
    for source in [
        "function pay(){let seen;const obj={get x(){seen=this;return 1}};obj.x;return seen===obj}console.log(pay());",
        "function pay(){let seen,value;const obj={tag:7,set x({a=this.tag}){seen=this;value=a}};obj.x={};return [seen===obj,value]}console.log(JSON.stringify(pay()));",
        "function pay(){const obj={tag:9,get [this.key](){return this.tag},set [this.key](v){this.tag=v}};obj.x=7;return obj.x}console.log(pay.call({key:'x'}));",
    ] {
        let config = ResolvedConfig::try_from(ConfigFlags {
            preset: Some(Intensity::Minify),
            seed: Some(7),
            virtualize: Some("pay".into()),
            require_virtualized: Some("pay".into()),
            ..Default::default()
        })
        .unwrap();
        let output = mangler_js::process(source, &ParseOpts::default(), &config)
            .unwrap()
            .0;
        assert_eq!(node_output(source), node_output(&output), "{source}");
    }
}

#[test]
fn direct_eval_retains_native_class_lexical_context() {
    if node_path().is_none() {
        return;
    }
    for (label, source, target) in [
        (
            "method_private_read",
            "class C{#x=7;pay(){return eval('this.#x')}}globalThis.__out=new C().pay();console.log(JSON.stringify(globalThis.__out));",
            "pay",
        ),
        (
            "method_private_write",
            "class C{#x=7;pay(){eval('this.#x+=3');return this.#x}}globalThis.__out=new C().pay();console.log(JSON.stringify(globalThis.__out));",
            "pay",
        ),
        (
            "method_private_in",
            "class C{#x=7;pay(){return eval('#x in this')}}globalThis.__out=new C().pay();console.log(JSON.stringify(globalThis.__out));",
            "pay",
        ),
        (
            "method_super_get",
            "class B{get x(){return this.y}}class C extends B{y=7;pay(){return eval('super.x')}}globalThis.__out=new C().pay();console.log(JSON.stringify(globalThis.__out));",
            "pay",
        ),
        (
            "method_super_call",
            "class B{m(x){return this.y+x}}class C extends B{y=7;pay(){return eval('super.m(2)')}}globalThis.__out=new C().pay();console.log(JSON.stringify(globalThis.__out));",
            "pay",
        ),
        (
            "default_private",
            "class C{#x=7;pay(x=eval('this.#x')){return x}}globalThis.__out=new C().pay();console.log(JSON.stringify(globalThis.__out));",
            "pay",
        ),
        (
            "field_private",
            "class C{#x=7;pay=eval('this.#x')}globalThis.__out=new C().pay;console.log(JSON.stringify(globalThis.__out));",
            "pay",
        ),
        (
            "static_block_private",
            "class C{static #x=7;static{globalThis.__out=eval('this.#x')}};console.log(JSON.stringify(globalThis.__out));",
            "<static>",
        ),
        (
            "eval_nested_class_outer_private",
            "class C{#x=7;pay(){return new (eval('(class D{get(c){return c.#x}})'))().get(this)}}globalThis.__out=new C().pay();console.log(JSON.stringify(globalThis.__out));",
            "pay",
        ),
        (
            "eval_nested_class_external_private_operations",
            "class C{#x=7;pay(){let D=eval('(class D{#y=3;get(c){c.#x+=2;return [c.#x+this.#y,#x in c,()=>class E{get(){return c.#x}}]}})');let [sum,brand,make]=new D().get(this);return [sum,brand,new (make())().get()]}}globalThis.__out=new C().pay();console.log(JSON.stringify(globalThis.__out));",
            "pay",
        ),
        (
            "eval_nested_class_external_private_escaping_arrow",
            "class C{#x=7;pay(){return new (eval('(class D{get(c){return ()=>c.#x}})'))().get(this)()}}globalThis.__out=new C().pay();console.log(JSON.stringify(globalThis.__out));",
            "pay",
        ),
        (
            "eval_nested_class_shadow_private",
            "class C{#x=3;pay(){return new (eval('(class D{#x=7;get(){return this.#x}})'))().get()}}globalThis.__out=new C().pay();console.log(JSON.stringify(globalThis.__out));",
            "pay",
        ),
        (
            "constructor_eval_this_after_super",
            "class B{constructor(x){this.x=x}}class C extends B{constructor(){eval('super(7);this.x+=1')}}globalThis.__out=new C().x;console.log(JSON.stringify(globalThis.__out));",
            "constructor",
        ),
        (
            "field_eval_arguments",
            "class C{pay=(()=>{try{return eval('typeof arguments')}catch(e){return e.name}})()}globalThis.__out=new C().pay;console.log(JSON.stringify(globalThis.__out));",
            "pay",
        ),
        (
            "method_eval_invalid_private",
            "class C{pay(){try{return eval('this.#missing')}catch(e){return e.name}}}globalThis.__out=new C().pay();console.log(JSON.stringify(globalThis.__out));",
            "pay",
        ),
        (
            "private_proto_name",
            "class C{#__proto__=7;pay(){return eval('this.#__proto__')}}console.log(new C().pay());",
            "pay",
        ),
    ] {
        let config = ResolvedConfig::try_from(ConfigFlags {
            preset: Some(Intensity::Minify),
            seed: Some(7),
            virtualize: Some(target.into()),
            require_virtualized: Some(target.into()),
            ..Default::default()
        })
        .unwrap();
        let output = mangler_js::process(source, &ParseOpts::default(), &config)
            .unwrap()
            .0;
        assert_eq!(
            node_output(source),
            node_output(&output),
            "{label}: {source}"
        );
    }
}

#[test]
fn class_async_generators_keep_native_iterator_keys_and_brands() {
    if node_path().is_none() {
        return;
    }
    for (source, target) in [
        (
            "class C{async *pay(){yield 7}}const it=new C().pay(),native=(async function*(){})();const next=Object.getPrototypeOf(Object.getPrototypeOf(native)).next;Reflect.apply(next,it,[]).then(result=>console.log(JSON.stringify([Reflect.ownKeys(it).map(String),Object.getPrototypeOf(it)===C.prototype.pay.prototype,result])));",
            "pay",
        ),
        (
            "class C{async *#pay(){yield 7}get(){return this.#pay}}const c=new C,f=c.get(),it=f.call(c),native=(async function*(){})();const next=Object.getPrototypeOf(Object.getPrototypeOf(native)).next;Reflect.apply(next,it,[]).then(result=>console.log(JSON.stringify([Reflect.ownKeys(it).map(String),Object.getPrototypeOf(it)===f.prototype,result])));",
            "#pay",
        ),
    ] {
        let config = ResolvedConfig::try_from(ConfigFlags {
            preset: Some(Intensity::Minify),
            seed: Some(7),
            virtualize: Some(target.into()),
            require_virtualized: Some(target.into()),
            ..Default::default()
        })
        .unwrap();
        let output = mangler_js::process(source, &ParseOpts::default(), &config)
            .unwrap()
            .0;
        assert_eq!(
            node_output(source),
            node_output(&output),
            "{source} => {output}"
        );
    }
}

#[test]
fn suspension_function_fields_preserve_strict_receivers() {
    if node_path().is_none() {
        return;
    }
    for source in [
        "class C{pay=async function(a=1){return this}}let f=new C().pay;console.log(f.name,f.length,f.constructor.name);f.call(7).then(v=>console.log(typeof v,v));",
        "class C{pay=async function*(a=1){yield this}}let f=new C().pay;console.log(f.name,f.length,f.constructor.name);f.call(7).next().then(v=>console.log(typeof v.value,v.value));",
        "class C{pay=function*(a=1){yield this}}let f=new C().pay;console.log(f.name,f.length,f.constructor.name);let v=f.call(7).next().value;console.log(typeof v,v);",
    ] {
        let config = ResolvedConfig::try_from(ConfigFlags {
            preset: Some(Intensity::Minify),
            seed: Some(7),
            virtualize: Some("pay".into()),
            require_virtualized: Some("pay".into()),
            ..Default::default()
        })
        .unwrap();
        let output = mangler_js::process(source, &ParseOpts::default(), &config)
            .unwrap()
            .0;
        assert_eq!(
            node_output(source),
            node_output(&output),
            "{source} => {output}"
        );
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
fn methods_requiring_native_home_objects_are_protected() {
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
    let (_, notes) = mangler_js::process(src, &ParseOpts::default(), &cfg).unwrap();
    assert_eq!(
        notes
            .iter()
            .filter(|n| n.message == "charge: virtualized")
            .count(),
        2
    );
}

#[test]
fn class_lexical_operations_are_protected_and_preserve_order() {
    if node_path().is_none() {
        return;
    }
    for (src, required) in [
        (
            "class B{constructor(){this.x=3}}class C extends B{constructor(x=super()){this.y=x.x}}console.log(new C().y);",
            "constructor",
        ),
        (
            "class C{#x=4;charge(v=this.#x){return v}}console.log(new C().charge());",
            "charge",
        ),
        (
            "class B{get x(){return this.n}}class C extends B{n=3;charge(v=super.x){return v}}console.log(new C().charge());",
            "charge",
        ),
        (
            "class C{constructor(v=new.target.name){this.x=v}}class D extends C{}console.log(new D().x);",
            "constructor",
        ),
        (
            "class C{set charge({x}){arguments[0]={x:9};this.x=x}}let c=new C;c.charge={x:3};console.log(c.x,Object.getOwnPropertyDescriptor(C.prototype,'charge').set.length);",
            "charge",
        ),
        (
            "let o={set charge(v=3){this.x=v;try{arguments.callee}catch(e){this.error=e.name}}};o.charge=undefined;console.log(o.x,o.error,Object.getOwnPropertyDescriptor(o,'charge').set.length);",
            "charge",
        ),
        (
            "class C{#x=2;charge(v){this.#x+=v;return this.#x++}} let c=new C;console.log(c.charge(3),c.charge(1));",
            "charge",
        ),
        (
            "class B{get x(){return this.n}set x(v){this.n=v}charge(v){return this.n+v}}class C extends B{charge(v){super.x=v;super.x++;return super.charge(4)}}console.log(new C().charge(3));",
            "charge",
        ),
        (
            "let a=[];class B{get charge(){a.push('get');return function(v){a.push(this.n);return v}}}class C extends B{n=8;charge(){return super.charge((a.push('arg'),3))}}console.log(new C().charge(),a.join(','));",
            "charge",
        ),
        (
            "class C{#x=4;#charge(v){return this.#x*v}charge(v){return this.#charge(v)}}console.log(new C().charge(3));",
            "*",
        ),
        (
            "class C{#x=1;get charge(){return this.#x+2}set charge(v){this.#x=v*3}}let c=new C;c.charge=4;console.log(c.charge);",
            "charge",
        ),
        (
            "class C{constructor(v){this.x=v*3;this.kind=new.target.name}}class D extends C{}console.log(JSON.stringify(new D(4)));",
            "constructor",
        ),
        (
            "class B{constructor(v){this.x=v}}class C extends B{constructor(v){try{this.x}catch(e){console.log(e.name)}if(v)super(v*2);else super(3);this.x++;}}console.log(new C(2).x,new C(0).x);",
            "constructor",
        ),
        (
            "class B{}class C extends B{constructor(){return {x:7}}}console.log(new C().x);",
            "constructor",
        ),
        (
            "class C{#x=2;charge(o){return #x in o}}let c=new C;console.log(c.charge(c),c.charge({}));",
            "charge",
        ),
        (
            "class C{#f;charge(){return this.#f?.(3)}}console.log(new C().charge());",
            "charge",
        ),
        (
            "class C{#x=3;#f(v){return this.#x+v}charge(o){return o?.#f(4)}}let c=new C;console.log(c.charge(c),c.charge(null));",
            "charge",
        ),
        (
            "class B{charge(v){return this.x+v}}class C extends B{x=8;charge(){return (()=>super.charge(2))()}}console.log(new C().charge());",
            "charge",
        ),
        (
            "const o={x:2,get charge(){return this.x*3},set charge(v){this.x=v+1}};o.charge=4;console.log(o.charge);",
            "charge",
        ),
        (
            "class B{charge(v){return this.x+v}}class C extends B{static #x=2;static charge(){return this.#x}}console.log(C.charge());",
            "charge",
        ),
    ] {
        let cfg = ResolvedConfig::try_from(ConfigFlags {
            preset: Some(Intensity::Minify),
            seed: Some(71),
            virtualize: Some("*".into()),
            require_virtualized: Some(required.into()),
            ..Default::default()
        })
        .unwrap();
        let (out, notes) = mangler_js::process(src, &ParseOpts::default(), &cfg)
            .unwrap_or_else(|e| panic!("{src}: {e}"));
        assert!(
            notes.iter().any(|n| n.message.ends_with(": virtualized")),
            "{notes:?}"
        );
        assert_eq!(node_output(src), node_output(&out), "{src} => {out}");
    }
}

#[test]
fn local_class_envelopes_protect_initializers_and_static_blocks() {
    for src in [
        "function pay(){return (class Foo{}?.name)}console.log(pay());",
        "function pay(){let C;C=class{static label=this.name};return [C.name,C.label]}console.log(JSON.stringify(pay()));",
        "function pay(){let C;C??=class{static label=this.name};return [C.name,C.label]}console.log(JSON.stringify(pay()));",
        "function pay(){const __proto__=class{static label=this.name};return [__proto__.name,__proto__.label]}console.log(JSON.stringify(pay()));",
        "function pay(v){class Account{#x=v*2;charge(n){return this.#x+n}}return new Account().charge(3)}console.log(pay(4));",
        "function pay(v){class Account{static amount=v*3;static{this.amount+=2}charge(){return Account.amount}}return new Account().charge()}console.log(pay(4));",
        "function pay(v){class Base{constructor(x){this.x=x}}class Account extends Base{extra=v+2;constructor(){super(v*2)}charge(){return this.x+this.extra}}return new Account().charge()}console.log(pay(4));",
        "function pay(){class Account{static self=this;self=this;[this.key]=3;charge(){return this.self===this}}return [Account.self===Account,new Account().charge(),new Account().slot]}console.log(JSON.stringify(pay.call({key:'slot'})));",
        "function pay(){let n=2;class Account{charge(){return ++n}}const a=new Account;return [a.charge(),a.charge(),n]}console.log(JSON.stringify(pay()));",
        "function pay(v){let C='outer';try{class C extends (()=>C)(){}}catch(e){return e.name}}console.log(pay());",
        "function pay(){let C=class{static label=this.name;charge(){return C}};let old=C;C=4;return [old.name,old.label,new old().charge()]}console.log(JSON.stringify(pay()));",
        "function pay(){class C{Inner=class{static label=this.name}}let c=new C;return [c.Inner.name,c.Inner.label]}console.log(JSON.stringify(pay()));",
    ] {
        let cfg = ResolvedConfig::try_from(ConfigFlags {
            preset: Some(Intensity::Minify),
            seed: Some(33),
            virtualize: Some("*".into()),
            require_virtualized: Some("*".into()),
            ..Default::default()
        })
        .unwrap();
        let (out, _) = mangler_js::process(src, &ParseOpts::default(), &cfg)
            .unwrap_or_else(|e| panic!("{src}: {e}"));
        if node_path().is_some() {
            assert_eq!(node_output(src), node_output(&out), "{src} => {out}");
        }
    }
}
