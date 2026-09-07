#!/usr/bin/env python3
"""Run focused JS regressions with an explicitly selected Node executable."""

import argparse
import json
from pathlib import Path
import subprocess
import tempfile


def run(command, **kwargs):
    return subprocess.run(command, text=True, capture_output=True, timeout=20, **kwargs)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--node", required=True, help="Path to the Node executable")
    parser.add_argument("--binary", required=True, help="Path to the mangler executable")
    args = parser.parse_args()
    binary = str(Path(args.binary).resolve())
    node = str(Path(args.node).resolve())
    checks = 0
    with tempfile.TemporaryDirectory(prefix="mangler-js-correctness-") as temporary:
        directory = Path(temporary)

        def transform(source, flags):
            result = run([binary, "-", "--lang", "js", "--seed", "1", *flags], input=source)
            assert result.returncode == 0, result.stderr
            return result.stdout

        def compare(source, flags, setup=""):
            nonlocal checks
            transformed = transform(source, flags)
            observed = []
            for index, program in enumerate([source, transformed]):
                path = directory / f"case-{checks}-{index}.cjs"
                path.write_text(setup + program)
                result = run([node, str(path)])
                assert result.returncode == 0, result.stderr
                observed.append(result.stdout)
            assert observed[0] == observed[1], (source, flags, observed)
            checks += 1

        global_flags = ["--preset", "minify", "--global-indirect", "aggressive"]
        for source in [
            "console.log(typeof require,typeof module,typeof exports,typeof __filename,typeof __dirname);",
            "console.log(require('node:path').basename(__filename).endsWith('.cjs'),module.exports===exports,typeof __dirname);",
            "globalThis.parseInt=()=>91;console.log(parseInt('3'));",
            "let hits=0;Object.defineProperty(globalThis,'liveValue',{get(){return ++hits}});console.log(liveValue,liveValue,hits);",
            "globalThis.Custom=function(){this.value=1};let a=new Custom();globalThis.Custom=function(){this.value=2};console.log(a.value,new Custom().value);",
            "globalThis.customGlobal=function(){'use strict';return this===undefined};console.log(customGlobal());",
            "try{missingManglerGlobal}catch(e){console.log(e.name)}console.log(typeof (missingManglerGlobal));",
            "function f(){return arguments[0]}console.log(f(7));",
            "let globalThis={};console.log(Math.PI);",
        ]:
            compare(source, global_flags)

        # Separate vm.Script evaluations model global lexical bindings established
        # by an earlier browser script (which are absent from globalThis).
        source = "console.log(sharedExternal);"
        transformed = transform(source, global_flags)
        setup = "const vm=require('node:vm');vm.runInThisContext('let sharedExternal=17');"
        outputs = []
        for program in [source, transformed]:
            result = run([node, "-e", setup + "vm.runInThisContext(" + json.dumps(program) + ");"])
            assert result.returncode == 0, result.stderr
            outputs.append(result.stdout)
        assert outputs == ["17\n", "17\n"], outputs
        checks += 1

        strict = "function f(a){'use strict';function nested(){return this===undefined}a=9;return [this===undefined,arguments[0],nested()]}console.log(JSON.stringify(f(1)));"
        for extra in [["--control-flow", "true"], ["--dead-code", "1"]]:
            compare(strict, ["--preset", "minify", *extra])
            compare("'use strict';function f(){let x=1;x++;return [this===undefined,x]}console.log(JSON.stringify(f()));", ["--preset", "minify", *extra])
        compare(
            "const f=()=>{'use strict';return (function(){return this===undefined})()};console.log(f());",
            ["--preset", "low"],
        )
        compare(
            "'use strict';console.log((function(){return this===undefined})());",
            ["--preset", "minify", "--self-defending", "true"],
        )

        for source in [
            "import data from './data.json' with {type:'json'};console.log(data);",
            "export {default as data} from './data.json' with {type:'json'};",
            "export * from './data.json' with {type:'json'};",
        ]:
            output = transform(source, ["--preset", "low", "--verify"])
            path = directory / "attributes.mjs"
            path.write_text(output)
            result = run([node, "--check", str(path)])
            assert result.returncode == 0, result.stderr
            checks += 1

    print(f"{checks} JavaScript correctness checks passed")


if __name__ == "__main__":
    main()
