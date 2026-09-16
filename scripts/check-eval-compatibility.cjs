// Exercise the real host's compatibility guard using minimal actual Wasm modules.
const fs = require('node:fs'), vm = require('node:vm'), assert = require('node:assert/strict');
const path = require('node:path');
const context = vm.createContext({WebAssembly, TextEncoder, TextDecoder});
new vm.Script(fs.readFileSync(path.join(__dirname, '../crates/mangler-eval/host/compiler.js'), 'utf8') + '\nglobalThis.Compiler=ManglerEvalCompiler;').runInContext(context);
const unsigned = n => {const out=[];do{let b=n&127;n>>>=7;out.push(b|(n?128:0));}while(n);return out;};
const signed = n => {const out=[];for(;;){const byte=Number(n&127n);n>>=7n;const done=(n===0n&&!(byte&64))||(n===-1n&&(byte&64));out.push(byte|(done?0:128));if(done)return out;}};
const name = s => [...unsigned(s.length), ...Buffer.from(s)];
const section = (id, body) => [id, ...unsigned(body.length), ...body];
function moduleBytes(value, includeFingerprint = true) {
  const body1=[0,0x41,1,0x0b],body2=[0,0x42,...signed(value),0x0b];
  return Uint8Array.from([0,97,115,109,1,0,0,0,
    ...section(1,[2,0x60,0,1,0x7f,0x60,0,1,0x7e]),
    ...section(3,[2,0,1]),
    ...section(7,[includeFingerprint?2:1,...name('mangler_abi_version'),0,0,...(includeFingerprint?[...name('mangler_compiler_fingerprint'),0,1]:[])]),
    ...section(10,[2,...unsigned(body1.length),...body1,...unsigned(body2.length),...body2])]);
}
for (const value of [1n, -1n, -9223372036854775808n, 9223372036854775807n]) {
  const matched = new context.Compiler(moduleBytes(value), [], {compilerFingerprint:value});
  assert.equal(matched.exports.mangler_compiler_fingerprint(), value);
  assert.throws(()=>new context.Compiler(moduleBytes(value), [], {compilerFingerprint:value^1n}), /build mismatch.*Rebuild/);
}
assert.throws(()=>new context.Compiler(moduleBytes(1n,false), [], {compilerFingerprint:1n}), /build mismatch.*Rebuild/);
assert.throws(()=>new context.Compiler(moduleBytes(1n), [], {}), /build mismatch.*Rebuild/);
console.log('Compiler fingerprint guard: 4 matched, 6 mismatch/missing rejection checks passed');
