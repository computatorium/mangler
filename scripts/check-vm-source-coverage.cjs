// Audit selected-source accounting and emitted source retention independently of behavior.
// Usage: node scripts/check-vm-source-coverage.cjs --binary <frozen-mangler> [--output report.json]
const fs = require('node:fs');
const path = require('node:path');
const cp = require('node:child_process');
const crypto = require('node:crypto');
let binary, destination;
const args = process.argv.slice(2);
while (args.length) {
  const flag = args.shift(), value = args.shift();
  if (!value) throw new Error(`Missing value for ${flag}`);
  if (flag === '--binary') binary = path.resolve(value);
  else if (flag === '--output') destination = value;
  else throw new Error(`Unknown option ${flag}`);
}
if (!binary) throw new Error('Supply --binary <mangler>');
const fingerprint = () => crypto.createHash('sha256').update(fs.readFileSync(binary)).digest('hex');
const binarySha256 = fingerprint();
const expression = 'globalThis.rate*78329+34691';
const cases = [
  {name:'regular_parameter', source:`function pay(x=${expression}){return x}globalThis.rate=2;globalThis.__out=pay()`},
  {name:'async_parameter', source:`async function pay(x=${expression}){return await x}globalThis.rate=2;globalThis.__out=pay()`},
  {name:'generator_parameter', source:`function* pay(x=${expression}){yield x}globalThis.rate=2;globalThis.__out=pay().next().value`},
  {name:'class_field', source:`function pay(){class Account{charge=${expression}}return new Account().charge}globalThis.rate=2;globalThis.__out=pay()`},
  {name:'class_static_block', source:`function pay(){class Account{static charge;static{this.charge=${expression}}}return Account.charge}globalThis.rate=2;globalThis.__out=pay()`},
  {name:'class_constructor', source:'function pay(x){class Account{constructor(v){this.charge=v*78329+34691}}return new Account(x).charge}globalThis.__out=pay(2)'},
  {name:'explicit_exclusion', source:'function pay(x){function keep(v){return v*78329+34691}return keep(x)}globalThis.__out=pay(2)', extra:['--virtualize-exclude','keep'], nativeBody:true},
  {name:'excluded_function_name', source:'function pay(){function keep(){return keep.name}return keep()}globalThis.__out=pay()', extra:['--virtualize-exclude','keep']},
  {name:'excluded_required_rejects', source:'function pay(x){function keep(v){return v*78329+34691}return keep(x)}globalThis.__out=pay(2)', extra:['--virtualize-exclude','keep','--require-virtualized','*'], rejection:'excluded'},
  {name:'required_unmatched_rejects', source:'function receipt(){return 7}globalThis.__out=receipt()', extra:['--require-virtualized','pay'], rejection:'matched no source'},
  {name:'selected_direct_eval', source:'function pay(x){return eval("x+2")}globalThis.__out=pay(5)'},
  {name:'simple_script', source:`globalThis.rate=2;globalThis.__out=${expression}`, whole:true},
  {name:'export_initializer', source:`globalThis.rate=2;export const total=${expression};globalThis.__out=total`, whole:true, module:true},
];

function evaluate(source, module) {
  return JSON.parse(cp.execFileSync(process.execPath,[path.join(__dirname,'check-vm-coverage.cjs'),module?'--worker-module':'--worker'],{
    input:source,encoding:'utf8',timeout:5000,maxBuffer:1024*1024,
  }));
}

const results=[];
for(const fixture of cases){
  const result=cp.spawnSync(binary,['-','--lang','js','--preset','minify','--seed','42','--keep-names','*','--verify',
    ...(fixture.whole?['--virtualize-program']:['--virtualize','pay']),...(fixture.extra||[])],{
    input:fixture.source,encoding:'utf8',timeout:15000,maxBuffer:16*1024*1024,
  });
  if(fixture.rejection){
    const correct=result.status!==0 && !result.stdout && result.stderr?.includes(fixture.rejection);
    results.push({name:fixture.name,status:correct?'pass':'accounting_failure',expected:'explicit rejection with no output',...(correct?{}:{detail:result.stderr})});
    continue;
  }
  if(result.status!==0){
    // Rejection is honest accounting, but unsupported source remains a coverage failure.
    results.push({name:fixture.name,status:'coverage_failure',fail_closed:!result.stdout,detail:result.stderr?.trim()||String(result.error)});
    continue;
  }
  const expected=evaluate(fixture.source,fixture.module),actual=evaluate(result.stdout,fixture.module);
  // The positive-control excluded function proves the probe can distinguish
  // native arithmetic from the same numeric values stored in a bytecode pool.
  // SWC may commute multiplication, so accept either operand order.
  const sourceBody=/78329\s*\*\s*[^;{}]{0,100}\+\s*34691|\*\s*78329\s*\+\s*34691/.test(result.stdout);
  const behavior=JSON.stringify(actual)===JSON.stringify(expected);
  const retention=sourceBody===Boolean(fixture.nativeBody);
  results.push({name:fixture.name,status:!behavior?'semantic_failure':!retention?'source_retention_failure':'pass',
    native_arithmetic_present:sourceBody,expected_native_arithmetic:Boolean(fixture.nativeBody),
    ...(!behavior?{expected,actual}:{})});
}
const binaryUnchanged=fingerprint()===binarySha256;
const totals=results.reduce((all,row)=>{all[row.status]=(all[row.status]||0)+1;return all},{});
const report={node:process.version,binary,binary_sha256:binarySha256,binary_unchanged:binaryUnchanged,totals,results};
if(destination)fs.writeFileSync(destination,JSON.stringify(report,null,2)+'\n');
process.stdout.write(JSON.stringify(report,null,2)+'\n');
if(!binaryUnchanged||results.some(row=>row.status!=='pass'))process.exitCode=1;
