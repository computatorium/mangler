//! CLI end-to-end tests: argv → config → engine → io plumbing, exit codes, and
//! --verbose. The library entry point `run_args` covers file plumbing; the real
//! binary (CARGO_BIN_EXE_mangler) covers stdin→stdout and exit codes.

use std::io::Write;
use std::process::{Command, Stdio};

/// Path to the built `mangler` binary (cargo provides this for integration tests).
fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_mangler")
}

#[test]
fn stdin_to_stdout_with_lang() {
    let mut child = Command::new(bin())
        .args(["-", "--lang", "css", "--preset", "minify"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"  .a  {  color : red  }  ")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "exit: {:?}", out.status);
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains(".a"), "stdout: {stdout}");
    assert!(stdout.len() < 25, "not minified: {stdout}");
}

#[test]
fn stdin_without_lang_fails() {
    let mut child = Command::new(bin())
        .args(["-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b".a{}").unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(!out.status.success(), "missing --lang on stdin must fail");
}

#[test]
fn file_arg_to_output_file() {
    let dir = tempfile::tempdir().unwrap();
    let inp = dir.path().join("in.css");
    let outp = dir.path().join("out.css");
    std::fs::write(&inp, "/* c */ .a { color: #ffffff }").unwrap();

    let had_errors = mangler_cli::run_args([
        "mangler",
        inp.to_str().unwrap(),
        "-o",
        outp.to_str().unwrap(),
        "--preset",
        "minify",
    ])
    .unwrap();
    assert!(!had_errors);
    let written = std::fs::read_to_string(&outp).unwrap();
    assert!(!written.contains("/* c */"), "comment survived: {written}");
}

#[test]
fn in_place_overwrites_source() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("a.css");
    std::fs::write(&f, "/* x */ .a{ color: red }").unwrap();
    let had_errors =
        mangler_cli::run_args(["mangler", f.to_str().unwrap(), "--in-place", "--preset", "minify"])
            .unwrap();
    assert!(!had_errors);
    let after = std::fs::read_to_string(&f).unwrap();
    assert!(!after.contains("/* x */"), "in-place did not transform: {after}");
}

#[test]
fn parse_error_yields_nonzero_exit() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("bad.js");
    std::fs::write(&f, "function (").unwrap();
    // Aborts (no --keep-going) and reports an error → had_errors true.
    let had_errors = mangler_cli::run_args(["mangler", f.to_str().unwrap()]).unwrap();
    assert!(had_errors, "broken JS must flip the error flag");
}

#[test]
fn keep_going_processes_remaining_inputs() {
    let dir = tempfile::tempdir().unwrap();
    let good = dir.path().join("good.css");
    let bad = dir.path().join("bad.js");
    let outdir = dir.path().join("out");
    std::fs::create_dir_all(&outdir).unwrap();
    std::fs::write(&good, ".a{color:red}").unwrap();
    std::fs::write(&bad, "function (").unwrap();

    let had_errors = mangler_cli::run_args([
        "mangler",
        bad.to_str().unwrap(),
        good.to_str().unwrap(),
        "-o",
        outdir.to_str().unwrap(),
        "--keep-going",
        "--preset",
        "minify",
    ])
    .unwrap();
    assert!(had_errors, "the broken input still flips the flag");
    // The good input was still written despite the earlier failure.
    assert!(outdir.join("good.css").exists(), "good input skipped after error");
}

#[test]
fn multi_input_to_non_dir_output_is_fatal() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.css");
    let b = dir.path().join("b.css");
    std::fs::write(&a, ".a{}").unwrap();
    std::fs::write(&b, ".b{}").unwrap();
    let r = mangler_cli::run_args([
        "mangler",
        a.to_str().unwrap(),
        b.to_str().unwrap(),
        "-o",
        dir.path().join("single.css").to_str().unwrap(),
    ]);
    assert!(r.is_err(), "two inputs into one file must be rejected");
}

#[test]
fn verbose_emits_size_report_to_stderr() {
    let out = Command::new(bin())
        .args(["-", "--lang", "css", "--preset", "minify", "--verbose"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .and_then(|mut c| {
            c.stdin.take().unwrap().write_all(b".a{color:red}")?;
            c.wait_with_output()
        })
        .unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("[size]"), "no size report: {stderr}");
}
