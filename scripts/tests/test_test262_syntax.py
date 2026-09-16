"""Run with python3 -m unittest discover -s scripts/tests -p test_test262_syntax.py.

MANGLER_TESTKIT_NODE selects Node. MANGLER_TEST262_ROOT enables fixture tests
using the official metadata reader; MANGLER_TEST262_BINARY additionally enables
real CLI parse-contract integration. No test builds Cargo or edits a checkout.
"""
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest


ADAPTER = Path(__file__).resolve().parents[1] / "check-test262-vm.py"
spec = importlib.util.spec_from_file_location("test262_adapter", ADAPTER)
adapter = importlib.util.module_from_spec(spec)
spec.loader.exec_module(adapter)
NODE = os.environ.get("MANGLER_TESTKIT_NODE") or shutil.which("node")
ROOT = os.environ.get("MANGLER_TEST262_ROOT")
BINARY = os.environ.get("MANGLER_TEST262_BINARY")


class SyntaxContract(unittest.TestCase):
    def test_only_exact_parse_rejection_passes(self):
        rejection = json.dumps({"phase": "parse", "error": "SyntaxError"})
        self.assertEqual(adapter.syntax_outcome((1, rejection, "")), "rejected")
        for code, stdout, stderr in [
            (0, rejection, ""), (-1, rejection, "execution deadline exceeded"),
            (-11, rejection, ""), (101, rejection, "panic"),
            (1, "", "<stdin>: parse error (js): expected token"),
            (1, '{"phase":"coverage","error":"SyntaxError"}', ""),
            (1, rejection + "\nnot JSON", ""),
            (1, '{"phase":"parse","error":"TypeError"}', ""),
            (1, '{"phase":"parse","error":"SyntaxError","extra":true}', ""),
        ]:
            with self.subTest(code=code, stdout=stdout):
                self.assertEqual(adapter.syntax_outcome((code, stdout, stderr)), "protocol_failure")
        self.assertEqual(adapter.syntax_outcome((0, '{"phase":"parse","error":null}', "")), "accepted")

    def test_strict_and_raw_source_variants(self):
        body = "var x = ;"
        self.assertEqual(list(adapter.syntax_variants({"test": body})),
                         [("sloppy", body), ("strict", '"use strict";\n' + body)])
        self.assertEqual(list(adapter.syntax_variants({"test": body, "flags": ["onlyStrict"]})),
                         [("strict", '"use strict";\n' + body)])
        self.assertEqual(list(adapter.syntax_variants({"test": body, "flags": ["noStrict"]})),
                         [("sloppy", body)])
        raw = "#!/not-an-executable\n/* metadata */\n" + body
        self.assertEqual(list(adapter.syntax_variants({"test": body, "flags": ["raw"]}, raw)),
                         [("raw", raw)])
        self.assertEqual(list(adapter.syntax_variants({"test": body, "flags": ["module"]})),
                         [("module", body)])
        self.assertEqual(list(adapter.syntax_variants({"test": body, "flags": ["module", "raw"]}, raw)),
                         [("module", raw)])

    @unittest.skipUnless(NODE, "Node is required")
    def test_native_compiles_without_execution_or_wrapper(self):
        for body, expected in [
            ("throw new SyntaxError('runtime is not parse');", "accepted"),
            ("process.exit(99);", "accepted"),
            ("return;", "rejected"),
            ("var x = ;", "rejected"),
            ("var await = 1;", "accepted"),
            ("import 'missing';", "rejected"),
        ]:
            with self.subTest(body=body):
                result = adapter.execute([NODE, "-e", adapter.NODE_SYNTAX_RUNNER],
                                         json.dumps({"body": body}), 5)
                self.assertEqual(adapter.syntax_outcome(result), expected, result)

    @unittest.skipUnless(NODE, "Node is required")
    def test_native_module_compiles_without_link_or_evaluation(self):
        for body, expected in [
            ("import 'does-not-exist';", "accepted"),
            ("export * from 'does-not-exist';", "accepted"),
            ("await new Promise(() => {}); process.exit(99);", "accepted"),
            ("throw new SyntaxError('runtime');", "accepted"),
            ("export var x = ;", "rejected"),
            ("export const x = 1; export {x};", "rejected"),
            ("with({}){}", "rejected"),
            ("var await = 1;", "rejected"),
        ]:
            with self.subTest(body=body):
                result = adapter.execute([NODE, "--experimental-vm-modules", "-e", adapter.NODE_SYNTAX_RUNNER],
                                         json.dumps({"body": body, "goal": "module"}), 5)
                self.assertEqual(adapter.syntax_outcome(result), expected, result)

    @unittest.skipUnless(NODE and ROOT, "Node and MANGLER_TEST262_ROOT are required")
    def test_fixture_selection_and_cli_protocol(self):
        with tempfile.TemporaryDirectory() as directory:
            temp = Path(directory)
            (temp / "tools").mkdir()
            (temp / "tools" / "packaging").symlink_to(Path(ROOT).resolve() / "tools" / "packaging",
                                                       target_is_directory=True)
            (temp / "test").mkdir()
            records = {
                "parse": ("", "phase: parse\n  type: SyntaxError", "var x = ;"),
                "early": ("[onlyStrict]", "phase: early\n  type: SyntaxError", "with({}){}"),
                "raw": ("[raw]", "phase: parse\n  type: SyntaxError", "var x = ;"),
                "host": ("[noStrict]", "phase: parse\n  type: SyntaxError", "$262; var x = ;"),
                "runtime": ("[noStrict]", "phase: runtime\n  type: SyntaxError", "throw new SyntaxError();"),
                "module": ("[module]", "phase: parse\n  type: SyntaxError", "export var x = ;"),
                "resolution": ("[module]", "phase: resolution\n  type: SyntaxError", "import {missing} from 'missing';"),
                "positive": ("[noStrict]", None, "throw new Error('must not execute');"),
            }
            for name, (flags, negative, body) in records.items():
                attrs = "description: Syntax adapter fixture\ngenerated: true\n"
                if flags:
                    attrs += "flags: " + flags + "\n"
                if negative:
                    attrs += "negative:\n  " + negative + "\n"
                (temp / "test" / (name + ".js")).write_text("/*---\n" + attrs + "---*/\n" + body)
            # This executable models a protocol producer, not a JS implementation.
            fake = temp / "fake-cli.py"
            fake.write_text("#!" + sys.executable + "\nimport json,sys\n"
                            "assert sys.argv[1:5] == ['-', '--lang', 'js', '--check-syntax'] and sys.argv[5] in ('script','module')\n"
                            "sys.stdin.read()\nprint(json.dumps({'phase':'parse','error':'SyntaxError'}))\nsys.exit(1)\n")
            fake.chmod(0o755)
            report = temp / "report.json"
            command = [sys.executable, str(ADAPTER), "--test262", str(temp), "--binary", str(fake),
                       "--node", NODE, "--contract", "syntax", "--output", str(report), "test"]
            result = subprocess.run(command, text=True, capture_output=True, timeout=30)
            self.assertEqual(result.returncode, 0, result.stderr)
            data = json.loads(report.read_text())
            self.assertEqual(data["summary"], {"pass": 6, "inapplicable": 3})
            self.assertEqual({row["test"] for row in data["results"] if row["status"] == "pass"},
                             {"test/parse.js:sloppy", "test/parse.js:strict", "test/early.js:strict",
                              "test/raw.js:raw", "test/host.js:sloppy", "test/module.js:module"})
            self.assertTrue(data["binary_unchanged"])
            # An arbitrary crash cannot become a negative pass.
            fake.write_text("#!" + sys.executable + "\nimport sys\nsys.exit(101)\n")
            result = subprocess.run(command, text=True, capture_output=True, timeout=30)
            self.assertEqual(result.returncode, 1)
            self.assertEqual(json.loads(report.read_text())["summary"],
                             {"syntax_protocol_failure": 6, "inapplicable": 3})

    @unittest.skipUnless(NODE and BINARY, "Node and MANGLER_TEST262_BINARY are required")
    def test_real_cli_parse_goal_and_rejection(self):
        for goal, body, expected in [
            ("script", "var x = ;", "rejected"),
            ("script", "return;", "rejected"),
            ("script", "import 'missing';", "rejected"),
            ("module", "import 'missing';", "accepted"),
            ("module", "with({}){}", "rejected"),
            ("module", "await new Promise(() => {});", "accepted"),
            ("script", "throw new SyntaxError();", "accepted"),
            ("script", "process.exit(99);", "accepted"),
        ]:
            with self.subTest(goal=goal, body=body):
                result = adapter.execute([BINARY, "-", "--lang", "js", "--check-syntax", goal], body, 5)
                self.assertEqual(adapter.syntax_outcome(result), expected, result)


if __name__ == "__main__":
    unittest.main()
