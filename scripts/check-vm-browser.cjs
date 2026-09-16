// Actual Chrome execution of representative required-virtualized artifacts.
// Supply --binary and optionally --chrome, --output, --presets, --fixtures.
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const cp = require('node:child_process');
const crypto = require('node:crypto');
const {pathToFileURL} = require('node:url');
const {fixtures} = require('./check-vm-coverage.cjs');
let binary, output, chrome = '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome';
let presets = ['minify', 'high'];
let names = ['payment_async', 'async_generator', 'await_mutated_promise_resolve',
  'await_replaced_global_promise', 'dynamic_async_function_constructor',
  'async_function_constructor_identity', 'class_private', 'class_super',
  'private_brand_primitive', 'arguments_alias_indirect', 'arguments_object',
  'duplicate_parameter_alias', 'async_arguments_alias_callee', 'async_arguments_write_parameter',
  'mutated_string_charcode', 'mutated_array_methods',
  'hoisted_intrinsic_function', 'hoisted_reflect_function', 'hoisted_symbol_function',
  'shadowed_global_and_intrinsic', 'document_all_nullish', 'document_all_optional_call'];
const args = process.argv.slice(2);
while (args.length) {
  const flag = args.shift(), value = args.shift();
  if (!value) throw Error(`Missing value for ${flag}`);
  if (flag === '--binary') binary = path.resolve(value);
  else if (flag === '--chrome') chrome = value;
  else if (flag === '--output') output = value;
  else if (flag === '--presets') presets = value.split(',');
  else if (flag === '--fixtures') names = value.split(',');
  else throw Error(`Unknown option ${flag}`);
}
if (!binary) throw Error('Supply --binary');
const hash = () => crypto.createHash('sha256').update(fs.readFileSync(binary)).digest('hex');
const binaryHash = hash();
const rows = [], results = [];
const known = new Map(fixtures.map(f => [f[0], f]));
// IsHTMLDDA is supplied only by browsers and cannot be emulated in Node.
known.set('document_all_nullish', ['document_all_nullish',
  'function pay(){let x=document.all;return [(x??7)===x,x?.length===x.length]}', 'pay()']);
known.set('document_all_optional_call', ['document_all_optional_call',
  'function pay(){let e=document.createElement("div");e.id="payment-probe";document.body.appendChild(e);let x=document.all;return [x?.("payment-probe")===e,x?.missing?.()]}', 'pay()']);
for (const name of names) {
  const fixture = known.get(name);
  if (!fixture) throw Error(`Unknown fixture ${name}`);
  const [, declarations, expression, module, required = 'pay'] = fixture;
  if (module) throw Error(`Module fixture needs a module browser adapter: ${name}`);
  const source = `${declarations}\nglobalThis.__out=(${expression});`;
  for (const preset of presets) {
    const compiled = cp.spawnSync(binary, ['-', '--lang', 'js', '--preset', preset,
      '--seed', '42', '--keep-names', '*', '--require-virtualized', required, '--verify'],
    {input: source, encoding: 'utf8', timeout: 15000, maxBuffer: 16 * 1024 * 1024});
    if (compiled.status !== 0) results.push({name, preset, status: 'coverage_failure', detail: compiled.stderr || String(compiled.error)});
    else rows.push({name, preset, source, protected: compiled.stdout});
  }
}
const temporary = fs.mkdtempSync(path.join(os.tmpdir(), 'mangler-browser-'));
try {
  // Every run has an isolated realm, including intrinsics changed by source.
  const html = `<!doctype html><pre id="result">pending</pre><script>
const rows=${JSON.stringify(rows).replace(/</g, '\\u003c')};
async function run(source){
 const frame=document.createElement('iframe');document.body.appendChild(frame);
 const asynchronousErrors=[];
 frame.contentWindow.addEventListener('unhandledrejection',event=>{asynchronousErrors.push({error:'UnhandledRejection',message:String(event.reason&&event.reason.message||event.reason)});event.preventDefault()});
 frame.contentWindow.addEventListener('error',event=>{asynchronousErrors.push({error:'UncaughtError',message:event.message});event.preventDefault()});
 try{frame.contentWindow.eval(source);const value=await Promise.race([
   frame.contentWindow.__out,new Promise((_,reject)=>setTimeout(()=>reject(Error('execution deadline exceeded')),1500))]);
   await new Promise(resolve=>setTimeout(resolve,0));
   if(asynchronousErrors.length)return asynchronousErrors[0];
   return {value:JSON.stringify(value,(_k,v)=>typeof v==='bigint'?{$bigint:String(v)}:v===undefined?{$undefined:true}:typeof v==='number'&&!Number.isFinite(v)?{$number:String(v)}:Object.is(v,-0)?{$number:'-0'}:v)};
 }catch(e){return {error:e.name,message:e.message}}finally{frame.remove()}
}
(async()=>{const results=[];for(const row of rows){const expected=await run(row.source),actual=await run(row.protected);
 results.push({name:row.name,preset:row.preset,status:expected.error?'native_failure':JSON.stringify(expected)===JSON.stringify(actual)?'pass':'semantic_failure',expected,actual});}
 document.getElementById('result').textContent=JSON.stringify({user_agent:navigator.userAgent,results});})();
</script>`;
  const page = path.join(temporary, 'audit.html'); fs.writeFileSync(page, html);
  const browser = cp.spawnSync(chrome, ['--headless', '--disable-gpu', '--no-sandbox',
    `--user-data-dir=${path.join(temporary, 'profile')}`, '--virtual-time-budget=60000',
    '--dump-dom', pathToFileURL(page).href], {encoding: 'utf8', timeout: 60000, maxBuffer: 32 * 1024 * 1024});
  const match = browser.stdout?.match(/<pre id="result">([\s\S]*?)<\/pre>/);
  if (!match || match[1] === 'pending') throw Error(`Browser did not finish: ${browser.error || browser.stderr}`);
  const captured = JSON.parse(match[1].replace(/&lt;/g, '<').replace(/&gt;/g, '>').replace(/&amp;/g, '&'));
  results.push(...captured.results);
  const report = {binary, binary_sha256: binaryHash, binary_unchanged: binaryHash === hash(),
    user_agent: captured.user_agent, presets, seed: 42,
    totals: results.reduce((a, r) => (a[r.status] = (a[r.status] || 0) + 1, a), {}), results};
  const json = JSON.stringify(report, null, 2) + '\n';
  if (output) fs.writeFileSync(output, json);
  process.stdout.write(json);
  if (!report.binary_unchanged || results.some(r => r.status !== 'pass')) process.exitCode = 1;
} finally { fs.rmSync(temporary, {recursive: true, force: true}); }
