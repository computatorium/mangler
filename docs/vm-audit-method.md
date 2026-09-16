# Virtual machine verification

Run the required-coverage differential matrix after building the CLI:

```sh
cargo build -p mangler-cli
/absolute/path/to/node scripts/check-vm-coverage.cjs \
  --binary target/debug/mangler --output docs/vm-coverage.json
```

Each fixture selects its `pay` function with `--require-virtualized pay`, preserves
names, and asks the CLI to reparse its output. A compiler rejection is a coverage
failure, never a passing semantic comparison. Native and protected programs run
in separate Node processes and fresh JavaScript contexts. Compiler execution has
a 15-second deadline; each runtime has a 5-second process deadline and a 3-second
synchronous script deadline. Module fixtures run as native ES modules in isolated
processes, using data URLs for their import dependencies. Promise results are awaited. Comparison preserves
`undefined`, `BigInt`, negative zero, `NaN`, and infinities.
An unhandled Promise rejection fails the worker even if its selected result was
already correct; startup errors must not disappear behind a successful value.

The default seeds are `0`, `42`, and `4294967295`. `--seeds 42` is useful during
development; `--fixtures name,other_name` narrows a reproduction. The command exits
nonzero if any case fails. Report rows distinguish coverage rejection, behavioral
disagreement, and runtime-process failure. This suite covers concrete examples of
payment arithmetic, language features, and adversarial evaluation-order cases;
its result is not a claim to complete ECMAScript conformance.

`--preset low|medium|high|max` exercises interactions with the rest of the
obfuscation pipeline. `--coverage all` requires every source function instead of
only the named target. `--mode plain` checks the same behavior without requesting
virtualization, and explicitly reports coverage as `none`; a plain-mode pass
cannot establish protection. Script workers provide the `atob`, `btoa`,
`TextEncoder`, and `TextDecoder` host APIs available in current Node and browsers.

The selected-function gate is separate from whole-source coverage. Require `*`
when every source function must be accounted for. Explicitly excluded functions
and native closure factories are native code, even if an enclosing function runs
in the interpreter. A wrapper or factory retaining a body as JavaScript must not
be reported as bytecode protection for that body.

`scripts/check-vm-source-coverage.cjs --binary <frozen-binary>` independently
checks emitted arithmetic retention in parameter initializers, class behavior,
simple scripts, and exported initializers. An explicitly excluded native
function is its positive control: the same arithmetic must remain present there.
The script also checks runtime equivalence and rejection without output for
unsatisfied required targets. Unsupported selected code remains a coverage
failure even when rejection is correct. This targeted retention probe supplements
source accounting; it is not a general proof that arbitrary output contains no
source fragments.

The Test262 adapter's default `--contract function` first evaluates each original
case as a Script, then its native function wrapper. Only cases passing both reach protected execution.
`wrapper_inapplicable` means the original case passed but wrapping changed its
semantics; `native_failure` includes an `original_script` stage and captured
stdout/stderr. This keeps wrapper limitations separate from VM failures and
native host-policy differences. Module, raw, parse-negative, and `$262` host
cases remain explicitly inapplicable to this adapter.
Its script realms expose Node's `atob`, `btoa`, `TextEncoder`, and `TextDecoder`
for the embedded runtime compiler, consistently with the differential matrix.
It queues at most twice the worker count instead of retaining every test's
harness and source in memory. When `--output` is supplied, an adjacent
`.jsonl` journal records the binary and suite revision and flushes each completed
result immediately; the final JSON report remains the authoritative completed
run. Partial journals are useful failure evidence, not completed suite results.
Test262 `_FIXTURE.js` dependency files are counted separately and are not run as
standalone tests.
The adjacent `.runner.py` snapshot preserves the exact adapter source; its hash,
Node version, and execution contract are recorded in the report and journal.

Use `--contract script` to protect the original Script with
`--virtualize-program`, while installing the official assertion harness in a
separate setup script in the same realm. This preserves the original global
scope and eliminates the function-wrapper compatibility step. Whole-program
selection fails on unsupported source bodies and top-level partitions, including
scripts with no named functions. A wildcard function-name requirement is not
applied to such scripts because it would reject the absence of functions.
Runtime-negative cases compare exception constructors, including the harness's
Test262Error, which does not supply an Error-style name property. Module, raw,
parse-negative, and host-agent cases still need their own execution contracts.

`scripts/test262-vm-subset.txt` selects a bounded, reproducible sample across
optional chaining, mapped/unmapped arguments, iterator closing, try/finally,
generators, and async generators. Run it with:

```sh
python3 scripts/check-test262-vm.py --test262 /path/to/test262 \
  --binary /path/to/frozen-mangler --node /absolute/path/to/node \
  --manifest scripts/test262-vm-subset.txt --output /tmp/test262-vm.json
```

For browser and second-engine validation, run the repository test suites with
explicit `MANGLER_TESTKIT_NODE`, `MANGLER_TESTKIT_CHROME`, and
`MANGLER_TESTKIT_REQUIRE_ENGINES=1`. Node-only results do not establish browser
compatibility. File and network module loading and host APIs also need
host-specific integration tests beyond this matrix.

`node scripts/check-vm-browser.cjs --binary /path/to/frozen-mangler` executes
representative matrix fixtures in actual headless Chrome, with fresh iframe
realms for native and protected programs. Its defaults cover async execution,
class/private behavior, arguments aliasing and shape, and intrinsic mutations
under both `minify` and `high`. `--chrome` selects the browser executable;
`--fixtures`, `--presets`, and `--output` narrow and preserve a reproduction.
The report records the browser user agent and binary hash. This is a bounded
browser execution check, not whole-browser or module-loader conformance.
It observes uncaught errors and unhandled rejections through the next event-loop
turn before removing each iframe, including startup errors unrelated to `__out`.
Browser-only cases also exercise `document.all`, whose unusual nullish behavior
cannot be reproduced with an ordinary JavaScript object in Node.

Measure cold startup, warm calls, and delivered bytes with:

```sh
/absolute/path/to/node scripts/measure-vm.cjs \
  --binary target/release/mangler --baseline /path/to/baseline-mangler \
  --output docs/vm-benchmark.json --check
```

The benchmark requires current payment functions to virtualize. It checks raw,
gzip, and Brotli budgets and reports timing medians rather than comparing noisy
wall-clock samples against a fixed threshold.

For compilation and project growth, run:

```sh
/absolute/path/to/node scripts/measure-vm-scale.cjs \
  --binary target/release/mangler --output docs/vm-scale.json
```

This audit requires every source function to virtualize in increasingly large
straight-line functions, bundles of independent functions, wide local frames,
deep expressions, and linked ES-module projects. It checks actual Node results,
reports compiler duration, runtime startup, generated size, compressed size, and
macOS peak resident memory. The 70,000-local case crosses the old 16-bit frame
boundary. Project fixtures link and execute the transformed files together.
Every compiler process has a deadline (60 seconds by default), and each runtime
process has a 20-second deadline. Completed cases are flushed to the report as
they finish. Binary SHA-256 provenance and a final unchanged-binary check prevent
an audit from silently combining results across concurrent rebuilds. Timings
describe the measured machine and build profile; they are not portable SLAs.

## Protection boundary

The emitted interpreter executes the supplied bytecode and must recover its
constants. Opcode permutations, handler variants, packed bytecode, and masked
constants increase the work of inspecting a program. They do not prevent a host
that controls execution from observing decoded instructions, values, or calls.
The bytecode decoder and its masking material ship with the program; native
closure factory constants contain executable JavaScript source.

Payment amount calculations and authorization rules therefore need correct
server-side enforcement. Virtualization can protect implementation details from
casual inspection, but the transformed browser program cannot serve as an
authorization boundary or a place to keep payment credentials secret from its
execution host.
