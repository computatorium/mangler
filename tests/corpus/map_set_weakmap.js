// Map/Set/WeakMap semantics: insertion order, key identity, deletion.
(function () {
  const m = new Map();
  m.set("a", 1).set("b", 2).set("a", 10); // overwrite keeps position
  m.delete("b");
  m.set("c", 3);
  const mapEntries = [...m.entries()];
  const mapHas = [m.has("a"), m.has("b")];

  const s = new Set([1, 2, 3, 2, 1]);
  s.add(4);
  s.delete(2);
  const setVals = [...s];
  const setSize = s.size;

  // object keys use identity
  const k1 = {};
  const k2 = {};
  const objMap = new Map();
  objMap.set(k1, "one");
  objMap.set(k2, "two");
  const idLookup = [objMap.get(k1), objMap.get(k2), objMap.get({})];

  const wm = new WeakMap();
  const wk = {};
  wm.set(wk, 99);
  const weakGet = [wm.get(wk), wm.has(wk), wm.has({})];

  globalThis.__out = JSON.stringify({ mapEntries, mapHas, setVals, setSize, idLookup, weakGet });
})();
