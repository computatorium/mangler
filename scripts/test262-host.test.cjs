'use strict';
const assert = require('node:assert/strict');
const { test } = require('node:test');
const { createTest262Realm, capabilities } = require('./test262-host.cjs');

test('realm API has fresh intrinsics, native realm functions and correct globals', () => {
  const realm = createTest262Realm();
  const child = realm.host.createRealm();
  const sibling = realm.host.createRealm();
  assert.equal(realm.run('$262.global === globalThis'), true);
  assert.equal(child.global.$262, child);
  assert.equal(child.global.Object.getPrototypeOf(child), child.global.Object.prototype);
  assert.equal(child.global.Object.getPrototypeOf(child.evalScript), child.global.Function.prototype);
  assert.notEqual(child.global.Object, realm.host.global.Object);
  assert.notEqual(child.global.Object, sibling.global.Object);
  assert.equal(child.evalScript('Object'), child.global.Object);
  assert.equal(child.evalScript('this'), child.global);
  for (const name of ['$262', 'print']) {
    const descriptor = realm.run(`Object.getOwnPropertyDescriptor(globalThis, ${JSON.stringify(name)})`);
    assert.equal(descriptor.writable, true);
    assert.equal(descriptor.configurable, true);
    assert.equal(descriptor.enumerable, false);
  }
});

test('evalScript preserves the Script lexical environment and completion values', () => {
  const { host, run } = createTest262Realm();
  assert.equal(host.evalScript('let lexical = 40; var property = 2; lexical + property'), 42);
  assert.equal(host.global.property, 2);
  assert.equal(Object.hasOwn(host.global, 'lexical'), false);
  assert.equal(host.evalScript('lexical += 1'), 41);
  assert.equal(run('lexical'), 41);
  assert.equal(host.createRealm().evalScript('typeof lexical'), 'undefined');
  assert.equal(host.evalScript.call(null, 'lexical'), 41);
  assert.throws(() => host.evalScript('let lexical;'), host.global.SyntaxError);
  assert.throws(() => host.evalScript('return 1;'), host.global.SyntaxError);
  assert.throws(() => host.evalScript('throw new TypeError("realm");'), host.global.TypeError);
  assert.throws(() => host.evalScript({ toString() { throw new Error('must not coerce'); } }), host.global.TypeError);
  const value = host.evalScript('({})');
  host.global.thrownValue = value;
  assert.throws(() => host.evalScript('throw thrownValue'), error => error === value);
});

test('parse and execution errors retain the evaluated realm across callers', () => {
  const parent = createTest262Realm();
  parent.host.global.child = parent.host.createRealm();
  assert.equal(parent.run(`(() => {
    try { child.evalScript('let = ;'); } catch (e) {
      return e.constructor === child.global.SyntaxError && !(e instanceof SyntaxError);
    }
  })()`), true);
  assert.equal(parent.run(`(() => {
    try { child.evalScript('missingName'); } catch (e) {
      return e.constructor === child.global.ReferenceError && !(e instanceof ReferenceError);
    }
  })()`), true);
});

test('detachment uses real transfer for fixed, zero-length, resizable and cross-realm buffers', () => {
  const { host } = createTest262Realm();
  for (const expression of ['new ArrayBuffer(8)', 'new ArrayBuffer(0)', 'new ArrayBuffer(8, {maxByteLength:16})']) {
    const buffer = host.evalScript(expression);
    const view = new Uint8Array(buffer);
    assert.equal(host.detachArrayBuffer(buffer), undefined);
    assert.equal(buffer.byteLength, 0);
    assert.equal(view.byteLength, 0);
    assert.throws(() => new Uint8Array(buffer), TypeError);
    assert.equal(host.detachArrayBuffer(buffer), undefined);
  }
  const foreign = host.createRealm().evalScript('new ArrayBuffer(3)');
  host.detachArrayBuffer(foreign);
  assert.throws(() => new DataView(foreign), TypeError);
});

test('detachment rejects invalid brands and wrong keys without altering the input', () => {
  const { host } = createTest262Realm();
  for (const value of [null, {}, new SharedArrayBuffer(8), new Uint8Array(8), new Proxy(new ArrayBuffer(8), {})]) {
    assert.throws(() => host.detachArrayBuffer(value), host.global.TypeError);
  }
  const buffer = new ArrayBuffer(8);
  assert.throws(() => host.detachArrayBuffer(buffer, 'wrong'), host.global.TypeError);
  assert.equal(buffer.byteLength, 8);
  const wasm = new WebAssembly.Memory({ initial: 1 }).buffer;
  assert.throws(() => host.detachArrayBuffer(wasm), host.global.TypeError);
  assert.equal(wasm.byteLength, 65536);
});

test('unsupported host capabilities are explicit and garbage collection is genuine', () => {
  const { host } = createTest262Realm();
  assert.equal(Object.hasOwn(host, 'IsHTMLDDA'), false);
  for (const capability of ['AbstractModuleSource']) {
    assert.throws(() => host[capability], error => error.code === 'MANGLER_TEST262_UNSUPPORTED_HOST' && error.capability === capability);
  }
  if (capabilities.gc) assert.equal(host.gc(), undefined);
  else assert.throws(() => host.gc(), error => error.capability === 'gc');
});

test('print converts once and nested eval obeys the execution deadline', () => {
  const output = [];
  const realm = createTest262Realm({ print: value => output.push(value), timeout: 20 });
  realm.run('print({toString(){ return "message"; }})');
  realm.host.createRealm().evalScript('print(42)');
  assert.deepEqual(output, ['message', '42']);
  assert.throws(() => realm.host.evalScript('while(true){}'), error => error.code === 'ERR_SCRIPT_EXECUTION_TIMEOUT');
});
