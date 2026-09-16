function _pattern_open(value) {
    var iterator = Reflect.apply(value[Symbol.iterator], value, []);
    if (iterator === null || (typeof iterator !== 'object' && typeof iterator !== 'function')) throw new TypeError('Iterator is not an object');
    return {iterator: iterator, next: iterator.next, done: false};
}
function _pattern_step(record, read) {
    if (record.done) return;
    try {
        var result = Reflect.apply(record.next, record.iterator, []);
        if (result === null || (typeof result !== 'object' && typeof result !== 'function')) throw new TypeError('Iterator result is not an object');
        if (result.done) { record.done = true; return; }
        if (read) return result.value;
    } catch (error) { record.done = true; throw error; }
}
function _pattern_close(record, abrupt) {
    if (record.done) return;
    record.done = true;
    try {
        var close = record.iterator.return;
        if (close === null || close === void 0) return;
        var result = Reflect.apply(close, record.iterator, []);
        if (result === null || (typeof result !== 'object' && typeof result !== 'function')) throw new TypeError('Iterator close result is not an object');
    } catch (error) { if (!abrupt) throw error; }
}
function _pattern_rest(record) {
    var result = [];
    while (!record.done) {
        var value = _pattern_step(record, true);
        if (!record.done) Object.defineProperty(result, result.length, {value: value, writable: true, enumerable: true, configurable: true});
    }
    return result;
}
function _pattern_object(value) {
    if (value === null || value === void 0) throw new TypeError('Cannot destructure null or undefined');
    return value;
}
function _pattern_key(value) { return Reflect.ownKeys({[value]: 0})[0]; }
function _pattern_object_rest(value, excluded) {
    var result = {}, source = Object(value), keys = Reflect.ownKeys(source);
    for (var i = 0; i < keys.length; i++) {
        var key = keys[i], skip = false;
        for (var j = 0; j < excluded.length; j++) if (excluded[j] === key) { skip = true; break; }
        if (skip) continue;
        var descriptor = Object.getOwnPropertyDescriptor(source, key);
        if (descriptor && descriptor.enumerable) Object.defineProperty(result, key, {value: source[key], writable: true, enumerable: true, configurable: true});
    }
    return result;
}

// Object literal prototype setters ignore primitive values and define no own
// property. Compatibility object splitting must retain that distinction.
function _pattern_prototype(object, value) {
    if (value === null || Object(value) === value)
        Object.setPrototypeOf(object, value);
    return object;
}
