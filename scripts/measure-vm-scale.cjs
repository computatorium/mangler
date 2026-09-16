// Required-virtualization scaling audit: independent function, bundle, and ESM project growth.
// Usage: node scripts/measure-vm-scale.cjs --binary target/release/mangler [--output report.json]
//        [--groups statements,functions,locals,expressions,project,workflow] [--timeout 60000]
const fs = require('node:fs');
const path = require('node:path');
const os = require('node:os');
const cp = require('node:child_process');
const vm = require('node:vm');
const zlib = require('node:zlib');
const crypto = require('node:crypto');

if (process.argv[2] === '--worker') {
  const {source} = JSON.parse(fs.readFileSync(0, 'utf8'));
  const context = {};
  const start = process.hrtime.bigint();
  new vm.Script(source).runInNewContext(context, {timeout: 15000});
  process.stdout.write(JSON.stringify({value: context.__out, startup_ms: Number(process.hrtime.bigint()-start)/1e6}));
} else {
  let binary, destination, timeout = 60000;
  let groups = new Set(['statements', 'functions', 'locals', 'expressions', 'project', 'workflow']);
  const args = process.argv.slice(2);
  while (args.length) {
    const flag = args.shift(), value = args.shift();
    if (!value) throw new Error(`Missing value for ${flag}`);
    if (flag === '--binary') binary = path.resolve(value);
    else if (flag === '--output') destination = value;
    else if (flag === '--timeout') timeout = Number(value);
    else if (flag === '--groups') groups = new Set(value.split(','));
    else throw new Error(`Unknown option ${flag}`);
  }
  if (!binary) throw new Error('Supply --binary <mangler>');
  const binarySha256 = crypto.createHash('sha256').update(fs.readFileSync(binary)).digest('hex');
  if (!Number.isFinite(timeout) || timeout <= 0) throw new Error('Timeout must be positive milliseconds');
  for (const group of groups) if (!['statements','functions','locals','expressions','project','workflow'].includes(group)) throw new Error(`Unknown group ${group}`);
  const flags = ['--lang','js','--preset','minify','--require-virtualized','*','--seed','42','--keep-names','*','--verify'];
  function compile(inputs, source) {
    const start = process.hrtime.bigint();
    const timed = process.platform === 'darwin';
    const child = cp.spawnSync(timed ? '/usr/bin/time' : binary, timed ? ['-l', binary, ...inputs, ...flags] : [...inputs, ...flags], {
      input: source, encoding:'utf8', timeout, maxBuffer:128*1024*1024,
    });
    const rss = timed ? child.stderr?.match(/(\d+)\s+maximum resident set size/) : null;
    return {status: child.status, signal: child.signal, output: child.stdout, detail: child.error?.message || child.stderr?.replace(/^.*(?:maximum resident set size|page reclaims|page faults|page swaps|block input operations|block output operations|messages sent|messages received|signals received|voluntary context switches|involuntary context switches|instructions retired|cycles elapsed|peak memory footprint).*$/gm,'').trim(), compile_ms:Number(process.hrtime.bigint()-start)/1e6, ...(rss ? {max_rss_bytes:Number(rss[1])} : {})};
  }
  function evaluate(source) {
    return JSON.parse(cp.execFileSync(process.execPath,[__filename,'--worker'],{input:JSON.stringify({source}),encoding:'utf8',timeout:20000,maxBuffer:1024*1024}));
  }
  const results=[];
  function record(result) {
    results.push(result);
    process.stderr.write(`${result.group} ${result.scale}: ${result.status}${result.compile_ms ? ` (${Math.round(result.compile_ms)}ms)` : ''}\n`);
    // Flush completed evidence even when a later stress case is interrupted.
    if (destination) fs.writeFileSync(destination,JSON.stringify({node:process.version,binary,timeout_ms:timeout,results},null,2)+'\n');
  }
  function scriptCase(group, scale, source) {
    const original=evaluate(source);
    const result=compile(['-'],source);
    const {compile_ms,max_rss_bytes}=result;
    const base={group,scale,input_bytes:Buffer.byteLength(source),compile_ms,max_rss_bytes};
    if(result.status!==0){record({...base,status:'coverage_failure',signal:result.signal,detail:result.detail});return}
    try {
      const transformed=evaluate(result.output);
      record({...base,status:Object.is(original.value,transformed.value)?'pass':'semantic_failure',output_bytes:Buffer.byteLength(result.output),gzip_bytes:zlib.gzipSync(result.output,{level:9}).length,native_startup_ms:original.startup_ms,protected_startup_ms:transformed.startup_ms,...(!Object.is(original.value,transformed.value)?{expected:original.value,actual:transformed.value}:{})});
    }catch(error){record({...base,status:'runtime_failure',detail:String(error)})}
  }
  function body(count){return Array.from({length:count},(_,i)=>`n=(n*3+${i})%1000003;`).join('')}
  if(groups.has('statements'))for(const size of [100,1000,10000,25000])scriptCase('statements',size,`function pay(x){var n=x;${body(size)}return n}globalThis.__out=pay(7);`);
  if(groups.has('functions'))for(const size of [1,10,100,1000,5000]){
    const functions=Array.from({length:size},(_,i)=>`function pay${i}(x){var n=x;${body(10)}return n}`).join('\n');
    scriptCase('functions',size,`${functions}\nglobalThis.__out=0;${Array.from({length:size},(_,i)=>`globalThis.__out+=pay${i}(${i});`).join('')}`);
  }
  if(groups.has('locals'))for(const size of [100,1000,10000,70000]){
    const locals=Array.from({length:size},(_,i)=>`var v${i}=${i};`).join('');
    scriptCase('locals',size,`function pay(){${locals}return v0+v${size-1}}globalThis.__out=pay();`);
  }
  if(groups.has('expressions'))for(const size of [100,500,1000,5000])scriptCase('expressions',size,`function pay(x){return ${Array.from({length:size},()=>`x`).join('+')}}globalThis.__out=pay(7);`);
  const workflows = [
    'return Math.round(x*0.029)+30;',
    'const lines=[{price:x,quantity:2},{price:17,quantity:3}];let total=0;for(const line of lines)total+=line.price*line.quantity;return total;',
    'return /^[0-9]+$/.test(String(x))?x:0;',
    'return Number(BigInt(x)*3n+7n);',
    'let n=x;const adjust=fee=>n+=fee;return adjust(2);',
    'const invoice={amount:x,discount:2,metadata:{tax:3}};const {amount,...rest}=invoice;return Math.max(0,amount-rest.discount)+(rest.metadata?.tax??0);',
    'try{if(x<0)throw new Error("negative amount");return x+7}catch(e){return 0}finally{globalThis.checked=true}',
    'let tax=0;switch(x%3){case 0:tax=1;break;case 1:tax=2;break;default:tax=3}return x+tax;',
  ];
  for(const group of ['project','workflow'])if(groups.has(group))for(const size of [10,100,500]){
    const directory=fs.mkdtempSync(path.join(os.tmpdir(),'mangler-scale-'));
    try {
      const input=path.join(directory,'src'),output=path.join(directory,'out');fs.mkdirSync(input);fs.mkdirSync(output);
      let bytes=0;
      for(let i=0;i<size;i++){
        const source=`export function pay${i}(x){${group==='workflow'?workflows[i%workflows.length]:`var n=x;${body(20)}return n`}}`;
        bytes+=Buffer.byteLength(source);fs.writeFileSync(path.join(input,`part${i}.js`),source);
      }
      const entry=Array.from({length:size},(_,i)=>`import {pay${i}} from './part${i}.js';`).join('\n')+`\nexport function pay(){var n=0;${Array.from({length:size},(_,i)=>`n+=pay${i}(${i});`).join('')}return n}console.log(pay());`;
      bytes+=Buffer.byteLength(entry);fs.writeFileSync(path.join(input,'entry.js'),entry);
      const result=compile([input,'--output',output]);
      const base={group,scale:size+1,input_bytes:bytes,compile_ms:result.compile_ms,max_rss_bytes:result.max_rss_bytes};
      if(result.status!==0){record({...base,status:'coverage_failure',detail:result.detail});continue}
      fs.writeFileSync(path.join(input,'package.json'),'{"type":"module"}');fs.writeFileSync(path.join(output,'package.json'),'{"type":"module"}');
      const native=cp.execFileSync(process.execPath,[path.join(input,'entry.js')],{encoding:'utf8',timeout:20000});
      const start=process.hrtime.bigint();
      const protectedResult=cp.execFileSync(process.execPath,[path.join(output,'entry.js')],{encoding:'utf8',timeout:20000});
      const protected_startup_ms=Number(process.hrtime.bigint()-start)/1e6;
      const outputFiles=fs.readdirSync(output).filter(name=>name.endsWith('.js')).map(name=>fs.readFileSync(path.join(output,name)));
      record({...base,status:native===protectedResult?'pass':'semantic_failure',output_bytes:outputFiles.reduce((sum,b)=>sum+b.length,0),gzip_bytes:outputFiles.reduce((sum,b)=>sum+zlib.gzipSync(b,{level:9}).length,0),output_files:outputFiles.length,protected_startup_ms,...(native!==protectedResult?{expected:native,actual:protectedResult}:{})});
    }catch(error){record({group,scale:size+1,status:'runtime_failure',detail:String(error)})}
    finally{fs.rmSync(directory,{recursive:true,force:true})}
  }
  const binaryUnchanged = crypto.createHash('sha256').update(fs.readFileSync(binary)).digest('hex') === binarySha256;
  const report={node:process.version,platform:process.platform,arch:process.arch,cpu:os.cpus()[0]?.model,binary,binary_sha256:binarySha256,binary_unchanged:binaryUnchanged,timeout_ms:timeout,results};
  if(destination)fs.writeFileSync(destination,JSON.stringify(report,null,2)+'\n');
  process.stdout.write(JSON.stringify(report,null,2)+'\n');
  if(!binaryUnchanged || results.some(row=>row.status!=='pass'))process.exitCode=1;
}
