// Labeled break/continue, nested loops, do-while, switch fallthrough.
(function () {
  let continued = "";
  outer: for (let i = 0; i < 3; i++) {
    for (let j = 0; j < 3; j++) {
      if (j === 1) continue outer;
      continued += `${i}${j}`;
    }
  }

  let broken = "";
  search: for (let i = 0; i < 4; i++) {
    for (let j = 0; j < 4; j++) {
      if (i + j === 3) {
        broken = `${i},${j}`;
        break search;
      }
    }
  }

  let dw = 0, k = 0;
  do {
    dw += k;
    k++;
  } while (k < 5); // 0+1+2+3+4 = 10

  function classify(n) {
    let r = "";
    switch (n) {
      case 1:
        r += "one";
      case 2:
        r += "two";
        break;
      case 3:
        r += "three";
        break;
      default:
        r += "other";
    }
    return r;
  }
  const switches = [classify(1), classify(2), classify(3), classify(9)];

  globalThis.__out = JSON.stringify({ continued, broken, dw, switches });
})();
