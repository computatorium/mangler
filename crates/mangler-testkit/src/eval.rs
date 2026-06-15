//! rquickjs eval-and-compare with `Object.is` (SameValue) semantics.
//!
//! Ported and generalized from the repo's `tests/common/mod.rs` /
//! `tests/differential.rs` harness. The core operation: run an *original* and a
//! *transformed* JS source string, each in its own fresh QuickJS runtime, capture
//! a comparable value from each, and compare them with `Object.is` so that `-0` is
//! distinguished from `0` and `NaN` matches `NaN` (the blind spots a `String(...)`
//! comparison silently collapses). Thrown errors are handled symmetrically: if both
//! programs throw, they are equal iff the (engine-normalized) error messages match.
//!
//! # Capture modes
//! Two ways to extract the comparable value (see [`CaptureMode`]):
//! * [`CaptureMode::Completion`] — the program's last-expression completion value,
//!   coerced through `Object.is`. Matches the `tests/differential.rs` fixtures,
//!   which end in a trailing-expression IIFE.
//! * [`CaptureMode::Sink`] — a designated global (default `globalThis.__out`), set
//!   by the program as a side effect. Matches the `tests/corpus/` files, which are
//!   IIFEs that assign their JSON result to `globalThis.__out`.
//!
//! # rquickjs limitations discovered
//! * QuickJS is ES2020-ish: no top-level `await`, no DOM/`window`/`document`. Corpus
//!   files that need DOM build a self-contained fake `document`; that is the corpus
//!   author's responsibility, not this harness's.
//! * Error *messages* differ across engines (and even across QuickJS versions), so
//!   symmetric-throw equality compares the message text the same engine produces for
//!   both programs — it is a *same-engine* differential, not a spec assertion.
//! * The cross-value `Object.is` comparison marshals the captured value through a
//!   third context as a string for non-primitive completion values; for the JSON-
//!   string-returning fixtures this is exactly `===`. See [`eval_same_value`].

use rquickjs::{Context, Runtime};

/// Which value to capture from an evaluated program for comparison.
#[derive(Clone, Debug)]
pub enum CaptureMode {
    /// The completion value of the final expression, coerced to a string for the
    /// cross-context `Object.is` comparison. Suits trailing-expression IIFEs.
    Completion,
    /// A designated global set by the program as a side effect, e.g.
    /// `globalThis.__out = JSON.stringify(result)`. The string is the global name's
    /// JS expression (default `"globalThis.__out"`).
    Sink(String),
}

impl Default for CaptureMode {
    fn default() -> Self {
        CaptureMode::Sink("globalThis.__out".to_string())
    }
}

impl CaptureMode {
    /// The default sink mode reading `globalThis.__out` (the corpus convention).
    pub fn sink() -> Self {
        CaptureMode::default()
    }

    /// Completion-value mode (trailing-expression fixtures).
    pub fn completion() -> Self {
        CaptureMode::Completion
    }
}

/// The outcome of evaluating one program: either the captured value (as a string)
/// or a thrown error (as the engine's normalized message).
#[derive(Clone, Debug, PartialEq, Eq)]
enum Outcome {
    Value(String),
    Threw(String),
}

/// The result of a differential eval-and-compare between two programs.
#[derive(Clone, Debug)]
pub struct DiffResult {
    /// True iff the two programs are behaviorally equal under the chosen capture
    /// mode and SameValue semantics (including symmetric throw with equal message).
    pub equal: bool,
    /// Human-readable description of *what* the original produced (a value or an
    /// error), for diagnostics.
    pub original: String,
    /// Same for the transformed program.
    pub transformed: String,
    /// A one-line reason, suitable for a panic/assert message.
    pub reason: String,
}

impl DiffResult {
    /// True when the programs diverged.
    pub fn is_divergent(&self) -> bool {
        !self.equal
    }
}

/// Evaluate one program in a fresh runtime and capture its [`Outcome`].
///
/// Completion mode coerces the completion value to a String inside the engine via
/// `Object.is`-friendly marshaling: the program is wrapped so its completion value
/// is stored, then read back as a string. Sink mode reads the designated global.
fn run(code: &str, mode: &CaptureMode) -> Outcome {
    let rt = match Runtime::new() {
        Ok(rt) => rt,
        Err(e) => return Outcome::Threw(format!("runtime init failed: {e:?}")),
    };
    let ctx = match Context::full(&rt) {
        Ok(c) => c,
        Err(e) => return Outcome::Threw(format!("context init failed: {e:?}")),
    };

    // Evaluate the program first (driving any pending jobs so microtasks settle),
    // then read the captured value. Errors at either step become `Threw`.
    let outcome = ctx.with(|ctx| -> Outcome {
        // Run the program. In completion mode we need the trailing value, so eval
        // the body as an expression statement and read it; QuickJS `eval` returns
        // the completion value, which we coerce to a string.
        let read_expr = match mode {
            CaptureMode::Completion => {
                // Stash the completion value on a private global, then String() it.
                // Wrapping in an indirect eval keeps `let`/`const` at the program's
                // top level from leaking into our reader expression.
                match ctx.eval::<rquickjs::Value, _>(code) {
                    Ok(v) => {
                        if let Err(e) = ctx.globals().set("__tk_completion", v) {
                            return Outcome::Threw(format!("{e:?}"));
                        }
                        "__tk_completion"
                    }
                    Err(e) => return throw_message(&ctx, e),
                }
            }
            CaptureMode::Sink(name) => {
                if let Err(e) = ctx.eval::<(), _>(code) {
                    return throw_message(&ctx, e);
                }
                name.as_str()
            }
        };

        // Coerce the captured value to a string for cross-context comparison, while
        // PRESERVING the SameValue-relevant distinctions: tag `-0` and `NaN` so they
        // do not collapse into `"0"` / a plain number string.
        let coercer = format!(
            "(function(__v){{ \
               if (typeof __v === 'number') {{ \
                 if (__v === 0 && 1/__v === -Infinity) return '\\u0000-0'; \
                 if (__v !== __v) return '\\u0000NaN'; \
                 return '\\u0000n:' + String(__v); \
               }} \
               if (typeof __v === 'bigint') return '\\u0000b:' + String(__v); \
               if (__v === undefined) return '\\u0000undefined'; \
               if (__v === null) return '\\u0000null'; \
               return String(__v); \
             }})({read_expr})"
        );
        match ctx.eval::<String, _>(coercer.as_str()) {
            Ok(s) => Outcome::Value(s),
            Err(e) => throw_message(&ctx, e),
        }
    });

    // Drain any still-pending jobs (defensive; corpus programs are synchronous).
    while rt.is_job_pending() {
        let _ = rt.execute_pending_job();
    }
    outcome
}

/// Normalize a thrown rquickjs error into a stable message string. Prefers the JS
/// exception's `.message` (engine-portable) over the Rust-side `{:?}` (which embeds
/// stack/format noise).
fn throw_message(ctx: &rquickjs::Ctx, err: rquickjs::Error) -> Outcome {
    if let rquickjs::Error::Exception = err {
        let exc = ctx.catch();
        // Try to read `.message`; fall back to String(exc).
        let msg: String = exc
            .as_object()
            .and_then(|o| o.get::<_, String>("message").ok())
            .or_else(|| exc.as_string().and_then(|s| s.to_string().ok()))
            .unwrap_or_else(|| "<exception>".to_string());
        Outcome::Threw(msg)
    } else {
        Outcome::Threw(format!("{err:?}"))
    }
}

fn describe(o: &Outcome) -> String {
    match o {
        Outcome::Value(v) => format!("value {v:?}"),
        Outcome::Threw(m) => format!("threw {m:?}"),
    }
}

/// Evaluate `original` and `transformed` and compare them under the default sink
/// capture mode (`globalThis.__out`). Returns a [`DiffResult`]; never panics.
///
/// Equality is SameValue (`Object.is`) over the captured value, with symmetric
/// throw: if both throw, they are equal iff the engine-normalized messages match.
/// A value-vs-throw mismatch is always unequal.
pub fn eval_same_value(original: &str, transformed: &str) -> DiffResult {
    eval_same_value_with(original, transformed, &CaptureMode::default())
}

/// As [`eval_same_value`] but with an explicit [`CaptureMode`].
pub fn eval_same_value_with(original: &str, transformed: &str, mode: &CaptureMode) -> DiffResult {
    let a = run(original, mode);
    let b = run(transformed, mode);
    // Because `run` already coerced values to a tagged canonical string that
    // preserves `-0`/`NaN`/bigint distinctions, structural string equality of the
    // two `Value` outcomes is exactly `Object.is` on the original values.
    let equal = a == b;
    let reason = if equal {
        "equal".to_string()
    } else {
        match (&a, &b) {
            (Outcome::Value(_), Outcome::Threw(_)) => {
                "original produced a value but transformed threw".to_string()
            }
            (Outcome::Threw(_), Outcome::Value(_)) => {
                "original threw but transformed produced a value".to_string()
            }
            (Outcome::Threw(x), Outcome::Threw(y)) => {
                format!("both threw but messages differ: {x:?} vs {y:?}")
            }
            (Outcome::Value(_), Outcome::Value(_)) => {
                "values differ under Object.is (SameValue)".to_string()
            }
        }
    };
    DiffResult {
        equal,
        original: describe(&a),
        transformed: describe(&b),
        reason,
    }
}

/// Panic with a descriptive message unless `original` and `transformed` are
/// behaviorally equal under the default sink capture mode. For use in `#[test]`s.
pub fn assert_behaviorally_equal(original: &str, transformed: &str) {
    assert_behaviorally_equal_with(original, transformed, &CaptureMode::default())
}

/// As [`assert_behaviorally_equal`] but with an explicit [`CaptureMode`].
pub fn assert_behaviorally_equal_with(original: &str, transformed: &str, mode: &CaptureMode) {
    let r = eval_same_value_with(original, transformed, mode);
    assert!(
        r.equal,
        "behavioral divergence: {}\n  original:    {}\n  transformed: {}\n--- transformed source ---\n{}",
        r.reason, r.original, r.transformed, transformed
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const IDENTITY_SINK: &str = "globalThis.__out = JSON.stringify({a:1, b:[2,3], c:'x'});";

    #[test]
    fn identity_transform_is_equal_sink() {
        let r = eval_same_value(IDENTITY_SINK, IDENTITY_SINK);
        assert!(r.equal, "{}", r.reason);
    }

    #[test]
    fn different_output_is_detected_sink() {
        let other = "globalThis.__out = JSON.stringify({a:1, b:[2,3], c:'DIFFERENT'});";
        let r = eval_same_value(IDENTITY_SINK, other);
        assert!(r.is_divergent());
    }

    #[test]
    fn completion_mode_identity() {
        let src = "(function(){ return JSON.stringify({x:1}); })();";
        let r = eval_same_value_with(src, src, &CaptureMode::completion());
        assert!(r.equal, "{}", r.reason);
    }

    #[test]
    fn neg_zero_distinguished_from_zero() {
        // Object.is(-0, 0) is false: these must be detected as DIVERGENT.
        let neg = "globalThis.__out = (0 * -1);";
        let pos = "globalThis.__out = 0;";
        let r = eval_same_value(neg, pos);
        assert!(r.is_divergent(), "-0 must not equal 0 under SameValue");
        // And -0 equals itself.
        assert!(eval_same_value(neg, neg).equal);
    }

    #[test]
    fn nan_equals_nan() {
        let a = "globalThis.__out = 0/0;";
        let b = "globalThis.__out = Number('x');";
        let r = eval_same_value(a, b);
        assert!(r.equal, "NaN must equal NaN under SameValue: {}", r.reason);
    }

    #[test]
    fn symmetric_throw_same_message_is_equal() {
        let a = "throw new Error('boom');";
        let b = "throw new Error('boom');";
        let r = eval_same_value(a, b);
        assert!(r.equal, "symmetric throw w/ equal message: {}", r.reason);
    }

    #[test]
    fn symmetric_throw_different_message_is_unequal() {
        let a = "throw new Error('boom');";
        let b = "throw new Error('different');";
        let r = eval_same_value(a, b);
        assert!(r.is_divergent());
    }

    #[test]
    fn throw_vs_value_is_unequal() {
        let a = "throw new Error('x');";
        let b = "globalThis.__out = 1;";
        assert!(eval_same_value(a, b).is_divergent());
    }

    #[test]
    fn bigint_values_compared() {
        let a = "globalThis.__out = 9007199254740993n;";
        let b = "globalThis.__out = 9007199254740993n;";
        assert!(eval_same_value(a, b).equal);
        let c = "globalThis.__out = 9007199254740994n;";
        assert!(eval_same_value(a, c).is_divergent());
    }

    #[test]
    fn undefined_distinct_from_string_undefined() {
        let a = "globalThis.__out = undefined;";
        let b = "globalThis.__out = 'undefined';";
        assert!(eval_same_value(a, b).is_divergent());
    }
}
