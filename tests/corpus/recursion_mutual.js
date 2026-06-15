// Recursion and mutual recursion (bounded, deterministic).
(function () {
  function fib(n) {
    return n < 2 ? n : fib(n - 1) + fib(n - 2);
  }
  const fibs = Array.from({ length: 12 }, (_, i) => fib(i));

  function ack(m, n) {
    if (m === 0) return n + 1;
    if (n === 0) return ack(m - 1, 1);
    return ack(m - 1, ack(m, n - 1));
  }
  const ackSmall = ack(2, 3); // 9

  // mutual recursion
  function isEven(n) { return n === 0 ? true : isOdd(n - 1); }
  function isOdd(n) { return n === 0 ? false : isEven(n - 1); }
  const parity = [isEven(10), isOdd(7), isEven(0)];

  // recursive tree sum
  const tree = { v: 1, kids: [{ v: 2, kids: [] }, { v: 3, kids: [{ v: 4, kids: [] }] }] };
  function treeSum(node) {
    return node.v + node.kids.reduce((s, k) => s + treeSum(k), 0);
  }
  const total = treeSum(tree); // 10

  globalThis.__out = JSON.stringify({ fibs, ackSmall, parity, total });
})();
