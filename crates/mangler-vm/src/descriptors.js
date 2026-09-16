(function (prototype, names, symbols, own, apply, enumerable) {
    function descriptor(value) {
        if (prototype(value) === null) return value;
        var result = {__proto__: null};
        var fields = ['enumerable', 'configurable', 'value', 'writable', 'get', 'set'];
        for (var i = 0; i < fields.length; i++) {
            var field = fields[i];
            if (apply(own, value, [field])) result[field] = value[field];
        }
        return result;
    }
    function descriptors(values) {
        var result = {__proto__: null};
        // These maps are compiler-owned ordinary objects, never source proxies.
        // Their native string-key list followed by symbol keys is their own-key
        // order, without requiring recovery of the Reflect namespace.
        var groups = [names(values), symbols(values)];
        for (var group = 0; group < groups.length; group++) {
            var keys = groups[group];
            for (var i = 0; i < keys.length; i++) {
                var key = keys[i];
                if (apply(enumerable, values, [key])) result[key] = descriptor(values[key]);
            }
        }
        return result;
    }
    return function (kind, intrinsic) {
        if (kind === 0) return function (target, key, value) {
            return intrinsic(target, key, descriptor(value));
        };
        if (kind === 1) return function (target, values) {
            return intrinsic(target, descriptors(values));
        };
        if (kind === 2) return function (target, values) {
            return values === undefined ? intrinsic(target) : intrinsic(target, descriptors(values));
        };
        if (kind === 3) return function (target, key) {
            var value = intrinsic(target, key);
            return value === undefined ? value : descriptor(value);
        };
        return function (target) {
            return descriptors(intrinsic(target));
        };
    };
})
