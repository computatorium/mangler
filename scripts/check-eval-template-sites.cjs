#!/usr/bin/env node
// Run against a matched build: node --expose-gc scripts/check-eval-template-sites.cjs PATH_TO_PACKAGE
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const directory = process.argv[2];
if (!directory) throw new Error('Provide the directory containing matched mangler-eval.js and mangler-eval.wasm');
const context = vm.createContext({
    TextEncoder, TextDecoder, console,
    bytes: fs.readFileSync(path.join(directory, 'mangler-eval.wasm')),
});
vm.runInContext(fs.readFileSync(path.join(directory, 'mangler-eval.js'), 'utf8'), context);
vm.runInContext('globalThis.compiler = new ManglerEvalCompiler(bytes).attach(eval); globalThis.nativeEval = eval;', context);

const cases = [
    ['root sites', 'function(){let a=[];function tag(s){a.push(s)};for(let i=0;i<2;i++)eval("tag`x`");return a[0]!==a[1]}'],
    ['nested loop site', 'function(){let a=[];function tag(s){a.push(s)};for(let i=0;i<2;i++)eval("(function(){for(let j=0;j<2;j++)tag`x`})()");return [a[0]===a[1],a[1]!==a[2],a[2]===a[3]]}'],
    ['escaping closures', 'function(){function tag(s){return s}let f=eval("(function(){return tag`x`})"),g=eval("(function(){return tag`x`})");return [f()===f(),g()===g(),f()!==g(),f()===f()]}'],
    ['generator sites', 'function(){function tag(s){return s}let source="(function*(){for(let i=0;i<2;i++)yield tag`x`})",f=eval(source),g=eval(source),a=[...f()],b=[...f()],c=[...g()];return [a[0]===a[1],a[0]===b[0],a[0]!==c[0],Object.isFrozen(a[0]),Object.isFrozen(a[0].raw)]}'],
    ['nested escaping closures', 'function(){function tag(s){return s}let f=eval("(()=>()=>tag`x`)()"),g=eval("(()=>()=>tag`x`)()");return [f()===f(),g()===g(),f()!==g()]}'],
    ['reentrant closures', 'function(){let other,entered=false;function tag(s){if(other&&!entered){entered=true;other();entered=false}return s}let f=eval("(()=>tag`x`)"),g=eval("(()=>tag`x`)");other=g;return [f()===f(),g()===g(),f()!==g()]}'],
    ['class support scopes', 'function(){function tag(s){return s}let f=eval("(class {m(){return tag`x`}})"),g=eval("(class {m(){return tag`x`}})");return [new f().m()===new f().m(),new g().m()===new g().m(),new f().m()!==new g().m()]}'],
    ['support side effects', 'function(){let count=0;for(let i=0;i<3;i++)eval("class C{static{count++}};C");return count}'],
    ['copied environments', 'function(){let z=3;function tag(s){return s}let f=eval("function a(){return ()=>{eval(\\"z\\");return tag`x`}};a()"),g=eval("function a(){return ()=>{eval(\\"z\\");return tag`x`}};a()");return [f()===f(),g()===g(),f()!==g()]}'],
    ['frozen template arrays', 'function(){let s=eval("((s)=>s)`a${1}b`");return [Object.isFrozen(s),Object.isFrozen(s.raw),s[0],s[1],s.raw[0],Object.getOwnPropertyDescriptor(s,"raw").enumerable]}'],
];
for (const [name, source] of cases) {
    context.source = source;
    const expected = JSON.stringify(vm.runInNewContext('(' + source + ')()'));
    const actual = JSON.stringify(vm.runInContext('compiler.compileFunction(source,{eval:nativeEval})()', context));
    assert.equal(actual, expected, name);
}

vm.runInContext(`
    globalThis.requests=0;
    const originalRequest=compiler.request;
    compiler.request=function(input){requests++;return Reflect.apply(originalRequest,this,[input])};
    globalThis.refs=[];
    function tag(template){refs.push(new WeakRef(template));return template}
    globalThis.repeat=compiler.compileFunction('function(){return eval("tag\\x60weak\\x60")}',{eval:nativeEval,tag});
    repeat();globalThis.beforeRows=compiler.table.length;globalThis.beforeRequests=requests;
    for(let i=0;i<200;i++)repeat();
    globalThis.afterRows=compiler.table.length;globalThis.afterRequests=requests;
    const publicSource='(()=>tag\\x60public\\x60)';
    const publicContext={outer:{tag}};
    const first=compiler.evaluate(publicSource,publicContext),second=compiler.evaluate(publicSource,publicContext);
    globalThis.publicIdentity=first()===first()&&second()===second()&&first()!==second();
    globalThis.publicBeforeRows=compiler.table.length;globalThis.publicBeforeRequests=requests;
    for(let i=0;i<200;i++)compiler.evaluate(publicSource,publicContext)();
    globalThis.publicAfterRows=compiler.table.length;globalThis.publicAfterRequests=requests;
    const variables=Object.create(null),other={variables};
    compiler.evaluate('var amount=3',other);compiler.evaluate('amount+=4',other);
    globalThis.publicVariables=variables.amount===7;
    void 0;
`, context);
assert.equal(context.afterRows, context.beforeRows, 'VM eval retained new rows');
assert.equal(context.afterRequests, context.beforeRequests, 'VM eval recompiled cached source');
assert.equal(context.publicAfterRows, context.publicBeforeRows, 'public eval retained new rows');
assert.equal(context.publicAfterRequests, context.publicBeforeRequests, 'public eval recompiled cached source');
assert.equal(context.publicIdentity, true, 'public eval template identity');
assert.equal(context.publicVariables, true, 'public eval context variables');

(async () => {
    let collected;
    if (global.gc) {
        for (let i=0;i<5;i++) { await new Promise(resolve => setTimeout(resolve,5)); global.gc(); }
        collected = vm.runInContext('refs.filter(ref=>ref.deref()===undefined).length',context);
        assert(collected > 0, 'released eval templates remain strongly retained');
    }
    console.log(JSON.stringify({ok:true,identityCases:cases.length+1,vmRows:context.afterRows-context.beforeRows,publicRows:context.publicAfterRows-context.publicBeforeRows,collected}));
})().catch(error => { console.error(error); process.exitCode=1; });
