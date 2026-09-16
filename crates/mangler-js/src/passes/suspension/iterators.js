// Checked iterator acquisition shared by generator delegation and for-await.
function _ts_values(value) {
    var method = value[Symbol.iterator];
    if (typeof method !== "function") throw new TypeError("Object is not iterable");
    var iterator = Reflect.apply(method, value, []);
    if (iterator === null || (typeof iterator !== "object" && typeof iterator !== "function"))
        throw new TypeError("Iterator is not an object");
    return iterator;
}
function _async_iterator(value) {
    var method = value[Symbol.asyncIterator];
    if (method != null) {
        if (typeof method !== "function") throw new TypeError("Async iterator method is not callable");
        var iterator = check(Reflect.apply(method, value, [])), next = iterator.next;
        if (typeof next !== "function") throw new TypeError("Iterator next is not callable");
        var record = {validate: check, next: function () { return Reflect.apply(next, iterator, arguments); }};
        // SWC's for-await close guard reads return before invoking it. Cache that
        // GetMethod until the call, so accessors run once per close operation.
        forward("return"); forward("throw");
        return record;
    }
    method = value[Symbol.iterator];
    if (typeof method !== "function") throw new TypeError("Object is not async iterable");
    var sync = check(Reflect.apply(method, value, [])), syncNext = sync.next;
    if (typeof syncNext !== "function") throw new TypeError("Iterator next is not callable");
    return {
        validate: check,
        next: function () { try { return continuation(Reflect.apply(syncNext, sync, arguments), true); } catch(e) { return Promise.reject(e); } },
        return: function (value) {
            try {
                var method = sync.return;
                if (method == null) return Promise.resolve({value: value, done: true});
                return continuation(Reflect.apply(method, sync, arguments), false);
            } catch(e) { return Promise.reject(e); }
        },
        throw: function (value) {
            try {
                var method = sync.throw;
                if (method == null) {
                    var close = sync.return;
                    if (close != null) check(Reflect.apply(close, sync, []));
                    return Promise.reject(new TypeError("The iterator does not provide a throw method"));
                }
                return continuation(Reflect.apply(method, sync, arguments), true);
            } catch(e) { return Promise.reject(e); }
        }
    };
    function check(result) {
        if (result === null || (typeof result !== "object" && typeof result !== "function"))
            throw new TypeError("Iterator result is not an object");
        return result;
    }
    function closeRejected(error) {
        // IteratorClose with a throwing completion preserves the original error,
        // including when obtaining or invoking return throws during cleanup.
        try {
            var method = sync.return;
            if (method != null) Reflect.apply(method, sync, []);
        } catch (_) {}
        throw error;
    }
    function continuation(result, closeOnRejection) {
        check(result);
        var done = !!result.done, value = result.value;
        // The shared native-await bridge performs PromiseResolve and invokes
        // the rejection continuation, including synchronous constructor errors.
        // Reading result.value remains outside that protected Promise operation.
        return Promise.resolve(value).then(function (value) { return {value: value, done: done}; },
            !done && closeOnRejection ? closeRejected : undefined);
    }
    function forward(key) {
        var cached, obtained = false;
        Object.defineProperty(record, key, {get: function () {
            if (!obtained) { cached = iterator[key]; obtained = true; }
            if (cached == null) return undefined;
            return function () {
                var method = cached; obtained = false;
                return Reflect.apply(method, iterator, arguments);
            };
        }});
    }
}
