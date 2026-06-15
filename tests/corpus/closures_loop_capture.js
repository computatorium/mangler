// The classic let-per-iteration capture vs a shared var capture.
(function () {
  const letFns = [];
  for (let i = 0; i < 3; i++) {
    letFns.push(() => i);
  }
  const letVals = letFns.map((f) => f()); // [0,1,2]

  const varFns = [];
  for (var j = 0; j < 3; j++) {
    varFns.push(() => j);
  }
  const varVals = varFns.map((f) => f()); // [3,3,3]

  // Counter closure: independent state per factory call.
  function makeCounter(start) {
    let n = start;
    return {
      inc() { return ++n; },
      get() { return n; },
    };
  }
  const a = makeCounter(10);
  const b = makeCounter(100);
  a.inc(); a.inc();
  b.inc();
  const counters = [a.get(), b.get()];

  globalThis.__out = JSON.stringify({ letVals, varVals, counters });
})();
