// Template literals and tagged templates that inspect strings.raw.
(function () {
  const name = "world";
  const n = 3;
  const plain = `hello ${name} x${n * 2}`; // "hello world x6"

  function tag(strings, ...values) {
    // include raw to prove escapes are preserved verbatim
    return strings.raw.join("|") + "##" + values.join(",");
  }
  const tagged = tag`a\n${1}\t${2}b`;

  const multiline = `line1
line2`;

  globalThis.__out = JSON.stringify({ plain, tagged, multiline });
})();
