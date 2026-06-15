// Multiple closures sharing one mutable cell; module-pattern privacy.
(function () {
  function makePair() {
    let shared = 0;
    const inc = () => ++shared;
    const dec = () => --shared;
    const read = () => shared;
    return { inc, dec, read };
  }
  const p = makePair();
  p.inc(); p.inc(); p.inc();
  p.dec();
  const shared = p.read(); // 2

  // IIFE module with private state
  const counter = (function () {
    let n = 0;
    return {
      next() { return n++; },
      reset() { n = 0; },
    };
  })();
  const seq = [counter.next(), counter.next(), counter.next()]; // [0,1,2]
  counter.reset();
  const afterReset = counter.next(); // 0

  // memoization closure
  function memoize(fn) {
    const cache = new Map();
    return (x) => {
      if (cache.has(x)) return cache.get(x);
      const r = fn(x);
      cache.set(x, r);
      return r;
    };
  }
  let calls = 0;
  const square = memoize((x) => {
    calls++;
    return x * x;
  });
  const memo = [square(4), square(4), square(5), calls]; // [16,16,25,2]

  globalThis.__out = JSON.stringify({ shared, seq, afterReset, memo });
})();
