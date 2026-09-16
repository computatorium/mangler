//! Super arguments cross one native initialization capability without extra iteration.
use super::*;

fn check(cases: &[(&str, &str, &str)]) {
    use mangler_testkit::cross_engine::{Engine, chrome_path, evaluate_many, node_path};
    let mut programs = Vec::new();
    for (name, source, expected) in cases {
        programs.push(format!("globalThis.__out=JSON.stringify({expected});"));
        programs.push(format!(
            "{source};globalThis.__out=JSON.stringify(globalThis.__out);"
        ));
        for whole in [false, true] {
            let (output, protected) = if whole {
                run_whole_program(source, 4281)
            } else {
                run_virtualize(source, "pay", 4281)
            };
            assert!(protected, "{name} emitted bytecode");
            programs.push(format!(
                "{output};globalThis.__out=JSON.stringify(globalThis.__out);"
            ));
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
        "super constructor semantics requires a real engine"
    );
    for engine in engines {
        let results = evaluate_many(&engine, &sources).unwrap();
        for (case, group) in results.chunks_exact(4).enumerate() {
            for (variant, result) in group.iter().enumerate().skip(1) {
                assert_eq!(
                    result,
                    &group[0],
                    "{} {} variant {variant}",
                    engine.name(),
                    cases[case].0
                );
            }
        }
    }
}

#[test]
fn super_uses_source_iteration_only() {
    check(&[
        (
            "alias_without_bytecode_iteration",
            r###"function pay(){class A{constructor(a,b){this.value=a+b}}class D extends A{constructor(){super(1,2)}}return new D().value}globalThis.__out=pay();"###,
            "3",
        ),
        (
            "direct",
            r###"function pay(){const key=globalThis.Symbol.iterator;const descriptor=Object.getOwnPropertyDescriptor(Array.prototype,key);const original=descriptor.value;let log=[];let result;function poison(){log.push('poison');return original.call([7,8])}class Base{constructor(a,b,c){log.push('base');this.result=[a,b,c===void 0?null:c,arguments.length];this.target=new.target.name}}try{Array.prototype[key]=poison;class Derived extends Base{constructor(){super(1,2)}}const value=new Derived();result=[value.result,value.target||null,value.error||null]}finally{Object.defineProperty(Array.prototype,key,descriptor)}return [result,log]}globalThis.__out=pay();"###,
            r###"[[[1,2,null,2],"Derived",null],["base"]]"###,
        ),
        (
            "source_spread",
            r###"function pay(){const key=globalThis.Symbol.iterator;const descriptor=Object.getOwnPropertyDescriptor(Array.prototype,key);const original=descriptor.value;let log=[];let result;function poison(){log.push('poison');return original.call([7,8])}class Base{constructor(a,b,c){log.push('base');this.result=[a,b,c===void 0?null:c,arguments.length];this.target=new.target.name}}try{Array.prototype[key]=poison;class Derived extends Base{constructor(){super(...[1,2])}}const value=new Derived();result=[value.result,value.target||null,value.error||null]}finally{Object.defineProperty(Array.prototype,key,descriptor)}return [result,log]}globalThis.__out=pay();"###,
            r###"[[[7,8,null,2],"Derived",null],["poison","base"]]"###,
        ),
        (
            "argument_mutates_iterator",
            r###"function pay(){const key=globalThis.Symbol.iterator;const descriptor=Object.getOwnPropertyDescriptor(Array.prototype,key);const original=descriptor.value;let log=[];let result;function poison(){log.push('poison');return original.call([7,8])}class Base{constructor(a,b,c){log.push('base');this.result=[a,b,c===void 0?null:c,arguments.length];this.target=new.target.name}}try{Array.prototype[key]=poison;Array.prototype[key]=original;class Derived extends Base{constructor(){super((Array.prototype[key]=poison,1),2)}}const value=new Derived();result=[value.result,value.target||null,value.error||null]}finally{Object.defineProperty(Array.prototype,key,descriptor)}return [result,log]}globalThis.__out=pay();"###,
            r###"[[[1,2,null,2],"Derived",null],["base"]]"###,
        ),
        (
            "spread_changes_iterator",
            r###"function pay(){const key=globalThis.Symbol.iterator;const descriptor=Object.getOwnPropertyDescriptor(Array.prototype,key);const original=descriptor.value;let log=[];let result;function poison(){log.push('poison');return original.call([7,8])}class Base{constructor(a,b,c){log.push('base');this.result=[a,b,c===void 0?null:c,arguments.length];this.target=new.target.name}}try{Array.prototype[key]=poison;Array.prototype[key]=function(){log.push('first');Array.prototype[key]=poison;return original.call([4,5])};class Derived extends Base{constructor(){super(...[1,2],3)}}const value=new Derived();result=[value.result,value.target||null,value.error||null]}finally{Object.defineProperty(Array.prototype,key,descriptor)}return [result,log]}globalThis.__out=pay();"###,
            r###"[[[4,5,3,3],"Derived",null],["first","base"]]"###,
        ),
        (
            "custom_source_iterator",
            r###"function pay(){const key=globalThis.Symbol.iterator;const descriptor=Object.getOwnPropertyDescriptor(Array.prototype,key);const original=descriptor.value;let log=[];let result;function poison(){log.push('poison');return original.call([7,8])}class Base{constructor(a,b,c){log.push('base');this.result=[a,b,c===void 0?null:c,arguments.length];this.target=new.target.name}}try{Array.prototype[key]=poison;const custom={[key](){log.push('custom');return original.call([4,5])}};class Derived extends Base{constructor(){super(...custom,3)}}const value=new Derived();result=[value.result,value.target||null,value.error||null]}finally{Object.defineProperty(Array.prototype,key,descriptor)}return [result,log]}globalThis.__out=pay();"###,
            r###"[[[4,5,3,3],"Derived",null],["custom","base"]]"###,
        ),
        (
            "iterator_getter_direct",
            r###"function pay(){const key=globalThis.Symbol.iterator;const descriptor=Object.getOwnPropertyDescriptor(Array.prototype,key);const original=descriptor.value;let log=[];let result;function poison(){log.push('poison');return original.call([7,8])}class Base{constructor(a,b,c){log.push('base');this.result=[a,b,c===void 0?null:c,arguments.length];this.target=new.target.name}}try{Array.prototype[key]=poison;Object.defineProperty(Array.prototype,key,{configurable:true,get(){log.push('get');return poison}});class Derived extends Base{constructor(){super(1,2)}}const value=new Derived();result=[value.result,value.target||null,value.error||null]}finally{Object.defineProperty(Array.prototype,key,descriptor)}return [result,log]}globalThis.__out=pay();"###,
            r###"[[[1,2,null,2],"Derived",null],["base"]]"###,
        ),
        (
            "iterator_getter_spread",
            r###"function pay(){const key=globalThis.Symbol.iterator;const descriptor=Object.getOwnPropertyDescriptor(Array.prototype,key);const original=descriptor.value;let log=[];let result;function poison(){log.push('poison');return original.call([7,8])}class Base{constructor(a,b,c){log.push('base');this.result=[a,b,c===void 0?null:c,arguments.length];this.target=new.target.name}}try{Array.prototype[key]=poison;Object.defineProperty(Array.prototype,key,{configurable:true,get(){log.push('get');return poison}});class Derived extends Base{constructor(){super(...[1,2])}}const value=new Derived();result=[value.result,value.target||null,value.error||null]}finally{Object.defineProperty(Array.prototype,key,descriptor)}return [result,log]}globalThis.__out=pay();"###,
            r###"[[[7,8,null,2],"Derived",null],["get","poison","base"]]"###,
        ),
        (
            "lexical_arrow",
            r###"function pay(){const key=globalThis.Symbol.iterator;const descriptor=Object.getOwnPropertyDescriptor(Array.prototype,key);const original=descriptor.value;let log=[];let result;function poison(){log.push('poison');return original.call([7,8])}class Base{constructor(a,b,c){log.push('base');this.result=[a,b,c===void 0?null:c,arguments.length];this.target=new.target.name}}try{Array.prototype[key]=poison;class Derived extends Base{constructor(){(()=>super(1,2))()}}const value=new Derived();result=[value.result,value.target||null,value.error||null]}finally{Object.defineProperty(Array.prototype,key,descriptor)}return [result,log]}globalThis.__out=pay();"###,
            r###"[[[1,2,null,2],"Derived",null],["base"]]"###,
        ),
        (
            "repeated_super",
            r###"function pay(){const key=globalThis.Symbol.iterator;const descriptor=Object.getOwnPropertyDescriptor(Array.prototype,key);const original=descriptor.value;let log=[];let result;function poison(){log.push('poison');return original.call([7,8])}class Base{constructor(a,b,c){log.push('base');this.result=[a,b,c===void 0?null:c,arguments.length];this.target=new.target.name}}try{Array.prototype[key]=poison;class Derived extends Base{constructor(){super(1,2);try{super(3,4)}catch(e){this.error=e.name}}}const value=new Derived();result=[value.result,value.target||null,value.error||null]}finally{Object.defineProperty(Array.prototype,key,descriptor)}return [result,log]}globalThis.__out=pay();"###,
            r###"[[[1,2,null,2],"Derived","ReferenceError"],["base","base"]]"###,
        ),
        (
            "source_iterator_throws",
            r###"function pay(){const key=globalThis.Symbol.iterator;const descriptor=Object.getOwnPropertyDescriptor(Array.prototype,key);const original=descriptor.value;let log=[];let result;function poison(){log.push('poison');return original.call([7,8])}class Base{constructor(a,b,c){log.push('base');this.result=[a,b,c===void 0?null:c,arguments.length];this.target=new.target.name}}try{Array.prototype[key]=poison;Array.prototype[key]=function(){log.push('throw');throw Error('source iterator')};class Derived extends Base{constructor(){try{super(...[1,2])}catch(e){return {result:e.message}}}}const value=new Derived();result=[value.result,value.target||null,value.error||null]}finally{Object.defineProperty(Array.prototype,key,descriptor)}return [result,log]}globalThis.__out=pay();"###,
            r###"[["source iterator",null,null],["throw"]]"###,
        ),
        (
            "symbol_shadow",
            r###"function pay(){const key=globalThis.Symbol.iterator;const descriptor=Object.getOwnPropertyDescriptor(Array.prototype,key);const original=descriptor.value;let log=[];let result;function poison(){log.push('poison');return original.call([7,8])}class Base{constructor(a,b,c){log.push('base');this.result=[a,b,c===void 0?null:c,arguments.length];this.target=new.target.name}}try{Array.prototype[key]=poison;const Symbol={iterator:"wrong"};class Derived extends Base{constructor(){super(1,2)}}const value=new Derived();result=[value.result,value.target||null,value.error||null]}finally{Object.defineProperty(Array.prototype,key,descriptor)}return [result,log]}globalThis.__out=pay();"###,
            r###"[[[1,2,null,2],"Derived",null],["base"]]"###,
        ),
    ]);
}

#[test]
fn eval_super_uses_source_iteration_only() {
    check(&[
        (
            "eval_direct",
            r###"function pay(){const key=globalThis.Symbol.iterator;const descriptor=Object.getOwnPropertyDescriptor(Array.prototype,key);const original=descriptor.value;let log=[];let result;function poison(){log.push('poison');return original.call([7,8])}class Base{constructor(a,b,c){log.push('base');this.result=[a,b,c===void 0?null:c,arguments.length];this.target=new.target.name}}try{Array.prototype[key]=poison;class Derived extends Base{constructor(){eval('super(1,2)')}}const value=new Derived();result=[value.result,value.target||null,value.error||null]}finally{Object.defineProperty(Array.prototype,key,descriptor)}return [result,log]}globalThis.__out=pay();"###,
            r###"[[[1,2,null,2],"Derived",null],["base"]]"###,
        ),
        (
            "eval_spread",
            r###"function pay(){const key=globalThis.Symbol.iterator;const descriptor=Object.getOwnPropertyDescriptor(Array.prototype,key);const original=descriptor.value;let log=[];let result;function poison(){log.push('poison');return original.call([7,8])}class Base{constructor(a,b,c){log.push('base');this.result=[a,b,c===void 0?null:c,arguments.length];this.target=new.target.name}}try{Array.prototype[key]=poison;class Derived extends Base{constructor(){eval('super(...[1,2])')}}const value=new Derived();result=[value.result,value.target||null,value.error||null]}finally{Object.defineProperty(Array.prototype,key,descriptor)}return [result,log]}globalThis.__out=pay();"###,
            r###"[[[7,8,null,2],"Derived",null],["poison","base"]]"###,
        ),
        (
            "eval_arrow",
            r###"function pay(){const key=globalThis.Symbol.iterator;const descriptor=Object.getOwnPropertyDescriptor(Array.prototype,key);const original=descriptor.value;let log=[];let result;function poison(){log.push('poison');return original.call([7,8])}class Base{constructor(a,b,c){log.push('base');this.result=[a,b,c===void 0?null:c,arguments.length];this.target=new.target.name}}try{Array.prototype[key]=poison;class Derived extends Base{constructor(){eval('(()=>super(1,2))()')}}const value=new Derived();result=[value.result,value.target||null,value.error||null]}finally{Object.defineProperty(Array.prototype,key,descriptor)}return [result,log]}globalThis.__out=pay();"###,
            r###"[[[1,2,null,2],"Derived",null],["base"]]"###,
        ),
    ]);
}

#[test]
fn eval_arrows_defer_only_generated_receiver_reads() {
    check(&[
        (
            "source_this_before_super",
            r###"function pay(){class A{constructor(a,b){this.a=a;this.b=b}}class D extends A{constructor(){let log=[];try{eval("(()=>{log.push('body');this})()")}catch(e){log.push(e.name)}eval("(()=>super())()");this.result=log}}return new D().result}globalThis.__out=pay();"###,
            r###"["body","ReferenceError"]"###,
        ),
        (
            "source_default_this_before_super",
            r###"function pay(){class A{constructor(a,b){this.a=a;this.b=b}}class D extends A{constructor(){let log=[];try{eval("((x=(log.push('default'),this))=>super())()")}catch(e){log.push(e.name)}super();this.result=log}}return new D().result}globalThis.__out=pay();"###,
            r###"["default","ReferenceError"]"###,
        ),
        (
            "captured_this_after_super",
            r###"function pay(){class A{constructor(a,b){this.a=a;this.b=b}}class D extends A{constructor(){const read=eval("()=>this");eval("(()=>super())()");this.result=read()===this}}return new D().result}globalThis.__out=pay();"###,
            r###"true"###,
        ),
        (
            "ordinary_function_receiver",
            r###"function pay(){class A{constructor(a,b){this.a=a;this.b=b}}class D extends A{constructor(){const result=eval("(()=>{let own=(function(){return this.v}).call({v:9});super();return own})()");this.result=result}}return new D().result}globalThis.__out=pay();"###,
            r###"9"###,
        ),
        (
            "escaped_super_arrow",
            r###"function pay(){class A{constructor(a,b){this.a=a;this.b=b}}class D extends A{constructor(){const init=eval("()=>super(1,2)");init();this.result=[this.a,this.b]}}return new D().result}globalThis.__out=pay();"###,
            r###"[1,2]"###,
        ),
    ]);
}
