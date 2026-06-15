// Array/object/nested destructuring, defaults, rest, swap, computed keys.
(function () {
  const [a, b, ...restArr] = [1, 2, 3, 4, 5];
  const { x, y: yy, z = 99 } = { x: 10, y: 20 };
  const {
    p: { q },
    arr: [first, second],
  } = { p: { q: 7 }, arr: [8, 9] };

  // swap
  let m = 1, nv = 2;
  [m, nv] = [nv, m];

  // computed-key destructuring
  const key = "dyn";
  const { [key]: dynVal } = { dyn: 42 };

  // parameter destructuring with defaults
  function draw({ size = 1, color = "red" } = {}) {
    return `${size}-${color}`;
  }
  const d0 = draw();
  const d1 = draw({ size: 3 });

  // object rest
  const { a1, ...others } = { a1: 1, b1: 2, c1: 3 };

  globalThis.__out = JSON.stringify({
    a, b, restArr, x, yy, z, q, first, second,
    m, nv, dynVal, d0, d1, a1, others,
  });
})();
