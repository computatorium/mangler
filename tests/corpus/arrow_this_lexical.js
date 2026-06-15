// Arrow functions capture lexical `this`; default/rest params.
(function () {
  const obj = {
    factor: 3,
    scaleAll(...nums) {
      // arrow closes over `this` of scaleAll
      return nums.map((n) => n * this.factor);
    },
    delayed() {
      const inner = () => this.factor + 1;
      return inner();
    },
  };
  const scaled = obj.scaleAll(1, 2, 3); // [3,6,9]
  const delayed = obj.delayed(); // 4

  function withDefaults(a, b = a * 2, c = a + b) {
    return [a, b, c];
  }
  const d1 = withDefaults(5); // [5,10,15]
  const d2 = withDefaults(5, 1); // [5,1,6]

  function restSum(first, ...rest) {
    return first + rest.reduce((s, x) => s + x, 0);
  }
  const rs = restSum(1, 2, 3, 4); // 10

  globalThis.__out = JSON.stringify({ scaled, delayed, d1, d2, rs });
})();
