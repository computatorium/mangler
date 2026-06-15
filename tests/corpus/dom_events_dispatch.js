// DOM-shaped recorder: addEventListener + synchronous dispatch ordering.
(function () {
  const __ops = [];
  function makeNode(name) {
    const listeners = {};
    return {
      name,
      addEventListener(type, fn) {
        __ops.push(["addEventListener", name, type]);
        (listeners[type] || (listeners[type] = [])).push(fn);
      },
      dispatch(type, payload) {
        __ops.push(["dispatch", name, type]);
        for (const fn of listeners[type] || []) fn(payload);
      },
    };
  }

  const btn = makeNode("button");
  let clicks = 0;
  btn.addEventListener("click", (e) => {
    clicks++;
    __ops.push(["handler", "click#1", e.x]);
  });
  btn.addEventListener("click", (e) => {
    __ops.push(["handler", "click#2", e.x + clicks]);
  });
  btn.addEventListener("hover", () => {
    __ops.push(["handler", "hover"]);
  });

  btn.dispatch("click", { x: 10 });
  btn.dispatch("hover", {});
  btn.dispatch("click", { x: 20 });

  __ops.push(["totalClicks", clicks]);
  globalThis.__out = JSON.stringify(__ops);
})();
