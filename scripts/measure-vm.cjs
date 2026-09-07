// Usage: node scripts/measure-vm.cjs --binary <mangler> [--baseline <mangler>] [--output <json>] [--check]
// Requires an explicitly selected Node executable. Child processes have deadlines;
// timed VM operations run inside a worker with an outer 60-second deadline.
const fs = require('node:fs');
const cp = require('node:child_process');
const vm = require('node:vm');
const zlib = require('node:zlib');
const os = require('node:os');

const fixtures = {
  payment: {
    required: 'total',
    source: `function total(lines,discount,tax){var sum=0;for(var i=0;i<lines.length;i++){var line=lines[i];sum+=line.price*line.quantity;}sum=Math.max(0,sum-discount);return Math.round(sum*(1+tax));}globalThis.__out=total([{price:1250,quantity:2},{price:799,quantity:3}],300,0.0825);`,
    call: 'total([{price:1250,quantity:2},{price:799,quantity:3}],300,0.0825)',
  },
  closure: {
    required: 'checkout',
    source: `function makeFee(rate){return function fee(amount){return Math.round(amount*rate)+30;};}function checkout(amount){try{if(amount<0)throw new Error('negative amount');return makeFee(0.029)(amount);}catch(e){return e.message;}}globalThis.__out=[checkout(1000),checkout(-1)].join(',');`,
    call: 'checkout(1000)',
  },
  bundle: {
    required: 'calculate',
    source: `function calculate(value){var result=value;${Array.from({length: 500}, (_, i) => `result=(result*${i % 7 + 1}+${i})%1000003;`).join('')}return result;}globalThis.__out=calculate(17);`,
    call: 'calculate(17)',
  },
};

function median(samples) {
  return samples.sort((a, b) => a - b)[Math.floor(samples.length / 2)];
}

function milliseconds(start) {
  return Number(process.hrtime.bigint() - start) / 1e6;
}

function measureRuntime(source, output, call) {
  const native = {};
  new vm.Script(source).runInNewContext(native);
  const cold = [];
  for (let i = 0; i < 50; i++) {
    const context = {};
    const start = process.hrtime.bigint();
    new vm.Script(output).runInNewContext(context);
    cold.push(milliseconds(start));
    if (!Object.is(context.__out, native.__out)) {
      throw new Error(`Behavior changed: ${context.__out} != ${native.__out}`);
    }
  }
  const context = vm.createContext({});
  new vm.Script(output).runInContext(context);
  const loop = new vm.Script(`for(var round=0;round<100;round++){${call}}`);
  loop.runInContext(context);
  const warm = [];
  for (let i = 0; i < 15; i++) {
    const start = process.hrtime.bigint();
    loop.runInContext(context);
    warm.push(milliseconds(start) / 100);
  }
  return {cold_median_ms: median(cold), warm_call_median_ms: median(warm)};
}

if (process.argv[2] === '--worker') {
  try {
    const {source, output, call} = JSON.parse(fs.readFileSync(0, 'utf8'));
    process.stdout.write(JSON.stringify(measureRuntime(source, output, call)));
  } catch (error) {
    process.stderr.write(`${error.name}: ${error.message}\n`);
    process.exitCode = 1;
  }
} else {
  const args = process.argv.slice(2);
  let binary, baseline, destination, check = false;
  while (args.length) {
    const flag = args.shift();
    if (flag === '--check') { check = true; continue; }
    const value = args.shift();
    if (!value) throw new Error(`Missing value for ${flag}`);
    if (flag === '--binary') binary = value;
    else if (flag === '--baseline') baseline = value;
    else if (flag === '--output') destination = value;
    else throw new Error(`Unknown option ${flag}`);
  }
  if (!binary) throw new Error('Supply --binary <mangler>; optionally --baseline <mangler> --output <json> --check.');
  const flags = ['-', '--lang', 'js', '--preset', 'minify', '--virtualize', '*', '--seed', '42', '--keep-names', 'total,makeFee,checkout,calculate'];
  function measure(executable, requireCoverage) {
    return Object.entries(fixtures).map(([name, {source, call, required}]) => {
      const output = cp.execFileSync(executable, requireCoverage ? [...flags, '--require-virtualized', required] : flags, {
        input: source, encoding: 'utf8', timeout: 60_000, maxBuffer: 16 * 1024 * 1024,
      });
      const runtime = JSON.parse(cp.execFileSync(process.execPath, [__filename, '--worker'], {
        input: JSON.stringify({source, output, call}), encoding: 'utf8',
        timeout: 60_000, maxBuffer: 1024 * 1024,
      }));
      return {
        name, required, input: Buffer.byteLength(source), raw: Buffer.byteLength(output),
        gzip: zlib.gzipSync(output, {level: 9}).length,
        brotli: zlib.brotliCompressSync(output).length, ...runtime,
      };
    });
  }
  const report = {
    node: process.version, platform: process.platform, arch: process.arch,
    cpu: os.cpus()[0]?.model, flags, cold_samples: 50,
    warm_batches: 15, warm_calls_per_batch: 100,
    ...(baseline ? {baseline: measure(baseline, false)} : {}), current: measure(binary, true),
  };
  const json = JSON.stringify(report, null, 2) + '\n';
  if (destination) fs.writeFileSync(destination, json);
  process.stdout.write(json);
  if (check) {
    // Shipping-byte budgets, with room for correctness changes. Timing is reported
    // but never compared to an unstable machine-dependent wall-clock threshold.
    const budgets = {
      payment: {raw: 3000, gzip: 1400, brotli: 1250},
      closure: {raw: 6000, gzip: 2200, brotli: 1900},
      bundle: {raw: 25000, gzip: 6500, brotli: 3200},
    };
    for (const result of report.current) {
      for (const [metric, limit] of Object.entries(budgets[result.name])) {
        if (result[metric] > limit) throw new Error(`${result.name} ${metric}: ${result[metric]} exceeds budget ${limit}`);
      }
    }
  }
}
