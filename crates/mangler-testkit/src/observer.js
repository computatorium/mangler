(function () {
    const root = globalThis;
    const stringify = JSON.stringify;
    const string = String;
    const keys = Object.keys;
    const create = Object.create;
    const getPrototypeOf = Object.getPrototypeOf;
    const getOwnPropertyDescriptor = Object.getOwnPropertyDescriptor;
    const objectPrototype = Object.prototype;
    const isArray = Array.isArray;
    const ErrorType = Error;
    // Internal storage never inherits user-mutable Array/Object methods. The wire
    // encoder writes JSON directly; Array.prototype.toJSON cannot rewrite it.
    const logs = create(null);
    let logCount = 0;
    let entries = 0;
    function unsupported() { throw new ErrorType('testkit: unsupported capture'); }
    function primitive(v) {
        if (v === undefined) return '["undefined"]';
        if (v === null) return '["null"]';
        switch (typeof v) {
            case 'boolean': return '["boolean",' + (v ? 'true' : 'false') + ']';
            case 'string': return '["string",' + stringify(v) + ']';
            case 'number': return '["number",' + stringify(v !== v ? 'NaN' : v === 0 && 1 / v < 0 ? '-0' : string(v)) + ']';
            case 'bigint': return '["bigint",' + stringify(string(v)) + ']';
            default: return unsupported();
        }
    }
    function structure(v, seen, depth) {
        if (++entries > 10000 || depth > 50) return unsupported();
        if (v === null || typeof v !== 'object') return primitive(v);
        for (let i = 0; i < depth; i++) if (seen[i] === v) return unsupported();
        const array = isArray(v);
        if (!array && getPrototypeOf(v) !== objectPrototype && getPrototypeOf(v) !== null) return unsupported();
        seen[depth] = v;
        let result = '[';
        const names = keys(v);
        for (let i = 0; i < names.length; i++) {
            const key = names[i];
            const descriptor = getOwnPropertyDescriptor(v, key);
            if (!descriptor || !('value' in descriptor)) return unsupported();
            if (i) result += ',';
            result += '[' + stringify(key) + ',' + structure(descriptor.value, seen, depth + 1) + ']';
        }
        return '[' + (array ? '"array",' + v.length : '"object",null') + ',' + result + ']]';
    }
    function bounded(out) {
        if (out.length > 1024 * 1024) return unsupported();
        return out;
    }
    root.console = create(null);
    const methods = ['log', 'info', 'warn', 'error', 'debug'];
    for (let i = 0; i < methods.length; i++) {
        const method = methods[i];
        root.console[method] = function (...args) {
            entries = 0;
            if (logCount >= 10000) return unsupported();
            logs[logCount++] = '[' + stringify(method) + ',' + structure(args, create(null), 0) + ']';
        };
    }
    return {
        value(v) { return bounded(primitive(v)); },
        thrown(v) {
            if (v instanceof ErrorType) return bounded('["error",' + primitive(v.name) + ',' + primitive(v.message) + ']');
            return bounded(primitive(v));
        },
        trace() {
            entries = 0;
            let out = '[';
            for (let i = 0; i < logCount; i++) out += (i ? ',' : '') + logs[i];
            return bounded('[' + out + '],' + structure(root.__trace, create(null), 0) + ']');
        }
    };
})()
