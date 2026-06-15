// DOM-shaped recorder: classList add/remove/toggle/contains sequence.
(function () {
  const __ops = [];
  function makeClassList() {
    const set = new Set();
    return {
      add(...cls) {
        for (const c of cls) {
          __ops.push(["add", c]);
          set.add(c);
        }
      },
      remove(...cls) {
        for (const c of cls) {
          __ops.push(["remove", c]);
          set.delete(c);
        }
      },
      toggle(c) {
        const present = set.has(c);
        __ops.push(["toggle", c, !present]);
        if (present) set.delete(c);
        else set.add(c);
        return !present;
      },
      contains(c) {
        const r = set.has(c);
        __ops.push(["contains", c, r]);
        return r;
      },
      values() {
        return [...set];
      },
    };
  }

  const cl = makeClassList();
  cl.add("a", "b");
  cl.toggle("c"); // add
  cl.toggle("a"); // remove
  cl.remove("b");
  const hasC = cl.contains("c");
  cl.add("d");
  const final = cl.values();

  globalThis.__out = JSON.stringify({ ops: __ops, hasC, final });
})();
