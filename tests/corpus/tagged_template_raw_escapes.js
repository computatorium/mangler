// Tagged template raw vs cooked strings, including escape sequences.
(function () {
  function diff(strings, ...vals) {
    return {
      cooked: strings.slice(),
      raw: strings.raw.slice(),
      vals,
    };
  }
  const x = 42;
  const r = diff`tab\t${x}newline\n${x + 1}end`;

  // String.raw built-in
  const path = String.raw`C:\temp\${"node"}\file`;

  // nested template inside expression
  const label = "L";
  const nested = `[${`${label}-${x}`}]`;

  globalThis.__out = JSON.stringify({
    cooked: r.cooked,
    raw: r.raw,
    vals: r.vals,
    path,
    nested,
  });
})();
