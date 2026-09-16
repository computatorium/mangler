'use strict';

// Test262 INTERPRETING.md host contract. Each context has independent intrinsics
// and a Script global environment, including persistent lexical declarations.
const vm = require('node:vm');
let workerThreads;
try { workerThreads = require('node:worker_threads'); } catch {}
const transfer = globalThis.structuredClone;
const collect = globalThis.gc;
const byteLength = Object.getOwnPropertyDescriptor(ArrayBuffer.prototype, 'byteLength').get;
const apply = Reflect.apply;
const Uint8ArrayIntrinsic = Uint8Array;

const capabilities = Object.freeze({
  createRealm: true,
  evalScript: true,
  detachArrayBuffer: typeof transfer === 'function',
  gc: typeof collect === 'function',
  agent: !!workerThreads?.Worker && typeof SharedArrayBuffer === 'function' && typeof Atomics.wait === 'function',
  IsHTMLDDA: false,
  AbstractModuleSource: false,
});

// Build the API's objects/functions in their own realm. The bridge is a closure
// argument; no temporary Node capability is installed on the test global.
const installSource = `(bridge => {
  const TypeErrorIntrinsic = TypeError;
  const ErrorIntrinsic = Error;
  const StringIntrinsic = String;
  function unsupported(capability) {
    bridge.onUnsupported(capability);
    const error = new ErrorIntrinsic('Unsupported Test262 host capability: ' + capability);
    error.name = 'Test262UnsupportedHostError';
    error.code = 'MANGLER_TEST262_UNSUPPORTED_HOST';
    error.capability = capability;
    throw error;
  }
  const host = {
    global: globalThis,
    createRealm() { return bridge.createRealm(); },
    evalScript(source) {
      if (typeof source !== 'string') throw new TypeErrorIntrinsic('evalScript requires a string');
      return bridge.run(source);
    },
    detachArrayBuffer(buffer, key) {
      if (!bridge.canDetach) return unsupported('detachArrayBuffer');
      if (key !== undefined) throw new TypeErrorIntrinsic('ArrayBuffer detach key mismatch');
      const error = bridge.detach(buffer);
      if (error !== undefined) throw new TypeErrorIntrinsic(error);
    },
    gc() {
      if (!bridge.canCollect) return unsupported('gc');
      return bridge.collect();
    },
  };
  for (const capability of ['AbstractModuleSource']) {
    Object.defineProperty(host, capability, {
      configurable: true,
      get() { return unsupported(capability); },
    });
  }
  Object.defineProperty(host, 'agent', {
    value: bridge.agent || undefined, writable: true, configurable: true, enumerable: true,
  });
  if (!bridge.agent) Object.defineProperty(host, 'agent', { get() { return unsupported('agent'); } });
  Object.defineProperties(globalThis, {
    $262: { value: host, writable: true, configurable: true },
    print: {
      value: function print(value) { bridge.print(StringIntrinsic(value)); },
      writable: true, configurable: true,
    },
  });
  return host;
})`;

function detach(buffer) {
  // The intrinsic getter performs a cross-realm ArrayBuffer brand check and
  // rejects SharedArrayBuffer, views, proxies and forged prototypes.
  try { apply(byteLength, buffer, []); }
  catch { return 'detachArrayBuffer requires an ArrayBuffer'; }
  // DetachArrayBuffer is idempotent. A zero-length attached buffer must still
  // transfer; constructing a view distinguishes it from a detached buffer.
  try { new Uint8ArrayIntrinsic(buffer, 0, 0); }
  catch { return undefined; }
  try { transfer(buffer, { transfer: [buffer] }); }
  catch { return 'ArrayBuffer cannot be detached'; }
  return undefined;
}

function createTest262Realm({ globals = {}, timeout, print = console.log, onUnsupported = () => {}, onError = () => {}, agentCluster, initializeScriptOptions } = {}) {
  if (timeout !== undefined && (!Number.isInteger(timeout) || timeout < 1)) {
    throw new RangeError('Test262 timeout must be a positive integer in milliseconds');
  }
  const context = vm.createContext({ ...globals });
  let scriptOptions = {};
  const run = (source, options = {}) => vm.runInContext(source, context, {
    filename: 'test262-evalScript.js', ...scriptOptions, ...options,
    ...(timeout === undefined ? {} : { timeout }),
  });
  // Each child realm installs its own loader and captures its own intrinsics.
  // The host supplies no loader by default and never shares module records
  // across contexts. Explicit run options remain authoritative for that Script.
  if (initializeScriptOptions) scriptOptions = initializeScriptOptions({ context, run });
  const cluster = agentCluster || (capabilities.agent ? createAgentCluster({ timeout, onError, onUnsupported }) : null);
  const host = run(installSource, { filename: 'test262-host.js' })({
    createRealm: () => createTest262Realm({ globals, timeout, print, onUnsupported, onError, agentCluster: cluster, initializeScriptOptions }).host,
    run,
    print,
    onUnsupported,
    agent: cluster ? run(agentFactorySource)(cluster) : null,
    detach,
    canDetach: capabilities.detachArrayBuffer,
    canCollect: capabilities.gc,
    collect: () => collect(),
  });
  return { context, host, run, checkAgents: () => cluster?.check(), dispose: () => cluster?.dispose() };
}

// A MessagePort transports the real shared buffer and report strings. Atomic
// control words let parent calls synchronize without pumping Node's event loop.
// Slots: 0 wake generation, 1 started, 2 broadcast acknowledgement, 3 state
// (0 active, 1 leaving, 2 failed). Acknowledgement precedes the callback: the
// callback may wait for a subsequent parent-side Atomics.notify.
const agentFactorySource = `(bridge => {
  const StringIntrinsic = String;
  const TypeErrorIntrinsic = TypeError;
  const agent = {
    sleep(milliseconds) { bridge.sleep(+milliseconds); },
    monotonicNow() { return bridge.now(); },
  };
  if (bridge.start) {
    agent.start = function start(source) {
      if (typeof source !== 'string') throw new TypeErrorIntrinsic('agent.start requires source text');
      bridge.start(source);
    };
    agent.broadcast = function broadcast(buffer, id) {
      bridge.broadcast(buffer, typeof id === 'bigint' ? id : id | 0);
    };
    agent.getReport = function getReport() { return bridge.getReport(); };
  } else {
    agent.receiveBroadcast = function receiveBroadcast(callback) {
      if (typeof callback !== 'function') throw new TypeErrorIntrinsic('receiveBroadcast requires a callback');
      return bridge.receiveBroadcast(callback);
    };
    agent.report = function report(message) { bridge.report(StringIntrinsic(message)); };
    agent.leaving = function leaving() { bridge.leaving(); };
  }
  return agent;
})`;

function agentWorkerMain(data) {
  const vm = require('node:vm');
  const { moveMessagePortToContext, receiveMessageOnPort } = require('node:worker_threads');
  const { performance } = require('node:perf_hooks');
  const control = new Int32Array(data.control);
  const context = vm.createContext({ setTimeout, clearTimeout, queueMicrotask });
  // Node creates received objects in this context, including the SAB wrapper;
  // no prototype replacement or copied data masquerades as a realm transfer.
  const port = moveMessagePortToContext(data.port, context);
  const remaining = () => Math.max(0, data.deadline - (performance.timeOrigin + performance.now()));
  const wake = () => { for (let i = 0; i < 4; i++) Atomics.notify(control, i); };
  const keepAlive = setInterval(() => {}, 1000);
  function fail(error) {
    port.postMessage({ type: 'error', message: error && error.stack || String(error) });
    Atomics.store(control, 3, 2);
    wake();
    clearInterval(keepAlive);
    port.close();
  }
  process.on('uncaughtException', fail);
  process.on('unhandledRejection', fail);
  const bridge = {
    now: () => performance.timeOrigin + performance.now(),
    sleep: milliseconds => {
      const delay = Math.max(0, milliseconds || 0);
      if (delay > remaining()) throw Error('Test262 agent deadline exceeded');
      Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, delay);
    },
    report: message => port.postMessage({ type: 'report', message }),
    leaving: () => {
      Atomics.store(control, 3, 1);
      wake();
      clearInterval(keepAlive);
      port.close();
    },
    receiveBroadcast: callback => {
      for (;;) {
        const generation = Atomics.load(control, 0);
        const received = receiveMessageOnPort(port);
        if (received) {
          const message = received.message;
          Atomics.store(control, 2, message.sequence);
          Atomics.notify(control, 2);
          return callback(message.buffer, message.id);
        }
        const delay = remaining();
        if (!delay) throw Error('Test262 agent broadcast deadline exceeded');
        Atomics.wait(control, 0, generation, delay);
      }
    },
  };
  try {
    const agent = vm.runInContext(data.factory, context)(bridge);
    vm.runInContext('(agent => Object.defineProperty(globalThis,"$262",{value:{agent},writable:true,configurable:true}))', context)(agent);
    const script = new vm.Script(data.source, { filename: 'test262-agent.js' });
    Atomics.store(control, 1, 1);
    Atomics.notify(control, 1);
    script.runInContext(context, { timeout: Math.max(1, Math.ceil(remaining())) });
  } catch (error) { fail(error); }
}

function createAgentCluster({ timeout = 10000, onError = () => {}, onUnsupported = () => {} } = {}) {
  const { Worker, MessageChannel, receiveMessageOnPort } = workerThreads;
  const { performance } = require('node:perf_hooks');
  const clock = () => performance.timeOrigin + performance.now();
  const deadline = clock() + timeout;
  const sharedLength = Object.getOwnPropertyDescriptor(SharedArrayBuffer.prototype, 'byteLength').get;
  const sleeper = new Int32Array(new SharedArrayBuffer(4));
  const workers = [], reports = [];
  let failure, closed = false, sequence = 0, cleanup;
  const remaining = () => Math.max(0, deadline - clock());
  function recordFailure(error) {
    if (!failure) { failure = error; onError(error); }
  }
  function drain() {
    for (const record of workers) {
      for (let entry; (entry = receiveMessageOnPort(record.port));) {
        if (entry.message.type === 'error') recordFailure(new Error('Test262 agent: ' + entry.message.message));
        else reports.push(entry.message.message);
      }
    }
  }
  function check() {
    drain();
    if (failure) throw failure;
  }
  function active() {
    check();
    if (closed) throw Error('Test262 agent cluster is closed');
    if (!remaining()) throw Error('Test262 agent deadline exceeded');
  }
  function waitFor(record, slot, value) {
    while (Atomics.load(record.control, slot) !== value) {
      active();
      if (Atomics.load(record.control, 3) !== 0) {
        check();
        throw Error('Test262 agent stopped before synchronization');
      }
      const current = Atomics.load(record.control, slot);
      if (current !== value) Atomics.wait(record.control, slot, current, Math.min(10, remaining()));
    }
    check();
  }
  function dispose() {
    if (!cleanup) {
      closed = true;
      clearTimeout(timer);
      clearInterval(pump);
      cleanup = Promise.all(workers.map(record => {
        record.port.close();
        return record.worker.terminate();
      }));
    }
    return cleanup;
  }
  const pump = setInterval(() => { drain(); if (failure) void dispose(); }, 5);
  pump.unref();
  const timer = setTimeout(() => {
    if (workers.length) recordFailure(new Error('Test262 agent deadline exceeded'));
    void dispose();
  }, timeout);
  timer.unref();
  return {
    check, dispose,
    now: clock,
    sleep: milliseconds => {
      active();
      const delay = Math.max(0, milliseconds || 0);
      if (delay > remaining()) throw Error('Test262 agent deadline exceeded');
      Atomics.wait(sleeper, 0, 0, delay);
      check();
    },
    start: source => {
      active();
      const { port1, port2 } = new MessageChannel();
      const control = new Int32Array(new SharedArrayBuffer(16));
      let worker;
      try { worker = new Worker('(' + agentWorkerMain.toString() + ')(require("node:worker_threads").workerData)', {
        eval: true,
        workerData: { source, factory: agentFactorySource, port: port2, control: control.buffer, deadline },
        transferList: [port2],
      }); } catch (error) {
        port1.close(); port2.close();
        if (error.code === 'ERR_ACCESS_DENIED' || error.code === 'ERR_WORKER_UNSUPPORTED_OPERATION') onUnsupported('agent');
        throw error;
      }
      const record = { worker, port: port1, control };
      workers.push(record);
      worker.on('error', recordFailure);
      worker.on('exit', code => {
        if (!closed && code && Atomics.load(control, 3) !== 1) recordFailure(new Error('Test262 agent exited with code ' + code));
      });
      waitFor(record, 1, 1);
    },
    broadcast: (buffer, id) => {
      active();
      apply(sharedLength, buffer, []);
      const recipients = workers.filter(record => Atomics.load(record.control, 3) === 0);
      const next = ++sequence;
      for (const record of recipients) {
        record.port.postMessage({ buffer, id, sequence: next });
        Atomics.add(record.control, 0, 1);
        Atomics.notify(record.control, 0);
      }
      for (const record of recipients) waitFor(record, 2, next);
    },
    getReport: () => { active(); return reports.length ? reports.shift() : null; },
  };
}

module.exports = { createTest262Realm, capabilities };
