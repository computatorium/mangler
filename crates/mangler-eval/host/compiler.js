// Shared static VM host. Input bodies compile to bytecode; only canonical native
// parameter shells use dynamic function creation when arguments mapping needs it.
const ManglerEvalHostFunction = Function;
const ManglerEvalApply = Reflect.apply;
const ManglerEvalConstruct = Reflect.construct;
const ManglerEvalProxy = Proxy;
const ManglerEvalCall = Function.prototype.call;
const ManglerEvalFunctionApply = Function.prototype.apply;
const ManglerEvalBind = Function.prototype.bind;
const ManglerEvalGetPrototype = Object.getPrototypeOf;
const ManglerEvalWeakGet = WeakMap.prototype.get;
const ManglerEvalWeakSet = WeakMap.prototype.set;
const ManglerEvalConstructors = [Function, Object.getPrototypeOf(async function(){}).constructor, Object.getPrototypeOf(function*(){}).constructor, Object.getPrototypeOf(async function*(){}).constructor];
const ManglerEvalWrite = Reflect.set;
const ManglerEvalDelete = Reflect.deleteProperty;
const ManglerEvalObject = Object;
const ManglerEvalDefine = Object.defineProperty;
const ManglerEvalCreate = Object.create;
const ManglerEvalPrototype = Object.setPrototypeOf;
const ManglerEvalFreeze = Object.freeze;
const ManglerEvalKeys = Object.keys;
const ManglerEvalOwn = Object.prototype.hasOwnProperty;
const ManglerEvalParse = JSON.parse;
const ManglerEvalStringify = JSON.stringify;
const ManglerEvalChar = String.fromCharCode;
const ManglerEvalCodeUnit = String.prototype.charCodeAt;
const ManglerEvalEncode = TextEncoder.prototype.encode;
const ManglerEvalDecode = TextDecoder.prototype.decode;
const ManglerEvalTypedSet = Uint8Array.prototype.set;
const ManglerEvalMapGet = Map.prototype.get;
const ManglerEvalMapSet = Map.prototype.set;
const ManglerEvalSetAdd = Set.prototype.add;
const ManglerEvalSetHas = Set.prototype.has;
const ManglerEvalMapConstructor = ManglerEvalIntrinsics(['Map']);
const ManglerEvalSetConstructor = ManglerEvalIntrinsics(['Set']);
const ManglerEvalWeakMapConstructor = ManglerEvalIntrinsics(['WeakMap']);
const ManglerEvalTextEncoderConstructor = ManglerEvalIntrinsics(['TextEncoder']);
const ManglerEvalTextDecoderConstructor = ManglerEvalIntrinsics(['TextDecoder']);
const ManglerEvalUint8ArrayConstructor = ManglerEvalIntrinsics(['Uint8Array']);
const ManglerEvalErrorConstructor = ManglerEvalIntrinsics(['Error']);
const ManglerEvalTypeErrorConstructor = ManglerEvalIntrinsics(['TypeError']);
const ManglerEvalReferenceErrorConstructor = ManglerEvalIntrinsics(['ReferenceError']);
const ManglerEvalSyntaxErrorConstructor = ManglerEvalIntrinsics(['SyntaxError']);
const ManglerEvalWasmModule = ManglerEvalIntrinsics(['WebAssembly','Module']);
const ManglerEvalWasmInstance = ManglerEvalIntrinsics(['WebAssembly','Instance']);
const ManglerEvalWasmImports = ManglerEvalIntrinsics(['WebAssembly','Module','imports']);
function ManglerEvalMap(items, callback, visible = false) {
    const result = visible ? [] : ManglerEvalPrototype([], null);
    for (let i = 0; i < items.length; i++) ManglerEvalDefine(result, i, {value: callback(items[i], i), writable: true, enumerable: true, configurable: true});
    return result;
}
function ManglerEvalOwnValue(object, key) {
    return ManglerEvalApply(ManglerEvalOwn, object, [key]) ? object[key] : undefined;
}
function ManglerEvalClassGrammar(value) {
    if (!value) return undefined;
    const grammar = ManglerEvalCreate(null);
    grammar.privateNames = ManglerEvalMap(value.privateNames, name => name);
    grammar.allowSuperProperty = !!value.allowSuperProperty;
    grammar.allowSuperCall = !!value.allowSuperCall;
    grammar.argumentsForbidden = !!value.argumentsForbidden;
    return grammar;
}
function ManglerEvalGlobalBinding(table, name) {
    for (let record = ManglerEvalOwnValue(table, 'globalEnvironment'); record; record = record.p) {
        if (record.b && ManglerEvalApply(ManglerEvalOwn, record.b, [name])) {
            const reference = record.b[name].r;
            return ManglerEvalApply(ManglerEvalOwn, reference, ['resolve']) ? reference.resolve() : reference;
        }
    }
}
function ManglerEvalRequest(request, runtime) {
    const input = ManglerEvalCreate(null);
    input.version = 1;
    const units = value => ManglerEvalMap(value, (_, index) => ManglerEvalApply(ManglerEvalCodeUnit, value, [index]));
    for (let keys = ManglerEvalKeys(request), i = 0; i < keys.length; i++) {
        const key = keys[i], value = request[key];
        if ((key === 'source' || key === 'body') && typeof value === 'string') input[key + 'Units'] = units(value);
        else if (key === 'parameters') input.parameterUnits = ManglerEvalMap(value, units);
        else input[key] = key === 'classContext' ? ManglerEvalClassGrammar(value) : value;
    }
    input.op = ManglerEvalOwnValue(runtime, 'op') && ManglerEvalMap(runtime.op, x => x);
    input.bin = ManglerEvalOwnValue(runtime, 'bin') && ManglerEvalMap(runtime.bin, x => x);
    input.un = ManglerEvalOwnValue(runtime, 'un') && ManglerEvalMap(runtime.un, x => x);
    return ManglerEvalStringify(input);
}
class ManglerEvalCompiler {
    constructor(bytes, table, runtime) {
        this.table = table || manglerEvalTable;
        this.runtime = runtime || {compilerFingerprint: manglerEvalCompilerFingerprint, variants: [manglerEvalRun,manglerEvalRun,manglerEvalRunStrict,manglerEvalRunStrict]};
        this.module = new ManglerEvalWasmModule(bytes);
        const imports = ManglerEvalWasmImports(this.module);
        if (imports.length) throw new ManglerEvalErrorConstructor('Compiler must not require host imports: ' + ManglerEvalStringify(imports));
        this.exports = new ManglerEvalWasmInstance(this.module, {}).exports;
        if (this.exports.mangler_abi_version() !== 1) throw new ManglerEvalErrorConstructor('Unsupported Mangler compiler ABI');
        if (typeof this.runtime.compilerFingerprint !== 'bigint' ||
            typeof this.exports.mangler_compiler_fingerprint !== 'function' ||
            this.exports.mangler_compiler_fingerprint() !== this.runtime.compilerFingerprint) {
            throw new ManglerEvalErrorConstructor('Mangler compiler/runtime build mismatch. Rebuild with scripts/build-eval-runtime.sh and install mangler-eval.wasm from the same build.');
        }
        this.encoder = new ManglerEvalTextEncoderConstructor();
        this.decoder = new ManglerEvalTextDecoderConstructor();
        this.lifecycle = {failed: false};
        this.boundFunctions = new ManglerEvalWeakMapConstructor();
        this.parameterFactories = new ManglerEvalMapConstructor();
        this.evalPrograms = new ManglerEvalMapConstructor();
    }
    request(request) {
        if (this.lifecycle.failed) throw new ManglerEvalErrorConstructor('Compiler instance cannot be reused after a trap');
        const input = ManglerEvalApply(ManglerEvalEncode, this.encoder, [ManglerEvalRequest(request, this.runtime)]);
        const e = this.exports;
        const pointer = e.mangler_alloc(input.length);
        try {
            ManglerEvalApply(ManglerEvalTypedSet, new ManglerEvalUint8ArrayConstructor(e.memory.buffer, pointer, input.length), [input]);
            try { e.mangler_compile(pointer, input.length); }
            catch (trap) {
                this.lifecycle.failed = true;
                const diagnostic = ManglerEvalApply(ManglerEvalDecode, this.decoder, [new ManglerEvalUint8ArrayConstructor(e.memory.buffer, e.mangler_result_ptr(), e.mangler_result_len())]);
                throw new ManglerEvalErrorConstructor('Wasm compiler trapped: ' + diagnostic, {cause: trap});
            }
            return ManglerEvalParse(ManglerEvalApply(ManglerEvalDecode, this.decoder, [new ManglerEvalUint8ArrayConstructor(e.memory.buffer, e.mangler_result_ptr(), e.mangler_result_len())]));
        } finally {
            if (!this.lifecycle.failed) e.mangler_free(pointer, input.length);
        }
    }
    fork(table, runtime) {
        const compiler = ManglerEvalCreate(ManglerEvalCompiler.prototype);
        compiler.table = table;
        compiler.runtime = runtime;
        compiler.module = this.module;
        compiler.exports = this.exports;
        compiler.encoder = this.encoder;
        compiler.decoder = this.decoder;
        compiler.lifecycle = this.lifecycle;
        compiler.boundFunctions = this.boundFunctions;
        compiler.parameterFactories = this.parameterFactories;
        compiler.evalPrograms = new ManglerEvalMapConstructor();
        return compiler;
    }
    attach(intrinsicEval) {
        ManglerEvalDefine(this.table, 'evalIntrinsic', {value: intrinsicEval, writable: true, configurable: true});
        ManglerEvalDefine(this.table, 'invoke', {value: (fn, receiver, args, indirectEval) => this.invoke(fn, receiver, args, indirectEval), configurable: true});
        // Native producer sites retain the actual bound exotic object. Capture
        // the property before evaluating arguments, then use the same registry
        // operation as protected bind calls. Overrides remain ordinary calls.
        ManglerEvalDefine(this.table, 'captureBind', {value: (receiver, key, value) => {
            const fn = value ? key : receiver[key];
            return (...args) => this.invoke(fn, receiver, args, undefined, true);
        }, configurable: true});
        // Lazy native references keep getters out of delete evaluation and keep
        // actual undefined values distinct from an optional-chain short circuit.
        ManglerEvalDefine(this.table, 'chainReference', {value: (reference, call, optional, raw) => {
            if (reference === undefined) return undefined;
            const value = reference.v;
            if (optional && (value === null || value === undefined)) return undefined;
            if (call) return (...args) => {const result = this.invoke(value, reference.r, args, undefined, true);return raw ? result : {__proto__: null, v: result};};
            return key => ({__proto__: null, r: value,
                get v() { return value[key]; },
                d(strict) {
                    if (value === null || value === undefined) throw new ManglerEvalTypeErrorConstructor('Cannot delete a property of null or undefined');
                    const deleted = ManglerEvalDelete(ManglerEvalObject(value), key);
                    if (!deleted && strict) throw new ManglerEvalTypeErrorConstructor('Cannot delete a non-configurable property');
                    return deleted;
                }
            });
        }, configurable: true});
        ManglerEvalDefine(this.table, 'deleteReference', {value: (reference, strict) => reference === undefined || !reference.d ? true : reference.d(strict), configurable: true});
        ManglerEvalDefine(this.table, 'construct', {value: (fn, args, target) => this.construct(fn, args, target), configurable: true});
        ManglerEvalDefine(this.table, 'eval', {value: (source, options) => this.evalResult(source, options), configurable: true});
        return this;
    }
    evalResult(source, options) {
        const classContext = ManglerEvalClassGrammar(ManglerEvalOwnValue(options, 'classContext'));
        const key = source.length + ':' + source + ':' + !!options.strict + ':' + !!options.allowNewTarget + ':' + options.sourceContext + ':' + ManglerEvalStringify(classContext);
        let cached = ManglerEvalApply(ManglerEvalMapGet, this.evalPrograms, [key]);
        if (!cached) {
            const result = this.request({mode: 'eval', source, ...options, classContext});
            if (!result.ok) {
                const error = result.error.kind === 'syntax' ? new ManglerEvalSyntaxErrorConstructor(result.error.message) : new ManglerEvalErrorConstructor(result.error.message);
                error.kind = result.error.kind;
                throw error;
            }
            const sites = this.hasTemplateSites(result.program) ? new ManglerEvalWeakMapConstructor() : undefined;
            const index = this.install(result.program, result.strict, !!ManglerEvalOwnValue(result, 'support'), sites, true);
            cached = {result, row: this.table[index], factory: this.supportFactory(result)};
            ManglerEvalApply(ManglerEvalMapSet, this.evalPrograms, [key, cached]);
        }
        const result = cached.result;
        // Support tables have their own lexical identities and lifetime.
        // Keep their emitted factory cached, not an evaluated namespace.
        return {support: this.prepareSupport(result, cached.factory), row: cached.row, program: result.program, declaredVars: result.declaredVars, declaredFunctions: result.declaredFunctions, classCaptures: ManglerEvalOwnValue(result, 'classCaptures'), strict: result.strict};
    }
    compileFunction(source, environment = ManglerEvalCreate(null)) {
        const result = this.request({mode: 'function', source});
        if (!result.ok) {
            const error = new ManglerEvalErrorConstructor(result.error.message);
            error.kind = result.error.kind;
            throw error;
        }
        return this.functionResult(result, environment);
    }
    functionResult(result, environment) {
        const support = this.prepareSupport(result);
        const root = this.install(result.program, result.strict, !!ManglerEvalOwnValue(result, 'support'));
        const row = this.table[root];
        let callable;
        const globalBinding = name => ManglerEvalGlobalBinding(this.table, name);
        const references = ManglerEvalMap(result.program.captures, name => ({
            get() {
                if (!ManglerEvalOwnValue(result, 'entry') && name === result.name) return callable;
                if (ManglerEvalApply(ManglerEvalOwn, support, [name])) return support[name];
                if (name in environment) return environment[name];
                const reference = globalBinding(name);
                if (reference) return reference.get();
                if (name in globalThis) return globalThis[name];
                throw new ManglerEvalReferenceErrorConstructor(name + ' is not defined');
            },
            set(value) {
                if (!ManglerEvalOwnValue(result, 'entry') && name === result.name) { if (result.strict) throw new ManglerEvalTypeErrorConstructor('Assignment to immutable function name'); return; }
                if (name in environment) { if (!ManglerEvalWrite(environment,name,value,environment) && result.strict) throw new ManglerEvalTypeErrorConstructor('Cannot assign captured binding'); }
                else {
                    const reference = globalBinding(name);
                    if (reference) { reference.set(value, result.strict); return; }
                    if (result.strict && !(name in globalThis)) throw new ManglerEvalReferenceErrorConstructor(name + ' is not defined');
                    if (!ManglerEvalWrite(globalThis,name,value,globalThis) && result.strict) throw new ManglerEvalTypeErrorConstructor('Cannot assign global binding');
                }
            },
            strictSet(value) {
                if (!ManglerEvalOwnValue(result, 'entry') && name === result.name) throw new ManglerEvalTypeErrorConstructor('Assignment to immutable function name');
                const reference = !(name in environment) && globalBinding(name);
                if (reference) { reference.set(value, true); return; }
                const owner = name in environment ? environment : globalThis;
                if (!(name in owner)) throw new ManglerEvalReferenceErrorConstructor(name + ' is not defined');
                if (!ManglerEvalWrite(owner,name,value,owner)) throw new ManglerEvalTypeErrorConstructor('Cannot assign captured binding');
            },
            type() {
                if (!ManglerEvalOwnValue(result, 'entry') && name === result.name) return 'function';
                const reference = !(name in environment) && globalBinding(name);
                return name in environment ? typeof environment[name] : reference ? reference.type() : typeof globalThis[name];
            },
            del() { const reference = globalBinding(name); return name in environment || !ManglerEvalOwnValue(result, 'entry') && name === result.name ? false : reference ? reference.del() : ManglerEvalDelete(globalThis,name); }
        }));
        const ambient = {b: ManglerEvalCreate(null), p: ManglerEvalOwnValue(this.table, 'globalEnvironment') || null};
        for (let names = ManglerEvalKeys(environment), i = 0; i < names.length; i++) { const name = names[i]; ambient.b[name] = {l: false, r: {
            get() { return environment[name]; }, set(value, strict) { if (!ManglerEvalWrite(environment,name,value,environment) && strict) throw new ManglerEvalTypeErrorConstructor('Cannot assign captured binding'); },
            type() { return typeof environment[name]; }, del() { return false; }, receiver: undefined
        }}; }
        if (!ManglerEvalOwnValue(result, 'entry') && result.name) ambient.b[result.name] = {l: false, r: {
            get() { return callable; }, set(value, strict) { if (strict) throw new ManglerEvalTypeErrorConstructor('Assignment to immutable function name'); },
            type() { return 'function'; }, del() { return false; }, receiver: undefined
        }};
        const invoke = (receiver, args, paramRefs, target) => row[2](row[0], row[1], args, references, result.program.capStart, result.program.pcount, result.strict ? receiver : receiver === undefined || receiver === null ? globalThis : Object(receiver), true, paramRefs, target, ambient);
        callable = row[5] ? row[5](invoke) : result.strict ? function() { 'use strict'; return invoke(this, arguments, undefined, new.target); }
            : function() { return invoke(this, arguments, undefined, new.target); };
        ManglerEvalDefine(callable, 'length', {value: result.length, configurable: true});
        ManglerEvalDefine(callable, 'name', {value: ManglerEvalOwnValue(result, 'displayName') || result.name || '', configurable: true});
        if (ManglerEvalOwnValue(result, 'entry')) {
            const value = callable();
            ManglerEvalDefine(value, 'name', {value: ManglerEvalOwnValue(result, 'displayName') || result.name || '', configurable: true});
            ManglerEvalDefine(value, 'length', {value: result.length, configurable: true});
            return value;
        }
        return callable;
    }
    supportFactory(result) {
        const metadata = ManglerEvalOwnValue(result, 'support');
        // Factory text is emitted runtime, bytecode and structural syntax. The
        // source frontend virtualizes every user expression before emitting it.
        return metadata ? new ManglerEvalHostFunction('return (' + metadata.factory + ')')() : undefined;
    }
    prepareSupport(result, factory = this.supportFactory(result)) {
        const metadata = ManglerEvalOwnValue(result, 'support');
        if (!metadata) return ManglerEvalCreate(null);
        const support = factory(ManglerEvalIntrinsics);
        for (let i = 0; i < metadata.tables.length; i++) {
            const table = support[metadata.tables[i]];
            ManglerEvalDefine(table, 'globalEnvironment', {value: ManglerEvalOwnValue(this.table, 'globalEnvironment'), writable: true, configurable: true});
            if (ManglerEvalOwnValue(table, 'runtime')) this.fork(table, table.runtime).attach(this.table.evalIntrinsic);
        }
        return support;
    }
    constructorKind(fn) {
        for (let i = 0; i < ManglerEvalConstructors.length; i++) if (fn === ManglerEvalConstructors[i]) return i;
        return -1;
    }
    mediated(fn) {
        return fn === this.table.evalIntrinsic || this.constructorKind(fn) >= 0 || fn === ManglerEvalCall || fn === ManglerEvalFunctionApply || fn === ManglerEvalBind || fn === ManglerEvalApply || fn === ManglerEvalConstruct || !!ManglerEvalApply(ManglerEvalWeakGet, this.boundFunctions, [fn]);
    }
    ensureConstructor(value) {
        // The private proxy trap tests [[Construct]] without reading the source
        // newTarget.prototype before argument conversion and grammar validation.
        ManglerEvalConstruct(new ManglerEvalProxy(value, {construct() { return {}; }}), []);
    }
    argumentList(value) {
        return ManglerEvalApply(function() { return arguments; }, undefined, value);
    }
    invoke(fn, receiver, args, indirectEval, nativeConstructors = false) {
        // Native producer bridges share alias/bind registration with protected
        // calls, while preserving native behavior if a source override turns the
        // producer into an eval or constructor invocation.
        const bound = ManglerEvalApply(ManglerEvalWeakGet, this.boundFunctions, [fn]);
        if (bound) {
            const combined = ManglerEvalMap(bound.args, x => x);
            for (let i = 0; i < args.length; i++) ManglerEvalDefine(combined, combined.length, {value: args[i], writable: true, enumerable: true, configurable: true});
            return this.invoke(bound.fn, bound.receiver, combined, indirectEval, nativeConstructors);
        }
        if (fn === this.table.evalIntrinsic) return nativeConstructors ? ManglerEvalApply(fn, receiver, args) : indirectEval(args[0]);
        const kind = this.constructorKind(fn);
        if (kind >= 0) return nativeConstructors ? ManglerEvalApply(fn, receiver, args) : this.dynamicFunction(kind, args, fn);
        if (fn === ManglerEvalCall && this.mediated(receiver)) return this.invoke(receiver, args[0], ManglerEvalMap({length: (args.length ? args.length - 1 : 0)}, (_, i) => args[i + 1]), indirectEval, nativeConstructors);
        if (fn === ManglerEvalFunctionApply && this.mediated(receiver)) return this.invoke(receiver, args[0], args[1] === null || args[1] === undefined ? [] : this.argumentList(args[1]), indirectEval, nativeConstructors);
        if (fn === ManglerEvalApply && this.mediated(args[0])) return this.invoke(args[0], args[1], this.argumentList(args[2]), indirectEval, nativeConstructors);
        if (fn === ManglerEvalConstruct && !nativeConstructors && this.mediated(args[0])) {
            const target = args.length > 2 ? args[2] : args[0];
            this.ensureConstructor(args[0]);
            this.ensureConstructor(target);
            return this.construct(args[0], this.argumentList(args[1]), target);
        }
        if (fn === ManglerEvalBind && this.mediated(receiver)) {
            const value = ManglerEvalApply(fn, receiver, args);
            ManglerEvalApply(ManglerEvalWeakSet, this.boundFunctions, [value, {fn: receiver, receiver: args[0], args: ManglerEvalMap({length: (args.length ? args.length - 1 : 0)}, (_, i) => args[i + 1])}]);
            return value;
        }
        return ManglerEvalApply(fn, receiver, args);
    }
    construct(fn, args, target) {
        const bound = ManglerEvalApply(ManglerEvalWeakGet, this.boundFunctions, [fn]);
        if (bound) {
            const combined = ManglerEvalMap(bound.args, x => x);
            for (let i = 0; i < args.length; i++) ManglerEvalDefine(combined, combined.length, {value: args[i], writable: true, enumerable: true, configurable: true});
            return this.construct(bound.fn, combined, target === fn ? bound.fn : target);
        }
        const kind = this.constructorKind(fn);
        return kind < 0 ? ManglerEvalConstruct(fn, args, target) : this.dynamicFunction(kind, args, target);
    }
    dynamicFunction(kind, args, target) {
        const parameters = ManglerEvalMap({length: args.length ? args.length - 1 : 0}, (_, i) => `${args[i]}`);
        const body = args.length ? `${args[args.length - 1]}` : '';
        const result = this.request({mode: 'constructor', kind: ['normal', 'async', 'generator', 'async-generator'][kind], parameters, body});
        if (!result.ok) {
            const error = result.error.kind === 'syntax' ? new ManglerEvalSyntaxErrorConstructor(result.error.message) : new ManglerEvalErrorConstructor(result.error.message);
            error.kind = result.error.kind;
            throw error;
        }
        const prototype = target.prototype;
        const callable = this.functionResult(result, ManglerEvalCreate(null));
        ManglerEvalPrototype(callable, Object(prototype) === prototype ? prototype : ManglerEvalConstructors[kind].prototype);
        return callable;
    }
    evaluate(source, context = {}) {
        if (typeof source !== 'string') return source;
        const result = this.evalResult(source, {strict: !!context.strict,
            allowNewTarget: !!context.allowNewTarget || 'newTarget' in context});
        const support = result.support;
        const variables = context.variables || (context.variables = ManglerEvalCreate(null));
        const outer = context.outer || ManglerEvalCreate(null);
        const deletable = context.deletable || (context.deletable = new ManglerEvalSetConstructor());
        for (let i = 0; i < result.declaredVars.length; i++) {
            const name = result.declaredVars[i];
            if (!ManglerEvalApply(ManglerEvalOwn, variables, [name])) {
                ManglerEvalDefine(variables, name, {value: undefined, writable: true, enumerable: true, configurable: true});
                ManglerEvalApply(ManglerEvalSetAdd, deletable, [name]);
            }
        }
        const references = ManglerEvalMap(result.program.captures, name => {
            const owner = () => ManglerEvalApply(ManglerEvalOwn, variables, [name]) ? variables : name in outer ? outer : globalThis;
            return {
                get() { if (ManglerEvalApply(ManglerEvalOwn, support, [name])) return support[name]; const object = owner(); if (!(name in object)) throw new ManglerEvalReferenceErrorConstructor(name + ' is not defined'); return object[name]; },
                set(value) { if (ManglerEvalApply(ManglerEvalOwn, support, [name])) throw new ManglerEvalTypeErrorConstructor('Immutable runtime binding'); const object = owner(); if (result.strict && !(name in object)) throw new ManglerEvalReferenceErrorConstructor(name + ' is not defined'); if (!ManglerEvalWrite(object,name,value,object) && result.strict) throw new ManglerEvalTypeErrorConstructor('Cannot assign binding'); },
                strictSet(value) { if (ManglerEvalApply(ManglerEvalOwn, support, [name])) throw new ManglerEvalTypeErrorConstructor('Immutable runtime binding'); const object = owner(); if (!(name in object)) throw new ManglerEvalReferenceErrorConstructor(name + ' is not defined'); if (!ManglerEvalWrite(object,name,value,object)) throw new ManglerEvalTypeErrorConstructor('Cannot assign binding'); },
                type() { return ManglerEvalApply(ManglerEvalOwn, support, [name]) ? typeof support[name] : typeof owner()[name]; },
                del() { if (ManglerEvalApply(ManglerEvalOwn, support, [name])) return false; const object = owner(); return object === variables ? ManglerEvalApply(ManglerEvalSetHas, deletable, [name]) && ManglerEvalDelete(variables,name) : object === outer ? false : ManglerEvalDelete(globalThis,name); }
            };
        });
        const row = result.row;
        const receiver = 'receiver' in context ? context.receiver : globalThis;
        return row[2](row[0], row[1], [], references, result.program.capStart, result.program.pcount,
            receiver, true, undefined, context.newTarget);
    }
    hasOwnTemplateSites(program) {
        for (let i = 0; i < program.constants.length; i++) if (program.constants[i].tag === 'template') return true;
        return false;
    }
    hasTemplateSites(program) {
        if (this.hasOwnTemplateSites(program)) return true;
        for (let i = 0; i < program.children.length; i++) if (this.hasTemplateSites(program.children[i].program)) return true;
        return false;
    }
    templateRun(program, constants, invoke, sites, entry, prepared) {
        const compiler = this, hasTemplates = this.hasOwnTemplateSites(program);
        return function(code, pool, args, caps, capStart, pcount, receiver, live, paramRefs, target, environment) {
            let registry;
            if (entry) {
                // JoinEnvironment copies records but preserves each bindings
                // object's identity, including this empty private record.
                const bindings = ManglerEvalCreate(null);
                registry = new ManglerEvalMapConstructor();
                ManglerEvalApply(ManglerEvalWeakSet, sites, [bindings, registry]);
                environment = {b: bindings, p: environment, v: false, g: false, w: undefined};
            } else {
                for (let record = environment; record; record = record.p) {
                    if (record.b) registry = ManglerEvalApply(ManglerEvalWeakGet, sites, [record.b]);
                    if (registry) break;
                }
            }
            if (hasTemplates) {
                if (!registry) throw new ManglerEvalErrorConstructor('Missing eval template environment');
                let instance = ManglerEvalApply(ManglerEvalMapGet, registry, [constants]);
                if (!instance) {
                    instance = ManglerEvalMap(constants, (value, index) => program.constants[index].tag === 'template' ? compiler.constant(program.constants[index], prepared) : value);
                    ManglerEvalDefine(instance, 'd', {value: 1});
                    ManglerEvalApply(ManglerEvalMapSet, registry, [constants, instance]);
                }
                pool = instance;
            }
            return invoke(code, pool, args, caps, capStart, pcount, receiver, live, paramRefs, target, environment);
        };
    }
    install(program, strict, prepared = false, sites, entry = false) {
        const children = ManglerEvalMap(program.children, child => this.install(child.program, strict || child.strict, prepared, sites));
        const code = ManglerEvalMap(program.code, x => x);
        for (let i = 0; i < program.relocations.length; i++) { const offset = program.relocations[i]; code[offset] = children[code[offset]]; }
        const constants = ManglerEvalMap(program.constants, value => this.constant(value, prepared));
        ManglerEvalDefine(code, 'd', {value: 1});
        ManglerEvalDefine(constants, 'd', {value: 1});
        const index = this.table.length;
        const invoke = this.runtime.variants[strict ? 3 : 1];
        const run = sites && (entry || this.hasOwnTemplateSites(program)) ? this.templateRun(program, constants, invoke, sites, entry, prepared) : invoke;
        ManglerEvalDefine(this.table, index, {value: [code, constants, run, strict, 0, this.parameterFactory(program.argumentFactory)], writable: true, enumerable: true, configurable: true});
        return index;
    }
    parameterFactory(source) {
        if (!source || source === '0') return 0;
        let factory = ManglerEvalApply(ManglerEvalMapGet, this.parameterFactories, [source]);
        if (!factory) {
            // This string is generated solely from numeric argument/slot metadata
            // by Rust's shared argument_factory. It contains no input program text.
            factory = new ManglerEvalHostFunction('return (' + source + ')')();
            ManglerEvalApply(ManglerEvalMapSet, this.parameterFactories, [source, factory]);
        }
        return factory;
    }
    constant(value, prepared) {
        switch (value.tag) {
            case 'number': return value.value === 'inf' ? Infinity : value.value === '-inf' ? -Infinity : Number(value.value);
            case 'boolean': return value.value;
            case 'utf16': return this.string(value.value);
            case 'bigint': return BigInt(value.value);
            case 'regexp': return {r: value.pattern, f: value.flags};
            case 'regexpUtf16': return {r: this.string(value.pattern), f: value.flags};
            case 'template': {
                const cooked = ManglerEvalMap(value.cooked, units => units === null ? undefined : this.string(units), true);
                ManglerEvalDefine(cooked, 'raw', {value: ManglerEvalFreeze(ManglerEvalMap(value.raw, units => this.string(units), true))});
                return ManglerEvalFreeze(cooked);
            }
            case 'environment': return {e: value.scopes, c: value.sourceContext, k: ManglerEvalOwnValue(value, 'classContext')};
            case 'nativeFactory':
                if (!prepared) throw new ManglerEvalErrorConstructor('Native structural syntax requires the protected source frontend');
                return new ManglerEvalHostFunction('return (' + value.source + ')')();
            default: throw new ManglerEvalErrorConstructor('Unknown VM constant tag: ' + value.tag);
        }
    }
    string(units) {
        let result = '';
        for (let offset = 0; offset < units.length; offset++) result += ManglerEvalChar(units[offset]);
        return result;
    }
}

// Compiler instances are private machinery; inherited source setters must not
// intercept state initialization when a support namespace forks after startup.
ManglerEvalPrototype(ManglerEvalCompiler.prototype, null);
