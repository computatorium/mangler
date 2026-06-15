// String/Array method round-trips: map/reduce/flatMap/sort/replace-fn/from/padStart.
(function () {
  const nums = [3, 1, 4, 1, 5, 9, 2, 6];
  const sorted = [...nums].sort((a, b) => a - b);
  const sumSq = nums.map((n) => n * n).reduce((a, b) => a + b, 0);
  const flat = [1, 2, 3].flatMap((n) => [n, n * 10]); // [1,10,2,20,3,30]
  const fromIter = Array.from({ length: 4 }, (_, i) => i * i); // [0,1,4,9]
  const fromSet = Array.from(new Set([5, 5, 6, 7]));

  // replace with function callback
  const replaced = "a1b2c3".replace(/(\d)/g, (m, d) => `[${+d * 2}]`);
  const padded = [1, 22, 333].map((n) => String(n).padStart(4, "0"));
  const joined = ["x", "y", "z"].join("-");
  const split = "p,q,r".split(",");
  const includes = "hello world".includes("wor");
  const repeated = "ab".repeat(3);
  const slices = [nums.slice(2, 5), "abcdef".slice(-3)];

  globalThis.__out = JSON.stringify({
    sorted, sumSq, flat, fromIter, fromSet, replaced, padded, joined, split,
    includes, repeated, slices,
  });
})();
