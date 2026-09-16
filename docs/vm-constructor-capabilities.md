# Constructor capability registration

Status: **partial producer integration; complete runtime-source coverage remains unproven**. Source-known bind producers, optional chains, and invocation adapters now share the consumer provenance analysis and host registry. The broader registration boundaries below remain open.

## Source-known bind producers and adapters

The original source analysis now returns one `SourceDependencies` value: protected consumer positions and bind-producing call positions. Both come from the same resolved monotone graph. Native call sites are rewritten only when the resulting VM table already requires the runtime compiler. An unrelated source bind operation alone does not attach the compiler.

After selected bodies become thunks, `source_producers.rs` mediates remaining known bind calls through the table's `captureBind` capability. The capture evaluates the actual receiver and computed key, then reads the property before the original argument expressions run. Those arguments remain in their original lexical execution scope; direct eval, await, yield, and parameter defaults are not moved into a callback. The host registers only the exact intrinsic bind identity via its existing `invoke` branch and existing `boundFunctions` WeakMap. Producer bridges use the same recursive call/apply/Reflect/bound dispatcher with native constructor behavior, preserving source overrides that resolve to an ordinary eval or constructor call. Protected consumers retain normal bytecode compiler dispatch.

This covers available-source `Function.bind`, `call.bind(Function)`, `apply.bind(Function)`, `bind.call`, `bind.apply`, `Reflect.apply` binding, prebound call/apply/Reflect-to-bind adapters, repeated binding, known object/array aliases, and bind producers in unselected native functions. Adapter capabilities propagate through the same source graph. Call argument provenance is separately aggregated by the worklist, so wide argument lists are not rescanned after individual input changes. The same mechanism recognizes all four constructor kinds. It preserves the actual bound exotic object's identity, name/length descriptors, and construction behavior.

Optional producer chains use lazy native reference records through `chainReference`, retaining the native optional call that guards each original computed key and argument list. Parenthesized chain boundaries preserve their receiver while ending an earlier short circuit. Final property reads stay lazy so deletion does not invoke a getter; tagged calls preserve their native template site and receiver. Strict nullish checks preserve HTMLDDA behavior. The private reference capability still delegates calls to the same host dispatcher and bound registry. Anonymous roots use the existing assignment-target naming guard so compression cannot give them a synthetic record-property name.

Offline validation against the immutable checkpoint 35 Wasm recorded exactly one constructor request and an original native bound record for each of 40 producer/consumer shapes, including all four kinds, optional receiver/call combinations, native adapter chains, and explicit new targets. All 69 dynamic-host smoke assertions pass. Thirty-nine native/rewritten behavior comparisons pass after production compression, covering short-circuit regions, parentheses, getter/argument order, native overrides, suspension, deletion, templates, and anonymous names. Nine source-provenance unit groups pass, including the existing large-input checks. Permanent selected/whole-program integration tests additionally cover optional chains in Node and Chrome; those new integration tests still require the coordinator's matching-package build.

The bounded integration does not identify arbitrary computed property names, private/super call producers, or bare identifier producer references whose receiver comes from `with`, and bare direct `eval` references that need the native lexical-eval capability. Optional chains containing these reference boundaries retain the same limitations. A statically known final sequence key is supported, with preceding source side effects preserved. Those must be represented by further provenance/capability support; they are not solved by this patch. Preexisting opaque bound values and independently attached compiler registries also retain the boundaries described below.

## Original confirmed gap

A bound Function constructor created outside the selected VM can reach a protected call without a bound-function record. The existing identity dispatcher then invokes the native bound function. Simple programs still return their expected values, while their dynamically supplied body never passes through the Wasm compiler.

The static dependency analysis already recognizes the first example as requiring runtime compilation; packaging the compiler is insufficient to register the native producer:

```js
const make = Function.bind(null);
function pay() {
  return make('return 7')();
}
```

Equivalent producer shapes include:

```js
const call = Function.prototype.call.bind(Function);
const apply = Function.prototype.apply.bind(Function);
const first = Function.bind(null, 'a');
const second = first.bind(null);

function pay() {
  return [call(null, 'return 7')(), apply(null, ['return 7'])(),
          second('return a')(7), new first('return a')(7)];
}
```

The problem also applies to the async, generator, and async-generator constructor identities when their bind operation happens beyond the registered execution boundary. Those three kinds were identified by code inspection, not exercised by the bounded reproduction below.

## Evidence

The reproduction uses immutable `/tmp/mangler-audit-bins34`, Node v26.0.0, and the package's actual `ManglerEvalCompiler`. Script and results are retained at `/tmp/mangler-constructor-capabilities/probe.cjs` and `probe.json`. It wraps the public compiler instance's request method with a delegating counter; it neither changes source built-ins nor substitutes the source callable. Conditional direct Function use ensures the protected body enables the mediation path, without executing that conditional constructor.

| Producer / invocation | Returned value | Constructor requests before registration | Requests after diagnostic registration |
|---|---:|---:|---:|
| Direct Function | 7 | 1 | 1 |
| Function.bind inside protected body | 7 | 1 | 1 |
| Function.bind outside protected body | 7 | 0 | 1 |
| call.bind(Function) outside | 7 | 0 | 1 |
| apply.bind(Function) outside | 7 | 0 | 1 |
| Rebinding an external bound Function | 7 | 0 | 1 |
| Constructing external bound Function with new | 7 | 0 | 1 |

Diagnostic registration inserts the exact known producer inputs into the existing `boundFunctions` WeakMap, keyed by the original native bound-function object. This is a feasibility check of the current dispatcher, not a production integration. Every observed result remains 7, and the five previously omitted constructor requests then occur.

## Ownership and call paths

`crates/mangler-js/src/passes/virtualize/source_compiler.rs` owns source dependency provenance. Its resolver copy distinguishes lexical bindings; its worklist propagates dynamic-constructor, call, apply, bind, and container information. `dependencies()` returns both consumer and producer expression positions. The separate `source_producers` emitter consumes that analysis after selected-body virtualization and emits native capture capabilities. Its test `known_dynamic_values_flow_into_selected_source` explicitly includes an external `Function.bind` producer.

`Compiled.requires_source_compiler`, instruction-usage aggregation, and table metadata cause protected invocation primitives to use `SourceInvoke` / `SourceConstruct`. The emitted interpreter delegates these to the table's attached host dispatcher. This selects a call route; it cannot manufacture unavailable bound-function metadata.

`crates/mangler-eval/host/compiler.js` owns runtime constructor identity and bound invocation:

- `constructorKind` compares exact identities of Function and the three suspension constructors.
- `mediated` recognizes those constructors, intrinsic eval, native call/apply/bind, Reflect.apply/construct, and registered bound objects.
- `invoke` unwraps registered bound arguments/receiver, then recursively handles call/apply aliases and constructor invocation. The native bind branch first executes the actual intrinsic bind, then records `{fn, receiver, args}` against its returned native object.
- `construct` unwraps recorded bound arguments and preserves the native bound-function new-target substitution.
- `fork` shares `boundFunctions` with descendant runtime-compiled tables. Separate compiler instances allocate separate registries.

Ordinary VM closures are created by `Mk`; suspension and method envelopes retain native callable shape around their bytecode entry. The interpreter's separate `Functions` WeakMap associates method wrappers with invocation metadata (`strict`, `kind`, `factory`). It is not a constructor-capability registry and should not become a second bound-function registry. Source-facing native function declarations, expressions, arrows, methods, class envelopes, and excluded function bodies can also produce a bound constructor through a call site; their mere creation provides no bound target/argument data.

Native `bind` produces an exotic object with internal target, receiver, and argument slots. Later construction also substitutes the underlying target when the supplied new target is the bound object. These are the semantics the existing host records reproduce. The language does not provide a general reflective operation to recover those slots from an already-created opaque bound value. See [bound function exotic objects](https://tc39.es/ecma262/multipage/ordinary-and-exotic-objects-behaviours.html#sec-bound-function-exotic-objects) and [Function.prototype.bind](https://tc39.es/ecma262/multipage/fundamental-objects.html#sec-function.prototype.bind).

## Broader authoritative integration contract

The source-known producer path implements the graph, bridge, registration, and adapter portions below. Explicit external registry integration and the remaining native lexical reference boundaries are still open. Retain the existing host dispatcher and WeakMap as the authority, with explicit producer records rather than a second spelling-based scan.

1. Extend provenance output from a set of consumer positions to include the relevant call-producer identities and their dependency relation to protected consumers. Keep ordinary unrelated calls lean. Conservative extra producer sites are acceptable only when the runtime identity check preserves their native behavior.
2. Route those producer invocations, including native top-level initializer expressions and producers inside unselected source functions, through a structural call bridge to the same table dispatcher. Do not replace Function, Function.prototype.bind/call/apply, or any source-visible property. Evaluate the actual callee, receiver, computed key, and arguments once, in their original order.
3. Factor the existing native-bind branch into the registry's single bind-and-register operation. It executes the actual native bind exactly once, returns that identical native object, and stores the actual receiver/arguments only after successful creation. The same operation serves protected bytecode, producer bridges, and runtime-compiled support. Source overrides of `.bind` must remain ordinary source calls; exact intrinsic identity decides registration.
4. Preserve call/apply/Reflect alias chains through the current recursive dispatcher, including bound call/apply functions and repeated binding. Avoid special handling based solely on the property spelling `bind` or the returned function's display name.
5. Ensure the producer bridge and selected consumer share the registry before the producer executes. Existing `fork` sharing handles descendant tables; source-aware bundles or multiple independently attached compilers need an explicit shared registry parameter. The registry must remain private and weakly keyed.
6. Preserve native lexical execution boundaries when introducing the bridge. Source argument expressions must not move into an arbitrary new callback scope, especially in parameter defaults, class initializers, computed keys, or around await/yield. Bare direct eval needs its existing lexical-eval path, not a generic native producer wrapper. Use certified internal temporary bindings and existing source-context metadata.

All supplied business body strings still use the existing constructor frontend and bytecode installer. The proposed producer bridge records invocation information; it does not introduce an alternative body evaluator or a replacement interpreter.

### Boundary requiring explicit provenance

A preexisting bound value imported from an uninstrumented library, another compiler instance, or an embedding host has no usable record merely because it enters a protected function. Its native target and bound arguments cannot be inferred authoritatively from its visible name, length, or prototype.

For that boundary, provide an explicit producer/registration integration whose owner supplies the original bind inputs when creating the callable, and share the registry with its consumers. Instrumenting a known producer in available source is sufficient; classifying an opaque existing object by appearance is not. Arbitrary callback parameters intentionally remain ordinary calls in today's source analysis, so this is a real coverage boundary rather than a solved consequence of enabling the compiler everywhere.

A registered native bound object subsequently called entirely from uninstrumented native code also bypasses the table dispatcher. Registration alone does not redirect its native internal call. Coverage therefore requires mediation of the relevant consumer call sites too; preserving native identity rules out silently replacing the returned bound object with a different trampoline.

## Required validation before implementation can be called complete

- Count constructor compilation requests as well as returned values for the seven reproduced shapes and all four constructor kinds; simple value equality alone misses the defect.
- Exercise top-level initializers, nested/excluded producers, getter-produced constructors, object/array aliases, call/apply/Reflect chains, repeated bind, construction, and explicit new targets.
- Preserve native bound identity, name/length descriptors, constructibility, argument/receiver binding, and the observable order of target prototype/name/length accesses during bind.
- Verify source `.bind` overrides, getters, abrupt arguments, revoked proxies, and native errors are unchanged; no global source API mutation is permitted.
- Test cross-table registry sharing, separately attached instances with explicit shared provenance, and documented opaque external values.
- Test whole-program and selected-function modes separately. Whole-program mediation of initializers must not conceal the selected-mode producer gap.
- Re-run the existing compiler-size, warm-runtime, and cold-start budgets; a selective producer bridge must not turn ordinary payment callbacks into an unconditional compiler dependency.
