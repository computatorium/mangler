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
    let had_errors = mangler_cli::run_args([
        "mangler",
        f.to_str().unwrap(),
        "--in-place",
        "--preset",
        "minify",
    ])
    .unwrap();
    assert!(!had_errors);
    let after = std::fs::read_to_string(&f).unwrap();
    assert!(
        !after.contains("/* x */"),
        "in-place did not transform: {after}"
    );
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
    assert!(
        outdir.join("good.css").exists(),
        "good input skipped after error"
    );
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

#[test]
fn duplicate_output_names_fail_before_replacing_any_output() {
    let dir = tempfile::tempdir().unwrap();
    for sub in ["a", "b", "out"] {
        std::fs::create_dir(dir.path().join(sub)).unwrap();
    }
    std::fs::write(dir.path().join("a/x.js"), "console.log('first');").unwrap();
    std::fs::write(dir.path().join("b/x.js"), "console.log('second');").unwrap();
    let destination = dir.path().join("out/x.js");
    std::fs::write(&destination, "previous build").unwrap();
    let out = Command::new(bin())
        .current_dir(dir.path())
        .args([
            "a/x.js",
            "b/x.js",
            "-o",
            "out",
            "--preset",
            "minify",
            "--keep-going",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("output collision"));
    assert_eq!(
        std::fs::read_to_string(destination).unwrap(),
        "previous build"
    );
    assert_eq!(
        std::fs::read_dir(dir.path().join("out")).unwrap().count(),
        1
    );
}

#[test]
fn existing_literal_bracket_filename_wins_over_glob() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("[id] route.js"), "console.log('wanted');").unwrap();
    std::fs::write(dir.path().join("i route.js"), "console.log('wrong');").unwrap();
    let out = Command::new(bin())
        .current_dir(dir.path())
        .args(["[id] route.js", "--preset", "minify"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let code = String::from_utf8(out.stdout).unwrap();
    assert!(code.contains("wanted"), "{code}");
    assert!(!code.contains("wrong"), "{code}");
}

#[test]
fn unmatched_glob_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let out = Command::new(bin())
        .current_dir(dir.path())
        .args(["missing/**/*.js", "--preset", "minify"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("matched no files"));
    assert!(out.stdout.is_empty());
}

#[test]
fn nested_output_directory_is_excluded_on_repeated_runs() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("tree/dist")).unwrap();
    std::fs::write(dir.path().join("tree/source.js"), "console.log('source');").unwrap();
    for _ in 0..3 {
        let out = Command::new(bin())
            .current_dir(dir.path())
            .args(["tree", "-o", "tree/dist", "--preset", "minify"])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    assert!(dir.path().join("tree/dist/source.js").is_file());
    assert!(!dir.path().join("tree/dist/dist").exists());
}

#[test]
fn keep_going_handles_discovery_read_and_transform_failures() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("out")).unwrap();
    std::fs::write(dir.path().join("invalid_utf8.js"), [0xff]).unwrap();
    std::fs::write(dir.path().join("syntax.js"), "function (").unwrap();
    std::fs::write(dir.path().join("good.js"), "console.log('good');").unwrap();
    let out = Command::new(bin())
        .current_dir(dir.path())
        .args([
            "missing.js",
            "invalid_utf8.js",
            "syntax.js",
            "good.js",
            "-o",
            "out",
            "--keep-going",
            "--preset",
            "minify",
            "--jobs",
            "2",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let errors = String::from_utf8_lossy(&out.stderr);
    for name in ["missing.js", "invalid_utf8.js", "syntax.js"] {
        assert!(errors.contains(name), "missing {name}: {errors}");
    }
    assert!(errors.find("missing.js") < errors.find("invalid_utf8.js"));
    assert!(errors.find("invalid_utf8.js") < errors.find("syntax.js"));
    assert!(
        std::fs::read_to_string(dir.path().join("out/good.js"))
            .unwrap()
            .contains("good")
    );
    assert_eq!(
        std::fs::read_dir(dir.path().join("out")).unwrap().count(),
        1
    );
}

#[test]
fn missing_input_without_keep_going_does_not_write_later_inputs() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("out")).unwrap();
    std::fs::write(dir.path().join("good.js"), "console.log('good');").unwrap();
    let out = Command::new(bin())
        .current_dir(dir.path())
        .args(["missing.js", "good.js", "-o", "out", "--preset", "minify"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert_eq!(
        std::fs::read_dir(dir.path().join("out")).unwrap().count(),
        0
    );
}

#[test]
fn multiple_stdout_inputs_are_rejected_before_emission() {
    let dir = tempfile::tempdir().unwrap();
    for name in ["a.js", "b.js"] {
        std::fs::write(dir.path().join(name), "console.log('hello');").unwrap();
    }
    let out = Command::new(bin())
        .current_dir(dir.path())
        .args(["a.js", "b.js", "--preset", "minify"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("stdout accepts one input"));
}

#[test]
#[cfg(unix)]
fn in_place_follows_file_symlink_and_preserves_executable_permissions() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source.js");
    let alias = dir.path().join("alias.js");
    std::fs::write(&source, "// comment\nconsole.log('hello');").unwrap();
    std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o751)).unwrap();
    symlink(&source, &alias).unwrap();
    let out = Command::new(bin())
        .arg(&alias)
        .args(["--in-place", "--preset", "minify"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        std::fs::symlink_metadata(&alias)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(
        !std::fs::read_to_string(&source)
            .unwrap()
            .contains("comment")
    );
    assert_eq!(
        std::fs::metadata(source).unwrap().permissions().mode() & 0o777,
        0o751
    );
}

#[test]
#[cfg(unix)]
fn directory_and_symlink_alias_inputs_are_deduplicated() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::create_dir(dir.path().join("out")).unwrap();
    let file = dir.path().join("src/only.js");
    std::fs::write(&file, "console.log('hello');").unwrap();
    symlink(&file, dir.path().join("alias.js")).unwrap();
    let out = Command::new(bin())
        .current_dir(dir.path())
        .args([
            "src",
            "src/only.js",
            "alias.js",
            "-o",
            "out",
            "--preset",
            "minify",
            "--verbose",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stderr)
            .matches("[size]")
            .count(),
        1
    );
    assert!(dir.path().join("out/only.js").exists());
    assert!(!dir.path().join("out/alias.js").exists());
}

#[test]
fn zero_jobs_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.js"), "console.log(1);").unwrap();
    let out = Command::new(bin())
        .current_dir(dir.path())
        .args(["a.js", "--jobs", "0"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("--jobs must be at least 1"));
}

#[test]
fn case_aliased_output_names_fail_on_case_insensitive_filesystems() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("CaseProbe"), "").unwrap();
    if !dir.path().join("caseprobe").exists() {
        return;
    }
    for sub in ["a", "b", "out"] {
        std::fs::create_dir(dir.path().join(sub)).unwrap();
    }
    std::fs::write(dir.path().join("a/Foo.js"), "console.log('first');").unwrap();
    std::fs::write(dir.path().join("b/foo.js"), "console.log('second');").unwrap();
    let out = Command::new(bin())
        .current_dir(dir.path())
        .args(["a/Foo.js", "b/foo.js", "-o", "out", "--preset", "minify"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("output collision"));
    assert_eq!(
        std::fs::read_dir(dir.path().join("out")).unwrap().count(),
        0
    );
}

#[test]
fn write_failure_preserves_destination_and_obeys_keep_going() {
    for keep_going in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("out/a.js")).unwrap();
        std::fs::write(dir.path().join("out/a.js/sentinel"), "keep").unwrap();
        for name in ["a.js", "b.js"] {
            std::fs::write(dir.path().join(name), "console.log('hello');").unwrap();
        }
        let mut command = Command::new(bin());
        command.current_dir(dir.path()).args([
            "a.js", "b.js", "-o", "out", "--preset", "minify", "--jobs", "2",
        ]);
        if keep_going {
            command.arg("--keep-going");
        }
        let out = command.output().unwrap();
        assert!(!out.status.success());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("out/a.js/sentinel")).unwrap(),
            "keep"
        );
        assert_eq!(dir.path().join("out/b.js").exists(), keep_going);
        assert_eq!(
            std::fs::read_dir(dir.path().join("out")).unwrap().count(),
            if keep_going { 2 } else { 1 }
        );
    }
}
