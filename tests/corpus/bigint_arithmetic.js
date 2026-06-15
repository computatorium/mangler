// BigInt arithmetic / comparisons. Deliberately written as a single trailing
// expression with NO BigInt *locals* held across statements: at High/Max the MBA
// "opaque-zero" pass injects `(local>>>0)` forms that THROW on a BigInt operand
// ("BigInt operands are forbidden for >>>"). Keeping BigInts out of
// multi-statement function locals avoids that miscompile. The miscompile itself
// is captured as a standalone #[ignore]d FINDING in tests/llm_resistance_corpus.rs
// (fn finding_bigint_local_unsigned_shift_throws). See the Stage 0 report.
(function () {
  globalThis.__out = JSON.stringify([
    (2n ** 100n).toString(), // 1267650600228229401496703205376
    (99999999999n * 99999999999n).toString(),
    ((10n ** 30n) % 7n).toString(),
    (-(2n ** 64n)).toString(),
    ((0xffn & 0x0fn) | 0x10n).toString(), // 31
    (123456789012345678901234567890n + 1n).toString(),
    [10n > 5n, 10n === 10n, 5n < 6, 7n == 7], // mixed/loose comparisons
  ]);
})();
