// Generators, yield*, return value of delegation, infinite-with-take.
(function () {
  function* inner() {
    yield 1;
    yield 2;
    return "inner-done";
  }
  function* outer() {
    const got = yield* inner();
    yield got;
    yield 3;
  }
  const collected = [...outer()]; // [1,2,"inner-done",3]

  function* nats() {
    let i = 0;
    while (true) yield i++;
  }
  const g = nats();
  const taken = [g.next().value, g.next().value, g.next().value]; // [0,1,2]

  // generator with explicit .next(arg) feeding
  function* echo() {
    const a = yield "first";
    const b = yield a;
    return a + b;
  }
  const e = echo();
  e.next();
  e.next(10);
  const final = e.next(20).value; // 30

  globalThis.__out = JSON.stringify({ collected, taken, final });
})();
