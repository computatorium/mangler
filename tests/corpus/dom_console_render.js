// DOM-shaped recorder: a tiny "render" that logs to a fake console and builds
// a string-template view, recording the call sequence.
(function () {
  const __ops = [];
  const console = {
    log(...args) {
      __ops.push(["log", ...args]);
    },
    warn(...args) {
      __ops.push(["warn", ...args]);
    },
  };

  function render(state) {
    console.log("render:start", state.id);
    const rows = state.items
      .filter((it) => it.active)
      .map((it) => {
        console.log("row", it.id);
        return `<li>${it.label}</li>`;
      });
    if (rows.length === 0) console.warn("empty");
    console.log("render:end", rows.length);
    return `<ul>${rows.join("")}</ul>`;
  }

  const html = render({
    id: "list1",
    items: [
      { id: 1, label: "one", active: true },
      { id: 2, label: "two", active: false },
      { id: 3, label: "three", active: true },
    ],
  });

  globalThis.__out = JSON.stringify({ html, ops: __ops });
})();
