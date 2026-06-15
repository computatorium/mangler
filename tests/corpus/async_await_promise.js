// async/await resolving deterministically; result is written to the sink, and
// the harness drains the microtask queue before reading __out.
(function () {
  globalThis.__out = "PENDING";
  async function double(x) { return (await x) * 2; }
  (async () => {
    const a = await double(Promise.resolve(5)); // 10
    const all = await Promise.all([
      Promise.resolve(1),
      double(Promise.resolve(2)), // 4
      Promise.resolve(3),
    ]);
    const chained = await Promise.resolve(7)
      .then((v) => v + 1)
      .then((v) => v * 2); // 16
    let racey = await Promise.race([Promise.resolve("first"), Promise.resolve("second")]);
    globalThis.__out = JSON.stringify({ a, all, chained, racey });
  })();
})();
