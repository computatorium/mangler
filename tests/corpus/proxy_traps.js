// Proxy get/set/has traps recording a trace.
(function () {
  const trace = [];
  const target = { existing: 1 };
  const p = new Proxy(target, {
    get(t, key, recv) {
      if (typeof key === "symbol") return Reflect.get(t, key, recv);
      trace.push(["get", key]);
      return key in t ? t[key] : `default:${key}`;
    },
    set(t, key, value) {
      trace.push(["set", key, value]);
      t[key] = value * 2;
      return true;
    },
    has(t, key) {
      trace.push(["has", key]);
      return Reflect.has(t, key);
    },
  });

  const g1 = p.existing; // 1
  const g2 = p.missing; // "default:missing"
  p.score = 10; // stored as 20
  const stored = target.score; // 20
  const h = "existing" in p; // true (records has)

  globalThis.__out = JSON.stringify({ g1, g2, stored, h, trace });
})();
