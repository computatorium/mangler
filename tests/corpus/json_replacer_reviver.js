// JSON.stringify replacer + JSON.parse reviver round-trip.
(function () {
  const data = { a: 1, b: 2, secret: "hide", nested: { c: 3, secret: "x" } };
  const json = JSON.stringify(data, (key, value) =>
    key === "secret" ? undefined : value
  );

  const parsed = JSON.parse('{"n":5,"doubleMe":10}', (key, value) =>
    key === "doubleMe" ? value * 2 : value
  );

  // array replacer (allow-list of keys)
  const allow = JSON.stringify(data, ["a", "nested", "c"]);

  // indentation
  const pretty = JSON.stringify({ x: [1, 2] }, null, 2);

  globalThis.__out = JSON.stringify({ json, parsed, allow, pretty });
})();
