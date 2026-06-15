// -0, NaN, Infinity, float precision, bitwise, integer wraparound.
(function () {
  const negZero = Object.is(-0, 0 * -1); // true
  const negZeroDiv = 1 / (0 * -1) === -Infinity; // true (-0 reciprocal)
  const nanSelf = NaN === NaN; // false
  const nanIs = Object.is(NaN, 0 / 0); // true
  const inf = [1 / 0, -1 / 0, Number.MAX_VALUE * 2]; // [Inf,-Inf,Inf]
  const floatSum = 0.1 + 0.2; // 0.30000000000000004
  const floatEq = floatSum === 0.3; // false

  // bitwise / shifts
  const bits = [5 & 3, 5 | 2, 5 ^ 1, ~5, 1 << 10, -8 >> 1, -8 >>> 28];

  // 32-bit overflow via |0
  const wrap = (0x7fffffff + 1) | 0; // -2147483648

  // toFixed / precision round-trip
  const fixed = (1 / 3).toFixed(5);

  globalThis.__out = JSON.stringify({
    negZero, negZeroDiv, nanSelf, nanIs, inf, floatSum, floatEq, bits, wrap, fixed,
  });
})();
