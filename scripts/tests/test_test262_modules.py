"""Exercise native module linking and protection of every loaded dependency."""
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

SCRIPTS = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("module_adapter", SCRIPTS / "check-test262-vm.py")
adapter = importlib.util.module_from_spec(spec)
spec.loader.exec_module(adapter)
NODE = os.environ.get("MANGLER_TESTKIT_NODE") or shutil.which("node")
BINARY = os.environ.get("MANGLER_TEST262_BINARY")
ROOT = os.environ.get("MANGLER_TEST262_ROOT")
HOST = (SCRIPTS / "test262-host.cjs").read_text()


@unittest.skipUnless(NODE, "Node required")
class ModuleRunner(unittest.TestCase):
    def execute_graph(self, files, *, phase=None, negative=None, protected=False, complete=True, script=False, dependency_binary=None, setup=""):
        with tempfile.TemporaryDirectory(prefix="mangler-module-contract-") as directory:
            root = Path(directory)
            for name, source in files.items():
                path = root / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(source)
            module = {"path": str(root / "main.js"), "root": directory,
                      "seed": 42, "negative_phase": phase}
            if protected or dependency_binary:
                module["binary"] = str(Path(dependency_binary or BINARY).resolve())
            body = files["main.js"]
            if protected and script:
                transformed = adapter.execute([str(Path(BINARY).resolve()), "-", "--lang", "js",
                                               "--preset", "minify", "--seed", "42", "--keep-names", "*",
                                               "--virtualize-program"], body, 15)
                self.assertEqual(transformed[0], 0, transformed)
                body = transformed[1]
            payload = {"host_source": HOST, "body": body, "timeout": 5000,
                       "setup": setup + "\nlet completed=false;function $DONE(e){if(e)throw e;if(completed)throw Error('duplicate');completed=true;console.log('MANGLER_TEST262_OK')}process.on('beforeExit',()=>{if(!completed)process.exitCode=1})",
                       "complete": complete, "negative": negative,
                       "module": None if script else module, "imports": module if script else None}
            result = adapter.execute([NODE, "--experimental-vm-modules", "-e", adapter.NODE_RUNNER],
                                     json.dumps(payload), 15)
            return result

    def test_cycles_live_bindings_and_shared_dynamic_namespaces(self):
        files = {
            "main.js": "import {value,add} from './dep.js';export function initial(){return 40}add();if(value!==42)throw Error(value);const a=await import('./dynamic.js'),b=await import('./dynamic.js');if(a!==b||a.value!==42||this!==undefined)throw Error('module identity');",
            "dep.js": "import {initial} from './main.js';export let value=initial();export function add(){value+=2}",
            "dynamic.js": "export const value=42;",
        }
        for protected in [False, *([True] if BINARY else [])]:
            with self.subTest(protected=protected):
                result = self.execute_graph(files, protected=protected)
                self.assertEqual(result[0], 0, result)
                self.assertEqual(result[1], "MANGLER_TEST262_OK\n")
                if protected:
                    modules = json.loads(next(line.split(":", 1)[1] for line in result[2].splitlines()
                                              if line.startswith("MANGLER_TEST262_MODULES:")))
                    self.assertEqual(len(modules), 3, modules)
                    self.assertEqual(len(set(modules)), 3)

    def test_resolution_errors_never_evaluate_source(self):
        files = {"main.js": "import {missing} from './dep.js';throw Error('executed');",
                 "dep.js": "export const present=1;throw Error('executed dependency');"}
        for protected in [False, *([True] if BINARY else [])]:
            result = self.execute_graph(files, protected=protected, phase="resolution", negative="SyntaxError")
            self.assertEqual(result[0], 0, result)
        result = self.execute_graph({"main.js": "throw new SyntaxError();"}, phase="resolution", negative="SyntaxError")
        self.assertNotEqual(result[0], 0, result)

    def test_link_failure_identity_is_cached_per_module_and_realm(self):
        imports = "Promise.all([import('./bad.js').catch(e=>e),import('./bad.js').catch(e=>e)]).then(async errors=>[...errors,await import('./bad.js').catch(e=>e)])"
        files = {
            "main.js": "const child=$262.createRealm();Promise.all([" + imports + ",child.evalScript(" + json.dumps(imports) + ")]).then(([parent,foreign])=>{for(const [errors,Expected] of [[parent,SyntaxError],[foreign,child.global.SyntaxError]]){if(errors.some(e=>e.constructor!==Expected)||errors[0]!==errors[1]||errors[1]!==errors[2])throw Error('link failure identity')}if(parent[0]===foreign[0])throw Error('cross-realm error cache');$DONE()},$DONE);",
            "bad.js": "import {missing} from './dep.js';throw Error('evaluated');",
            "dep.js": "export const present=1;",
        }
        for script in [False, True]:
            with self.subTest(script=script):
                result = self.execute_graph(files, script=script, complete=False)
                self.assertEqual(result[0], 0, result)
                self.assertEqual(result[1], "MANGLER_TEST262_OK\n")

    def test_overlapping_graphs_reuse_evaluation_error_and_still_validate_new_imports(self):
        for concurrent in [False, True]:
            requests = ["grab('./a.js')", "grab('./b.js')", "grab('./c.js')", "grab('./a.js')"]
            collected = ("await Promise.all([" + ",".join(requests) + "])" if concurrent
                         else "[" + ",".join("await " + request for request in requests) + "]")
            files = {
                "main.js": "(async()=>{async function grab(path){try{return await import(path)}catch(e){return e}}const errors=" + collected + ";if(errors.some(e=>e!==errors[0]||e.constructor!==TypeError||e.message!=='original'))throw Error('evaluation error identity');const syntax=await grab('./invalid.js');if(syntax.constructor!==SyntaxError)throw Error('new root must validate its imports');})().then($DONE,$DONE);",
                "a.js": "import './b.js';",
                "b.js": "export const present=1;throw new TypeError('original');",
                "c.js": "import './b.js';",
                "invalid.js": "import {missing} from './b.js';",
            }
            for script in [False, True]:
                for protected in [False, *([True] if BINARY else [])]:
                    with self.subTest(concurrent=concurrent, script=script, protected=protected):
                        result = self.execute_graph(files, script=script, complete=False, protected=protected)
                        self.assertEqual(result[0], 0, result)
                        self.assertEqual(result[1], "MANGLER_TEST262_OK\n")

    def test_runtime_errors_use_the_test_realm(self):
        for source, succeeds in [("throw new TypeError();", True),
                                 ("throw new ($262.createRealm().global.TypeError)();", False),
                                 ("export {};", False)]:
            result = self.execute_graph({"main.js": source}, phase="runtime", negative="TypeError")
            self.assertEqual(result[0] == 0, succeeds, result)

    def test_runtime_negative_constructor_is_captured_after_setup_only_in_entry_realm(self):
        # Child realms intentionally do not receive the entry harness definition.
        # Source mutation must not change the already-captured expected type.
        files = {"main.js": "const Expected=Test262Error;$262.createRealm();globalThis.Test262Error=function replacement(){};throw new Expected();"}
        for script in [False, True]:
            with self.subTest(script=script):
                result = self.execute_graph(files, script=script, phase="runtime", negative="Test262Error",
                                            setup="globalThis.Test262Error=class Test262Error extends Error {};")
                self.assertEqual(result[0], 0, result)
                self.assertEqual(result[1], "MANGLER_TEST262_OK\n")

    def test_invalid_dependency_is_rejected_by_both_parsers_without_execution(self):
        for protected in [False, *([True] if BINARY else [])]:
            result = self.execute_graph({
                "main.js": "await import('./bad.js').then(()=>{throw Error('accepted')},e=>{if(e.constructor!==SyntaxError)throw e});",
                "bad.js": "throw Error('executed');break;",
            }, protected=protected)
            self.assertEqual(result[0], 0, result)
            if protected:
                self.assertIn('MANGLER_TEST262_MODULE_SYNTAX:', result[2])
                self.assertNotIn('MANGLER_TEST262_MODULE_COVERAGE:', result[2])

    def test_json_modules_have_realm_values_and_shared_identity(self):
        for protected in [False, *([True] if BINARY else [])]:
            result = self.execute_graph({
                "main.js": "import value from './data.json' with {type:'json'};const other=await import('./data.json',{with:{type:'json'}});if(value!==other.default||Object.getPrototypeOf(value)!==Object.prototype||value.answer!==42)throw Error();await import('./data.json').then(()=>{throw Error('missing type')},e=>{if(e.constructor!==TypeError)throw e});",
                "data.json": '{"answer":42}',
            }, protected=protected)
            self.assertEqual(result[0], 0, result)

    def test_async_completion_and_unresolved_top_level_await(self):
        result = self.execute_graph({"main.js": "await Promise.resolve();queueMicrotask(()=>$DONE());"}, complete=False)
        self.assertEqual(result[0], 0, result)
        result = self.execute_graph({"main.js": "await new Promise(()=>{});"})
        self.assertNotEqual(result[0], 0, result)
        # Awaiting dynamic import of the current static cycle cannot settle.
        result = self.execute_graph({"main.js": "import './dep.js';await import('./dep.js');",
                                     "dep.js": "import './main.js';"})
        self.assertNotEqual(result[0], 0, result)

    def test_script_imports_share_cycles_and_fixture_callbacks_in_the_script_realm(self):
        files = {
            "main.js": "let observed=[];globalThis.callback=value=>observed.push(value);Promise.all([import('./a.js'),import('./a.js'),import('./b.js')]).then(([a,again,b])=>{if(a!==again||a.realmObject!==Object||a.read()!==42||b.read()!==42)throw Error('identity');a.call();if(observed.join()!=='42')throw Error('callback');return import('./a.js')}).then(()=>$DONE(),$DONE);",
            "a.js": "import {value} from './b.js';export const realmObject=Object;export function read(){return value}export function call(){globalThis.callback(value)}",
            "b.js": "import {read as other} from './a.js';export const value=42;export function read(){return other()}",
        }
        for protected in [False, *([True] if BINARY else [])]:
            result = self.execute_graph(files, script=True, complete=False, protected=protected)
            self.assertEqual(result[0], 0, result)
            if protected:
                modules = json.loads(next(line.split(":", 1)[1] for line in result[2].splitlines()
                                          if line.startswith("MANGLER_TEST262_MODULES:")))
                self.assertEqual(len(modules), 2, modules)
                self.assertEqual(len(set(modules)), 2)

    def test_concurrent_imports_wait_for_shared_top_level_await_once(self):
        files = {
            "main.js": "globalThis.count=0;let settled=0;const a=import('./a.js').then(m=>{settled++;return m}),b=import('./b.js').then(m=>{settled++;return m});setTimeout(()=>{if(settled!==0)throw Error('premature settlement');globalThis.release()},0);Promise.all([a,b]).then(([a,b])=>{if(count!==1||a.value!==42||b.value!==42)throw Error('shared evaluation');$DONE()},$DONE);",
            "a.js": "export {value} from './shared.js';",
            "b.js": "export {value} from './shared.js';",
            "shared.js": "globalThis.count++;await new Promise(resolve=>globalThis.release=resolve);export const value=42;",
        }
        for script in [False, True]:
            for protected in [False, *([True] if BINARY else [])]:
                result = self.execute_graph(files, script=script, complete=False, protected=protected)
                self.assertEqual(result[0], 0, result)

    def test_script_self_import_is_a_distinct_module_loaded_once(self):
        files = {"main.js": "var first=globalThis.count===undefined;globalThis.count=(globalThis.count||0)+1;var work=Promise.all([import('./main.js'),import('./main.js')]).then(([a,b])=>{if(a!==b||globalThis.count!==2)throw Error('self identity')});if(first)work.then($DONE,$DONE);"}
        for protected in [False, *([True] if BINARY else [])]:
            result = self.execute_graph(files, script=True, complete=False, protected=protected)
            self.assertEqual(result[0], 0, result)

    def test_script_eval_inherits_fixture_resolution_and_link_errors_use_realm_intrinsics(self):
        files = {
            "main.js": "const Expected=SyntaxError;globalThis.SyntaxError=function replacement(){};Promise.all([eval(\"import('./bad.js')\"),import('./bad.js')].map(p=>p.then(()=>{throw Error('accepted')},error=>{if(error.constructor!==Expected)throw error}))).then(()=>$DONE(),$DONE);",
            "bad.js": "import {absent} from './dep.js';throw Error('evaluated');",
            "dep.js": "export const present=1;",
        }
        result = self.execute_graph(files, script=True, complete=False)
        self.assertEqual(result[0], 0, result)

    def test_script_attributes_validate_cached_modules_and_json_values_use_realm(self):
        files = {
            "main.js": "import('./data.json',{with:{type:'json'}}).then(async a=>{const b=await import('./data.json',{with:{type:'json'}});if(a!==b||Object.getPrototypeOf(a.default)!==Object.prototype)throw Error('json realm');for(const options of [undefined,{with:{type:'javascript'}},{with:{extra:'value'}}])await import('./data.json',options).then(()=>{throw Error('accepted attributes')},e=>{if(e.constructor!==TypeError)throw e})}).then($DONE,$DONE);",
            "data.json": '{"answer":42}',
        }
        for protected in [False, *([True] if BINARY else [])]:
            result = self.execute_graph(files, script=True, complete=False, protected=protected)
            self.assertEqual(result[0], 0, result)

    def test_eval_script_and_child_realms_get_separate_module_records(self):
        files = {
            "main.js": "const child=$262.createRealm();const source=\"import('./dep.js')\";Promise.all([import('./dep.js'),$262.evalScript(source),child.evalScript(source),child.evalScript(source)]).then(([parent,fresh,foreign,again])=>{if(parent!==fresh||foreign!==again||parent===foreign)throw Error('realm cache');if(Object.getPrototypeOf(parent.value)!==Object.prototype||Object.getPrototypeOf(foreign.value)!==child.global.Object.prototype)throw Error('value realm');if(parent.realm!==Object||foreign.realm!==child.global.Object||globalThis.evaluations!==1||child.global.evaluations!==1)throw Error('evaluation realm');$DONE()},$DONE);",
            "dep.js": "globalThis.evaluations=(globalThis.evaluations||0)+1;export const value={};export const realm=Object;",
        }
        for script in [False, True]:
            for protected in [False, *([True] if BINARY else [])]:
                result = self.execute_graph(files, script=script, complete=False, protected=protected)
                self.assertEqual(result[0], 0, result)
                if protected:
                    modules = json.loads(next(line.split(":", 1)[1] for line in result[2].splitlines()
                                              if line.startswith("MANGLER_TEST262_MODULES:")))
                    self.assertEqual(sum(url.endswith('/dep.js') for url in modules), 2, modules)

    def test_child_loader_errors_and_json_values_use_child_intrinsics(self):
        files = {
            "main.js": "const child=$262.createRealm(),Expected=child.global.SyntaxError;child.evalScript('globalThis.SyntaxError=function replacement(){};JSON.parse=function(){throw Error()}');Promise.all([child.evalScript(\"import('./bad.js')\").then(()=>{throw Error('accepted')},e=>{if(e.constructor!==Expected||e.constructor===SyntaxError)throw Error('syntax realm')}),child.evalScript(\"import('./data.json',{with:{type:'json'}})\").then(m=>{if(Object.getPrototypeOf(m.default)!==child.global.Object.prototype)throw Error('json realm')}),child.evalScript(\"import('./data.json')\").then(()=>{throw Error('accepted')},e=>{if(e.constructor!==child.global.TypeError)throw Error('type realm')})]).then(()=>$DONE(),$DONE);",
            "bad.js": "break;",
            "data.json": '{"value":42}',
        }
        for protected in [False, *([True] if BINARY else [])]:
            result = self.execute_graph(files, script=True, complete=False, protected=protected)
            self.assertEqual(result[0], 0, result)

    @unittest.skipUnless(ROOT and BINARY, "Test262 checkout and binary required")
    def test_cli_budgets_per_realm_module_loads_and_archives_both_loader_files(self):
        with tempfile.TemporaryDirectory(prefix="mangler-script-import-budget-") as directory:
            root = Path(directory)
            (root / "tools").mkdir()
            (root / "tools" / "packaging").symlink_to(Path(ROOT) / "tools" / "packaging", target_is_directory=True)
            (root / "harness").symlink_to(Path(ROOT) / "harness", target_is_directory=True)
            (root / "test").mkdir()
            (root / "test" / "main.js").write_text(
                "/*---\ndescription: Per-realm dynamic import budget\ngenerated: true\nflags: [noStrict, async]\n---*/\n"
                "const child=$262.createRealm();Promise.all([import('./dep_FIXTURE.js'),child.evalScript(\"import('./dep_FIXTURE.js')\")]).then(([a,b])=>{assert.notSameValue(a,b);assert.sameValue(a.value,42);assert.sameValue(b.value,42);$DONE()},$DONE);"
            )
            (root / "test" / "dep_FIXTURE.js").write_text("export const value=42;")
            output = root / "result.json"
            result = subprocess.run([sys.executable, str(SCRIPTS / "check-test262-vm.py"),
                                     "--test262", directory, "--binary", str(Path(BINARY).resolve()),
                                     "--node", NODE, "--contract", "script", "--jobs", "1",
                                     "--timeout", "5", "--output", str(output), "test"],
                                    text=True, capture_output=True, timeout=30)
            self.assertEqual(result.returncode, 0, (result.stdout, result.stderr))
            report = json.loads(output.read_text())
            self.assertEqual(report["summary"], {"pass": 1})
            case = report["results"][0]
            self.assertEqual(case["native_js_module_count"], 2)
            self.assertEqual(len(set(case["native_js_modules"])), 1)
            self.assertEqual(case["protected_modules"], case["native_js_modules"])
            self.assertEqual(case["module_transform_timeout_seconds"], 5)
            self.assertEqual(case["execution_timeout_seconds"], 5)
            self.assertEqual(case["protected_outer_timeout_seconds"], 15)
            self.assertEqual(Path(report["host_module"]).read_text(), HOST)
            self.assertIn("createModuleLoader", Path(report["adapter"]).read_text())

    def test_caught_dependency_compiler_failure_still_fails_required_coverage(self):
        # A real unsuccessful executable represents compiler startup failure.
        failure_binary = shutil.which("false")
        if not failure_binary:
            self.skipTest("false executable required")
        result = self.execute_graph({
            "main.js": "import('./dep.js').then(()=>{throw Error('accepted')},()=>{}).then(()=>$DONE(),$DONE);",
            "dep.js": "export const answer=42;",
        }, script=True, complete=False, dependency_binary=failure_binary)
        self.assertNotEqual(result[0], 0, result)
        self.assertIn("MANGLER_TEST262_OK", result[1])
        self.assertIn("MANGLER_TEST262_MODULE_COVERAGE:", result[2])

    def test_runtime_negative_constructor_is_captured_before_source_mutation(self):
        for script in [False, True]:
            result = self.execute_graph({"main.js": "const Expected=TypeError;globalThis.TypeError=function replacement(){};throw new Expected();"},
                                        script=script, phase="runtime", negative="TypeError")
            self.assertEqual(result[0], 0, result)
            result = self.execute_graph({"main.js": "globalThis.TypeError=function replacement(){};throw new TypeError();"},
                                        script=script, phase="runtime", negative="TypeError")
            self.assertNotEqual(result[0], 0, result)

    def test_script_import_runtime_errors_preserve_foreign_realm_identity(self):
        files = {
            "main.js": "globalThis.foreign=$262.createRealm().global.TypeError;import('./dep.js').then(()=>{throw Error('accepted')},e=>{if(e.constructor!==foreign||e.constructor===TypeError)throw Error('rewrapped')}).then($DONE,$DONE);",
            "dep.js": "throw new globalThis.foreign();",
        }
        for protected in [False, *([True] if BINARY else [])]:
            result = self.execute_graph(files, script=True, complete=False, protected=protected)
            self.assertEqual(result[0], 0, result)


if __name__ == "__main__":
    unittest.main()
