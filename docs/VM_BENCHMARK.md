# VM size and execution measurements

The VM now packs bytecode into a printable variable-length string and emits only
the instructions and operators used by each strictness/EH interpreter variant.
Nested bytecode closures contribute to their interpreter's usage union. Configured
decoy handlers remain present. An EH-only program no longer includes an unused
lean interpreter.

The packed representation uses five payload bits and one continuation bit per
character, with a seed-derived mask. It expands once, in place, into the shared
word array on first execution. Later calls reuse that array. This replaces the
previous decimal arrays of XOR-masked 32-bit words; there is one production
representation. The mask is obfuscation, not cryptographic encryption.

## Recorded result

The [machine-readable snapshot](vm-benchmark.json) compares baseline `0d4928c`
with the final release build of this overhaul on an Apple M4, macOS arm64,
Node 26.0.0. Both binaries use
`--preset minify --virtualize '*' --seed 42`, with public benchmark function names
preserved. The current binary also requires the named benchmark routine to be
virtualized. These figures include the overhaul's semantic fixes and live capture
handling, rather than isolating the serializer alone.

| Workload | Raw bytes, before → after | gzip bytes | Brotli bytes |
| --- | ---: | ---: | ---: |
| Payment line items, discount, tax | 4,891 → 2,684 | 1,452 → 1,209 | 1,311 → 1,115 |
| Nested fee closure and exception handling | 8,306 → 5,033 | 2,203 → 1,780 | 1,895 → 1,609 |
| Generated 500-operation arithmetic function | 104,072 → 21,592 | 7,217 → 5,381 | 3,873 → 2,404 |

| Workload | Cold evaluation, ms, before → after | Warm call, µs, before → after |
| --- | ---: | ---: |
| Payment | 0.197 → 0.201 | 3.287 → 4.983 |
| Closure/EH | 0.222 → 0.226 | 1.560 → 4.430 |
| Generated arithmetic | 0.630 → 0.877 | 57.967 → 11.414 |

Shipping size improves on all three fixtures. Runtime has a tradeoff: the
closure/EH workload is approximately 2.8 times slower per warm call in this
snapshot. Correct live capture descriptors and lexical initialization checks add
work that the baseline omitted. The arithmetic workload's first evaluation also
slows despite its smaller payload. Further runtime work should preserve those
semantics and measure allocation and property-access costs explicitly.

A focused descriptor count on the final release build explains part of the closure
cost: a successful warm checkout defines five accessors and reads two property
descriptors; its failure path defines three accessors, including a fresh catch
binding. The baseline did none of that work and did not preserve the corresponding
binding semantics.

These are three focused fixtures, not an application-scale corpus or a universal
performance claim. The third is deliberately generated to expose payload growth.
Cold measurements include fresh context creation, parsing and initial execution,
including bytecode decoding; they are medians of 50 evaluations in one Node
process. Warm measurements are medians of 15 batches of 100 calls after one
warm-up batch. Machine load and engine optimization affect timings. gzip uses
level 9; Brotli uses Node's default settings. Input and transformed results must
match before a measurement is accepted.

## Reproduce and enforce budgets

Build a baseline in a separate checkout, then run the checked-in harness with
an explicitly chosen Node executable:

```sh
git worktree add --detach /tmp/mangler-before 0d4928c
cargo build --release --manifest-path /tmp/mangler-before/Cargo.toml -p mangler-cli
cargo build --release -p mangler-cli
node scripts/measure-vm.cjs --binary target/release/mangler \
  --baseline /tmp/mangler-before/target/release/mangler \
  --output docs/vm-benchmark.json --check
```

Omit `--baseline` to check the current build alone. `--check` enforces raw,
gzip and Brotli byte budgets with room for correctness changes. Timings are
reported without machine-dependent pass/fail thresholds. Transform and runtime
worker subprocesses each have a 60-second deadline; timed execution uses an
outer worker deadline to avoid adding watchdog overhead to every call.

The VM unit tests additionally execute every packed digit mask, unsigned word
boundaries through `u32::MAX`, repeated decoding, an empty payload and 100,000
words. Interpreter tests verify omitted handlers, retained decoys and required
instructions inside nested closures across multiple seeds. The full VM library
suite passed 96 tests at the measurement checkpoint.
