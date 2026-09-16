#!/usr/bin/env python3
"""Run Test262 Script and Module cases through required virtualization.

Supply a Test262 checkout including harness/ and tools/packaging/. This adapter
does not claim whole-suite conformance. Each execution contract reports its
applicable tests and actual host capabilities explicitly.
The syntax contract checks Script and Module parse/early negatives without execution or
virtualization, using an explicit parse-only CLI protocol; its passes are parser
conformance evidence, not protected-execution coverage.
Every applicable test must pass natively before its protected result is accepted.
The script contract protects the original Script with whole-program coverage,
leaving the harness in its separate setup script. The function contract first
checks a native wrapper and reports wrapping incompatibilities separately.
Script and function execution also protect dynamically imported JavaScript fixtures.
The module contract protects every loaded source module, preserving the native
linker, module namespace, live bindings, cycles and dynamic import identities.
The official Test262 metadata reader supplies flags and harness dependencies.
"""

import argparse
import hashlib
from collections import Counter
from concurrent.futures import FIRST_COMPLETED, ThreadPoolExecutor, wait
import json
from pathlib import Path
import subprocess
import sys


NODE_RUNNER = r"""
const fs=require('node:fs'), vm=require('node:vm');
const input=JSON.parse(fs.readFileSync(0,'utf8'));
const hostModule={exports:{}};
vm.compileFunction(input.host_source,['module','exports','require'],{filename:'test262-host.cjs'})(hostModule,hostModule.exports,require);
const unsupported=new Set();
process.on('exit',()=>{
  if(unsupported.size){
    process.stderr.write('MANGLER_TEST262_HOST_UNSUPPORTED:'+JSON.stringify([...unsupported])+'\n');
    process.exitCode=1;
  }
});
const moduleConfig=input.module||input.imports;
const moduleLoaders=new WeakMap();
const protectedModules=[],rejectedModules=[],loadedModules=[];
let coverageFailure=false,entryContext;
process.on('exit',()=>{
  if(coverageFailure)process.exitCode=1;
  if(moduleConfig)process.stderr.write('MANGLER_TEST262_MODULE_LOADS:'+JSON.stringify(loadedModules)+'\n');
  if(moduleConfig?.binary)process.stderr.write('MANGLER_TEST262_MODULES:'+JSON.stringify(protectedModules)+'\n');
  if(rejectedModules.length)process.stderr.write('MANGLER_TEST262_MODULE_SYNTAX:'+JSON.stringify(rejectedModules)+'\n');
});
let realm;
function failure(error){
  console.error(error&&error.stack||String(error));
  process.exitCode=1;
  if(realm)void realm.dispose();
}
process.on('unhandledRejection',failure);
realm=hostModule.exports.createTest262Realm({
  globals:{console,process,setTimeout,clearTimeout,setInterval,clearInterval,queueMicrotask,atob,btoa,TextEncoder,TextDecoder},
  timeout:input.timeout,
  onUnsupported:capability=>unsupported.add(capability),
  onError:failure,
  initializeScriptOptions:moduleConfig?target=>{
    if(!entryContext)entryContext=target.context;
    const loader=createModuleLoader(moduleConfig,input.module&&entryContext===target.context?input.body:undefined,target);
    moduleLoaders.set(target.context,loader);
    return {filename:moduleConfig.path,importModuleDynamically:loader.importModuleDynamically};
  }:undefined,
});
const {run}=realm;
function finish(){
  try {realm.checkAgents();}catch(error){failure(error);}
  Promise.resolve(realm.dispose()).catch(failure);
}
process.once('mangler-test262-complete',finish);
process.once('beforeExit',finish);
function createModuleLoader(config,moduleEntrySource,target){
  const {run,context}=target;
  const path=require('node:path'),url=require('node:url'),os=require('node:os');
  const {spawnSync}=require('node:child_process');
  const entry=url.pathToFileURL(config.path).href;
  const root=fs.realpathSync(config.root)+path.sep;
  const modules=new Map(),failures=new Map(),linkages=new WeakMap(),evaluations=new WeakMap();
  const RealmSyntaxError=run('SyntaxError'),RealmTypeError=run('TypeError'),realmJSONParse=run('JSON.parse');
  const wired=new WeakSet();
  let links=Promise.resolve();
  function moduleError(error){
    // vm's host API creates parser/linker errors outside the test context.
    // Imported source and its errors belong to the importing test realm.
    return error instanceof SyntaxError?new RealmSyntaxError(error.message):error;
  }
  function load(identifier,attributes={}){
    if(Object.keys(attributes).some(key=>key!=='type'))throw new RealmTypeError('Unsupported import attribute');
    const json=new URL(identifier).pathname.endsWith('.json');
    if((json&&attributes.type!=='json')||(!json&&attributes.type!==undefined))throw new RealmTypeError('Invalid module type');
    if(modules.has(identifier))return modules.get(identifier);
    if(failures.has(identifier))throw failures.get(identifier);
    const parsed=new URL(identifier);
    if(parsed.protocol!=='file:')throw new Error('Unsupported module URL '+identifier);
    const filename=fs.realpathSync(url.fileURLToPath(parsed));
    if(!filename.startsWith(root))throw new Error('Module outside Test262 checkout '+identifier);
    // A Script and an imported Module at the same URL are distinct records.
    // Only the Module contract supplies an entry source override.
    let source=identifier===entry&&moduleEntrySource!==undefined?moduleEntrySource:fs.readFileSync(filename,'utf8');
    if(json){
      const value=realmJSONParse(source);
      const module=new vm.SyntheticModule(['default'],function(){this.setExport('default',value);},
        {context,identifier});
      modules.set(identifier,module);return module;
    }
    loadedModules.push(identifier);
    if(config.binary){
      const started=performance.now();
      const temporary=fs.mkdtempSync(path.join(os.tmpdir(),'mangler-test262-module-'));
      try{
        const file=path.join(temporary,'source.mjs');
        fs.writeFileSync(file,source);
        const result=spawnSync(config.binary,[file,'--preset','minify','--seed',String(config.seed),
          '--keep-names','*','--virtualize-program'],{encoding:'utf8',timeout:input.timeout,maxBuffer:64*1024*1024});
        if(result.status!==0){
          // Invalid dependencies must fail during loading. A typed parser
          // rejection, independently confirmed by Node's Module parser, supplies
          // that error without ever executing or accepting native source bodies.
          const remaining=Math.floor(input.timeout-(performance.now()-started));
          const syntax=remaining>0?spawnSync(config.binary,[file,'--check-syntax','module'],
            {encoding:'utf8',timeout:remaining,maxBuffer:1024*1024}):null;
          let record;
          try{record=JSON.parse(syntax?.stdout);}catch{}
          if(syntax?.status===1&&record?.phase==='parse'&&record.error==='SyntaxError'){
            try{new vm.SourceTextModule(source,{context,identifier});}
            catch(error){
              if(error?.name==='SyntaxError'){
                const failure=moduleError(error);
                rejectedModules.push(identifier);failures.set(identifier,failure);throw failure;
              }
            }
          }
          coverageFailure=true;
          process.stderr.write('MANGLER_TEST262_MODULE_COVERAGE:'+JSON.stringify({module:identifier,detail:String(result.error||result.stderr).slice(-4000)})+'\n');
          throw Error('Module protection failed: '+identifier);
        }
        source=result.stdout;
        protectedModules.push(identifier);
      }finally{fs.rmSync(temporary,{recursive:true,force:true});}
    }
    let module;
    try{module=new vm.SourceTextModule(source,{
      context,identifier,
      initializeImportMeta(meta){meta.url=identifier;},
      importModuleDynamically,
    });}catch(error){const failure=moduleError(error);failures.set(identifier,failure);throw failure;}
    modules.set(identifier,module);return module;
  }
  async function wire(module,seen){
    if(module.status!=='unlinked'||wired.has(module)||seen.has(module))return;
    seen.add(module);
    if(module instanceof vm.SyntheticModule){await module.link(()=>{});return;}
    const dependencies=module.moduleRequests.map(request=>
      load(new URL(request.specifier,module.identifier).href,request.attributes));
    for(const dependency of dependencies)await wire(dependency,seen);
    module.linkRequests(dependencies);wired.add(module);
  }
  function linked(module){
    // Reuse each root's link outcome, while letting the native linker inspect
    // already-errored dependencies and validate new roots before evaluation.
    if(linkages.has(module))return linkages.get(module);
    const ready=links.then(async()=>{
      if(module.status!=='unlinked')return;
      await wire(module,new Set());
      if(module.status==='unlinked')module.instantiate();
    }).catch(error=>{throw moduleError(error);});
    linkages.set(module,ready);
    links=ready.catch(()=>{});return ready;
  }
  function evaluated(module){
    if(!evaluations.has(module))evaluations.set(module,module.evaluate({timeout:input.timeout}));
    return evaluations.get(module);
  }
  async function importModuleDynamically(specifier,referencing,attributes){
    // Loading starts outside the caller's Script/Module evaluation frame, so
    // compiler subprocess time cannot consume its vm execution timeout.
    await Promise.resolve();
    let identifier;
    try{identifier=new URL(specifier,referencing?.identifier||entry).href;}
    catch(error){throw error instanceof TypeError?new RealmTypeError(error.message):error;}
    const imported=load(identifier,attributes);
    await linked(imported);await evaluated(imported);return imported;
  }
  async function executeModule(){
    // Harness error types exist only after setup in the entry realm. Capture
    // before evaluating source, which may replace its global constructor.
    const expectedNegative=input.negative?run(input.negative):undefined;
    const main=load(entry);
    try{await linked(main);}
    catch(error){
      if(!coverageFailure&&config.negative_phase==='resolution'&&error?.name===input.negative)return;
      throw error;
    }
    if(config.negative_phase==='resolution')throw Error('expected resolution error '+input.negative);
    let caught,threw=false;
    try{await evaluated(main);}catch(error){caught=error;threw=true;}
    if(input.negative){
      if(!threw)throw Error('expected runtime error '+input.negative);
      if(caught==null||caught.constructor!==expectedNegative)throw caught;
    }else if(threw)throw caught;
  }
  return {executeModule,importModuleDynamically};
}
async function executeTest(){
try {
  run(input.setup,{filename:'test262-harness.js'});
  const loader=moduleLoaders.get(realm.context);
  if(input.module){
    await loader.executeModule();
    if(input.complete)run('$DONE();');
    return;
  }
  const expected=input.negative?run(input.negative):undefined;
  let caught,threw=false;
  try {run(input.body,{filename:input.imports?.path||'test262-test.js',
    ...(loader?{importModuleDynamically:loader.importModuleDynamically}:{})})}
  catch(error){caught=error;threw=true;}
  if(input.negative){
    if(!threw)throw Error('expected runtime error '+input.negative);
    if(caught==null||caught.constructor!==expected)throw caught;
  }else if(threw)throw caught;
  if(input.complete)run('$DONE();');
}catch(error){failure(error);finish();}
}
executeTest().catch(error=>{failure(error);finish();});
"""


NODE_HOST_CAPABILITIES = r"""
const fs=require('node:fs'),vm=require('node:vm'),hostModule={exports:{}};
if(vm.SourceTextModule&&typeof vm.SourceTextModule.prototype.linkRequests!=='function')
  throw Error('Test262 module loading requires Node 22.21+, 24.8+, or newer');
vm.compileFunction(fs.readFileSync(0,'utf8'),['module','exports','require'],{filename:'test262-host.cjs'})(hostModule,hostModule.exports,require);
process.stdout.write(JSON.stringify(hostModule.exports.capabilities));
"""


def unsupported_host(result):
    """Return actual unavailable host requests, including source-caught errors."""
    prefix = "MANGLER_TEST262_HOST_UNSUPPORTED:"
    for line in result[2].splitlines():
        if line.startswith(prefix):
            try:
                capabilities = json.loads(line[len(prefix):])
                if isinstance(capabilities, list) and all(isinstance(item, str) for item in capabilities):
                    return sorted(set(capabilities))
            except ValueError:
                pass
    return []


NODE_SYNTAX_RUNNER = r"""
const fs=require('node:fs'), vm=require('node:vm');
const input=JSON.parse(fs.readFileSync(0,'utf8'));
let error=null;
try {
  if(input.goal==='module')new vm.SourceTextModule(input.body,{identifier:'test262-test.js'});
  else new vm.Script(input.body,{filename:'test262-test.js'});
}
catch(caught){error=caught instanceof SyntaxError?'SyntaxError':'UnexpectedError';}
process.stdout.write(JSON.stringify({phase:'parse',error}));
process.exitCode=error===null?0:1;
"""


def syntax_variants(record, raw_source=None):
    """Preserve parse goal; raw tests receive neither a prefix nor harness."""
    flags = record.get("flags", [])
    if "module" in flags:
        yield "module", raw_source if "raw" in flags and raw_source is not None else record["test"]
        return
    if "raw" in flags:
        yield "raw", record["test"] if raw_source is None else raw_source
        return
    strictness = [True] if "onlyStrict" in flags else [False] if "noStrict" in flags else [False, True]
    for strict in strictness:
        yield ("strict" if strict else "sloppy"), ('"use strict";\n' if strict else "") + record["test"]


def syntax_outcome(result):
    """Accept only the parse-only protocol, never generic failure diagnostics."""
    code, stdout, _ = result
    try:
        record = json.loads(stdout)
    except (ValueError, TypeError):
        return "protocol_failure"
    if record == {"phase": "parse", "error": None} and code == 0:
        return "accepted"
    if record == {"phase": "parse", "error": "SyntaxError"} and code == 1:
        return "rejected"
    return "protocol_failure"


def execute(command, source, timeout):
    try:
        result = subprocess.run(command, input=source, text=True, capture_output=True,
                                timeout=timeout, check=False)
        return result.returncode, result.stdout, result.stderr
    except subprocess.TimeoutExpired:
        return -1, "", "execution deadline exceeded"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--test262", type=Path, required=True)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--node", required=True)
    parser.add_argument("--host-gc", action="store_true",
                        help="enable Node's real garbage collector with --expose-gc")
    parser.add_argument("--seed", type=int, default=42)
    parser.add_argument("--jobs", type=int, default=2)
    parser.add_argument("--timeout", type=float, default=10)
    parser.add_argument("--contract", choices=("function", "script", "module", "syntax"), default="function",
                        help="function: require the test wrapper; script: require whole-program protection; "
                             "module: protect all loaded modules with native linking; "
                             "syntax: parse/early-negative Script/Module conformance only (no execution)")
    parser.add_argument("--output", type=Path)
    parser.add_argument("--manifest", type=Path, help="File containing relative test paths, one per line; # comments allowed")
    parser.add_argument("paths", nargs="*", help="Test files/directories relative to the checkout")
    args = parser.parse_args()
    if args.jobs < 1 or args.timeout <= 0:
        parser.error("jobs and timeout must be positive")
    requested_paths = list(args.paths)
    if args.manifest:
        requested_paths.extend(line.strip() for line in args.manifest.read_text().splitlines()
                               if line.strip() and not line.lstrip().startswith("#"))
    if not requested_paths:
        parser.error("supply test paths or --manifest")
    root = args.test262.resolve()
    binary_hash = hashlib.sha256(args.binary.read_bytes()).hexdigest()
    adapter_source = Path(__file__).read_bytes()
    runner_hash = hashlib.sha256(adapter_source).hexdigest()
    # Archived runners find their immutable sibling module; live runners use
    # the repository module. Both are buffered before any test begins.
    host_path = Path(__file__).with_suffix(".host.cjs")
    if not host_path.exists():
        host_path = Path(__file__).with_name("test262-host.cjs")
    host_source = host_path.read_bytes()
    host_hash = hashlib.sha256(host_source).hexdigest()
    adapter_hash = hashlib.sha256(adapter_source + b"\0test262-host.cjs\0" + host_source).hexdigest()
    node_flags = ["--expose-gc"] if args.host_gc else []
    if args.contract != "syntax":
        node_flags.append("--experimental-vm-modules")
    host_probe = execute([args.node, *node_flags, "-e", NODE_HOST_CAPABILITIES],
                         host_source.decode(), args.timeout)
    if host_probe[0]:
        parser.error("could not initialize Test262 host: " + host_probe[2])
    host_capabilities = json.loads(host_probe[1])
    node_version = subprocess.run([args.node, "--version"], text=True, capture_output=True,
                                  check=True).stdout.strip()
    sys.path.insert(0, str(root / "tools" / "packaging"))
    from parseTestRecord import parseTestRecord

    files = set()
    for requested in requested_paths:
        path = (root / requested).resolve()
        if not path.is_relative_to(root):
            parser.error(f"Test262 path is outside the checkout: {path}")
        if not path.exists():
            parser.error(f"missing Test262 path: {path}")
        files.update(path.rglob("*.js") if path.is_dir() else [path])
    fixtures = {path for path in files if "_FIXTURE" in path.name}
    files.difference_update(fixtures)
    harness = root / "harness"
    results = []

    def cases():
        for path in sorted(files):
            name = str(path.relative_to(root))
            metadata_errors = []
            source_text = path.read_text()
            record = parseTestRecord(source_text, name, metadata_errors.append)
            if metadata_errors:
                yield {"test": name, "status": "metadata_error", "detail": metadata_errors}
                continue
            flags = record.get("flags", [])
            negative = record.get("negative", {})
            if args.contract == "syntax":
                if negative.get("phase") not in ("parse", "early"):
                    detail = "syntax contract requires parse/early negative"
                elif negative.get("type") != "SyntaxError":
                    yield {"test": name, "status": "metadata_error",
                           "detail": "parse/early negative must expect SyntaxError"}
                    continue
                else:
                    # No includes, driver, wrapper, or $262 services: nothing is
                    # executed, including $DONOTEVALUATE and raw test bodies.
                    for variant, body in syntax_variants(record, source_text):
                        yield {"name": name + ":" + variant, "body": body,
                               "syntax_phase": negative["phase"],
                               "syntax_goal": "module" if "module" in flags else "script"}
                    continue
                yield {"test": name, "status": "inapplicable", "detail": detail}
                continue
            incompatible = (None if "module" in flags else "requires module flag") if args.contract == "module" else next((flag for flag in ("module", "raw") if flag in flags), None)
            if negative.get("phase") in (("parse", "early") if args.contract == "module" else ("parse", "early", "resolution")):
                incompatible = "negative " + negative["phase"]
            if incompatible:
                yield {"test": name, "status": "inapplicable", "detail": incompatible}
                continue
            if "IsHTMLDDA" in record.get("features", []) and not host_capabilities["IsHTMLDDA"]:
                yield {"test": name, "status": "unsupported_host", "stage": "host_requirements",
                       "capabilities": ["IsHTMLDDA"]}
                continue
            if "source-phase-imports-module-source" in record.get("features", []) and not host_capabilities["AbstractModuleSource"]:
                yield {"test": name, "status": "unsupported_host", "stage": "host_requirements",
                       "capabilities": ["AbstractModuleSource"]}
                continue
            includes = ["sta.js", "assert.js", *record.get("includes", [])]
            try:
                setup = "\n".join((harness / include).read_text() for include in dict.fromkeys(includes))
            except OSError as error:
                yield {"test": name, "status": "harness_error", "detail": str(error)}
                continue
            variants = [False] if args.contract == "module" else [True] if "onlyStrict" in flags else [False] if "noStrict" in flags else [False, True]
            for strict in variants:
                body = ('"use strict";\n' if strict else "") + record["test"]
                invocation = "__mangler_test262();"
                if negative.get("phase") == "runtime":
                    invocation = (
                        "var caught=false;try{__mangler_test262()}catch(e){"
                        f"if(e==null||e.constructor!=={negative['type']})throw e;caught=true;}}"
                        "if(!caught)throw Error('expected runtime error');"
                    )
                asynchronous = "async" in flags
                completion = "" if asynchronous else "$DONE();"
                # An async test that exits without calling $DONE must fail too.
                driver = (
                    "var __mangler_done=false;function $DONE(error){"
                    "if(error)throw error;if(__mangler_done)throw Error('duplicate $DONE');"
                    "__mangler_done=true;console.log('MANGLER_TEST262_OK');process.emit('mangler-test262-complete');}"
                    "process.on('beforeExit',function(){if(!__mangler_done)process.exitCode=1;});\n"
                )
                source = (setup + "\n" + driver + "function __mangler_test262(){\n" + body + "\n}\n" + invocation + completion
                          if args.contract == "function" else body)
                yield {"name": name + (":module" if args.contract == "module" else ":strict" if strict else ":sloppy"),
                       "source": source, "setup": setup + "\n" + driver,
                       "body": body, "complete": not asynchronous,
                       "imports": {"path": str(path), "root": str(root), "seed": args.seed},
                       "negative": negative.get("type") if negative.get("phase") in ("runtime", "resolution") else None,
                       "module": {"path": str(path), "root": str(root), "seed": args.seed,
                                  "negative_phase": negative.get("phase")} if args.contract == "module" else None}

    def run_script(body, setup="", complete=False, negative=None, module=None, imports=None, outer_timeout=None):
        payload = json.dumps({"body": body, "setup": setup, "complete": complete,
                              "negative": negative, "timeout": max(1, int(args.timeout * 1000)),
                              "host_source": host_source.decode(), "module": module, "imports": imports})
        return execute([args.node, *node_flags, "-e", NODE_RUNNER], payload,
                       args.timeout if outer_timeout is None else outer_timeout)

    def loaded_js_modules(result):
        return next((json.loads(line.split(":", 1)[1]) for line in result[2].splitlines()
                     if line.startswith("MANGLER_TEST262_MODULE_LOADS:")), [])

    def protection_budget(native):
        modules = loaded_js_modules(native)
        # Each native-observed JS load receives one bounded compiler allowance,
        # plus the unchanged test execution allowance. Duplicate URLs across
        # realms represent distinct compilations and must remain in the count.
        return {"native_js_modules": modules, "native_js_module_count": len(modules),
                "module_transform_timeout_seconds": args.timeout,
                "execution_timeout_seconds": args.timeout,
                "protected_outer_timeout_seconds": args.timeout * (len(modules) + 1)}

    def successful(result):
        return result[0] == 0 and "MANGLER_TEST262_OK" in result[1]

    def diagnostics(result):
        return {"exit_code": result[0], "stdout": result[1][-4000:], "stderr": result[2][-4000:]}

    def check(case):
        if "status" in case:
            return case
        if "syntax_phase" in case:
            name = case["name"]
            goal = case["syntax_goal"]
            node_flags = ["--experimental-vm-modules"] if goal == "module" else []
            original = execute([args.node, *node_flags, "-e", NODE_SYNTAX_RUNNER],
                               json.dumps({"body": case["body"], "goal": goal}), args.timeout)
            if syntax_outcome(original) != "rejected":
                return {"test": name, "status": "native_failure", "stage": "original_" + goal + "_parse",
                        "detail": diagnostics(original)}
            parsed = execute([str(args.binary.resolve()), "-", "--lang", "js",
                              "--check-syntax", goal], case["body"], args.timeout)
            outcome = syntax_outcome(parsed)
            if outcome == "protocol_failure":
                return {"test": name, "status": "syntax_protocol_failure", "stage": "input_parse",
                        "detail": diagnostics(parsed)}
            return {"test": name, "status": "pass" if outcome == "rejected" else "syntax_failure",
                    "stage": "input_parse", "parse_goal": goal, "negative_phase": case["syntax_phase"]}
        name, source = case["name"], case["source"]
        original = run_script(case["body"], case["setup"], case["complete"], case["negative"], case.get("module"), case["imports"])
        unavailable = unsupported_host(original)
        if unavailable:
            return {"test": name, "status": "unsupported_host", "stage": "original_script",
                    "capabilities": unavailable, "detail": diagnostics(original)}
        if not successful(original):
            return {"test": name, "status": "native_failure", "stage": "original_script",
                    "detail": diagnostics(original)}
        native = original
        if args.contract == "module":
            budget = protection_budget(native)
            protected = run_script(case["body"], case["setup"], case["complete"], case["negative"],
                                   {**case["module"], "binary": str(args.binary.resolve())},
                                   outer_timeout=budget["protected_outer_timeout_seconds"])
            if "MANGLER_TEST262_MODULE_COVERAGE:" in protected[2]:
                return {"test": name, "status": "coverage_failure", "detail": diagnostics(protected), **budget}
            if not successful(protected) or protected[1] != native[1]:
                return {"test": name, "status": "semantic_failure", "detail": diagnostics(protected),
                        "native_stdout": native[1], "protected_stdout": protected[1], **budget}
            if sorted(loaded_js_modules(protected)) != sorted(budget["native_js_modules"]):
                return {"test": name, "status": "semantic_failure", "stage": "module_load_inventory",
                        "protected_js_modules": loaded_js_modules(protected), **budget}
            modules = next((json.loads(line.split(":", 1)[1]) for line in protected[2].splitlines()
                            if line.startswith("MANGLER_TEST262_MODULES:")), [])
            if not modules:
                return {"test": name, "status": "coverage_failure", "detail": "module protection report missing"}
            rejected = next((json.loads(line.split(":", 1)[1]) for line in protected[2].splitlines()
                             if line.startswith("MANGLER_TEST262_MODULE_SYNTAX:")), [])
            return {"test": name, "status": "pass", "protected_modules": modules,
                    "rejected_module_syntax": rejected, **budget}
        if args.contract == "function":
            native = run_script(source, imports=case["imports"])
            unavailable = unsupported_host(native)
            if unavailable:
                return {"test": name, "status": "unsupported_host", "stage": "native_function_wrapper",
                        "capabilities": unavailable, "detail": diagnostics(native)}
            if not successful(native) or native[1] != original[1]:
                return {"test": name, "status": "wrapper_inapplicable", "stage": "native_function_wrapper",
                        "original": diagnostics(original), "wrapper": diagnostics(native)}
        # Whole-program selection already rejects unsupported source bodies and
        # top-level partitions. A function-name requirement would incorrectly
        # reject valid scripts containing no function declarations at all.
        coverage = (["--virtualize-program"] if args.contract == "script"
                    else ["--require-virtualized", "__mangler_test262"])
        transformed = execute([str(args.binary.resolve()), "-", "--lang", "js", "--preset", "minify",
                               "--seed", str(args.seed), "--keep-names", "*",
                               *coverage], source, args.timeout)
        if transformed[0]:
            return {"test": name, "status": "coverage_failure", "detail": transformed[2][-4000:]}
        budget = protection_budget(native)
        protected_imports = {**case["imports"], "binary": str(args.binary.resolve())}
        protected = (run_script(transformed[1], case["setup"], case["complete"], case["negative"],
                                imports=protected_imports, outer_timeout=budget["protected_outer_timeout_seconds"])
                     if args.contract == "script" else run_script(transformed[1], imports=protected_imports,
                                                                    outer_timeout=budget["protected_outer_timeout_seconds"]))
        if "MANGLER_TEST262_MODULE_COVERAGE:" in protected[2]:
            return {"test": name, "status": "coverage_failure", "detail": diagnostics(protected), **budget}
        if protected[0] or protected[1] != native[1]:
            return {"test": name, "status": "semantic_failure", "detail": protected[2][-4000:],
                    "native_stdout": native[1], "protected_stdout": protected[1], **budget}
        if sorted(loaded_js_modules(protected)) != sorted(budget["native_js_modules"]):
            return {"test": name, "status": "semantic_failure", "stage": "module_load_inventory",
                    "protected_js_modules": loaded_js_modules(protected), **budget}
        modules = next((json.loads(line.split(":", 1)[1]) for line in protected[2].splitlines()
                        if line.startswith("MANGLER_TEST262_MODULES:")), [])
        rejected = next((json.loads(line.split(":", 1)[1]) for line in protected[2].splitlines()
                         if line.startswith("MANGLER_TEST262_MODULE_SYNTAX:")), [])
        return {"test": name, "status": "pass", "protected_modules": modules,
                "rejected_module_syntax": rejected, **budget}

    revision = subprocess.run(["git", "-C", str(root), "rev-parse", "HEAD"],
                              text=True, capture_output=True, check=False).stdout.strip()
    journal_path = args.output.with_suffix(args.output.suffix + ".jsonl") if args.output else None
    adapter_path = args.output.with_suffix(args.output.suffix + ".runner.py") if args.output else None
    host_archive = adapter_path.with_suffix(".host.cjs") if adapter_path else None
    if adapter_path:
        adapter_path.write_bytes(adapter_source)
        host_archive.write_bytes(host_source)
    host_record = {"runner_sha256": runner_hash, "host_sha256": host_hash,
                   "host_module": str(host_archive) if host_archive else str(host_path),
                   "host_capabilities": host_capabilities, "node_flags": node_flags}
    journal = journal_path.open("w", buffering=1) if journal_path else None
    counts = Counter()
    try:
        if journal:
            journal.write(json.dumps({"type": "run", **host_record, "test262_revision": revision,
                                      "binary_sha256": binary_hash, "seed": args.seed,
                                      "adapter_sha256": adapter_hash,
                                      "adapter": str(adapter_path),
                                      "contract": args.contract,
                                      "binary": str(args.binary.resolve()), "node": args.node,
                                      "node_version": node_version}) + "\n")
        # ThreadPoolExecutor.map eagerly consumes the entire input on supported
        # Python versions. Keep source/harness strings bounded by worker count.
        iterator = iter(cases())
        with ThreadPoolExecutor(max_workers=args.jobs) as pool:
            pending = set()
            exhausted = False
            while pending or not exhausted:
                while not exhausted and len(pending) < 2 * args.jobs:
                    case = next(iterator, None)
                    if case is None:
                        exhausted = True
                    else:
                        pending.add(pool.submit(check, case))
                if not pending:
                    break
                completed, pending = wait(pending, return_when=FIRST_COMPLETED)
                for future in completed:
                    result = future.result()
                    results.append(result)
                    counts[result["status"]] += 1
                    if journal:
                        journal.write(json.dumps({"type": "result", **result}) + "\n")
                    if result["status"] != "pass":
                        print(f"{result['status']}: {result['test']}", file=sys.stderr, flush=True)
                    if len(results) % 100 == 0:
                        print(f"Completed {len(results)}: {json.dumps(dict(counts), sort_keys=True)}",
                              file=sys.stderr, flush=True)
    finally:
        if journal:
            journal.close()
    results.sort(key=lambda result: result["test"])
    summary = dict(counts)
    binary_unchanged = hashlib.sha256(args.binary.read_bytes()).hexdigest() == binary_hash
    report = {**host_record, "test262_revision": revision, "seed": args.seed, "contract": args.contract,
              "adapter_sha256": adapter_hash, "adapter": str(adapter_path) if adapter_path else None,
              "node": args.node, "node_version": node_version,
              "binary": str(args.binary.resolve()), "binary_sha256": binary_hash,
              "binary_unchanged": binary_unchanged,
              "selection": {"manifest": str(args.manifest) if args.manifest else None,
                            "files": len(files), "fixture_files": len(fixtures), "paths": requested_paths},
              "journal": str(journal_path) if journal_path else None,
              "summary": summary, "results": results}
    if args.output:
        args.output.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(summary, sort_keys=True))
    return int(not binary_unchanged or not summary.get("pass") or any(result["status"] not in ("pass", "inapplicable", "wrapper_inapplicable") for result in results))


if __name__ == "__main__":
    sys.exit(main())
