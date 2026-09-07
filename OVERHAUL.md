# Mangler overhaul

Goal: substantially improve JavaScript semantic correctness, reliable protection
coverage, generated size, bounded batch processing, and executable verification.
Baseline: `0d4928c`; clean checkout; prior workspace tests passed despite confirmed
semantic regressions. Main thread orchestrates ten agents; no nested delegation.

## Ownership

| Agent | Scope |
| --- | --- |
| vm_correctness | VM compiler and interpreter semantics |
| vm_bindings | Virtualization wrappers, captures, required coverage and config |
| js_correctness | Global indirection, flattening, dead code, string collection |
| js_pipeline | Emission, runner, guards, names, directives, expr VM subtree guards |
| class_lowering | Class desugaring semantics |
| cli_config | Discovery, destinations, bounded processing, CLI configuration |
| size_performance | VM tables, serialization and size reduction |
| test_quality | Differential harness, runtime checks, CI, benchmarks |
| reviewer | Independent audit and regression probes |
| integration | Shared API coordination, docs, final checks, commit |

Agents edit owned files; cross-owner changes require coordination. Integration
runs final workspace gates after all concurrent edits settle. Correctness fixes
must include focused regressions or equivalent executable evidence.

## Work ledger

- [x] Fix duplicated parameter initialization and eager/snapshot captures.
- [x] Fix VM increment, object key/prototype behavior, and class lowering.
- [x] Preserve directives, shebangs, import attributes, names and global reads.
- [x] Make verification inspect the same protected artifact; clarify guarantees.
- [x] Report virtualization coverage and fail required targets that remain native.
- [x] Prevent output collisions and recursive reprocessing; bound batch memory.
- [x] Reduce generated VM overhead with measured size evidence.
- [x] Repair typed/async differential outcomes and add production runtime gates.
- [x] Resolve independent review findings and update documentation.
- [x] Complete final verification and prepare the integrated commit.

## Verification ledger

Pending: formatting; full locked workspace tests; clippy; actual Node execution
across presets, VM and protection options; CLI IO regressions; representative
size/startup/runtime measurements; browser execution when a browser is available.
Record exact commands and outcomes below as completed.

- Baseline `cargo fmt --all -- --check` fails across untouched legacy code
  (~7,400 diff lines); format changed files without a blanket rewrite.
- Baseline strict clippy reports existing VM and virtualization style warnings;
  owners are cleaning their files. Removed unused workspace `base64` and config
  `mangler-core`; moved config's test-only `toml` to dev-dependencies.
- Node and macOS Google Chrome are available. Baseline executable retained at
  `/tmp/mangler-baseline-0d4928c` for emitted-size comparisons.
- Independent review found expression rewriting of VM table initializers can
  call the string decoder before initialization. Added generated-subtree
  protection to js_pipeline ownership and full-preset runtime acceptance.
- Shared API checkpoint: VM library builds with live-capture descriptors, new
  lexical instructions, packed serialization and instruction-usage metadata.
  All-target check identified constructor/test fixture updates, assigned to VM
  and binding owners before the integrated gate.
- Class refactor retains native class skeletons and protects eligible method
  bodies; unsafe class/prototype and regex-constructor emulation is removed.
  Required coverage must report native constructors and unsupported methods.
- Review also added case-insensitive output collisions and protected-parent /
  native-child coverage as acceptance cases.
- `cargo build -p mangler-cli --locked` passed at the first merged checkpoint.
- CLI owner: 48 focused tests passed (16 unit, 19 real CLI, 13 Engine), including
  bounded batches, atomic writes, destination aliases and input failure handling.
- Class owner: three semantic tests passed across 17 class cases, three regex
  cases and required method coverage; migrating subprocesses to bounded helper.
- Independent Node review confirms defaults, live/lazy captures, lexical errors,
  loop captures, computed prototype properties, multi-preset VM initialization,
  CommonJS globals and shebang behavior. Additional optimizer side-effect and
  strictness cases remain assigned to js_pipeline before final acceptance.
- Virtualization owner: 76 focused tests passed. Independent required-coverage
  matrix: nine cases across three presets passed, including native descendants,
  missing/async/excluded targets, nested functions and class methods.
- CLI independent probes passed case-insensitive collisions, literal bracket
  paths, recursive-output exclusion, read errors with keep-going, symlink mode
  preservation and scratch-file cleanup. CLI touched-file formatting is clean.
- Independent extended VM matrix passed 17 of 18 cases across three seeds;
  catch-binding reentry capture lifetime is assigned to vm_correctness.
- Bounded subprocess audit found inherited pipes can outlive a child process;
  test_quality owns applying the deadline to final stream draining as well.
- Node focused correctness script passed all 19 cases. Independent checkpoint
  passed 25 VM cases, 40 VM/preset combinations, 55 plain pipeline/preset cases,
  54 lexical/iterator/coercion cases and six exact protection/verify configurations.
- Pipeline: 41 jsast and 10 runner tests passed. Selective SWC compression guards
  preserve strictness and observable initializers; post-resolver repair preserves
  the named class binding inside heritage expressions.
- Class: six bounded Node tests passed 22 class cases, six heritage/TDZ cases,
  three regex cases and real required-method protection. Retained differential
  class/regex parity and determinism tests passed 13 tests.
- First full gate completed and identified stale-snapshot class/callee fixes,
  missing serializer opcode samples, QuickJS's incorrect destructuring elision
  oracle, and Chrome's failure to exit after producing headless output. Owners
  are closing each before a fresh final gate. Node full-corpus matrix passed.
- Final gate will use bounded test concurrency without parallel LTO or benchmark
  work; the exec-trace decoder passed its focused check in 0.49s after a contended
  parallel run exceeded the explicit two-second evaluation deadline.
- Release dependency build completed in 3m08s. Final release source rebuild and
  benchmark refresh remain after the production freeze.
- Actual-engine checkpoint: all four suites passed in 18s, with Node and Chrome
  explicitly required. Covers 32 corpus files across five Node presets, browser
  presets, named/whole-program VM under all presets with required coverage, and
  seven exact protected artifacts with verify/nonverify byte equality.
- Testkit typed/async/limit regressions passed 42 tests; two final resource tests
  are finishing. Default evaluation deadline is five seconds, with hard timeout
  negative cases retaining their explicitly short limits. CI runs two test threads.
- Production minification fixture shrank 60,943 to 3,932 bytes and transformed in
  189ms in debug; release throughput verification remains in the final gate.

## Final acceptance

- Final production snapshot: `cargo test --workspace --locked -- --test-threads=2`
  with `MANGLER_TESTKIT_NODE=/opt/homebrew/bin/node`,
  `MANGLER_TESTKIT_CHROME=/Applications/Google Chrome.app/Contents/MacOS/Google Chrome`,
  and `MANGLER_TESTKIT_REQUIRE_ENGINES=1`: **749 passed, 0 failed, four existing
  ignored doctests, 28 test/doc targets**. Both real engines executed.
- `cargo clippy --workspace --all-targets --locked -- -D warnings`: **passed**.
- Changed-file rustfmt: **56 tracked and seven new Rust files passed**;
  `git diff --check`: **passed**. Legacy untouched files were not reformatted.
- Independent reviewer explicitly approved the frozen implementation and binary.
  Final matrices passed 25 base VM, 54 extended edge, 12 class, 30 heritage,
  33 parameter, 42 seeded arithmetic, 27 coverage and seven protection cases;
  additional intrinsic/dynamic-scope, import-attribute, keep-name, wildcard source
  catalog and integrity-mutation probes passed. Earlier full-preset and actual
  filesystem matrices also passed. No scoped blocker remains.
- Exact release CLI rebuild passed in 1m05s. Final baseline comparison and
  `scripts/measure-vm.cjs --check` passed required coverage, runtime equivalence
  and raw/gzip/Brotli budgets. Final raw bytes: payment 4,891 → 2,684; closure/EH
  8,306 → 5,033; generated arithmetic 104,072 → 21,592. All compressed fixtures
  also shrank. Warm closure calls cost 1.560 → 4.430µs (2.8x slower); arithmetic
  calls improved 57.967 → 11.414µs. Full methodology and tradeoffs are recorded in
  `docs/VM_BENCHMARK.md` and `docs/vm-benchmark.json`.
- `cargo test --locked --release -p mangler-js --test performance -- --nocapture`:
  **passed**. Representative source shrank 60,943 → 3,932 bytes (6.5% retained);
  the release test finished in 0.02s and enforced its three-second transform limit.
- All scoped review findings are closed; implementation and review owners are
  quiescent. This ledger, documentation and benchmark snapshot accompany the
  integrated commit. No runtime speedup is claimed beyond measured fixtures.
