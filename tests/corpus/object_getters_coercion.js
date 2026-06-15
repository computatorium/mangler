// Object-literal getters/setters, valueOf/toString coercion ORDER (observable).
(function () {
  const order = [];
  const obj = {
    _v: 3,
    get value() {
      order.push("get");
      return this._v;
    },
    set value(x) {
      order.push("set:" + x);
      this._v = x;
    },
    valueOf() {
      order.push("valueOf");
      return this._v;
    },
    toString() {
      order.push("toString");
      return "T" + this._v;
    },
  };

  obj.value = 5; // set:5
  const got = obj.value; // get
  const asNum = obj + 1; // valueOf -> 6
  const asStr = `${obj}`; // toString -> "T5"
  const concat = "" + obj; // valueOf (string concat prefers valueOf when no Symbol.toPrimitive) -> "5"

  globalThis.__out = JSON.stringify({ got, asNum, asStr, concat, order });
})();
