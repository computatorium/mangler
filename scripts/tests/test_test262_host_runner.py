"""Real Node host execution; set MANGLER_TEST262_ROOT/BINARY for CLI integration."""
import hashlib
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
ADAPTER = SCRIPTS / "check-test262-vm.py"
spec = importlib.util.spec_from_file_location("test262_host_adapter", ADAPTER)
adapter = importlib.util.module_from_spec(spec)
spec.loader.exec_module(adapter)
HOST_SOURCE = (SCRIPTS / "test262-host.cjs").read_text()
NODE = os.environ.get("MANGLER_TESTKIT_NODE") or shutil.which("node")
ROOT = os.environ.get("MANGLER_TEST262_ROOT")
BINARY = os.environ.get("MANGLER_TEST262_BINARY")
DRIVER = "function $DONE(e){if(e)throw e;console.log('MANGLER_TEST262_OK')}"


@unittest.skipUnless(NODE, "Node is required")
class HostRunner(unittest.TestCase):
    def execute(self, body, setup=DRIVER, negative=None, gc=False):
        return adapter.execute([NODE, *(["--expose-gc"] if gc else []), "-e", adapter.NODE_RUNNER],
                               json.dumps({"host_source": HOST_SOURCE, "body": body, "setup": setup,
                                           "complete": True, "negative": negative, "timeout": 1000}), 3)

    def test_realms_eval_and_runtime_negative(self):
        result = self.execute("let child=$262.createRealm(); if(child.global.Object===Object)throw Error();"
                              "child.evalScript('let x=40'); if(child.evalScript('x+2')!==42)throw Error();")
        self.assertEqual(result[0], 0, result)
        self.assertIn("MANGLER_TEST262_OK", result[1])
        self.assertEqual(self.execute("throw new TypeError()", negative="TypeError")[0], 0)
        self.assertNotEqual(self.execute("throw new ($262.createRealm().global.TypeError)()", negative="TypeError")[0], 0)
        self.assertNotEqual(self.execute("42", negative="TypeError")[0], 0)

    def test_supported_host_is_installed_before_included_harness(self):
        setup = "const foreign=$262.createRealm(); const b=new ArrayBuffer(4);$262.detachArrayBuffer(b);" + DRIVER
        result = self.execute("if(b.byteLength!==0||foreign.global.Object===Object)throw Error();", setup=setup)
        self.assertEqual(result[0], 0, result)

    def test_unavailable_requests_remain_visible_even_when_caught_or_in_child_realm(self):
        for body, expected in [("try{$262.createRealm().gc()}catch(e){}", ["gc"]),
                               ("try{$262.AbstractModuleSource}catch(e){}", ["AbstractModuleSource"])]:
            with self.subTest(body=body):
                result = self.execute(body)
                self.assertNotEqual(result[0], 0)
                self.assertEqual(adapter.unsupported_host(result), expected)
        self.assertEqual(adapter.unsupported_host(self.execute("throw new Error('ordinary failure')")), [])

    def test_gc_requires_explicit_real_node_capability(self):
        result = self.execute("$262.gc()", gc=True)
        self.assertEqual(result[0], 0, result)
        self.assertEqual(adapter.unsupported_host(result), [])

    def test_script_and_subprocess_deadlines_are_failures(self):
        result = self.execute("while(true){}")
        self.assertNotEqual(result[0], 0)
        self.assertIn("Script execution timed out", result[2])
        self.assertEqual(adapter.unsupported_host(result), [])

    @unittest.skipUnless(ROOT and BINARY, "MANGLER_TEST262_ROOT and MANGLER_TEST262_BINARY are required")
    def test_real_protected_host_cases_and_replayable_archive(self):
        with tempfile.TemporaryDirectory() as directory:
            temp = Path(directory)
            (temp / "tools").mkdir()
            (temp / "tools" / "packaging").symlink_to(Path(ROOT).resolve() / "tools" / "packaging", target_is_directory=True)
            (temp / "harness").symlink_to(Path(ROOT).resolve() / "harness", target_is_directory=True)
            (temp / "test").mkdir()
            records = {
                "realm": ("", "var r=$262.createRealm();assert.notSameValue(r.global.Object,Object);assert.sameValue(r.evalScript('this'),r.global);"),
                "detach": ("includes: [detachArrayBuffer.js]\n", "var b=new ArrayBuffer(8);$DETACHBUFFER(b);assert.sameValue(b.byteLength,0);assert.throws(TypeError,function(){new DataView(b)});"),
                "negative": ("negative:\n  phase: runtime\n  type: TypeError\n", "throw new TypeError('expected');"),
                "agent": ("", "$262.agent.start(' $262.agent.report(42); $262.agent.leaving(); ');var r;while((r=$262.agent.getReport())===null)$262.agent.sleep(1);assert.sameValue(r,'42');"),
                "html": ("features: [IsHTMLDDA]\n", "throw new Error('must not execute without IsHTMLDDA');"),
                "gc": ("", "$262.gc();"),
            }
            for name, (metadata, body) in records.items():
                (temp / "test" / (name + ".js")).write_text("/*---\ndescription: Host adapter fixture\ngenerated: true\nflags: [noStrict]\n" + metadata + "---*/\n" + body)
            report = temp / "report.json"
            command = [sys.executable, str(ADAPTER), "--test262", str(temp), "--binary", str(Path(BINARY).resolve()),
                       "--node", NODE, "--contract", "script", "--timeout", "10", "--output", str(report), "test"]
            result = subprocess.run(command, text=True, capture_output=True, timeout=90)
            self.assertEqual(result.returncode, 1, result.stderr)  # Unsupported hosts remain uncompleted work.
            data = json.loads(report.read_text())
            self.assertEqual(data["summary"], {"unsupported_host": 2, "pass": 4}, data)
            self.assertFalse(data["host_capabilities"]["gc"])
            self.assertEqual(data["node_flags"], ["--experimental-vm-modules"])
            for case in data["results"]:
                if case["status"] == "pass":
                    self.assertEqual(case["native_js_modules"], [])
                    self.assertEqual(case["native_js_module_count"], 0)
                    self.assertEqual(case["module_transform_timeout_seconds"], 10)
                    self.assertEqual(case["execution_timeout_seconds"], 10)
                    self.assertEqual(case["protected_outer_timeout_seconds"], 10)
            archived_runner = Path(data["adapter"])
            archived_host = Path(data["host_module"])
            self.assertEqual(archived_host, archived_runner.with_suffix(".host.cjs"))
            self.assertEqual(hashlib.sha256(archived_host.read_bytes()).hexdigest(), data["host_sha256"])
            self.assertEqual(hashlib.sha256(archived_runner.read_bytes()).hexdigest(), data["runner_sha256"])
            digest = hashlib.sha256(archived_runner.read_bytes() + b"\0test262-host.cjs\0" + archived_host.read_bytes()).hexdigest()
            self.assertEqual(digest, data["adapter_sha256"])
            metadata = json.loads(Path(data["journal"]).read_text().splitlines()[0])
            self.assertEqual(metadata["host_sha256"], data["host_sha256"])
            # Run the archived pair, outside the source tree, using its real gc.
            replay = command.copy()
            replay[1] = str(archived_runner)
            replay[replay.index("--output") + 1] = str(temp / "replay.json")
            replay[-1] = "test/gc.js"
            replay.insert(2, "--host-gc")
            result = subprocess.run(replay, text=True, capture_output=True, timeout=30)
            self.assertEqual(result.returncode, 0, result.stderr)
            replay_data = json.loads((temp / "replay.json").read_text())
            self.assertEqual(replay_data["summary"], {"pass": 1})
            self.assertTrue(replay_data["host_capabilities"]["gc"])
            self.assertEqual(replay_data["node_flags"], ["--expose-gc", "--experimental-vm-modules"])
            self.assertEqual(replay_data["adapter_sha256"], data["adapter_sha256"])


if __name__ == "__main__":
    unittest.main()
