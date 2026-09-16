# Callable reflection proposal

Status: **proposed and unimplemented**, 2026-09-07. This document records a
bounded design review, not a completed conformance fix. Package 35 integration
takes precedence; no production implementation was added by this review.

## Evidence and intended contract

The package 32 partial Test262 report
`/tmp/mangler-test262-32-full-script.partial.json` contains 82 semantic failures
under `built-ins/Function/prototype/toString`: 41 files, each run in sloppy and
strict mode. It also records 72 passing variants and six native failures in that
directory. The failing cases expose rewritten JavaScript source: VM entry
functions, arrows, methods, accessors, class skeletons, or suspension envelopes.
The passing source-function cases include async callables already represented by
real proxies. These are historical results, not a measurement of the current
working tree.

The affected tests use `assertToStringOrNativeFunction`: they accept original
source text or a syntactically valid native-function representation. ECMAScript
requires available `[[SourceText]]` to be returned exactly, but permits host-native
syntax for callable objects without it. A callable Proxy follows that latter
branch under the unmodified intrinsic. Ordinary JavaScript cannot install an
original function's `[[SourceText]]` onto a different VM entry function, nor can
it implement the host's source-availability hook. See
[Function.prototype.toString and HostHasSourceTextAvailable](https://tc39.es/ecma262/multipage/fundamental-objects.html#sec-function.prototype.tostring).

The proposed contract is therefore **source-unavailable protected callables,
represented by genuine host-native callable syntax**. It is not exact preservation
of the original program's source-string observations. Code comparing a callable's
string against original source can observe this choice. Passing tests that allow
either representation must not be described as proof of exact source-text fidelity.

Do not override global or per-function `toString`, add `Symbol.toPrimitive`,
intercept RegExp coercion, or retain executable source bodies to satisfy these
tests. Such approaches either change additional observable properties or fail
when a pristine intrinsic is borrowed from another realm.

`RegExp/prototype/exec/S15.10.6.2_A1_T9.js` illustrates the practical defect:
the hoisted `__string` binding holds `function __string(){}`, despite its preceding
`var` declaration. `/1|12/.exec(__string)` should return null. Generated thunk
digits currently affect native coercion. A real proxy's native representation in
Node 26 is `function () { [native code] }`, which avoids that incidental match.
This does not make arbitrary source-dependent regular expressions invariant.

## One callable publication mechanism

Publish one final callable identity for each source callable, after constructing
its VM entry envelope and before making it observable. A transparent proxy with
a captured intrinsic constructor and a null-prototype empty handler preserves
the target's call/construct capabilities and delegates ordinary property access.
Existing async rejection or iterator adapters remain authoritative; do not stack
another suspension implementation around them.

Prefer extending existing callable metadata at registration rather than adding
independent caches at every creation site. `emit.rs` currently has `Functions`
metadata connecting closures to invocation records for `Method`, and the class
method adapter already records wrapped identities. Constructor capability caches,
where introduced by the runtime work, should use that same canonical identity
registration. Determine whether metadata needs a public-callable field and a
target-to-record entry before introducing any new WeakMap. Avoid redundant
lookups for private targets created and published exactly once.

The required creation seams are:

| Seam | Current behavior | Required integration |
| --- | --- | --- |
| `virtualize/mod.rs::try_virtualize` | Replaces static ordinary function bodies in place | Publish at declaration instantiation or expression evaluation, preserving scope and timing |
| `virtualize/shells.rs::shell` | Native suspension envelope; async currently uses a proxy | Publish all suspension kinds through the same record without extra Promise jobs |
| VM `emit.rs::Mk` | Creates nested ordinary, arrow, and suspended closures | Publish before returning and before registering `Functions` metadata |
| VM `Method` | Creates object methods/accessors, with a parameter-factory early return | Publish every result path while retaining home object and nonconstructibility |
| `classes/method_shell.js` | Ordinary methods return the target; suspension methods are proxied | Reuse registration for public methods/accessors and private method value bridges |
| Eval host `functionResult` | Creates ordinary runtime functions or returns prepared entry callables | Publish both routes, keeping dynamic-constructor metadata and self bindings coherent |

`Mk` already represents a named expression's self capture as a getter returning
the mutable local `clo`. Assigning its final public identity before exposure can
preserve that mechanism. The eval host similarly closes over `callable`. These
are useful existing seams, not reasons to duplicate self-binding machinery.

## Identity, instantiation, and class constraints

For ordinary constructors, an empty Proxy handler naturally forwards the public
proxy as `new.target` when constructed with `new proxy()`. Preserve an explicit
different `Reflect.construct` newTarget and derived-class newTarget; do not
unconditionally replace either with the hidden target. Retarget the ordinary
prototype object's initial `constructor` value without changing its descriptor.

Sloppy native parameter shells expose their hidden target through
`arguments.callee`. Bridge that initial data-descriptor value to the public
identity while retaining the genuine arguments exotic object and mapping. Keep
strict poison accessors, source mutations/deletion, and a source parameter named
`arguments` intact. Existing async callee bridging is a relevant precedent.

A static declaration cannot simply become `var f = publish(function f(){...})`
at its textual location: calls before the declaration must work, duplicate
declarations must resolve in source-defined order, and eval/global/block Annex B
instantiation must retain their existing semantics. Install the final callable
at the authoritative declaration-instantiation point before source execution.
Adding a later assignment also incorrectly overwrites mutations performed by
earlier source statements.

Named function expressions require an immutable private self binding, distinct
from any mutable outer variable with the same spelling. That binding must denote
the final public callable. A generic wrapper leaves the native expression's
internal name bound to its hidden target; changing the outer variable does not
repair it. Preserve sloppy ignored writes, strict write errors, recursion,
`typeof`, deletion rules, and direct eval's view of this environment.

Classes require more than transparent publication. A naive proxy changes static
private receivers, lexical class self, `prototype.constructor`, and values
captured by source static initializers. A possible shared class-lowering design
allocates/registers the public identity in a generated first static block before
source static initialization. Source class-self and initializer `this` then use
the public identity, while authoritative static private operations canonicalize
only that class's registered public receiver to its branded native target.
Subclass and unrelated-receiver private-brand failures must remain failures.
This is a design hypothesis requiring tests, not a validated recipe. Computed
keys, heritage evaluation, class-name TDZ, and nested lexical class scopes must
retain their separate existing contexts.

Private method values need publication at the authoritative private get bridge;
native private method slots cannot be reassigned. Repeated reads must return the
same callable. Converting all private methods into private fields would change
brand installation and initialization order. Private accessors returning arbitrary
source values are not permission to wrap those values indiscriminately. Preserve
native home objects and pass ordinary method receivers through unchanged.

## Required identity regression matrix

Run public frontend and runtime-source routes in real Node and Chrome, across
plain/minify and protected presets. Use explicit source-unavailable expectations
only for reflection; compare other semantics against the native source.

| Concern | Representative regression and expected behavior |
| --- | --- |
| Native reflection | Borrow pristine `Function.prototype.toString` from the same and another realm; every published callable satisfies native syntax |
| Ordinary coercion | `String(f)`, template interpolation, concatenation, and the exact RegExp test use the genuine host representation; user-defined conversion hooks retain precedence |
| Own properties | Compare own keys and all descriptors; publication adds no own `toString` or conversion property |
| Constructibility | Ordinary functions/classes remain constructible; arrows, methods, getters/setters, async and generator functions remain nonconstructible |
| Function kind | Preserve prototype chains, `.constructor`, name, length, generator prototype descriptors, and async/generator result brands |
| Declaration timing | `f(); function f(){}` works; earlier `f=other` is not overwritten at the declaration's textual position; duplicate declarations and eval/global/Annex B bindings retain ordering |
| Named self | `let f=function F(){return F===f}` returns true; outer rebinding leaves private `F` intact; strict/sloppy self assignments and direct eval retain their rules |
| Arguments | `function f(a){return arguments.callee===f}` remains true; mapped writes, deletion, descriptor replacement, poison accessors, and shadowed `arguments` remain correct |
| Constructor identity | `new f().constructor===f`; `new.target===f` inside `new f`; explicit foreign newTarget and derived subclass identity remain unchanged |
| Object methods | Repeated reads, accessor descriptors, receiver identity, computed names, and `super` after home-object prototype changes remain correct |
| Class identity | `static self(){return [this===A,A]}` and `static x=this; static y=A` expose the public class; class-expression inner names remain immutable and scoped |
| Static private | Direct, inherited, borrowed, and proxy-wrapped calls preserve successful brands and exact failures; `#x in receiver` agrees with private get/set/call |
| Private methods | `this.#m===this.#m`, returned private method reflection, call receiver behavior, and early brand installation retain semantics |
| Suspension | No added Promise jobs; synchronous parameter throws, async rejection, generator start timing, finally/return, mapped arguments, and iterator prototypes remain correct |
| User mutation | Freeze, seal, preventExtensions, prototype changes, name/length redefinition, source Proxy wrapping, and bound calls retain descriptors and behavior |
| Source compiler | All four dynamic constructors and eval-created callables use the same publication and immutable-self rules |

Read-only Node 26 probes confirmed the naive-wrapper hazards: a named ordinary
target returns false for both private-self/public identity and
`arguments.callee`/public identity, while its constructor `new.target` is already
the proxy. For a class proxy, `this.#x` throws, class lexical self and static
initializer values retain the hidden class, and instance `.constructor` differs.
These defects must be addressed before extending publication to those shapes.

## Caller reflection: confirmed unresolved defect

**Current status: unresolved; the integration below is proposed, not implemented.**
The package 32 remaining Test262 report records failures in
`language/arguments-object/10.6-13-a-2.js` and `10.6-13-a-3.js` (sloppy mode).
Both were independently reproduced using immutable package 34 and Node 26.
They expect a direct or indirect call through `arguments.callee.caller` to invoke
the source caller. The protected program instead throws
`TypeError: Cannot read properties of undefined (reading 'length')`.
Evidence is retained under `/tmp/mangler-caller-review`: `test262.json`,
`identity.js`, `identity.vm.js`, `invoke.js`, `invoke.vm.js`, and `invoke.json`.

The smaller identity fixture is:

```js
function pay() {
  function a() { return b(); }
  function b() { return arguments.callee.caller === a; }
  return a();
}
```

Native Node returns true; package 34 returns false. `arguments.callee` identifies
the native wrapper, but that wrapper was called by the interpreter's invocation
primitive. Its native `.caller` therefore denotes generated machinery. Calling
that value with `true`, as the larger fixture does, enters the interpreter with
invalid positional inputs and reaches `args.length` with `args` undefined.
The source35 descriptor changes normalize descriptor objects; they do not change
this native call stack. Code inspection confirms no caller mediation in the
current `Mk` ordinary wrapper or property-read primitive. This is separate from
mapped arguments, `arguments.callee` identity, and descriptor inheritance.

### Possible narrow integration

Extend the existing callable publication/`Functions` invocation metadata with a
canonical public identity and source activation information. Do not introduce a
second independent callable cache. Today `Functions` is conditionally emitted
for method conversion and allocated inside an interpreter activation; covering
ordinary callables, aliases, and cross-table invocations requires a shared
registration lifetime. This is an architectural change, not an existing feature.

Maintain source activation records at registered callable entry/exit, including
abrupt exits and recursion. A registered legacy-caller read could return the
public identity associated with the caller activation, while preserving native
restrictions for strict functions and other function kinds. Suspension must
restore active execution context only while the function actually executes;
a lexical parent or an unrelated prior Promise job is not automatically its
caller. Parameter initialization, callbacks through native code, nested native
calls, and reentry require explicit boundary handling. A stack containing only
managed VM frames would incorrectly skip an intervening source-native caller.

Use one property-observation policy for applicable registered callables, including
computed `.caller` reads, method-call receiver handling, source `Reflect.get`,
and mediated calls through a borrowed native caller getter. Preserve the ordinary
property lookup and source overrides before supplying activation-derived data.
Do not implement only the textual pattern `arguments.callee.caller`, and do not
replace arbitrary objects' `caller` properties. A function returning a caller
value must still return the canonical callable that can be invoked normally.

### Descriptor and Proxy constraints

The native property shape must be measured per supported host. Node 26 probes in
`reflection.cjs` / `reflection.json` show ordinary sloppy functions have **no own
`caller` property**. A configurable, non-enumerable accessor on Function.prototype
implements the extension. In the probe, a native sloppy parent is returned while
active, null is returned outside activation, and a strict parent is hidden as
null. These are observed Node results, not a universal function-descriptor model.
Consequently, `Object.getOwnPropertyDescriptor(f, 'caller')` must remain undefined
in this host unless source code adds an own property; the VM must not invent an
own descriptor merely to expose its activation metadata.

A transparent Proxy around that ordinary function makes `proxy.caller` throw
TypeError in Node 26 because the inherited native getter receives the Proxy.
A custom `get` trap can return an activation value for this default property
shape, but it is not universally unconstrained. Source code can install its own
non-configurable, non-writable `caller` data property. Returning a different value
through the Proxy then violates the native invariant and throws TypeError.
The saved probe confirms this. Any publication integration must honor source
properties and the host's actual target descriptors. See the specification's
[Proxy get invariant](https://tc39.es/ecma262/multipage/ordinary-and-exotic-objects-behaviours.html#sec-proxy-object-internal-methods-and-internal-slots-get-p-receiver).

### External observation limit and validation

Managed property reads can consult source activation records; uninstrumented
native observers do not automatically consult them. A public Proxy may intercept
an ordinary property read, but a pristine borrowed native caller getter can still
observe or reject its receiver according to the host's actual function object
and call stack. Existing JavaScript metadata cannot replace the host's activation
stack. Do not describe a managed-read fix as complete preservation of every
native external observation, and do not mutate global caller getters to hide
this distinction.

Before implementing this proposal, test direct and indirect caller invocation,
recursive activation, strict parents/callees, callbacks and nested native frames,
source-defined own caller properties, descriptor queries, Proxy invariants,
borrowed getters, and all supported callable kinds in Node and Chrome. Re-run
the two exact Test262 failures as well as the identity fixture. Publishing native
callable representations and implementing caller observation must be validated
together: transparent publication alone changes the Node result to an exception.
