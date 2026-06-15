// Optional chaining, nullish coalescing, logical assignment, exponentiation.
(function () {
  const data = { user: { name: "ann", roles: ["a", "b"] }, count: 0 };
  const name = data?.user?.name; // "ann"
  const missing = data?.admin?.name; // undefined
  const role0 = data?.user?.roles?.[0]; // "a"
  const callMaybe = data.user.greet?.(); // undefined (no throw)

  const nc1 = data.count ?? 99; // 0 (count is 0, not null/undefined)
  const nc2 = data.admin ?? "none"; // "none"

  let a = null;
  a ??= 7; // 7
  let b = 1;
  b &&= 4; // 4
  let c = 0;
  c ||= 9; // 9

  const power = 2 ** 8; // 256
  const chained = 2 ** 3 ** 2; // right-assoc => 2**9 = 512

  globalThis.__out = JSON.stringify({
    name, missing, role0, callMaybe, nc1, nc2, a, b, c, power, chained,
  });
})();
