// for-of over Array/Map/Set/String/generator; for-in key order.
(function () {
  const arr = [10, 20, 30];
  const ofArr = [];
  for (const v of arr) ofArr.push(v);

  const map = new Map([["a", 1], ["b", 2]]);
  const ofMap = [];
  for (const [k, v] of map) ofMap.push(k + v);

  const set = new Set([1, 1, 2, 3]);
  const ofSet = [...set];

  const ofStr = [];
  for (const ch of "héllo") ofStr.push(ch); // includes combining-safe code points

  function* g() { yield "x"; yield "y"; }
  const ofGen = [...g()];

  // for-in enumeration order: integer-like keys ascending, then string insertion order.
  const obj = { 2: "b", 1: "a", foo: "f", bar: "z" };
  const inKeys = [];
  for (const k in obj) inKeys.push(k);

  globalThis.__out = JSON.stringify({ ofArr, ofMap, ofSet, ofStr, ofGen, inKeys });
})();
