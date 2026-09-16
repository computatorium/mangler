// Required-coverage differential audit. No fixture may pass by staying native.
// Usage: node scripts/check-vm-coverage.cjs --binary target/debug/mangler
//        [--output docs/vm-coverage.json] [--seeds 0,42,4294967295]
const fs = require('node:fs');
const cp = require('node:child_process');
const vm = require('node:vm');
const crypto = require('node:crypto');

const fixtures = [
  ['payment_integer_cents', 'function pay(xs){let n=0;for(const x of xs)n+=x.price*x.quantity;return Math.round(Math.max(0,n-300)*1.0825)}', 'pay([{price:1250,quantity:2},{price:799,quantity:3}])'],
  ['payment_bigint', 'function pay(){const cents=9007199254740993n;return String(cents*3n-7n)}', 'pay()'],
  ['payment_currency_pattern', 'function pay(s){return /^\\d+(?:\\.\\d{2})?$/.test(s)}', '[pay("12.34"),pay("12.345")]'],
  ['payment_async', 'async function pay(){let x=await Promise.resolve(100);return x+30}', 'pay()'],
  ['payment_async_rejection', 'async function pay(){try{await Promise.reject(new Error("declined"))}catch(e){return e.message}finally{globalThis.closed=true}}', 'pay().then(x=>[x,closed])'],
  ['generator', 'function* pay(){yield 1;return 2}', '(()=>{const g=pay();return [g.next(),g.next()]})()'],
  ['generator_finally', 'function* pay(){try{yield 1;yield 2}finally{globalThis.closed=true}}', '(()=>{const g=pay();g.next();return [g.return(7),closed]})()'],
  ['async_generator', 'async function* pay(){yield await Promise.resolve(1);yield 2}', '(async()=>{let out=[];for await(const x of pay())out.push(x);return out})()'],
  ['class_private', 'function pay(){class Account{#n=7;charge(x){this.#n-=x;return this.#n}}return new Account().charge(2)}', 'pay()'],
  ['class_super', 'function pay(){class A{charge(){return 5}}class B extends A{charge(){return super.charge()+2}}return new B().charge()}', 'pay()'],
  ['class_static_block', 'function pay(){class A{static amount;static{this.amount=7}}return A.amount}', 'pay()'],
  ['class_method_target', 'class Account{pay(x){return x*7}}', 'new Account().pay(2)'],
  ['class_super_method_target', 'class A{pay(){return 5}}class B extends A{pay(){return super.pay()+2}}', 'new B().pay()'],
  ['class_private_method_target', 'class Account{#pay(x){return x*7}charge(x){return this.#pay(x)}}', 'new Account().charge(2)', false, '#pay'],
  ['class_getter_target', 'class Account{amount=7;get pay(){return this.amount}}', 'new Account().pay'],
  ['object_method', 'function pay(){const x={n:4,charge(a){return this.n+a}};return x.charge(2)}', 'pay()'],
  ['object_accessor', 'function pay(){let n=1;const x={get value(){return n},set value(x){n=x}};x.value=7;return x.value}', 'pay()'],
  ['object_proto', 'function pay(){let p={x:7};let o={__proto__:p};return [o.x,Object.getPrototypeOf(o)===p,Object.hasOwn(o,"__proto__")]}', 'pay()'],
  ['object_computed_proto', 'function pay(){let p={x:7};let o={["__proto__"]:p};return [Object.hasOwn(o,"__proto__"),o.__proto__===p]}', 'pay()'],
  ['optional_call_receiver', 'function pay(){let x={n:7,f(){return this.n}};return x.f?.()}', 'pay()'],
  ['optional_chain_shortcircuit', 'function pay(){let n=0;let x=null;let r=x?.a[n++].b;return [r,n]}', 'pay()'],
  ['logical_assignment', 'function pay(){let n=0;let o={x:0};o.x ||= ++n;o.x &&= ++n;o.y ??= ++n;return [o.x,o.y,n]}', 'pay()'],
  ['logical_assignment_reference', 'function pay(){let n=0;let o={x:0};function get(){n++;return o}get().x ||= 7;return [n,o.x]}', 'pay()'],
  ['destructure_default', 'function pay({a=7,b:{c}={c:2}}={}){return a+c}', 'pay()'],
  ['destructure_assignment', 'function pay(){let a,b;({x:a,y:b=7}={x:2});return [a,b]}', 'pay()'],
  ['destructure_iterator_close', 'function pay(){let closed=false;let x={[Symbol.iterator](){return {next(){return {value:7,done:false}},return(){closed=true;return {done:true}}}}};let [a]=x;return [a,closed]}', 'pay()'],
  ['destructure_rest_symbols', 'function pay(){let s=Symbol("s");let x={a:1,b:2,[s]:3};let {a,...r}=x;return [a,r.b,r[s]]}', 'pay()'],
  ['array_holes_spread', 'function pay(){let a=[,1,...[2,3],,];return [a.length,0 in a,4 in a,a[2]]}', 'pay()'],
  ['array_spread_iterator', 'function pay(){let x={[Symbol.iterator]:function*(){yield 1;yield 2}};return [...x]}', 'pay()'],
  ['for_of_close', 'function pay(){let closed=false;let x={[Symbol.iterator](){return {next(){return {value:7,done:false}},return(){closed=true;return {done:true}}}}};for(const n of x){break}return closed}', 'pay()'],
  ['for_of_per_iteration_binding', 'function pay(){let fs=[];for(let n of [1,2,3])fs.push(()=>n);return fs.map(f=>f())}', 'pay()'],
  ['for_per_iteration_binding', 'function pay(){let fs=[];for(let n=0;n<3;n++)fs.push(()=>n);return fs.map(f=>f())}', 'pay()'],
  ['for_in_inherited', 'function pay(){let x=Object.create({a:1});x.b=2;let out=[];for(let k in x)out.push(k);return out}', 'pay()'],
  ['labelled_continue', 'function pay(){let n=0;outer:for(let i=0;i<3;i++){for(let j=0;j<3;j++){if(j===1)continue outer;n++}}return n}', 'pay()'],
  ['chained_label_continue_finally', 'function pay(){var out=[];a:b:for(let i=0;i<3;i++){try{out.push(i);continue a;}finally{out.push(9);}}return out}', 'pay()'],
  ['finally_return', 'function pay(){try{return 1}finally{return 2}}', 'pay()'],
  ['finally_break', 'function pay(){let n=0;while(true){try{n=1;break}finally{n+=2}}return n}', 'pay()'],
  ['finally_throw', 'function pay(){try{try{throw 1}finally{throw 2}}catch(e){return e}}', 'pay()'],
  ['catch_destructure', 'function pay(){try{throw {n:7}}catch({n}){return n}}', 'pay()'],
  ['switch_tdz', 'function pay(){try{switch(1){case 1:return typeof x;case 2:let x=3}}catch(e){return e.name}}', 'pay()'],
  ['block_tdz', 'function pay(){let x=1;try{{return x;let x=2}}catch(e){return e.name}}', 'pay()'],
  ['const_assignment', 'function pay(){const x=1;try{x=2}catch(e){return [x,e.name]}}', 'pay()'],
  ['closure_mutation', 'function pay(){let n=1;const f=()=>++n;return [f(),f(),n]}', 'pay()'],
  ['closure_recursion', 'function pay(){const f=function sum(n){return n? n+sum(n-1):0};return f(4)}', 'pay()'],
  ['annex_b_if_bindings', 'function pay(){var before=typeof f;if(true)function f(){return 7}var first=f;if(false)function f(){return 2}return [before,first===f,f()]}', 'pay()'],
  ['annex_b_eval_alias', 'function pay(){var initial,current,outer;eval("{function f(){initial=f;f=123;current=f;return 7}}outer=f;f()");return [initial(),current,outer()]}', 'pay()'],
  ['annex_b_eval_catch_pattern', 'function pay(){return eval("try{throw {}}catch({f}){{function f(){}}}try{f;false}catch(e){e.name}")}', 'pay()'],
  ['closure_function_name', 'function pay(){const charge=function(){};return [charge.name,charge.length]}', 'pay()'],
  ['arguments_object', 'function pay(a){return [Object.prototype.toString.call(arguments),Array.isArray(arguments),Object.getOwnPropertyDescriptor(arguments,"length").enumerable]}', 'pay(7)'],
  ['arguments_alias', 'function pay(a){arguments[0]=2;return a}', 'pay(7)'],
  ['arguments_alias_indirect', 'function pay(a){let args=arguments;args[0]=2;return a}', 'pay(7)'],
  ['arguments_callee_indirect', 'function pay(){let args=arguments;return args.callee===pay}', 'pay()'],
  ['arguments_arrow', 'function pay(x){return (()=>arguments[0])()}', 'pay(7)'],
  ['strict_this', 'function pay(){"use strict";return this===undefined}', 'pay()'],
  ['sloppy_this', 'function pay(){return this===globalThis}', 'pay()'],
  ['new_target', 'function pay(){return {constructing:new.target===pay}}', '[pay(),new pay()]'],
  ['typeof_missing', 'function pay(){return typeof __missing_payment_variable__}', 'pay()'],
  ['delete_property', 'function pay(){let x={a:1};return [delete x.a,"a" in x]}', 'pay()'],
  ['strict_frozen_assignment', 'function pay(){"use strict";let o=Object.freeze({x:1});try{o.x=2}catch(e){return e.name}}', 'pay()'],
  ['update_bigint', 'function pay(n){let old=n++;return [String(old),String(n)]}', 'pay(2n)'],
  ['update_coercion_order', 'function pay(){let log=[];let x={get n(){log.push("get");return {valueOf(){log.push("value");return 2}}},set n(v){log.push(v)}};let old=x.n++;return [old,log]}', 'pay()'],
  ['template_tag_identity', 'let last;function tag(x){let r=last===x;last=x;return r}function pay(){return tag`amount`}', '[pay(),pay()]'],
  ['template_surrogate', 'function pay(){return `\\uD800`.charCodeAt(0)}', 'pay()'],
  ['string_surrogate', 'function pay(){return "\\uD800".charCodeAt(0)}', 'pay()'],
  ['direct_eval', 'function pay(x){return eval("x+2")}', 'pay(5)'],
  ['with_scope', 'function pay(x){with(x){return amount+2}}', 'pay({amount:5})'],
  ['global_capture_live', 'let amount=1;function pay(){return amount}let old=pay();amount=2;', '[old,pay()]'],
  ['global_capture_write', 'let amount=1;function pay(){amount++;return amount}', '[pay(),amount]'],
  ['symbol_property_coercion', 'function pay(){let n=0;let k={[Symbol.toPrimitive](){n++;return "a"}};let x={};x[k]=7;return [x.a,n]}', 'pay()'],
  ['call_getter_before_argument', 'let log=[];let o={get f(){log.push("get");return function(v){return log}}};function pay(){return o.f(log.push("arg"))}', 'pay()'],
  ['call_receiver_primitive', 'let strict=function(){"use strict";return typeof this};Number.prototype.f=strict;function pay(){return (7).f()}', 'pay()'],
  ['assignment_key_before_rhs', 'let log=[];let k={[Symbol.toPrimitive](){log.push("key");return "x"}};function pay(){let o={};o[k]=(log.push("rhs"),7);return log}', 'pay()'],
  ['compound_key_once', 'let log=[];let k={[Symbol.toPrimitive](){log.push("key");return "x"}};function pay(){let o={x:1};o[k]+=(log.push("rhs"),2);return [log,o.x]}', 'pay()'],
  ['optional_key_not_evaluated', 'function pay(){let n=0;let o=null;return [o?.[n++],n]}', 'pay()'],
  ['optional_call_not_evaluated', 'function pay(){let n=0;let f=null;return [f?.(n++),n]}', 'pay()'],
  ['delete_optional', 'function pay(){let n=0;let o=null;return [delete o?.[n++],n]}', 'pay()'],
  ['destructure_assignment_value', 'function pay(){let x;let a=[7];return [( [x]=a )===a,x]}', 'pay()'],
  ['object_rest_null_throws', 'function pay(){try{let {...x}=null;return 1}catch(e){return e.name}}', 'pay()'],
  ['iterator_next_throw_no_close', 'let log=[];let xs={[Symbol.iterator](){return {next(){throw 1},return(){log.push("close");return {done:true}}}}};function pay(){try{for(const x of xs){}}catch(e){return [e,log]}}', 'pay()'],
  ['iterator_body_throw_closes', 'let log=[];let xs={[Symbol.iterator](){return {next(){return {value:1,done:false}},return(){log.push("close");return {done:true}}}}};function pay(){try{for(const x of xs){throw 7}}catch(e){return [e,log]}}', 'pay()'],
  ['param_default_tdz', 'function pay(x=y,y=2){return x}', '(()=>{try{return pay()}catch(e){return e.name}})()'],
  ['param_default_outer_scope', 'let x=7;function pay(a=x){var x=2;return [a,x]}', 'pay()'],
  ['param_default_closure', 'function pay(x=1,f=()=>x){var x=2;return [f(),x]}', 'pay()'],
  ['function_hoist_block', 'function pay(){"use strict";let f=()=>1;{return [f(),g()];function g(){return 2}}}', 'pay()'],
  ['catch_shadow_closure', 'function pay(){let f;let x=1;try{throw 2}catch(x){f=()=>x}return [x,f()]}', 'pay()'],
  ['return_finally_value', 'function pay(){let x=1;try{return x}finally{x=2}}', 'pay()'],
  ['switch_discriminant_scope', 'function pay(){let x=1;switch(x){case 1:let x=2;return x}}', 'pay()'],
  ['for_break_finally_override', 'function pay(){let n=0;outer:for(let i=0;i<3;i++){try{break outer}finally{if(i<2){n++;continue outer}}}return n}', 'pay()'],
  ['object_spread_getter_order', 'let log=[];let o={get a(){log.push("a");return 1},get b(){log.push("b");return 2}};function pay(){let x={...o,c:(log.push("c"),3)};return [x,log]}', 'pay()'],
  ['numeric_special_values', 'function pay(x){return [x,1/x,Object.is(x,-0),Number.isNaN(x)]}', '[pay(-0),pay(NaN),pay(Infinity),pay(-Infinity)]'],
  ['regexp_lastindex_fresh', 'function pay(){let r=/a/g;return [r.test("a"),r.lastIndex]}', '[pay(),pay()]'],
  ['regexp_shadowed_constructor', 'let RegExp=()=>{throw 1};function pay(){return /a/.test("a")}', 'pay()'],
  ['bigint_type_error', 'function pay(){try{return 1n+1}catch(e){return e.name}}', 'pay()'],
  ['tagged_invalid_escape', 'let tag=(s)=>[s[0],s.raw[0]];function pay(){return tag`\\unicode`}', 'pay()'],
  ['iterator_done_getter_throw_no_close', 'let log=[];let xs={[Symbol.iterator](){return {next(){return {get done(){throw 1}}},return(){log.push("close");return {done:true}}}}};function pay(){try{for(const x of xs){}}catch(e){return [e,log]}}', 'pay()'],
  ['iterator_value_getter_throw_no_close', 'let log=[];let xs={[Symbol.iterator](){return {next(){return {done:false,get value(){throw 1}}},return(){log.push("close");return {done:true}}}}};function pay(){try{for(const x of xs){}}catch(e){return [e,log]}}', 'pay()'],
  ['iterator_return_primitive', 'let xs={[Symbol.iterator](){return {next(){return {value:1,done:false}},return(){return 1}}}};function pay(){try{for(const x of xs){break}}catch(e){return e.name}}', 'pay()'],
  ['iterator_body_error_wins_close_error', 'let xs={[Symbol.iterator](){return {next(){return {value:1,done:false}},return(){throw 2}}}};function pay(){try{for(const x of xs){throw 1}}catch(e){return e}}', 'pay()'],
  ['module_import', 'import {amount} from "data:text/javascript,export const amount=7";export function pay(){return amount+2}', 'pay()', true],
  ['module_live_import', 'import {amount,charge} from "data:text/javascript,export let amount=7;export function charge(){amount--}";export function pay(){charge();return amount}', '[pay(),pay()]', true],
  ['module_top_level_await', 'const amount=await Promise.resolve(7);export function pay(){return amount+2}', 'pay()', true],
  ['module_dynamic_import', 'export function pay(){return import("data:text/javascript,export const amount=7").then(m=>m.amount)}', 'pay()', true],
  ['module_import_meta', 'export function pay(){return [typeof import.meta.url,import.meta.url.startsWith("data:")]}', 'pay()', true],
  ['mutated_string_charcode', 'String.prototype.charCodeAt=function(){throw new Error("charcode override")};function pay(){return 7}', 'pay()'],
  ['mutated_array_methods', 'Array.prototype.push=Array.prototype.pop=Array.prototype.slice=Array.prototype.splice=function(){throw new Error("array override")};function pay(){let x=1;for(let i=0;i<3;i++)x+=i;return x}', 'pay()'],
  ['mutated_reflect_apply', 'Reflect.apply=function(){return 91};function helper(x){return x+2}function pay(){return [helper(5),Reflect.apply(helper,null,[5])]}', 'pay()'],
  ['mutated_object_defineproperty', 'Object.defineProperty=function(){throw new Error("define override")};function pay(){let x={a:7};return x.a}', 'pay()'],
  ['mutated_string_fromcharcode', 'String.fromCharCode=function(){return "patched"};function pay(){return ["paid",String.fromCharCode(65)]}', 'pay()'],
  ['mutated_regexp_global', 'globalThis.RegExp=function(){throw new Error("regexp override")};function pay(){return /paid/.test("paid")}', 'pay()'],
  ['mutated_object_prototype_marker', 'Object.prototype.d=1;function pay(){return "paid"}', 'pay()'],
  ['hoisted_intrinsic_function', 'function Object(){}function pay(){return 7}', 'pay()'],
  ['hoisted_reflect_function', 'function Reflect(){return 7}function pay(){let C=function(x){this.x=x};return [Reflect(),new C(9).x]}', 'pay()'],
  ['hoisted_symbol_function', 'function Symbol(){return 7}function pay(){let n=0;for(let x of [1,2])n+=x;return [Symbol(),n]}', 'pay()'],
  ['shadowed_global_and_intrinsic', 'var output;{let globalThis={};let Object={};function pay(){return 7}output=pay()}', 'output'],
  ['array_prototype_index_getter', 'Object.defineProperty(Array.prototype,"0",{get(){return 99},configurable:true});function pay(){return 7}', 'pay()'],
  ['object_prototype_index_getter', 'Object.defineProperty(Object.prototype,"0",{get(){return 99},configurable:true});function pay(){return 7}', 'pay()'],
  ['dynamic_function_constructor', 'function pay(){let f=new Function("x","return x*7");return f(3)}', 'pay()'],
  ['dynamic_async_function_constructor', 'function pay(){const C=(async()=>{}).constructor;return new C("return await 7")()}', 'pay()'],
  ['dynamic_generator_function_constructor', 'function pay(){const C=(function*(){}).constructor;return new C("yield 7")().next().value}', 'pay()'],
  ['typed_array_dataview', 'function pay(){let a=new Uint16Array([258,7]);let v=new DataView(a.buffer);return [a[0],v.getUint16(0,true),a.slice(1)[0]]}', 'pay()'],
  ['shared_array_atomics', 'function pay(){let a=new Int32Array(new SharedArrayBuffer(4));Atomics.store(a,0,7);return [Atomics.add(a,0,2),Atomics.load(a,0)]}', 'pay()'],
  ['proxy_call_and_construct', 'function pay(){let f=new Proxy(function(x){this.x=x;return x+1},{apply(t,o,args){return args[0]+7},construct(t,args){return {x:args[0]+2}}});return [f(3),new f(3).x]}', 'pay()'],
  ['map_set_symbol_identity', 'function pay(){let s=Symbol("payment"),m=new Map([[s,7]]),set=new Set([s,s]);return [m.get(s),set.size]}', 'pay()'],
  ['weakmap_function_identity', 'function pay(){let f=()=>7;let m=new WeakMap([[f,9]]);return [m.get(f),f()]}', 'pay()'],
  ['date_intl', 'function pay(){let d=new Date("2020-01-02T00:00:00.000Z");return [d.toISOString(),new Intl.NumberFormat("en-US",{style:"currency",currency:"USD"}).format(12.34)]}', 'pay()'],
  ['regexp_unicode_sets', 'function pay(){return /^[\\p{ASCII}&&\\p{Letter}]+$/v.test("Pay")}', 'pay()'],
  ['regexp_match_indices', 'function pay(){return /a(b)/d.exec("ab").indices[1]}', 'pay()'],
  ['generator_yield_delegate', 'function* pay(){let x=yield* [1,2];return x}', '(()=>{let g=pay();return [g.next(),g.next(),g.next()]})()'],
  ['generator_throw_catch', 'function* pay(){try{yield 1}catch(e){yield e+2}finally{globalThis.closed=true}}', '(()=>{let g=pay();return [g.next(),g.throw(5),g.next(),closed]})()'],
  ['generator_delegate_return', 'function* pay(){try{yield* [1,2]}finally{yield 9}}', '(()=>{let g=pay();return [g.next(),g.return(7),g.next()]})()'],
  ['async_generator_queued_next', 'async function* pay(){yield await 1;yield await 2;return 3}', '(()=>{let g=pay();return Promise.all([g.next(),g.next(),g.next()])})()'],
  ['async_generator_return_finally', 'async function* pay(){try{yield 1}finally{yield await 9}}', '(async()=>{let g=pay();return [await g.next(),await g.return(7),await g.next()]})()'],
  ['await_mutated_promise_resolve', 'Promise.resolve=function(){throw new Error("resolve override")};async function pay(){return await 7}', 'pay()'],
  ['await_replaced_global_promise', 'globalThis.Promise=function(){throw new Error("promise override")};async function pay(){return await 7}', 'pay()'],
  ['await_thenable_once', 'async function pay(){let n=0;let x={get then(){n++;return resolve=>resolve(7)}};return [await x,n]}', 'pay()'],
  ['async_default_rejection', 'async function pay(x=(()=>{throw new Error("default")})()){return x}', '(()=>{try{return pay().catch(e=>e.message)}catch(e){return "sync"}})()'],
  ['async_function_constructor_identity', 'async function pay(){return 7}', '[Object.prototype.toString.call(pay),pay.constructor.name]'],
  ['async_arguments_alias_callee', 'async function pay(a){a=2;return [arguments[0],arguments.callee===pay]}', 'pay(1)'],
  ['async_arguments_write_parameter', 'async function pay(a){arguments[0]=3;return a}', 'pay(1)'],
  ['generator_function_constructor_identity', 'function* pay(){yield 7}', '[Object.prototype.toString.call(pay),pay.constructor.name]'],
  ['private_brand_primitive', 'function pay(){class A{#x;has(v){return #x in v}}let a=new A();let out=[a.has(a),a.has({})];try{a.has(1)}catch(e){out.push(e.name)}return out}', 'pay()'],
  ['private_static_inheritance', 'function pay(){class A{static #x=7;static read(){return this.#x}}class B extends A{}try{return B.read()}catch(e){return e.name}}', 'pay()'],
  ['class_computed_order', 'function pay(){let log=[];class A{[(log.push("key"),"amount")]=(log.push("field"),7);static{log.push("static")}}let a=new A();return [a.amount,log]}', 'pay()'],
  ['class_extends_null', 'function pay(){class A extends null{constructor(){return Object.create(new.target.prototype)}}return Object.getPrototypeOf(A.prototype)===null&&new A() instanceof A}', 'pay()'],
  ['new_target_arrow', 'function pay(){return {constructing:(()=>new.target===pay)()}}', '[pay(),new pay()]'],
  ['duplicate_parameter_alias', 'function pay(a,a){arguments[0]=3;let x=a;arguments[1]=4;return [x,a,arguments[0],arguments[1]]}', 'pay(1,2)'],
  ['with_unscopables', 'let amount=7;function pay(){let o={amount:2,[Symbol.unscopables]:{amount:true}};with(o){return amount}}', 'pay()'],
  ['indirect_eval_global', 'globalThis.amount=7;function pay(){let amount=2;return eval?.("amount")}', 'pay()'],
  ['dynamic_generated_source_visible', 'function pay(){let f=new Function("x","return x*78329+34691");return [f(2),f.toString().includes("78329")]}', 'pay()'],
];

async function evaluate(source, module = false) {
  // vm contexts omit Web APIs which both current Node and browsers provide.
  const context = module ? globalThis : vm.createContext({atob, btoa, TextEncoder, TextDecoder});
  if (module) await import('data:text/javascript,' + encodeURIComponent(source));
  else new vm.Script(source).runInContext(context, {timeout: 3000});
  const value = await context.__out;
  return JSON.stringify(value, (_key, v) => {
    if (typeof v === 'bigint') return {$bigint: String(v)};
    if (v === undefined) return {$undefined: true};
    if (typeof v === 'number' && !Number.isFinite(v)) return {$number: String(v)};
    if (Object.is(v, -0)) return {$number: '-0'};
    return v;
  });
}

if (require.main !== module) {
  module.exports = {fixtures};
} else if (process.argv[2] === '--worker' || process.argv[2] === '--worker-module') {
  process.on('unhandledRejection', error => {
    console.error(`Unhandled rejection: ${error?.name || typeof error}: ${error?.message || String(error)}`);
    process.exitCode = 1;
  });
  (async () => {
    try {
      const source = fs.readFileSync(0, 'utf8');
      const value = await evaluate(source, process.argv[2] === '--worker-module');
      process.stdout.write(JSON.stringify({value: value ?? 'undefined'}));
    } catch (error) {
      process.stdout.write(JSON.stringify({error: error.name, message: error.message}));
    }
  })();
} else {
  let binary, destination, selected, preset = 'minify', coverage = 'selected', mode = 'virtualized', seeds = ['0', '42', '4294967295'];
  const args = process.argv.slice(2);
  while (args.length) {
    const flag = args.shift(), value = args.shift();
    if (!value) throw new Error(`Missing value for ${flag}`);
    if (flag === '--binary') binary = value;
    else if (flag === '--output') destination = value;
    else if (flag === '--seeds') seeds = value.split(',');
    else if (flag === '--fixtures') selected = new Set(value.split(','));
    else if (flag === '--preset') preset = value;
    else if (flag === '--coverage') coverage = value;
    else if (flag === '--mode') mode = value;
    else throw new Error(`Unknown option ${flag}`);
  }
  if (!binary) throw new Error('Supply --binary <mangler>');
  const binaryHash = () => crypto.createHash('sha256').update(fs.readFileSync(binary)).digest('hex');
  const binarySha256 = binaryHash();
  if (!['selected','all'].includes(coverage)) throw new Error('Coverage must be selected or all');
  if (!['virtualized','plain'].includes(mode)) throw new Error('Mode must be virtualized or plain');
  if (selected) {
    const known = new Set(fixtures.map(([name]) => name));
    for (const name of selected) if (!known.has(name)) throw new Error(`Unknown fixture ${name}`);
  }
  function execute(source, module) {
    return JSON.parse(cp.execFileSync(process.execPath, [__filename, module ? '--worker-module' : '--worker'], {
      input: source, encoding: 'utf8', timeout: 5000, maxBuffer: 1024 * 1024,
      stdio: ['pipe', 'pipe', 'pipe'],
    }));
  }
  const results = [];
  for (const [name, declarations, expression, module, required = 'pay'] of fixtures) {
    if (selected && !selected.has(name)) continue;
    const source = `${declarations}\nglobalThis.__out=(${expression});`;
    const expected = execute(source, module);
    if (expected.error) throw new Error(`Invalid native fixture ${name}: ${JSON.stringify(expected)}`);
    for (const seed of seeds) {
      const compiled = cp.spawnSync(binary, ['-', '--lang', 'js', '--preset', preset, ...(mode === 'virtualized' ? ['--require-virtualized', coverage === 'all' ? '*' : required] : []), '--keep-names', '*', '--seed', seed, '--verify'], {
        input: source, encoding: 'utf8', timeout: 15000, maxBuffer: 16 * 1024 * 1024,
      });
      if (compiled.status !== 0) {
        results.push({name, seed, status: 'coverage_failure', detail: compiled.stderr?.trim() || String(compiled.error)});
        continue;
      }
      try {
        const actual = execute(compiled.stdout, module);
        results.push({name, seed, status: JSON.stringify(expected) === JSON.stringify(actual) ? 'pass' : 'semantic_failure', ...(JSON.stringify(expected) === JSON.stringify(actual) ? {} : {expected, actual})});
      } catch (error) {
        results.push({name, seed, status: 'runtime_failure', detail: String(error)});
      }
    }
  }
  const totals = results.reduce((out, row) => {out[row.status] = (out[row.status] || 0) + 1; return out}, {});
  const binaryUnchanged = binaryHash() === binarySha256;
  const report = {node: process.version, binary, binary_sha256: binarySha256, binary_unchanged: binaryUnchanged, preset, mode, coverage: mode === 'plain' ? 'none' : coverage, seeds, fixture_count: new Set(results.map(row => row.name)).size, totals, results};
  const json = JSON.stringify(report, null, 2) + '\n';
  if (destination) fs.writeFileSync(destination, json);
  process.stdout.write(json);
  if (!binaryUnchanged || results.some(row => row.status !== 'pass')) process.exitCode = 1;
}
