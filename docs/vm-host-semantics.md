# Host semantic differences

## Assignment through `with` across suspension

Verified on 2026-09-06 using Node v26.0.0 and the QuickJS engine bundled with
`rquickjs-sys` 0.12.0. This difference also occurs in synchronous assignments.

```js
function* pay(object) {
  let x = 1;
  with (object) {
    x = yield 4;
  }
  yield x;
}
const object = { x: 2 };
const iterator = pay(object);
const first = iterator.next();
object[Symbol.unscopables] = { x: true };
const second = iterator.next(8);
console.log(first.value, second.value, object.x);
```

| Execution | Result |
|---|---|
| Original source in QuickJS | `4, 1, 8` |
| Protected source in QuickJS | `4, 1, 8` |
| Original source in Node v26.0.0 | `4, 8, 2` |
| Protected source in Node v26.0.0 | `4, 1, 8` |

The VM resolves and retains the assignment reference before evaluating the right
side, including before suspension. This follows
[ECMA-262 assignment evaluation](https://tc39.es/ecma262/multipage/ecmascript-language-expressions.html#sec-assignment-operators-runtime-semantics-evaluation):
the left reference is evaluated first, then the right value, then the write.
[Identifier evaluation](https://tc39.es/ecma262/multipage/ecmascript-language-expressions.html#sec-identifiers-runtime-semantics-evaluation)
resolves the binding immediately. Node delays this lookup for the bare identifier
assignment in this example, allowing the intervening change to `unscopables` to
redirect the write.

The permanent regression is
`suspension::lexical::tests::suspended_assignment_retains_the_resolved_object_environment`.
It checks both QuickJS source equivalence and the explicit result required by
that evaluation order. Code relying on Node's delayed lookup differs after
protection; this is a known host behavior difference, not a passing Node
differential check. CLI `--verify` only checks that output parses and does not
establish runtime equivalence.
