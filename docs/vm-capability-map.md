# JavaScript virtualization capability map

Coverage has three separate questions: whether source operations become
bytecode, whether their JavaScript behavior is preserved, and which operations
continue to use the host engine. The executable audits answer specific examples;
none of their finite case counts establishes complete ECMAScript conformance.

| Area | Mechanism and verification |
| --- | --- |
| Expressions, bindings, calls, control flow | Source operations compile into VM instructions. The required-coverage matrix checks evaluation order, coercion, lexical initialization, closures, arguments, loops, and abrupt completion. |
| Classes | User method, constructor, field, and static-block computations become VM callbacks. Native class structures retain private brands, inheritance, and home-object semantics. The source-retention audit checks that representative original arithmetic leaves these structures. |
| Async functions and generators | Suspension machinery bridges VM execution with host iteration and promises. Tests check delegation, throw/return completion, queued async-generator requests, default-parameter rejection, constructor identity, and primordial Promise behavior separately. A successful basic `await` example does not establish those other contracts. |
| Modules | Native module syntax performs linking. Selected function bodies and protected initializer expressions execute through the VM. Linked-project tests execute hundreds of transformed files together. Data-URL tests cover imports, live bindings, top-level await, dynamic import, and `import.meta`. |
| Standard-library objects | Maps, sets, weak maps, proxies, typed arrays, DataView, Atomics, dates, Intl, and RegExp remain host-engine objects. The VM evaluates source operations and calls their methods. Passing these tests demonstrates host interoperability, not a second implementation of those libraries. |
| Runtime-generated code | Identified eval and Function-family constructor calls use the embedded Wasm frontend and shared VM. Native shells preserve activation and reflection requirements. Runtime-generated code has a separate coverage boundary from the original source-function inventory: unregistered bound constructors produced outside selected code still need producer mediation. Required-source coverage alone does not prove those routes are protected. |
| Direct eval | Compilation carries the caller's lexical and variable environments, strictness, and opaque class/private/super capabilities. Matching runtime tests exercise live bindings, declarations, completion values, nested classes, and parameter contexts. Broad Test262 testing continues to find additional grammar and binding cases; a rejection remains a coverage failure. |
| Exclusions and native factories | An explicitly excluded function may remain native inside a protected parent. The source-retention positive control requires its representative native arithmetic to remain visible, while required wildcard coverage must reject the exclusion. Native factory bridges need their own binding, name, receiver, and reflection checks. |
| External programs | Code fetched by imports, workers, script URLs, or other host loaders is a separate input. Transforming the referring function alone does not transform that external program. |

Use `scripts/check-vm-coverage.cjs` for current per-feature results,
`scripts/check-vm-source-coverage.cjs` for source-accounting probes, and
`scripts/measure-vm-scale.cjs` for size, memory, and execution growth.
The reports identify the executable used; retain an immutable binary when other
work is rebuilding the repository.

The Test262 adapter distinguishes original native-script failures from protected
failures. Its function contract additionally checks native wrapper compatibility:
moving a top-level variable into a wrapper can break an indirect eval that found
it in the original global environment. Its Script contract preserves the global
scope and selects whole-program protection. Module, raw, parse-negative and
host-agent cases need separate execution contracts and are not counted as
protected passes by either adapter mode.
