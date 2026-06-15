// try/catch/finally: optional catch binding, finally overrides return, rethrow.
(function () {
  const log = [];

  function finallyOverrides() {
    try {
      return "from-try";
    } finally {
      return "from-finally"; // overrides
    }
  }
  log.push(finallyOverrides());

  function optionalCatch() {
    try {
      throw new Error("boom");
    } catch {
      return "caught-no-binding";
    }
  }
  log.push(optionalCatch());

  function rethrow() {
    try {
      try {
        throw new TypeError("inner");
      } finally {
        log.push("inner-finally");
      }
    } catch (e) {
      return e instanceof TypeError ? "rethrown-type:" + e.message : "wrong";
    }
  }
  log.push(rethrow());

  // finally runs even on normal completion
  function normalFinally() {
    let out = "";
    try {
      out += "t";
    } catch (e) {
      out += "c";
    } finally {
      out += "f";
    }
    return out;
  }
  log.push(normalFinally());

  globalThis.__out = JSON.stringify(log);
})();
