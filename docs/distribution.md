# Building a matched distribution

The CLI locates `mangler-eval.wasm` beside its executable when selected source
needs runtime compilation. The generated JavaScript embeds that compiler; end
users running the generated artifact do not fetch a separate sidecar. Ordinary
output that does not need runtime compilation does not embed it.

Build the native CLI, standalone interpreter, and Wasm compiler together:

```sh
rustup target add wasm32-unknown-unknown
scripts/build-eval-runtime.sh --node /absolute/path/to/node
```

This builds native workspace binaries with one Cargo feature graph, then builds
only the compiler library for `wasm32-unknown-unknown`. It does not invoke a
separate host `cargo run -p` that can rebuild shared dependencies with a different
feature set. Cargo's artifact messages provide executable paths, including when
`CARGO_TARGET_DIR` is customized. Builds use the lockfile and default to the
native release profile and size-optimized `wasm-release` compiler profile.
`--profile dev` selects a development native CLI; `--wasm-profile` separately
selects the compiler profile. Packaging enforces an 8 MiB Wasm budget because
the browser verification requires synchronous main-thread compilation.
These commands build for the current host and execute the resulting binaries.

The default output is `target/eval-runtime`:

- `mangler` and adjacent `mangler-eval.wasm` form the installed compiler.
- `interpreter.js`, `mangler-eval.js`, and `smoke.js` support standalone Node and
  browser verification.
- `manifest.json` records source-input and artifact hashes, the Rust compiler,
  build profile, and successful package checks. `sizes.json` and `.gz` files
  record delivered byte sizes.

Before publishing that directory, the script checks the Wasm ABI and absence of
host imports, runs the standalone runtime's smoke cases, compiles a
required-virtualized direct-eval function through the staged CLI, and executes
its output in Node. It removes `MANGLER_EVAL_WASM` from
that compiler process so the smoke exercises the adjacent-sidecar layout.
Compiler input changes during the build or verification reject the package.
A failed build leaves a previously published artifact directory intact.
The runtime host additionally compares a source-derived 64-bit compatibility
fingerprint exported by Wasm with the interpreter's expected fingerprint before
compiling dynamic source. Old or mismatched sidecars produce a clear rebuild
error. This detects mixed artifacts; it does not authenticate untrusted files.

Create a distributable host archive with:

```sh
scripts/package-distribution.sh --node /absolute/path/to/node
```

Its defaults are `target/distribution/mangler/` and
`target/distribution/mangler.tar.gz`. `--output` and `--archive` select other
destinations. The archive contains the matched assets and manifests in a
`mangler/` directory and retains the CLI executable bit. Archive metadata is
normalized; artifact hashes describe the exact build, including uncommitted
source changes. Keep `mangler` and `mangler-eval.wasm` together when installing or
moving the executable. A custom sidecar location can instead be selected with
`MANGLER_EVAL_WASM=/absolute/path/to/mangler-eval.wasm`; use the sidecar from the
same build.

The package smoke is a build-integrity gate, not a JavaScript conformance gate.
Run the broader [VM audit](vm-audit-method.md) and
`scripts/check-eval-runtime.py` with configured Node and Chrome before releasing.
