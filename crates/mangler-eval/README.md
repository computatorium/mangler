# Optional runtime compiler

This crate builds the existing Rust parser and VM compiler as a synchronous,
import-free WebAssembly module. Ordinary Mangler output does not include it.
Its compiler ABI feeds the same interpreter table used by protected applications;
ordinary scopes use live activation records for direct eval. The standalone host
and browser smoke exercise that path independently of CLI packaging.

`host/compiler.js` consumes versioned JSON bytecode and uses an interpreter
emitted ahead of time by `mangler-vm`. Protected input bodies always remain
bytecode. Functions requiring native mapped arguments use a generated parameter
shell containing only canonical parameter names and the VM invocation; this shell
uses the `Function` constructor and therefore requires JavaScript unsafe-eval.
Functions without mapped arguments retain the CSP-compatible path.
Constants retain UTF-16 code units, BigInts,
regular-expression metadata, and template-object identity. Dynamic class/private/suspension syntax still requires the shared frontend
lowering before runtime compilation can cover those constructs; native body
factory constants currently report a structured failure.

Build and verify:

```sh
rustup target add wasm32-unknown-unknown
scripts/build-eval-runtime.sh
MANGLER_TESTKIT_NODE=/path/to/node \
MANGLER_TESTKIT_CHROME=/path/to/chrome scripts/check-eval-runtime.py
```

The verification script runs identical Wasm compilation and bytecode execution
in Node and Chrome with native eval execution disabled, and serves the
browser under `script-src 'self' 'wasm-unsafe-eval'`. JavaScript unsafe-eval is not
permitted. Build artifacts, gzip size measurements, and engine verification
results are written under `target/eval-runtime`.

## ABI

A single instance serves synchronous requests; callers copy the response before
issuing another request. Instances do not share state or linear memory.

- `mangler_abi_version() -> 1`
- `mangler_compiler_fingerprint() -> u64` identifies the shared compiler sources.
  JavaScript receives the bits as a signed i64 BigInt. The host requires exact
  equality with the interpreter's build-derived fingerprint before issuing any
  compiler request. Missing or mismatched exports require rebuilding and
  installing matched assets; they never enable native-source fallback.
- `mangler_alloc(length) -> pointer`
- `mangler_compile(pointer, length) -> success`
- `mangler_result_ptr()` and `mangler_result_len()` expose the last response.
- `mangler_free(pointer, length)` releases the input allocation exactly once.

The fingerprint includes shared parser/frontend, preprocessing, compiler,
interpreter, serializer, runtime host, manifests, and dependency-lock inputs.
It excludes checkout paths and timestamps. It detects mixed builds; it is not a
signature or an authenticity guarantee. [Distribution builds](../../docs/distribution.md)
also record SHA-256 hashes for the exact source inputs and delivered assets.

The UTF-8 request is `{version:1, mode, source, strict?, allowNewTarget?, op?, bin?, un?}`.
Optional opcode/operator permutations use the destination interpreter encoding.
`function` mode accepts a function expression; `eval` mode parses a Script
StatementList. `allowNewTarget` enables that contextual grammar without allowing
an eval-level `return`. Eval compilation uses `mangler-vm::eval::compile_eval_body`
for completion values and declaration metadata.

Successful responses include `strict`, `declaredVars`, and a program containing
encoded code words, tagged constants, capture names, parameter metadata,
children, and child-index relocation offsets. This keeps the serializer and ISA
in Rust authoritative. Errors carry a kind and message and never activate a
native source fallback.

## Production environment integration

A direct-eval call must receive the actual caller lexical and variable
environments, strictness, receiver, `new.target`, and applicable home-object and
private-name metadata. The standalone host demonstrates persistent eval-created
var bindings, strict eval isolation, captured references, and completion values;
it does not substitute a global or helper-function eval for a source direct eval.

The runtime uses one variable-environment dictionary per function
activation, lexical descriptor records per scope, and object records for `with`.
Eval var declarations are activated when evaluation begins, after checking
lexical conflicts; they must not be hoisted into caller storage at compilation
time. Closures retain dictionary lookup references so they observe bindings
introduced by later eval calls. Non-string eval input is returned unchanged.

Production embedding must occur only for code requiring runtime compilation,
retain the selected opcode diversification, and reuse the existing interpreter
table. Dynamic class/private/super syntax and suspension lowering must feed the
same frontend before unsupported host-syntax constants can be removed.
