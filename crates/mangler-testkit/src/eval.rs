//! Bounded behavioral observations from independent QuickJS runtimes.
//!
//! Primitive results use an injective, type-tagged wire encoding (including -0,
//! NaN, BigInt and lone UTF-16 surrogates). Objects/functions/symbols are not
//! silently stringified: callers must explicitly serialize their result. Console
//! calls and the optional `globalThis.__trace` record observable side-effect order,
//! including effects before a throw. This is a fixture observation contract, not
//! a claim to compare every possible JavaScript side effect.

use rquickjs::{Context, Ctx, Function, Object, Persistent, Runtime, Value};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

/// Which value to read after all queued microtasks have settled.
#[derive(Clone, Debug)]
pub enum CaptureMode {
    Completion,
    Sink(String),
}

impl Default for CaptureMode {
    fn default() -> Self {
        Self::Sink("globalThis.__out".into())
    }
}
impl CaptureMode {
    pub fn sink() -> Self {
        Self::default()
    }
    pub fn completion() -> Self {
        Self::Completion
    }
}

/// Per-program limits, applied to original and transformed code independently.
#[derive(Clone, Copy, Debug)]
pub struct EvalLimits {
    pub timeout: Duration,
    pub memory_bytes: usize,
    pub stack_bytes: usize,
    pub max_jobs: usize,
}
impl Default for EvalLimits {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(5),
            memory_bytes: 64 * 1024 * 1024,
            stack_bytes: 512 * 1024,
            max_jobs: 10_000,
        }
    }
}

/// An incomplete run is never evidence of equivalence, even against itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Failure {
    Timeout,
    JobLimit,
    MemoryLimit,
    StackLimit,
    UnsupportedCapture,
    Engine(String),
}

/// Values and thrown values are encoded by the same type-tagged protocol.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Value(String),
    Threw(String),
    Incomplete(Failure),
}

/// A complete observation includes effects before an exception and unhandled
/// promise rejections. Handled rejections are removed after the job queue settles.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Evaluation {
    pub outcome: Outcome,
    pub trace: String,
    pub rejections: Vec<String>,
}
impl Evaluation {
    pub fn is_complete(&self) -> bool {
        !matches!(self.outcome, Outcome::Incomplete(_))
    }
    fn incomplete(failure: Failure) -> Self {
        Self {
            outcome: Outcome::Incomplete(failure),
            trace: String::new(),
            rejections: Vec::new(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct DiffResult {
    pub equal: bool,
    pub original: String,
    pub transformed: String,
    pub reason: String,
}
impl DiffResult {
    pub fn is_divergent(&self) -> bool {
        !self.equal
    }
}

// Shared with the real-engine harness. Keep the encoder in a closure created
// before user code, so replacing JSON/String/Array globals cannot forge a result.
pub const OBSERVER_SOURCE: &str = include_str!("observer.js");

fn engine_failure(ctx: &Ctx<'_>, err: rquickjs::Error, timed_out: bool) -> Failure {
    if timed_out {
        return Failure::Timeout;
    }
    if matches!(err, rquickjs::Error::Allocation) {
        return Failure::MemoryLimit;
    }
    if matches!(err, rquickjs::Error::Exception) {
        let exc = ctx.catch();
        if let Some(obj) = exc.as_object() {
            let msg = obj.get::<_, String>("message").unwrap_or_default();
            if msg.contains("out of memory") {
                return Failure::MemoryLimit;
            }
            if msg.contains("stack overflow") || msg.contains("Maximum call stack size exceeded") {
                return Failure::StackLimit;
            }
            if msg.contains("testkit: unsupported capture") {
                return Failure::UnsupportedCapture;
            }
            return Failure::Engine(msg);
        }
    }
    Failure::Engine(format!("{err:?}"))
}

fn encode<'js>(
    observer: &Object<'js>,
    value: Value<'js>,
    thrown: bool,
) -> rquickjs::Result<String> {
    let f: Function = observer.get(if thrown { "thrown" } else { "value" })?;
    f.call((value,))
}

type Saved = Persistent<Value<'static>>;

/// Evaluate a script with hard QuickJS time/memory/stack limits and a bounded
/// microtask queue. The sink is read only after settlement. No process-global state
/// is changed, so independent tests can execute concurrently.
pub fn evaluate_with_limits(code: &str, mode: &CaptureMode, limits: EvalLimits) -> Evaluation {
    // Reserve enough for QuickJS bootstrap and the observation machinery. In
    // rquickjs 0.12, failing JS_NewRuntime2's initial allocation dereferences its
    // null result before returning Error::Allocation; reject impossible budgets
    // before crossing that FFI boundary. Zero stack size disables QuickJS's limit.
    if limits.memory_bytes < 1024 * 1024 {
        return Evaluation::incomplete(Failure::MemoryLimit);
    }
    if limits.stack_bytes < 64 * 1024 {
        return Evaluation::incomplete(Failure::StackLimit);
    }
    let memory_exceeded = Rc::new(Cell::new(false));
    let rt = match Runtime::new_with_alloc(crate::allocation::BudgetAllocator::new(
        limits.memory_bytes,
        memory_exceeded.clone(),
    )) {
        Ok(rt) => rt,
        Err(e) => {
            return Evaluation::incomplete(if memory_exceeded.get() {
                Failure::MemoryLimit
            } else {
                Failure::Engine(format!("runtime init: {e}"))
            });
        }
    };
    rt.set_max_stack_size(limits.stack_bytes);
    let started = Instant::now();
    let timed_out = Rc::new(Cell::new(false));
    let flag = timed_out.clone();
    rt.set_interrupt_handler(Some(Box::new(move || {
        let expired = started.elapsed() >= limits.timeout;
        if expired {
            flag.set(true);
        }
        expired
    })));
    let ctx = match Context::full(&rt) {
        Ok(ctx) => ctx,
        Err(e) => {
            return Evaluation::incomplete(
                if memory_exceeded.get() || matches!(e, rquickjs::Error::Allocation) {
                    Failure::MemoryLimit
                } else {
                    Failure::Engine(format!("context init: {e}"))
                },
            );
        }
    };
    let pending = Rc::new(RefCell::new(Vec::<(Saved, Saved)>::new()));
    let tracked = pending.clone();
    rt.set_host_promise_rejection_tracker(Some(Box::new(move |ctx, promise, reason, handled| {
        let promise = Persistent::save(&ctx, promise);
        let mut list = tracked.borrow_mut();
        if handled {
            list.retain(|(p, _)| p != &promise);
        } else {
            list.push((promise, Persistent::save(&ctx, reason)));
        }
    })));

    // All persistent values are dropped and the tracker unregistered before the
    // runtime is freed (QuickJS rejects outstanding references at runtime drop).
    let result = (|| {
        let observer = ctx
            .with(|ctx| {
                ctx.eval::<Object, _>(OBSERVER_SOURCE)
                    .map(|v| Persistent::save(&ctx, v))
                    .map_err(|e| engine_failure(&ctx, e, timed_out.get()))
            })
            .map_err(Evaluation::incomplete)?;
        let mut failure = None;
        let mut thrown = None;
        let completion = ctx.with(|ctx| {
            let mut options = rquickjs::context::EvalOptions::default();
            options.strict = false;
            match ctx.eval_with_options::<Value, _>(code, options) {
                Ok(value) => Some(Persistent::save(&ctx, value)),
                Err(rquickjs::Error::Exception) => {
                    let value = ctx.catch();
                    if timed_out.get() {
                        failure = Some(Failure::Timeout);
                    } else if let Some(obj) = value.as_object() {
                        let msg = obj.get::<_, String>("message").unwrap_or_default();
                        if msg.contains("out of memory") {
                            failure = Some(Failure::MemoryLimit);
                        } else if msg.contains("stack overflow")
                            || msg.contains("Maximum call stack size exceeded")
                        {
                            failure = Some(Failure::StackLimit);
                        } else if msg.contains("testkit: unsupported capture") {
                            failure = Some(Failure::UnsupportedCapture);
                        }
                    }
                    thrown = Some(Persistent::save(&ctx, value));
                    None
                }
                Err(e) => {
                    failure = Some(engine_failure(&ctx, e, timed_out.get()));
                    None
                }
            }
        });
        let mut jobs = 0;
        while failure.is_none() && rt.is_job_pending() {
            if started.elapsed() >= limits.timeout {
                failure = Some(Failure::Timeout);
                break;
            }
            if jobs == limits.max_jobs {
                failure = Some(Failure::JobLimit);
                break;
            }
            jobs += 1;
            if let Err(error) = rt.execute_pending_job() {
                error.0.with(|ctx| {
                    let value = ctx.catch();
                    if timed_out.get() {
                        failure = Some(Failure::Timeout);
                    } else {
                        thrown = Some(Persistent::save(&ctx, value));
                    }
                });
            }
        }
        if timed_out.get() {
            failure = Some(Failure::Timeout);
        }
        if let Some(failure) = failure {
            return Err(Evaluation::incomplete(failure));
        }
        ctx.with(|ctx| -> Result<Evaluation, Evaluation> {
            let capture = || -> rquickjs::Result<Evaluation> {
                let observer = observer.restore(&ctx)?;
                let outcome = if let Some(value) = thrown {
                    Outcome::Threw(encode(&observer, value.restore(&ctx)?, true)?)
                } else {
                    let value = match mode {
                        CaptureMode::Completion => {
                            completion.expect("successful evaluation").restore(&ctx)?
                        }
                        CaptureMode::Sink(expr) => ctx.eval::<Value, _>(expr.as_str())?,
                    };
                    Outcome::Value(encode(&observer, value, false)?)
                };
                let trace: Function = observer.get("trace")?;
                let trace = trace.call(())?;
                let mut rejections = Vec::new();
                for (_, reason) in pending.borrow_mut().drain(..) {
                    rejections.push(encode(&observer, reason.restore(&ctx)?, true)?);
                }
                Ok(Evaluation {
                    outcome,
                    trace,
                    rejections,
                })
            };
            capture().map_err(|e| Evaluation::incomplete(engine_failure(&ctx, e, timed_out.get())))
        })
    })()
    .unwrap_or_else(|e| e);
    rt.set_host_promise_rejection_tracker(None);
    pending.borrow_mut().clear();
    if memory_exceeded.get() {
        Evaluation::incomplete(Failure::MemoryLimit)
    } else if result.rejections.iter().any(|value| {
        value.starts_with("[\"error\",")
            && (value.contains("stack overflow")
                || value.contains("Maximum call stack size exceeded"))
    }) {
        Evaluation::incomplete(Failure::StackLimit)
    } else {
        result
    }
}

pub fn evaluate(code: &str, mode: &CaptureMode) -> Evaluation {
    evaluate_with_limits(code, mode, EvalLimits::default())
}

pub fn eval_same_value(original: &str, transformed: &str) -> DiffResult {
    eval_same_value_with(original, transformed, &CaptureMode::default())
}
pub fn eval_same_value_with(original: &str, transformed: &str, mode: &CaptureMode) -> DiffResult {
    eval_same_value_with_limits(original, transformed, mode, EvalLimits::default())
}
pub fn eval_same_value_with_limits(
    original: &str,
    transformed: &str,
    mode: &CaptureMode,
    limits: EvalLimits,
) -> DiffResult {
    let a = evaluate_with_limits(original, mode, limits);
    let b = evaluate_with_limits(transformed, mode, limits);
    let equal = a.is_complete() && b.is_complete() && a == b;
    let reason = if !a.is_complete() || !b.is_complete() {
        "evaluation incomplete; equivalence unproven"
    } else if equal {
        "equal"
    } else {
        "observed values, exceptions, rejections or side effects differ"
    };
    DiffResult {
        equal,
        original: format!("{a:?}"),
        transformed: format!("{b:?}"),
        reason: reason.into(),
    }
}
pub fn assert_behaviorally_equal(original: &str, transformed: &str) {
    assert_behaviorally_equal_with(original, transformed, &CaptureMode::default())
}
pub fn assert_behaviorally_equal_with(original: &str, transformed: &str, mode: &CaptureMode) {
    let r = eval_same_value_with(original, transformed, mode);
    assert!(
        r.equal,
        "behavioral divergence: {}\n  original: {}\n  transformed: {}\n--- transformed source ---\n{}",
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
    #[test]
    fn type_tags_cannot_collide_with_user_strings() {
        for (a, b) in [
            ("true", "'true'"),
            ("false", "'false'"),
            ("1", "'\\u0000n:1'"),
            ("1n", "'\\u0000b:1'"),
            ("null", "'\\u0000null'"),
            ("undefined", "'\\u0000undefined'"),
            ("'\\ud800'", "'\\ufffd'"),
        ] {
            let r = eval_same_value(
                &format!("globalThis.__out={a}"),
                &format!("globalThis.__out={b}"),
            );
            assert!(r.is_divergent(), "{a} must differ from {b}: {r:?}");
        }
        assert_eq!(
            evaluate("globalThis.__out=true", &CaptureMode::sink()).outcome,
            Outcome::Value(r#"["boolean",true]"#.into())
        );
    }

    #[test]
    fn thrown_values_and_error_types_are_preserved() {
        for (a, b) in [
            ("1", "2"),
            ("1", "'1'"),
            ("null", "undefined"),
            ("new TypeError('x')", "new RangeError('x')"),
            ("new Error('x')", "'x'"),
        ] {
            assert!(eval_same_value(&format!("throw {a}"), &format!("throw {b}")).is_divergent());
        }
        assert_eq!(
            evaluate("throw 1", &CaptureMode::sink()).outcome,
            Outcome::Threw(r#"["number","1"]"#.into())
        );
    }

    #[test]
    fn async_results_are_read_after_settlement() {
        let a = "globalThis.__out='PENDING'; Promise.resolve().then(()=>globalThis.__out=1)";
        let b = "globalThis.__out='PENDING'; Promise.resolve().then(()=>globalThis.__out=2)";
        assert_eq!(
            evaluate(a, &CaptureMode::sink()).outcome,
            Outcome::Value(r#"["number","1"]"#.into())
        );
        assert!(eval_same_value(a, b).is_divergent());
    }

    #[test]
    fn unhandled_async_errors_are_observed_and_handled_ones_removed() {
        let bad = "globalThis.__out=1; Promise.resolve().then(()=>{throw new TypeError('async')})";
        let good = "globalThis.__out=1";
        let result = evaluate(bad, &CaptureMode::sink());
        assert_eq!(
            result.rejections,
            vec![r#"["error",["string","TypeError"],["string","async"]]"#]
        );
        assert!(eval_same_value(bad, good).is_divergent());
        let handled = "globalThis.__out=1; let p=Promise.reject(2); Promise.resolve().then(()=>p.catch(()=>{}))";
        assert!(eval_same_value(handled, good).equal);
    }

    #[test]
    fn trace_and_console_order_survive_throws() {
        let a = "globalThis.__trace=['before']; console.log(1); throw 5";
        let b = "globalThis.__trace=['after']; console.log(1); throw 5";
        assert!(eval_same_value(a, b).is_divergent());
        assert!(
            eval_same_value(
                "console.log(1);console.log(2);throw 5",
                "console.log(2);console.log(1);throw 5"
            )
            .is_divergent()
        );
        assert!(
            eval_same_value("console.log(true);throw 5", "console.log('true');throw 5")
                .is_divergent()
        );
        assert!(eval_same_value(a, a).equal);
    }

    #[test]
    fn objects_are_explicitly_unsupported_not_stringified() {
        let r = evaluate("globalThis.__out={x:1}", &CaptureMode::sink());
        assert_eq!(r.outcome, Outcome::Incomplete(Failure::UnsupportedCapture));
        assert!(!eval_same_value("globalThis.__out={x:1}", "globalThis.__out={x:1}").equal);
    }

    #[test]
    fn infinite_javascript_is_interrupted_and_never_equal() {
        let limits = EvalLimits {
            timeout: Duration::from_millis(20),
            ..EvalLimits::default()
        };
        let start = Instant::now();
        assert_eq!(
            evaluate_with_limits("while(true){}", &CaptureMode::sink(), limits).outcome,
            Outcome::Incomplete(Failure::Timeout)
        );
        assert!(
            !eval_same_value_with_limits(
                "while(true){}",
                "while(true){}",
                &CaptureMode::sink(),
                limits
            )
            .equal
        );
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn endlessly_queued_microtasks_hit_the_job_budget() {
        let limits = EvalLimits {
            max_jobs: 12,
            ..EvalLimits::default()
        };
        let code = "function again(){Promise.resolve().then(again)} again()";
        assert_eq!(
            evaluate_with_limits(code, &CaptureMode::sink(), limits).outcome,
            Outcome::Incomplete(Failure::JobLimit)
        );
        assert!(!eval_same_value_with_limits(code, code, &CaptureMode::sink(), limits).equal);
    }

    #[test]
    fn allocations_hit_the_runtime_memory_limit() {
        let limits = EvalLimits {
            memory_bytes: 2 * 1024 * 1024,
            ..EvalLimits::default()
        };
        let code = "const a=[]; for(let i=0;i<1000000;i++)a.push({i}); globalThis.__out=a.length";
        assert_eq!(
            evaluate_with_limits(code, &CaptureMode::sink(), limits).outcome,
            Outcome::Incomplete(Failure::MemoryLimit)
        );
    }

    #[test]
    fn user_prototype_hooks_cannot_forge_observations() {
        let prefix = "Array.prototype.toJSON=()=>''; Array.prototype.push=()=>{}; Object.prototype.toJSON=()=>''; ";
        assert!(
            eval_same_value(
                &format!("{prefix}globalThis.__out=true"),
                &format!("{prefix}globalThis.__out=false")
            )
            .is_divergent()
        );
        assert!(
            eval_same_value(
                &format!("{prefix}console.log(1)"),
                &format!("{prefix}console.log(2)")
            )
            .is_divergent()
        );
    }
    #[test]
    fn caught_memory_failure_still_invalidates_the_observation() {
        let limits = EvalLimits {
            memory_bytes: 2 * 1024 * 1024,
            ..EvalLimits::default()
        };
        let code =
            "try {const a=[]; for(let i=0;i<1000000;i++)a.push({i})}catch(e){} globalThis.__out=1";
        assert_eq!(
            evaluate_with_limits(code, &CaptureMode::sink(), limits).outcome,
            Outcome::Incomplete(Failure::MemoryLimit)
        );
    }

    #[test]
    fn scripts_are_sloppy_unless_the_source_requests_strict_mode() {
        let body = "function f(x){arguments[0]=7;return x} globalThis.__out=f(1)";
        assert_eq!(
            evaluate(body, &CaptureMode::sink()).outcome,
            Outcome::Value(r#"["number","7"]"#.into())
        );
        assert_eq!(
            evaluate(&format!("'use strict';{body}"), &CaptureMode::sink()).outcome,
            Outcome::Value(r#"["number","1"]"#.into())
        );
        assert_eq!(
            evaluate("with({x:42}){globalThis.__out=x}", &CaptureMode::sink()).outcome,
            Outcome::Value(r#"["number","42"]"#.into())
        );
    }
    #[test]
    fn initialization_and_stack_exhaustion_are_not_javascript_results() {
        let no_memory = EvalLimits {
            memory_bytes: 0,
            ..EvalLimits::default()
        };
        assert_eq!(
            evaluate_with_limits("1", &CaptureMode::completion(), no_memory).outcome,
            Outcome::Incomplete(Failure::MemoryLimit)
        );
        let recurse = "function recurse(){return recurse()} recurse()";
        assert_eq!(
            evaluate(recurse, &CaptureMode::completion()).outcome,
            Outcome::Incomplete(Failure::StackLimit)
        );
        assert!(!eval_same_value(recurse, recurse).equal);
    }
}
