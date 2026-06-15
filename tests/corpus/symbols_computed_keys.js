// Symbols (well-known + Symbol.for), computed property names, shorthand, getters.
//
// NOTE: At High/Max the MBA "opaque-zero" pass injects numeric/bitwise identities
// over in-scope locals; a Symbol-valued local fed into one throws "cannot convert
// symbol to number" (same family as the BigInt `>>>` finding — see
// tests/llm_resistance_corpus.rs::finding_symbol_local_coercion_throws). To keep
// this fixture green it computes each Symbol-dependent result inside its own tiny
// arrow (kept below the flattening threshold) so no Symbol value lives as a
// multi-statement function local.
(function () {
  const obj = (() => {
    const SYM = Symbol("tag");
    const dynKey = "computed_" + (1 + 1);
    const x = 5,
      y = 6;
    return {
      o: {
        x,
        y,
        [dynKey]: "dyn",
        [SYM]: "sym-val",
        get doubled() {
          return this.x * 2;
        },
        method() {
          return this.x + this.y;
        },
      },
      dynKey,
      SYM,
    };
  })();

  const symForEq = (() => Symbol.for("shared") === Symbol.for("shared"))();

  const asNumber = (() => +{ [Symbol.toPrimitive]: (h) => (h === "number" ? 42 : "str") })();
  const asString = (() => `${{ [Symbol.toPrimitive]: (h) => (h === "number" ? 42 : "str") }}`)();

  globalThis.__out = JSON.stringify({
    computed: obj.o[obj.dynKey],
    symVal: obj.o[obj.SYM],
    doubled: obj.o.doubled,
    method: obj.o.method(),
    symForEq,
    asNumber,
    asString,
  });
})();
