(function () {
    const apply = Reflect.apply;
    const bind = Function.prototype.bind;
    const describe = Object.getOwnPropertyDescriptor;
    const hasOwn = Object.prototype.hasOwnProperty;
    const keys = Reflect.ownKeys;
    const prototype = Object.getPrototypeOf;
    const create = Object.create;
    const stringify = Function.prototype.toString;
    const indexOf = String.prototype.indexOf;
    const get = WeakMap.prototype.get;
    const set = WeakMap.prototype.set;
    const records = new WeakMap();
    const roots = create(null);
    const names = [__INTRINSIC_ROOTS__];
    const pending = [];
    const failure = TypeError;
    const adaptDescriptor = (__DESCRIPTOR_FACTORY__)(prototype, Object.getOwnPropertyNames, Object.getOwnPropertySymbols, hasOwn, apply, Object.prototype.propertyIsEnumerable);
    const descriptorAdapters = create(null);
    function native(value) {
        return typeof value === 'function' &&
            apply(indexOf, apply(stringify, value, []), ['[native code]']) >= 0;
    }
    function remember(value) {
        if (value === null || (typeof value !== 'object' && typeof value !== 'function')) return;
        if (apply(get, records, [value])) return;
        const record = {properties: create(null), parent: null};
        apply(set, records, [value, record]);
        pending[pending.length] = value;
    }
    for (let i = 0; i < names.length; i++) {
        const name = names[i][0];
        roots[name] = names[i][1];
        remember(roots[name]);
    }
    for (let i = 0; i < pending.length; i++) {
        const value = pending[i];
        const record = apply(get, records, [value]);
        record.parent = prototype(value);
        remember(record.parent);
        const own = keys(value);
        for (let j = 0; j < own.length; j++) {
            const key = own[j];
            const descriptor = describe(value, key);
            record.properties[key] = descriptor;
            // Intrinsic function members and their prototypes are the helper
            // graph. Other object-valued data remains an identity, not a crawl
            // through arbitrary objects added to globals by embedding code.
            if (key === 'prototype' || native(descriptor.value)) remember(descriptor.value);
        }
    }
    function property(receiver, key) {
        let value = receiver;
        while (value !== null) {
            const record = apply(get, records, [value]);
            if (!record) throw new failure('Unknown helper intrinsic path');
            const descriptor = record.properties[key];
            if (descriptor) {
                if (apply(hasOwn, descriptor, ['value'])) return descriptor.value;
                return descriptor.get === undefined ? undefined : apply(descriptor.get, receiver, []);
            }
            value = record.parent;
        }
        return undefined;
    }
    return function (path, bindReceiver) {
        let value = roots[path[0]];
        let receiver;
        for (let i = 1; i < path.length; i++) {
            receiver = value;
            value = property(value, path[i]);
        }
        if (path.length === 2) {
            const root = path[0], member = path[1];
            let kind;
            if ((root === 'Object' || root === 'Reflect') && member === 'defineProperty') kind = 0;
            else if (root === 'Object' && member === 'defineProperties') kind = 1;
            else if (root === 'Object' && member === 'create') kind = 2;
            else if ((root === 'Object' || root === 'Reflect') && member === 'getOwnPropertyDescriptor') kind = 3;
            else if (root === 'Object' && member === 'getOwnPropertyDescriptors') kind = 4;
            if (kind !== undefined) {
                const key = root + '.' + member;
                return descriptorAdapters[key] || (descriptorAdapters[key] = adaptDescriptor(kind, value));
            }
        }
        const last = path[path.length - 1];
        if (bindReceiver && path[0] !== 'Reflect' && (last === 'call' || last === 'apply' || last === 'bind')) {
            return apply(bind, value, [receiver]);
        }
        return value;
    };
})
