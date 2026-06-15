// Private instance fields, private methods, private static. (QuickJS supports.)
(function () {
  class Account {
    #balance = 0;
    static #created = 0;
    constructor(initial) {
      this.#balance = initial;
      Account.#created++;
    }
    #fee(amount) { return amount * 0.1; }
    deposit(amount) {
      this.#balance += amount - this.#fee(amount);
      return this.#balance;
    }
    get balance() { return this.#balance; }
    static created() { return Account.#created; }
    has(obj) { return #balance in obj; }
  }
  const a = new Account(100);
  const after = a.deposit(50); // 100 + 50 - 5 = 145
  const b = new Account(0);
  globalThis.__out = JSON.stringify({
    after,
    balance: a.balance,
    created: Account.created(),
    brandA: a.has(a),
    brandObj: a.has({}),
  });
})();
