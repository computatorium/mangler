// Spread in calls, array literals, and object literals.
(function () {
  function sum3(a, b, c) { return a + b + c; }
  const args = [10, 20, 30];
  const called = sum3(...args); // 60

  const head = [1, 2];
  const tail = [4, 5];
  const merged = [...head, 3, ...tail]; // [1,2,3,4,5]

  const base = { a: 1, b: 2 };
  const ext = { ...base, b: 20, c: 30 }; // {a:1,b:20,c:30}

  const max = Math.max(...[3, 9, 2, 7]); // 9
  const chars = [..."abc"]; // ["a","b","c"]

  globalThis.__out = JSON.stringify({ called, merged, ext, max, chars });
})();
