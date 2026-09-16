function manglerCompilerSmoke(bytes) {
    const started = performance.now();
    const compiler = new ManglerEvalCompiler(bytes).attach(manglerSmokeIntrinsicEval);
    const instantiated = performance.now();
    const checks = [
        ['arithmetic', 'function(a,b){return (a+b)*3}', [2,5], 21],
        ['loop', 'function(n){let sum=0;for(let i=0;i<n;i++)sum+=i;return sum}', [20], 190],
        ['closure', 'function(x){return function(y){x+=y;return x}}', [4], null],
        ['exceptions', 'function(){try{throw 7}catch(e){return e+2}}', [], 9],
        ['object scope', 'function(o,x){with(o){x+=2;return x}}', [{x:5},1], 7],
        ['bigint', 'function(){return String(9007199254740993n+2n)}', [], '9007199254740995'],
        ['utf16', 'function(){return "\\ud800".charCodeAt(0)}', [], 55296],
        ['raw utf16', 'function(){return "\ud800".charCodeAt(0)}', [], 55296],
        ['raw low surrogate', 'function(){return "\udfff".charCodeAt(0)}', [], 57343],
        ['raw template', 'function(){return ((s)=>s[0].charCodeAt(0)+":"+s.raw[0].charCodeAt(0))`\ud800`}', [], '55296:55296'],
        ['raw regexp', 'function(){return /\ud800/u.source.charCodeAt(0)}', [], 55296],
        ['raw regexp range', 'function(){return /[\ud800-\udbff]/.test("\ud900")}', [], true],
        ['utf16 marker collision', 'function(){return "\\uE000d800\\uE000"+"\ud800"}', [], '\ue000d800\ue000\ud800'],
        ['raw comment', 'function(){/*\ud800*/return 3}', [], 3],
        ['raw identity escape', 'function(){return "\\\ud800".charCodeAt(0)}', [], 55296],
    ];
    for (const [name, source, args, expected] of checks) {
        const fn = compiler.compileFunction(source);
        const result = fn(...args);
        if (name === 'closure') {
            if (result(3) !== 7 || result(2) !== 9) throw new Error('Captured mutation failed');
        } else if (!Object.is(result, expected)) throw new Error(name + ': got ' + String(result));
    }
    const environment = {rate: 4};
    const captured = compiler.compileFunction('function(n){return n*rate}', environment);
    if (captured(3) !== 12) throw new Error('Lexical descriptor read failed');
    environment.rate = 7;
    if (captured(3) !== 21) throw new Error('Lexical descriptor was copied');
    const invalid = compiler.request({mode: 'function', source: 'function( {'});
    if (invalid.ok || invalid.error.kind !== 'syntax') throw new Error('Syntax error not surfaced');
    const direct = compiler.compileFunction("function(x=0){let y=2;eval('var z=x+y; y=7');return [y,z]}", {eval:manglerSmokeIntrinsicEval});
    if (JSON.stringify(direct(3)) !== '[7,5]') throw new Error('Direct eval did not share the VM activation');
    const late = compiler.compileFunction("function(dummy=0){let read=()=>extra;eval('var extra=8');return read()}", {eval:manglerSmokeIntrinsicEval});
    if (late() !== 8 || 'extra' in globalThis || 'z' in globalThis) throw new Error('Eval declaration activation or late closure lookup failed');
    const context = {variables: Object.create(null), outer: {rate: 3}};
    if (compiler.evaluate('var amount=4; amount*rate', context) !== 12) throw new Error('Eval completion failed');
    if (compiler.evaluate('amount+=2;amount', context) !== 6) throw new Error('Eval variable activation was not retained');
    if ('amount' in globalThis) throw new Error('Direct eval variable leaked globally');
    const closure = compiler.evaluate('(function(){return amount})', context);
    compiler.evaluate('amount=9', context);
    if (closure() !== 9) throw new Error('Eval closure lost dynamic variable binding');
    if (compiler.evaluate('"use strict";var local=7;local', context) !== 7 || 'local' in context.variables) throw new Error('Strict eval variable leaked');
    const constructor = function Constructor() {};
    if (compiler.evaluate('new.target', {newTarget: constructor}) !== constructor) throw new Error('Eval new.target context lost');
    if (compiler.evaluate('1;for(let i=0;i<3;i++){i*2}') !== 4) throw new Error('Eval loop completion failed');
    if (compiler.evaluate('try{4}finally{7}') !== 4) throw new Error('Eval finally completion failed');
    if (compiler.evaluate(17) !== 17) throw new Error('Non-string eval did not return its argument');
    const completed = performance.now();
    const startMemory = compiler.exports.memory.buffer.byteLength;
    for (let n = 0; n < 50; n++) {
        const result = compiler.request({mode: 'function', source: 'function(n){return n+1}'});
        if (!result.ok) throw new Error('Repeated compilation failed');
    }
    return {ok: true, cases: checks.length + 12, instantiateMs: +(instantiated-started).toFixed(2),
        compileAndExecuteMs: +(completed-instantiated).toFixed(2),
        initialMemoryBytes: startMemory, repeatedMemoryBytes: compiler.exports.memory.buffer.byteLength,
        imports: WebAssembly.Module.imports(compiler.module)};
}

// Native mapped-arguments shells require dynamic function creation, so this
// group runs separately from the browser's restrictive CSP policy.
function manglerCompilerMappedSmoke(bytes) {
    const compiler = new ManglerEvalCompiler(bytes).attach(manglerSmokeIntrinsicEval);
    const cases = [
        ['function(a){return Object.prototype.toString.call(arguments)}', [1], '[object Arguments]'],
        ['function(a){a=7;return [arguments[0],Object.getOwnPropertyDescriptor(arguments,"0").value]}', [1], [7,7]],
        ['function(a){arguments[0]=9;return a}', [1], 9],
        ['function(a){delete arguments[0];a=8;return [a,arguments[0]]}', [1], [8,undefined]],
        ['function(a){Object.freeze(arguments);a=8;return [a,arguments[0]]}', [1], [8,1]],
        ['function(a,a){a=8;return [arguments[0],arguments[1]]}', [1,2], [1,8]],
        ['function pay(a){return arguments.callee===pay}', [1], true],
        ['function pay(){return arguments.callee===pay}', [], true],
        ['function(a=1){try{return arguments.callee}catch(e){return e.name}}', [], 'TypeError'],
        ['function(){return this===globalThis}', [], true],
        ['function(a){eval("a=6");return [a,arguments[0]]}', [1], [6,6]],
    ];
    for (const [source,args,expected] of cases) {
        const result = compiler.compileFunction(source,{eval:manglerSmokeIntrinsicEval})(...args);
        if (JSON.stringify(result)!==JSON.stringify(expected)) throw new Error('Native arguments shell mismatch: '+source+' -> '+JSON.stringify(result));
    }
    return {ok:true,cases:cases.length};
}

function manglerCompilerIntrinsicSmoke(bytes) {
    const compiler = new ManglerEvalCompiler(bytes).attach(manglerSmokeIntrinsicEval);
    const targets = [[TextEncoder.prototype,'encode'],[TextDecoder.prototype,'decode'],
        [Uint8Array.prototype,'set'],[Map.prototype,'get'],[Map.prototype,'set'],
        [Array.prototype,'map'],[Array.prototype,'slice'],[Array.prototype,'push'],
        [Array.prototype,Symbol.iterator],[Object.prototype,'toJSON'],
        ...['support','entry','i','g','v','w','ambient','globalEnvironment'].map(key=>[Object.prototype,key])];
    const saved = targets.map(([object,key]) => Object.getOwnPropertyDescriptor(object,key));
    const define = Object.defineProperty;
    const poisoned = function(){throw new Error('Mutable host prototype was consulted');};
    try {
        for (let i=0;i<targets.length;i++) define(targets[i][0],targets[i][1],{value:poisoned,writable:true,configurable:true});
        const fn = compiler.compileFunction("function(a=0){return eval('var __mangler_temp=1;a+__mangler_temp')}",{eval:manglerSmokeIntrinsicEval});
        if (fn(6)!==7 || fn(8)!==9 || '__mangler_temp' in globalThis) throw new Error('Protected compiler intrinsic execution mismatch');
    } finally {
        for (let i=0;i<targets.length;i++) {
            if (saved[i]) define(targets[i][0],targets[i][1],saved[i]);
            else delete targets[i][0][targets[i][1]];
        }
    }
    const names = ['Object','Reflect','Map','Set','WeakMap','TextEncoder','TextDecoder',
        'Uint8Array','WebAssembly','Error','TypeError','ReferenceError','SyntaxError'];
    const globals = names.map(name=>Object.getOwnPropertyDescriptor(globalThis,name));
    const replacement = function(){throw 'Mutable host namespace was consulted';};
    replacement.marker = 5;
    try {
        for(let i=0;i<names.length;i++) define(globalThis,names[i],{value:replacement,writable:true,configurable:true});
        const late = new ManglerEvalCompiler(bytes).attach(manglerSmokeIntrinsicEval);
        const fn = late.compileFunction('function(){class Box{#n=4;read(){return this.#n}}function* values(){yield new Box().read()}return values().next().value+Object.marker+Reflect.marker}');
        if(fn()!==14)throw 'Runtime support lost live source globals';
        if(late.evaluate('var n=3;n+1')!==4)throw 'Late compiler namespace failed';
    } finally {
        for(let i=0;i<names.length;i++) define(globalThis,names[i],globals[i]);
    }
    return {ok:true,cases:targets.length+names.length+2};
}

function manglerCompilerDynamicSmoke(bytes) {
    const compiler = new ManglerEvalCompiler(bytes).attach(manglerSmokeIntrinsicEval);
    const environment = {Function:ManglerEvalHostFunction,eval:manglerSmokeIntrinsicEval};
    const cases = [
        ["function(){return Function('a','return a+2')(3)}",5],
        ["function(){const C=Function;return new C('return 8')()}",8],
        ["function(){return Function.call(null,'return 9')()}",9],
        ["function(){return Function.apply(null,['return 10'])()}",10],
        ["function(){return Reflect.apply(Function,null,['return 11'])()}",11],
        ["function(){return Reflect.construct(Function,['return 12'])()}",12],
        ["function(){const C=Function.bind(null,'a');return C('return a*3')(4)}",12],
        ["function(){const C=Function.bind(null,'a');return new C('return a*3')(5)}",15],
        ["function(){const C=Function.prototype.call.bind(Function);return C(null,'return 16')()}",16],
        ["function(){const f=Function('return typeof anonymous');return [f.name,f.length,f()]}",['anonymous',0,'undefined']],
        ["function(){const f=Function('a','a=7;return arguments[0]');return f(2)}",7],
        ["function(){try{Function('/*','*/ return 1')}catch(e){return e.name}}",'SyntaxError'],
        ["function(){eval('{using f=null;{function f(){}}}');return typeof f}",'undefined'],
        ["function(){eval('for(using f of [null]){function f(){}}');return typeof f}",'undefined'],
        ["function(){var events=[];function f(){events.push('call')}function rhs(){events.push('rhs')}try{eval('f()+=rhs()')}catch(e){events.push(e.name)}return events}",['call','ReferenceError']],
        ["function(){'use strict';try{eval('f()=1')}catch(e){return e.name}}",'SyntaxError'],
        ["function(){Object.prototype.value=19;try{return eval('let n=7;const f=()=>n;n=8;f()')}finally{delete Object.prototype.value}}",8],
        ["function(){let local=3;const e=eval;return e('typeof local')}",'undefined'],
        ["function(){'use strict';const e=eval;return e('var __mangler_indirect=23;__mangler_indirect')}",23],
        ["function(){return eval.call(null,'__mangler_indirect+=2')}",25],
        ["function(){try{const e=eval;return e('new.target')}catch(e){return e.name}}",'SyntaxError'],
    ];
    try {
        for (const [source,expected] of cases) {
            const actual=compiler.compileFunction(source,environment)();
            if(JSON.stringify(actual)!==JSON.stringify(expected))throw new Error('Dynamic source mismatch: '+source+' -> '+JSON.stringify(actual));
        }
        if(globalThis.__mangler_indirect!==25)throw new Error('Indirect eval variable did not reach global object');
    } finally { delete globalThis.__mangler_indirect; }
    const constructorRequests = [];
    const request = compiler.request;
    compiler.request = function(input) {
        if (input.mode === 'constructor') constructorRequests.push(input.kind);
        return ManglerEvalApply(request, this, [input]);
    };
    let producerChecks = 0;
    try {
        for (let kind = 0; kind < ManglerEvalConstructors.length; kind++) {
            const target = ManglerEvalConstructors[kind];
            const direct = compiler.table.captureBind(target, 'bind')(null);
            const call = compiler.table.captureBind(ManglerEvalCall, 'bind')(target);
            const apply = compiler.table.captureBind(ManglerEvalFunctionApply, 'bind')(target);
            const rebound = compiler.table.captureBind(direct, 'bind')(null);
            const viaCall = compiler.table.captureBind(ManglerEvalBind, 'call')(target, null);
            const viaApply = compiler.table.captureBind(ManglerEvalBind, 'apply')(target, [null]);
            const viaReflect = compiler.table.captureBind(undefined, ManglerEvalApply, true)(ManglerEvalBind, target, [null]);
            const callBinder = compiler.table.captureBind(ManglerEvalCall, 'bind')(ManglerEvalBind);
            const applyBinder = compiler.table.captureBind(ManglerEvalFunctionApply, 'bind')(ManglerEvalBind);
            const reflectBinder = compiler.table.captureBind(ManglerEvalApply, 'bind')(null, ManglerEvalBind);
            const viaCallBinder = compiler.table.captureBind(undefined, callBinder, true)(target, null);
            const viaApplyBinder = compiler.table.captureBind(undefined, applyBinder, true)(target, [null]);
            const viaReflectBinder = compiler.table.captureBind(undefined, reflectBinder, true)(target, [null]);
            const optionalReference = compiler.table.chainReference({v:target}, false, true)?.('bind');
            const viaOptional = compiler.table.chainReference(optionalReference, true, true)?.(null)?.v;
            const body = kind > 1 ? 'yield 7' : 'return 7';
            for (const [make, expression] of [
                [direct, 'make(body)'], [call, 'make(null,body)'],
                [apply, 'make(null,[body])'], [rebound, 'make(body)'],
                [direct, 'new make(body)'],
                ...[viaCall,viaApply,viaReflect,viaCallBinder,viaApplyBinder,viaReflectBinder,viaOptional].map(make => [make, 'make(body)']),
            ]) {
                if (!ManglerEvalApply(ManglerEvalWeakGet, compiler.boundFunctions, [make])) throw new Error('Native producer object is not registered');
                const before = constructorRequests.length;
                const protectedCall = compiler.compileFunction("function(){if(arguments.length)Function('return 0');return " + expression + "}", {make, body, Function:ManglerEvalHostFunction});
                const result = protectedCall();
                if (constructorRequests.length !== before + 1 || constructorRequests[before] !== ['normal','async','generator','async-generator'][kind]) throw new Error('Native producer bypassed constructor compilation');
                if (typeof result !== 'function' || ManglerEvalGetPrototype(result) !== target.prototype) throw new Error('Native producer changed constructor kind');
                const expected = ManglerEvalApply(ManglerEvalBind, target, [null]);
                if (direct.name !== expected.name || direct.length !== expected.length || ManglerEvalOwnValue(direct, 'prototype') !== undefined) throw new Error('Native bound shape changed');
                producerChecks++;
            }
        }
    } finally { compiler.request = request; }
    return {ok:true,cases:cases.length + producerChecks};
}
