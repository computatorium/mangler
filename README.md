# mangler

A JavaScript / CSS / HTML **obfuscator, minifier, and bytecode virtualizer**, written in Rust.

`mangler` rewrites source into functionally-identical but deliberately unreadable
output. It minifies, renames, encodes strings, flattens control flow, injects
decoys, and — for JavaScript — can lift functions or an *entire program* into a
custom register-based **bytecode VM** so the logic survives as interpreted bytes
rather than readable source.

Every transform is **bail-to-safe**: any construct it cannot lower soundly is left
native, never miscompiled. Output is **deterministic** under a fixed `--seed`
(same input + seed ⇒ byte-identical result).

---

## Build

```sh
cargo build --release
# binary at target/release/mangler
```

Requires a recent stable Rust toolchain. The workspace is pure Rust; the test
suite executes generated JS through an in-process engine (no Node required).

## Quick start

```sh
# JS from stdin → stdout
cat app.js | mangler - --lang js --preset high

# Mangle every JS/CSS file under src/ into dist/ with a fixed seed
mangler src/ -o dist/ --preset max --seed 42

# Mangle all CSS in place (overwrite sources)
mangler "assets/**/*.css" --in-place
```

Inputs are files, directories (recursed), quoted glob patterns, or `-` for stdin.
For multiple inputs, `-o` must be an existing directory.

## Presets

`--preset` (default `high`) sets a baseline every individual flag can override:

| Preset    | What you get |
|-----------|--------------|
| `minify`  | Whitespace/identifier minification only — no obfuscation. |
| `low`     | Minify + local mangling + `encode` strings. |
| `medium`  | Adds safe global indirection. |
| `high`    | Adds control-flow flattening, `encrypt` strings, hex identifier names, self-defending + debug-protection guards. |
| `max`     | Every pass at its strongest setting. |

## Driver flags

| Flag | Purpose |
|------|---------|
| `-o, --output <PATH>` | Output file, or directory for multiple inputs. |
| `--in-place` | Overwrite each input with its output (conflicts with `-o`). |
| `--config <TOML>` | Load defaults from a TOML file (explicit flags win). |
| `-j, --jobs <N>` | Worker threads (default: CPU count; `1` = serial). |
| `-v, --verbose` | Print notes + a per-file size report to stderr. |
| `--keep-going` | Skip failed inputs instead of aborting (exit code still reflects failures). |
| `-l, --lang <js\|css\|html>` | Force input language (required for stdin). |
| `--seed <U64>` | Fixed RNG seed for reproducible output. |
| `--verify` | Re-parse the output and assert it is still valid. |

Exit codes: `0` all inputs ok; `1` one or more failed (parse, I/O, or `--verify`).

## Obfuscation passes

Each is a per-pass override of the preset default; absence = preset default.

| Flag | Values | Notes |
|------|--------|-------|
| `--mangle` | `true\|false` | Local-identifier mangling. |
| `--identifier-naming` | `short\|hex\|soup` | `short` = `a,b,…`; `hex` = `_0x4e2a`; `soup` = homoglyph names. |
| `--strings` | `none\|encode\|encrypt` | String literal obfuscation. |
| `--control-flow` | `true\|false` | Control-flow flattening. |
| `--dead-code` | `0.0`–`1.0` | Decoy-code injection ratio (`0.0` disables). |
| `--global-indirect` | `off\|safe\|aggressive` | Indirect free-global references. |
| `--self-defending` | `true\|false` | Anti-beautification guard. |
| `--debug-protection` | `true\|false` | Debugger trap. |
| `--harden-global-anchor` | flag | Hide the literal `globalThis` behind a seeded derivation. |
| `--keep-names <GLOBS>` | csv globs | Identifier names to preserve from renaming. |

### String-decode key binding

Bind string decoding to a runtime value so the output only decodes in the
intended environment:

| Flag | Purpose |
|------|---------|
| `--domain-lock <HOST>` | Decode only on the given hostname (sugar for the two flags below). |
| `--key-source <EXPR>` / `--key-expected <VAL>` | Decode keyed off an arbitrary runtime JS expression. |
| `--remote-key [<EXPR>]` | Key off a session/remote slot (defaults to `globalThis.__MANGLER_SESSION_KEY`). |
| `--strings-in-vm <bool>` | Route the string decoder through the bytecode VM. |
| `--self-coupled-key <bool>` | Couple the key to the decoder bytes (fragile: re-minification breaks decoding). |
| `--exec-trace-key <bool>` | Oblivious execution-trace key (robust to re-minification). |

---

## Virtualization (JavaScript)

`mangler` can compile JS to a diversified, register-based bytecode VM that is
spliced back into the module: the original logic becomes a constant table plus a
generated interpreter, and call sites become thunks that re-enter the interpreter.
Each output file draws its own VM diversity from the seed.

### Named-function mode

```sh
mangler app.js --virtualize 'hot*'
```

Virtualizes every function whose (inferred) name matches the glob. Names are
inferred from the function's own identifier, its binding (`const render = …`,
`obj.render = …`), or its object/class property key. **Strict functions are fully
supported** — strict and sloppy bodies route to strictness-matched interpreter
variants so `this`, store-error semantics, and the rest stay correct.

### Whole-program mode

```sh
mangler app.js --virtualize-program
```

Wraps the entire top level and recurses into every reachable function. The
program is **partitioned** at `import`/`export`/top-level-`await` boundaries;
each maximal run of wrappable statements becomes its own chunk, and a run that
fails to compile is adaptively bisected so a single unsupported statement stays
native while its neighbors still virtualize. Cross-run `var`/`function` data flow
is preserved automatically.

### Keeping hot paths native

A bytecode VM is far slower than native JS, so a 60 fps render loop should stay
native. Exclude it by name:

```sh
mangler app.js --virtualize-program --virtualize-exclude 'render*'
```

Excluded (and structurally ineligible — `async`, generators, etc.) functions run
as **native JS at full speed inside the VM frame** via a native-closure escape
hatch, while still capturing the surrounding virtualized locals. With `-v`,
`mangler` prints a note listing which functions were kept native so you can
confirm your hot path. Note: exclusion is **name-based and manual** — a bare
anonymous callback (`requestAnimationFrame(() => …)`) has no name to match; give
it a name and exclude that.

### Opportunistic desugaring (opt-in, off by default)

These lower otherwise-native constructs into wrappable form to raise coverage.
Each is fuzz-verified and only fires for shapes proven sound:

| Flag | Effect |
|------|--------|
| `--virtualize-desugar-class` | Lower `class C extends B {…}` to function/prototype form (skips private fields, `static{}`, decorators, computed keys). |
| `--virtualize-desugar-regex` | Lower `/re/g` to `new RegExp("re","g")`. |

Both require `--virtualize-program`.

---

## Configuration file

Every flag is also settable from a `--config` TOML file (CLI flags override it);
unknown keys are a hard error.

```toml
preset = "max"
seed = 7
strings = "encrypt"
identifier_naming = "soup"
keep_names = ["myExport", "init*"]
harden_global_anchor = true
```

## Workspace layout

| Crate | Role |
|-------|------|
| `mangler-cli` | Binary driver (`mangler`): argv, I/O, parallelism. |
| `mangler-config` | The single flag declaration (clap + serde) + validation. |
| `mangler-core` | Shared types: errors, notes, RNG, pass traits. |
| `mangler-passgraph` | Dependency-ordered pass scheduler. |
| `mangler-jsast` | JS AST analysis helpers (eligibility, binding names). |
| `mangler-js` | JS passes: mangle, strings, control-flow, virtualize, … |
| `mangler-vm` | The bytecode VM: compiler, ISA, interpreter codegen, serializer. |
| `mangler-css` / `mangler-html` | CSS and HTML minification/mangling. |
| `mangler-testkit` | Differential fuzzing + in-process eval harness. |

## Determinism & safety

- **Deterministic:** same input + `--seed` ⇒ byte-identical output, including VM
  diversity and partition boundaries.
- **Bail-to-safe:** any construct that cannot be lowered soundly is left native.
  Differential equivalence (original vs. output: identical stdout, thrown errors,
  and side-effect order) is the test gate for every pass.

## Status

`0.2.0` — adds whole-program virtualization (top-level wrapper, partitioning +
bisection, native-closure escape hatch, full strict-mode coverage, name-glob
exclude, and opt-in class/regex desugaring).
