//! Optional second-engine differential path (cargo feature `cross-engine`, OFF by
//! default).
//!
//! rquickjs is the PRIMARY engine ([`crate::eval`]). This module is the documented
//! *seam* where a second JS engine plugs in to widen the differential — e.g. running
//! the same program through a V8/SpiderMonkey/`node` subprocess and comparing its
//! captured sink value against QuickJS's.
//!
//! # HARD RULE: never run bare `node` by default
//! The repo has a standing rule (see crate-root rustdoc): tests must NEVER invoke a
//! bare `node` binary. `node` may not exist on the machine, may be the wrong version,
//! or may hang — any of which turns a correctness test into a flaky/blocking one. So:
//! * this whole module is behind `#[cfg(feature = "cross-engine")]` and the feature
//!   is OFF by default;
//! * even with the feature on, [`available`] probes for an explicitly-configured
//!   engine binary via the `MANGLER_TESTKIT_JS_ENGINE` env var and returns `false`
//!   (skipping, never failing) when it is unset or the probe fails. There is no
//!   implicit fallback to `node`.
//!
//! A consumer that genuinely wants V8 coverage sets `MANGLER_TESTKIT_JS_ENGINE` to an
//! engine command that reads a JS file path as `$1`, evaluates it, and prints the
//! value of `globalThis.__out` to stdout. The harness then compares that stdout
//! against QuickJS's sink value.

#![cfg(feature = "cross-engine")]

use std::path::Path;
use std::process::Command;

/// Env var naming the external engine command. Its value is run as
/// `<engine> <tmp_js_file>`, and must print `String(globalThis.__out)` to stdout.
pub const ENGINE_ENV: &str = "MANGLER_TESTKIT_JS_ENGINE";

/// Whether a second engine is configured AND responds to a trivial probe. Returns
/// `false` (never panics, never falls back to bare `node`) when `MANGLER_TESTKIT_JS_ENGINE`
/// is unset or the probe fails — so cross-engine tests SKIP rather than break on a
/// machine without the engine.
pub fn available() -> bool {
    let engine = match std::env::var(ENGINE_ENV) {
        Ok(v) if !v.is_empty() => v,
        _ => return false,
    };
    // Probe: a file printing a known sentinel.
    let dir = std::env::temp_dir();
    let probe = dir.join(format!("mangler_testkit_probe_{}.js", std::process::id()));
    if std::fs::write(&probe, "globalThis.__out = 'PROBE_OK'; print(String(globalThis.__out));").is_err()
    {
        return false;
    }
    let out = run_engine(&engine, &probe);
    let _ = std::fs::remove_file(&probe);
    matches!(out, Ok(s) if s.trim() == "PROBE_OK")
}

/// Run the configured engine over `js_path`, returning its trimmed stdout.
fn run_engine(engine: &str, js_path: &Path) -> std::io::Result<String> {
    // Split the engine command on whitespace so callers can pass args (e.g.
    // "d8 --some-flag"). The script path is appended as the final argument.
    let mut parts = engine.split_whitespace();
    let bin = parts
        .next()
        .ok_or_else(|| std::io::Error::other("empty engine command"))?;
    let mut cmd = Command::new(bin);
    for a in parts {
        cmd.arg(a);
    }
    cmd.arg(js_path);
    let output = cmd.output()?;
    if !output.status.success() {
        return Err(std::io::Error::other(format!(
            "engine exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Evaluate `program` in the configured second engine and return `String(globalThis.__out)`.
/// The program must set `globalThis.__out`; this helper appends a print of it. Returns
/// `Ok(None)` if no engine is configured (so the caller skips).
pub fn eval_sink(program: &str) -> std::io::Result<Option<String>> {
    let engine = match std::env::var(ENGINE_ENV) {
        Ok(v) if !v.is_empty() => v,
        _ => return Ok(None),
    };
    let dir = std::env::temp_dir();
    let path = dir.join(format!(
        "mangler_testkit_xeng_{}_{}.js",
        std::process::id(),
        // cheap per-call uniqueness
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let full = format!("{program}\nprint(String(globalThis.__out));\n");
    std::fs::write(&path, full)?;
    let out = run_engine(&engine, &path);
    let _ = std::fs::remove_file(&path);
    out.map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_engine_is_unavailable_and_skips() {
        // With the env var unset, `available` must be false (not panic) and
        // `eval_sink` returns Ok(None) — proving the no-bare-node skip path.
        // SAFETY: single-threaded test; we restore by removing the var.
        unsafe { std::env::remove_var(ENGINE_ENV) };
        assert!(!available());
        assert!(eval_sink("globalThis.__out = 1;").unwrap().is_none());
    }
}
