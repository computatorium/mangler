// Custom [Symbol.iterator], for-of over it, spread of it.
(function () {
  class Range {
    constructor(start, end) {
      this.start = start;
      this.end = end;
    }
    [Symbol.iterator]() {
      let cur = this.start;
      const end = this.end;
      return {
        next() {
          return cur < end
            ? { value: cur++, done: false }
            : { value: undefined, done: true };
        },
      };
    }
  }
  const r = new Range(2, 6);
  const viaFor = [];
  for (const x of r) viaFor.push(x);
  const viaSpread = [...new Range(0, 3)];
  const sum = [...new Range(1, 5)].reduce((s, x) => s + x, 0);

  globalThis.__out = JSON.stringify({ viaFor, viaSpread, sum });
})();
