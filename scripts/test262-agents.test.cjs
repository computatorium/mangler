'use strict';
const assert = require('node:assert/strict');
const { test } = require('node:test');
const { createTest262Realm } = require('./test262-host.cjs');

function nextReport(agent) {
  for (;;) {
    const report = agent.getReport();
    if (report !== null) return report;
    agent.sleep(1);
  }
}

test('real workers share memory and acknowledge reception before a blocking callback', async () => {
  const realm = createTest262Realm({ timeout: 3000 });
  try {
    const a = realm.host.agent;
    assert.equal(a.getReport(), null);
    a.start(`$262.agent.receiveBroadcast((buffer, id) => {
      const view = new Int32Array(buffer);
      if (!(buffer instanceof SharedArrayBuffer)) throw Error('wrong buffer realm');
      Atomics.store(view, 0, 1);
      Atomics.wait(view, 1, 0);
      $262.agent.report(id + ':' + Atomics.add(view, 0, 1));
      $262.agent.leaving();
    });`);
    const buffer = new SharedArrayBuffer(8), view = new Int32Array(buffer);
    a.broadcast(buffer, 37);
    Atomics.store(view, 1, 1);
    Atomics.notify(view, 1);
    assert.equal(nextReport(a), '37:1');
    assert.equal(view[0], 2);
    assert.equal(a.getReport(), null);
  } finally { await realm.dispose(); }
});

test('broadcast reaches every active worker, preserves BigInt and supports later broadcasts', async () => {
  const realm = createTest262Realm({ timeout: 3000 });
  try {
    const a = realm.host.agent;
    for (let i = 0; i < 3; i++) a.start(`
      $262.agent.receiveBroadcast((buffer, id) => {
        Atomics.add(new Int32Array(buffer), 0, 1);
        $262.agent.report(typeof id + ':' + id);
      });
      $262.agent.receiveBroadcast((buffer, id) => {
        Atomics.add(new Int32Array(buffer), 0, 1);
        $262.agent.report(id);
        $262.agent.leaving();
      });
    `);
    const buffer = new SharedArrayBuffer(4), view = new Int32Array(buffer);
    a.broadcast(buffer, 9007199254740993n);
    assert.deepEqual([nextReport(a), nextReport(a), nextReport(a)], Array(3).fill('bigint:9007199254740993'));
    a.broadcast(buffer, -1);
    assert.deepEqual([nextReport(a), nextReport(a), nextReport(a)], Array(3).fill('-1'));
    assert.equal(view[0], 6);
  } finally { await realm.dispose(); }
});

test('reports convert once, preserve per-agent FIFO and child realms use the same agent cluster', async () => {
  const realm = createTest262Realm({ timeout: 3000 });
  try {
    const child = realm.host.createRealm();
    child.agent.start(`let conversions=0; String=()=>{throw Error('mutated String')};
      $262.agent.report({toString(){conversions++;return 'first'}});
      $262.agent.report(conversions);
      $262.agent.report('');
      $262.agent.leaving();`);
    assert.equal(nextReport(realm.host.agent), 'first');
    assert.equal(nextReport(realm.host.agent), '1');
    assert.equal(nextReport(realm.host.agent), '');
    assert.equal(Object.getPrototypeOf(child.agent), child.global.Object.prototype);
    assert.equal(Object.getPrototypeOf(child.agent.start), child.global.Function.prototype);
  } finally { await realm.dispose(); }
});

test('async broadcast callbacks stay alive while Atomics.waitAsync is pending', async () => {
  const realm = createTest262Realm({ timeout: 3000 });
  try {
    const a = realm.host.agent;
    a.start(`$262.agent.receiveBroadcast(async buffer => {
      const view = new Int32Array(buffer);
      const waiting = Atomics.waitAsync(view, 0, 0);
      $262.agent.report(waiting.async);
      $262.agent.report(await waiting.value);
      $262.agent.leaving();
    });`);
    const buffer = new SharedArrayBuffer(4), view = new Int32Array(buffer);
    a.broadcast(buffer);
    assert.equal(nextReport(a), 'true');
    Atomics.store(view, 0, 1);
    Atomics.notify(view, 0);
    assert.equal(nextReport(a), 'ok');
  } finally { await realm.dispose(); }
});

test('sleep blocks for elapsed time and monotonic clocks use a shared origin', async () => {
  const realm = createTest262Realm({ timeout: 3000 });
  try {
    const a = realm.host.agent;
    const before = a.monotonicNow();
    a.sleep(20);
    assert.ok(a.monotonicNow() - before >= 15);
    a.start(`const before=$262.agent.monotonicNow();$262.agent.sleep(20);$262.agent.report($262.agent.monotonicNow()-before);$262.agent.leaving();`);
    assert.ok(Number(nextReport(a)) >= 15);
  } finally { await realm.dispose(); }
});

test('worker exceptions and missing broadcasts fail within the deadline and dispose terminates busy workers', async () => {
  const invalid = createTest262Realm({ timeout: 1000 });
  try {
    assert.throws(() => invalid.host.agent.start('let = ;'), /Test262 agent/);
  } finally { await invalid.dispose(); }
  const stalled = createTest262Realm({ timeout: 100 });
  try {
    stalled.host.agent.start('while(true){}');
    assert.throws(() => stalled.host.agent.broadcast(new SharedArrayBuffer(4)), /deadline|timed out|stopped/);
  } finally { await stalled.dispose(); }
  const busy = createTest262Realm({ timeout: 3000 });
  busy.host.agent.start('while(true){}');
  const start = Date.now();
  await busy.dispose();
  assert.ok(Date.now() - start < 1000);
});

test('Node permission denial is reported as an unavailable agent host', () => {
  const { spawnSync } = require('node:child_process');
  const source = `const {createTest262Realm}=require(${JSON.stringify(require.resolve('./test262-host.cjs'))});
    const unavailable=[];const realm=createTest262Realm({onUnsupported:x=>unavailable.push(x)});
    try{realm.host.agent.start('42')}catch(error){
      if(error.code!=='ERR_ACCESS_DENIED')throw error;
    }finally{realm.dispose()}
    if(JSON.stringify(unavailable)!=='["agent"]')throw Error('missing unavailable host report');`;
  const result = spawnSync(process.execPath, ['--permission', '--allow-fs-read=*', '-e', source], { encoding: 'utf8', timeout: 3000 });
  assert.equal(result.status, 0, result.stderr);
});
