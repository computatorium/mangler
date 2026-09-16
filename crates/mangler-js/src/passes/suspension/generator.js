// State protocol used by SWC's generator lowering, with ECMAScript iterator
// validation and cached next methods for delegated iteration.
function _ts_generator(thisArg, body) {
    var executing, delegate, delegateNext, delegateReturn, sent, started = false;
    var state = {
        label: 0,
        sent: function () { if (sent[0] & 1) throw sent[1]; return sent[1]; },
        trys: [], ops: []
    };
    var iterator = Object.create((typeof Iterator === "function" ? Iterator : Object).prototype);
    Object.defineProperties(iterator, {
        internalReturn: {value: function(value) { return step([2, value, true]); }},
        hasReturn: {value: function(value) {
            if (state) for (var i = 0; i < state.ops.length; i++) {
                var op = state.ops[i];
                if (op[0] === 2 && op[2] && op[1] === value) return true;
            }
            return !!(delegate && delegate.hasReturn(value));
        }},
        next: {value: verb(0), writable: true, configurable: true},
        throw: {value: verb(1), writable: true, configurable: true},
        return: {value: verb(2), writable: true, configurable: true}
    });
    Object.defineProperty(iterator, Symbol.iterator, {
        value: function () { return this; }, writable: true, configurable: true
    });
    Object.defineProperty(iterator, Symbol.toStringTag, {value: "Generator", configurable: true});
    return iterator;

    function verb(kind) { return function (value) { return step([kind, value]); }; }
    function check(result) {
        if (result === null || (typeof result !== "object" && typeof result !== "function"))
            throw new TypeError("Iterator result is not an object");
        return result;
    }
    function step(op) {
        if (executing) throw new TypeError("Generator is already executing.");
        if (!started) { started = true; if (op[0]) state = 0; }
        while (state) try {
            executing = true;
            if (delegate) {
                var method, result;
                if (op[0] === 0) method = delegateNext;
                else if (op[0] === 2) {
                    if (op[2]) delegateReturn = op[1];
                    method = op[2] ? delegate.internalReturn : delegate.return;
                }
                else {
                    method = delegate.throw;
                    if (method == null) {
                        method = delegate.return;
                        if (method != null) check(Reflect.apply(method, delegate, []));
                        throw new TypeError("The iterator does not provide a throw method");
                    }
                }
                if (method != null) {
                    result = check(Reflect.apply(method, delegate, [op[1]]));
                    if (!result.done) return result;
                    // Internal loop callbacks can cancel a return using source
                    // break/continue. They are not public source delegation.
                    // A delegated loop/pattern callback can yield from finally.
                    // Its return token must survive the subsequent next/throw
                    // request until the callback completes or cancels it.
                    var value = result.value;
                    op = delegateReturn
                        ? [value === delegateReturn ? 2 : 0, value, value === delegateReturn]
                        : [op[0] & 2, value, op[2]];
                }
                delegate = delegateNext = delegateReturn = 0;
            }
            switch (op[0]) {
                case 0: case 1: sent = op; break;
                case 4: state.label++; return {value: op[1], done: false};
                case 5:
                    state.label++;
                    delegate = op[1]; delegateNext = delegate.next;
                    if (typeof delegateNext !== "function") throw new TypeError("Iterator next is not callable");
                    op = [0]; continue;
                case 7: op = state.ops.pop(); state.trys.pop(); continue;
                default:
                    var entry = state.trys.length ? state.trys[state.trys.length - 1] : null;
                    if (!entry && (op[0] === 6 || op[0] === 2)) { state = 0; continue; }
                    if (op[0] === 3 && (!entry || (op[1] > entry[0] && op[1] < entry[3]))) {
                        state.label = op[1]; break;
                    }
                    if (op[0] === 6 && state.label < entry[1]) { state.label = entry[1]; sent = op; break; }
                    if (entry && state.label < entry[2]) { state.label = entry[2]; state.ops.push(op); break; }
                    if (entry[2]) state.ops.pop();
                    state.trys.pop(); continue;
            }
            op = Reflect.apply(body, thisArg, [state]);
        } catch (error) { op = [6, error]; delegate = delegateNext = delegateReturn = 0; }
        finally { executing = false; }
        if (op[0] & 5) throw op[1];
        return {value: op[0] ? op[1] : void 0, done: true};
    }
}
