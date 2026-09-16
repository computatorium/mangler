var __RESOURCE_APPLY = Reflect.apply;
var __RESOURCE_PROMISE = (async()=>{})().constructor;
function __RESOURCE_ADD(env, value, asynchronous) {
    var method, inner;
    if (value !== null && value !== void 0) {
        if (Object(value) !== value) throw new TypeError('Object is not disposable');
        if (asynchronous) method = value[Symbol.asyncDispose];
        if (method === null || method === void 0) {
            method = value[Symbol.dispose];
            if (asynchronous) inner = method;
        }
        if (typeof method !== 'function' && !(Object(method) === method && typeof method === 'undefined')) {
            throw new TypeError('Object is not disposable');
        }
        if (asynchronous && inner !== void 0 && inner !== null) {
            method = function () {
                var receiver = this;
                return new __RESOURCE_PROMISE(function (resolve, reject) {
                    try { __RESOURCE_APPLY(inner, receiver, []); resolve(void 0); }
                    catch (error) { reject(error); }
                });
            };
        }
    } else if (!asynchronous) return value;
    Object.defineProperty(env.stack, env.stack.length, {
        value: { value: value, dispose: method, async: asynchronous },
        writable: true, enumerable: true, configurable: true
    });
    return value;
}
function __RESOURCE_SUPPRESS(error, suppressed) {
    if (typeof SuppressedError === 'function') return new SuppressedError(error, suppressed);
    var result = new Error();
    Object.defineProperties(result, {
        name: { value: 'SuppressedError', writable: true, configurable: true },
        error: { value: error, writable: true, configurable: true },
        suppressed: { value: suppressed, writable: true, configurable: true }
    });
    return result;
}
