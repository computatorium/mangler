// async generators + for-await-of; settled by the harness microtask drain.
(function () {
  globalThis.__out = "PENDING";
  async function* produce() {
    yield await Promise.resolve(1);
    yield await Promise.resolve(2);
    yield await Promise.resolve(3);
  }
  (async () => {
    let total = 0;
    const seen = [];
    for await (const x of produce()) {
      total += x;
      seen.push(x);
    }
    // await in a try/finally
    let cleanup = "";
    try {
      const v = await Promise.resolve("body");
      cleanup += v;
    } finally {
      cleanup += "-fin";
    }
    globalThis.__out = JSON.stringify({ total, seen, cleanup });
  })();
})();
