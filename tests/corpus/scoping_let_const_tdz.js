// let/const block scope, shadowing, and TDZ-safe patterns.
(function () {
  "use strict";
  const out = [];
  let x = 1;
  {
    let x = 2;
    const y = x * 10;
    out.push(x, y);
  }
  out.push(x);
  // const reference inside a nested block, no reassignment.
  const base = 100;
  for (let i = 0; i < 3; i++) {
    const scaled = base + i;
    out.push(scaled);
  }
  // TDZ-safe: read only after initialization in the same block.
  let z;
  z = 5;
  out.push(z);
  // Block produces its own binding shadowing the outer const-ish var.
  let result = (() => {
    let acc = 0;
    for (let k = 1; k <= 4; k++) acc += k;
    return acc;
  })();
  out.push(result);
  globalThis.__out = JSON.stringify(out);
})();
