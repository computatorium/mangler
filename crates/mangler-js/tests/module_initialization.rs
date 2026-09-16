//! Linked modules can enter a hoisted export before its module evaluates.
use mangler_config::{ConfigFlags, Intensity, ResolvedConfig};
use mangler_jsast::ParseOpts;
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
struct Input(PathBuf);
impl Drop for Input {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn evaluate(sources: &[String; 2]) -> serde_json::Value {
    let node = mangler_testkit::cross_engine::node_path().expect("module semantics require Node");
    let path = std::env::temp_dir().join(format!(
        "mangler-module-{}-{}.json",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)
        .unwrap();
    let input = Input(path);
    file.write_all(serde_json::to_string(sources).unwrap().as_bytes())
        .unwrap();
    drop(file);
    let output = mangler_testkit::cross_engine::run_bounded(
        Command::new(node)
            .args([
                "--experimental-vm-modules",
                "-e",
                r#"
const vm=require('vm'),fs=require('fs');
(async()=>{
 const sources=JSON.parse(fs.readFileSync(process.argv[1],'utf8'));
 const context=vm.createContext({TextEncoder,TextDecoder,atob,btoa});
 const modules={};
 for(let i=0;i<sources.length;i++){
   const name=String.fromCharCode(97+i);
   modules[name]=new vm.SourceTextModule(sources[i],{context,identifier:name});
 }
 await modules.a.link(name=>modules[name]);
 await modules.a.evaluate();
 process.stdout.write(JSON.stringify(context.__out));
})().catch(error=>{console.error(error);process.exitCode=1});
"#,
            ])
            .arg(&input.0),
        Duration::from_secs(30),
    )
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn imported_hoisted_functions_initialize_only_generated_module_state() {
    for sources in [
        [
            "import {early} from 'b';export default (function(){});globalThis.__out=[early];",
            "import value from 'a';export let early;try{typeof value;early='initialized'}catch(error){early=error.name}",
        ],
        [
            "import {early} from 'b';export default (function*(){});globalThis.__out=[early];",
            "import value from 'a';export let early;try{typeof value;early='initialized'}catch(error){early=error.name}",
        ],
        [
            "import {early} from 'b';export default (async function(){});globalThis.__out=[early];",
            "import value from 'a';export let early;try{typeof value;early='initialized'}catch(error){early=error.name}",
        ],
        [
            "import {early} from 'b';export default (async function*(){});globalThis.__out=[early];",
            "import value from 'a';export let early;try{typeof value;early='initialized'}catch(error){early=error.name}",
        ],
        [
            "import {early} from 'b';export default (class{});globalThis.__out=[early];",
            "import value from 'a';export let early;try{typeof value;early='initialized'}catch(error){early=error.name}",
        ],
        [
            "import pay from 'b';globalThis.__out=[pay(),pay];",
            "export default function pay(){pay=2;return 1}",
        ],
        [
            "import pay from 'b';globalThis.__out=[pay()(),pay];",
            "export default function pay(){return ()=>{pay=3;return 1}}",
        ],
        [
            "import pay from 'b';globalThis.__out=[pay(),pay];",
            "export default function pay(value=(pay=4)){return value}",
        ],
        [
            "import pay from 'b';globalThis.__out=[pay(),typeof pay];",
            "export default (function pay(){try{pay=2}catch(error){return error.name}})",
        ],
        [
            "import pay from 'b';globalThis.__out=[await pay(),pay];",
            "export default async function pay(){pay=2;return 1}",
        ],
        [
            "import pay from 'b';globalThis.__out=[pay().next().value,pay];",
            "export default function* pay(){pay=2;yield 1}",
        ],
        [
            "import{'☿' as a,'' as b,'with space' as c,'1' as d,'a-b' as e,'🚀' as f,'default' as g}from'b';globalThis.__out=[a,b,c,d,e,f,g];",
            "const pay=42;export{pay as '☿',pay as '',pay as 'with space',pay as '1',pay as 'a-b',pay as '🚀',pay as 'default'}",
        ],
        [
            "import{'☿' as a}from'b';import{'x' as b}from'b';import{'☿' as c}from'b';globalThis.__out=[a,b,c];",
            "const pay=42;export{pay as 'x',pay as '☿'}",
        ],
        [
            "import 'b';export function pay(){return 42}",
            "import {pay} from 'a';globalThis.__out=[typeof pay,pay(),pay()];",
        ],
        [
            "import 'b';export function pay(){return 'billing paid!'}",
            "import {pay} from 'a';globalThis.__out=[typeof pay,pay(),pay()];",
        ],
        [
            "import 'b';export function pay(){return value};export let value=42;",
            "import {pay} from 'a';try{pay()}catch(error){globalThis.__out=error.name}",
        ],
        [
            "import {early} from 'b';export function pay(){return pay};globalThis.__out=[early===pay,pay()===pay];",
            "import {pay} from 'a';export const early=pay();",
        ],
        [
            "import {early} from 'b';export default function(){return 42};globalThis.__out=early;",
            "import pay from 'a';export const early=[pay.name,pay()];",
        ],
        [
            "import 'b';export function keep(value='billing paid!'){return value}export function pay(){return 42}",
            "import {keep} from 'a';globalThis.__out=keep();",
        ],
        [
            "import 'b';export function keep({['value']:value='billing paid!'}={}){return value}export function pay(){return 42}",
            "import {keep} from 'a';globalThis.__out=keep();",
        ],
        [
            "import 'b';export function keep(n){var a=n+1;if(a>2)a*=3;return Math.max(a,42)}export function pay(){return 42}",
            "import {keep} from 'a';globalThis.__out=keep(2);",
        ],
        [
            "import 'b';export function keep(){let x=Math.max(1,2);if(x>0)x+=Math.min(3,4);return x}",
            "import {keep} from 'a';globalThis.__out=keep();",
        ],
        [
            "import 'b';export function keep(a=function(){},b=()=>{},c=class{static observed=this.name}){return [a.name,b.name,c.name,c.observed]}",
            "import {keep} from 'a';globalThis.__out=keep();",
        ],
        [
            "import 'b';export function keep({a=function(){},b=()=>{},c=class{static observed=this.name}}={}){return [a.name,b.name,c.name,c.observed]}",
            "import {keep} from 'a';globalThis.__out=keep();",
        ],
        [
            "import 'b';export async function pay(a=42){return await a}",
            "import {pay} from 'a';globalThis.__out=[typeof pay,pay.name,pay.length,Object.getPrototypeOf(pay).constructor.name,Object.hasOwn(pay,'prototype'),await pay()];",
        ],
        [
            "import {early} from 'b';export default async function(a=42){return await a};globalThis.__out=early;",
            "import pay from 'a';export const early=[pay.name,Object.getPrototypeOf(pay).constructor.name,await pay()];",
        ],
        [
            "import 'b';export async function pay(a=(()=>{throw 42})()){return a}",
            "import {pay} from 'a';let log=[];try{let p=pay();log.push(p instanceof Promise);p.catch(error=>log.push(error))}catch(error){log.push('synchronous throw')};await 0;await 0;globalThis.__out=log;",
        ],
        [
            "import 'b';export async function pay(a){return await (async()=>{await 0;return [this,a,arguments[0],new.target]})()}",
            "import {pay} from 'a';globalThis.__out=await pay(42);",
        ],
        [
            "import 'b';export async function pay(){globalThis.log.push('start');await 0;globalThis.log.push('one');await 1;globalThis.log.push('two');return 42}",
            "import {pay} from 'a';globalThis.log=[];let p=pay();Promise.resolve().then(()=>log.push('tick1')).then(()=>log.push('tick2'));p.then(x=>log.push(x));await p;await 0;globalThis.__out=log;",
        ],
        [
            "import 'b';export function* pay(a=(globalThis.log.push('parameter'),42)){globalThis.log.push('body');yield a;return a+1}",
            "import {pay} from 'a';globalThis.log=[];let it=pay();log.push('called',pay.name,pay.length,Object.getPrototypeOf(pay).constructor.name,Object.getPrototypeOf(it)===pay.prototype,it.next().value,it.next().value);globalThis.__out=log;",
        ],
        [
            "import {early} from 'b';export default function*(){yield 42};globalThis.__out=early;",
            "import pay from 'a';export const early=[pay.name,Object.getPrototypeOf(pay).constructor.name,pay().next().value];",
        ],
        [
            "import 'b';export function* pay(a=1,read=()=>a){var a=2;yield [a,read()];a=3;yield [a,read()]}",
            "import {pay} from 'a';let it=pay();globalThis.__out=[it.next().value,it.next().value];",
        ],
        [
            "import 'b';export function* pay(a=(()=>{throw 42})()){yield a}",
            "import {pay} from 'a';let log=[];try{pay();log.push('returned')}catch(error){log.push(error)}globalThis.__out=log;",
        ],
        [
            "import 'b';export function* pay(a=(globalThis.log.push('parameter'),42)){try{globalThis.log.push('body');yield a}finally{globalThis.log.push('finally')}}",
            "import {pay} from 'a';globalThis.log=[];let first=pay();log.push(first.return(3));let second=pay();log.push(second.next(),second.return(4));globalThis.__out=log;",
        ],
        [
            "import 'b';export function* pay(a){a=3;yield [a,arguments[0],this.value];yield arguments.length}",
            "import {pay} from 'a';let it=pay.call({value:42},2,8);globalThis.__out=[it.next().value,it.next().value];",
        ],
        [
            "import 'b';export function* pay(){yield* globalThis.delegate}",
            "import {pay} from 'a';let reads=[];const result={get done(){reads.push('done');return false},get value(){reads.push('value');return 42}};globalThis.delegate={[Symbol.iterator](){return this},next(){return result}};const output=pay().next();globalThis.__out=[output===result,reads];",
        ],
        [
            "import 'b';export async function* pay(a=(globalThis.log.push('parameter'),42)){globalThis.log.push('body');yield a;return a+1}",
            "import {pay} from 'a';globalThis.log=[];let it=pay();log.push('called',pay.name,pay.length,Object.getPrototypeOf(pay).constructor.name,Object.getPrototypeOf(it)===pay.prototype);let p=it.next();log.push('requested');log.push(await p,await it.next());globalThis.__out=log;",
        ],
        [
            "import {early} from 'b';export default async function*(){yield await 42};globalThis.__out=early;",
            "import pay from 'a';export const early=[pay.name,Object.getPrototypeOf(pay).constructor.name,(await pay().next()).value];",
        ],
        [
            "import 'b';export async function* pay(a=1,read=()=>a){var a=2;yield [a,read()];a=3;yield [a,read()]}",
            "import {pay} from 'a';let it=pay();globalThis.__out=[(await it.next()).value,(await it.next()).value];",
        ],
        [
            "import 'b';export async function* pay(a=(()=>{throw 42})()){yield a}",
            "import {pay} from 'a';let log=[];try{pay();log.push('returned')}catch(error){log.push(error)}globalThis.__out=log;",
        ],
        [
            "import 'b';export async function* pay(a=arguments){yield [a===arguments,arguments.length,a[0],this.value];yield (()=>arguments)()===a}",
            "import {pay} from 'a';let it=pay.call({value:42},undefined,8);globalThis.__out=[(await it.next()).value,(await it.next()).value];",
        ],
        [
            "import 'b';export async function* pay(value=this.value,read=()=>[arguments.length,new.target,this.value]){yield [value,read()]}",
            "import {pay} from 'a';globalThis.__out=(await pay.call({value:42},undefined,undefined,8).next()).value;",
        ],
        [
            "import 'b';export async function* pay(a=1,...rest){yield [a,rest,arguments.length]}",
            "import {pay} from 'a';globalThis.__out=(await pay(undefined,8,9).next()).value;",
        ],
        [
            "import 'b';export async function* pay(){try{yield 1}finally{yield 2;await 0;yield 3}}",
            "import {pay} from 'a';let it=pay();globalThis.__out=[await it.next(),await it.return(9),await it.next(),await it.next()];",
        ],
        [
            "import 'b';export async function* pay(){try{try{yield 1}finally{yield 2}}finally{yield 3}}",
            "import {pay} from 'a';let it=pay();globalThis.__out=[await it.next(),await it.return(9),await it.next(),await it.next()];",
        ],
        [
            "import 'b';export async function* pay(){yield* [Promise.resolve(20),22];for await(let value of [3,Promise.resolve(4)])yield value}",
            "import {pay} from 'a';let values=[];for await(let value of pay())values.push(value);globalThis.__out=values;",
        ],
        [
            "import 'b';export async function* pay(){globalThis.log.push('start');yield 1;globalThis.log.push('resume');await 0;yield 2;return 3}",
            "import {pay} from 'a';globalThis.log=[];let it=pay(),a=it.next(),b=it.next();a.then(x=>log.push(['a',x]));b.then(x=>log.push(['b',x]));Promise.resolve().then(()=>log.push('tick1')).then(()=>log.push('tick2'));await b;log.push(await it.next());globalThis.__out=log;",
        ],
    ] {
        let expected = evaluate(&sources.map(str::to_owned));
        for preset in [Intensity::Minify, Intensity::High] {
            for whole in [false, true] {
                let config = ResolvedConfig::try_from(ConfigFlags {
                    preset: Some(preset),
                    seed: Some(42),
                    virtualize: (!whole).then(|| "*".into()),
                    virtualize_program: whole,
                    virtualize_exclude: Some("keep".into()),
                    ..Default::default()
                })
                .unwrap();
                let protected = sources.map(|source| {
                    mangler_js::process(
                        source,
                        &ParseOpts {
                            module: true,
                            ..Default::default()
                        },
                        &config,
                    )
                    .unwrap()
                    .0
                });
                assert_eq!(
                    evaluate(&protected),
                    expected,
                    "{preset:?}, whole={whole}: {sources:?}"
                );
            }
        }
    }
}
