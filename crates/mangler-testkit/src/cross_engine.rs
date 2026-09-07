//! Explicit, bounded real-engine execution. CI requires both engines; local tests
//! may omit unconfigured engines. No implicit `node` or Chrome PATH lookup.

use std::io::{self, Read};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

pub const NODE_ENV: &str = "MANGLER_TESTKIT_NODE";
pub const CHROME_ENV: &str = "MANGLER_TESTKIT_CHROME";
pub const REQUIRE_ENV: &str = "MANGLER_TESTKIT_REQUIRE_ENGINES";

fn configured_path(name: &str) -> Option<PathBuf> {
    match std::env::var_os(name).filter(|v| !v.is_empty()) {
        Some(path) => {
            let path = PathBuf::from(path);
            assert!(
                path.is_absolute() && path.is_file(),
                "{name} must name an existing absolute engine path: {}",
                path.display()
            );
            Some(path)
        }
        None => {
            assert!(
                std::env::var_os(REQUIRE_ENV).is_none(),
                "{REQUIRE_ENV} requires {name}"
            );
            eprintln!("[engine skip] {name} is unset; CI sets {REQUIRE_ENV}");
            None
        }
    }
}
pub fn node_path() -> Option<PathBuf> {
    configured_path(NODE_ENV)
}
pub fn chrome_path() -> Option<PathBuf> {
    configured_path(CHROME_ENV)
}

/// Capture at most 8 MiB per stream while continuously draining both pipes. The
/// child (and its process group on Unix) is killed/reaped on timeout or overflow.
/// Callers pass paths/arguments structurally and must not rely on an inherited
/// stdin. Every spawned engine in this crate goes through this function.
pub fn run_bounded(command: &mut Command, timeout: Duration) -> io::Result<Output> {
    run_process(command, timeout, None).map(|(output, _)| output)
}

// Chrome is a long-running host on some platforms even after --dump-dom has
// printed a complete result. Stop our browser as soon as the page explicitly
// reports completion; preserve its actual exit status rather than inventing one.
fn run_process(
    command: &mut Command,
    timeout: Duration,
    completion: Option<&'static [u8]>,
) -> io::Result<(Output, bool)> {
    const OUTPUT_LIMIT: usize = 8 * 1024 * 1024;
    let started = Instant::now();
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn()?;
    let overflow = Arc::new(AtomicBool::new(false));
    let completed = Arc::new(AtomicBool::new(false));
    fn reader(
        mut stream: impl Read + Send + 'static,
        overflow: Arc<AtomicBool>,
        signal: Option<(&'static [u8], Arc<AtomicBool>)>,
    ) -> std::sync::mpsc::Receiver<io::Result<Vec<u8>>> {
        let (send, receive) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let result = (|| {
                let mut out = Vec::new();
                let mut buf = [0u8; 8192];
                loop {
                    let n = stream.read(&mut buf)?;
                    if n == 0 {
                        break;
                    }
                    let take = n.min(OUTPUT_LIMIT.saturating_sub(out.len()));
                    let previous_len = out.len();
                    out.extend_from_slice(&buf[..take]);
                    if let Some((marker, flag)) = &signal {
                        let start = previous_len.saturating_sub(marker.len().saturating_sub(1));
                        if out[start..]
                            .windows(marker.len())
                            .any(|window| window == *marker)
                        {
                            flag.store(true, Ordering::Relaxed);
                        }
                    }
                    if take != n {
                        overflow.store(true, Ordering::Relaxed);
                    }
                }
                Ok(out)
            })();
            let _ = send.send(result);
        });
        receive
    }
    let stdout = reader(
        child.stdout.take().expect("piped stdout"),
        overflow.clone(),
        completion.map(|marker| (marker, completed.clone())),
    );
    let stderr = reader(
        child.stderr.take().expect("piped stderr"),
        overflow.clone(),
        None,
    );
    let status = loop {
        if completed.load(Ordering::Relaxed) {
            break Ok(None);
        }
        match child.try_wait() {
            Ok(Some(status)) => break Ok(Some(status)),
            Err(error) => break Err(error),
            Ok(None) => {}
        }
        if started.elapsed() >= timeout || overflow.load(Ordering::Relaxed) {
            break Err(io::Error::new(
                io::ErrorKind::TimedOut,
                if overflow.load(Ordering::Relaxed) {
                    "engine output limit exceeded"
                } else {
                    "engine deadline exceeded"
                },
            ));
        }
        thread::sleep(Duration::from_millis(5));
    };
    #[cfg(unix)]
    // SAFETY: this child was placed in its own process group at spawn. Kill any
    // surviving children too, even when their immediate parent already exited.
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    if status.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    let status = match status? {
        Some(status) => status,
        None => {
            let _ = child.kill();
            child.wait()?
        }
    };
    // A descendant can escape the process group while retaining a pipe. Never
    // join an unbounded reader: the SAME deadline also covers draining output.
    let receive = |rx: std::sync::mpsc::Receiver<io::Result<Vec<u8>>>| {
        rx.recv_timeout(timeout.saturating_sub(started.elapsed()))
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "engine output did not close before deadline",
                )
            })?
    };
    let stdout = receive(stdout)?;
    let stderr = receive(stderr)?;
    if overflow.load(Ordering::Relaxed) {
        return Err(io::Error::other("engine output limit exceeded"));
    }
    Ok((
        Output {
            status,
            stdout,
            stderr,
        },
        completed.load(Ordering::Relaxed),
    ))
}

#[derive(Clone, Debug)]
pub enum Engine {
    Node(PathBuf),
    Chrome(PathBuf),
}
impl Engine {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Node(_) => "Node/V8",
            Self::Chrome(_) => "Chrome",
        }
    }
}

/// Execute a batch in fresh realms while amortizing browser/process startup.
/// Chrome loads each artifact through its own real external script tag.
pub fn evaluate_many(engine: &Engine, programs: &[&str]) -> io::Result<Vec<serde_json::Value>> {
    let dir = tempfile::tempdir()?;
    let mut files = Vec::new();
    for (i, source) in programs.iter().enumerate() {
        let name = format!("artifact-{i}.js");
        std::fs::write(dir.path().join(&name), source)?;
        files.push(name);
    }
    std::fs::write(dir.path().join("observer.js"), crate::eval::OBSERVER_SOURCE)?;
    let (output, complete_browser_report) = match engine {
        Engine::Node(binary) => {
            std::fs::write(
                dir.path().join("manifest.json"),
                serde_json::to_vec(&files)?,
            )?;
            std::fs::write(
                dir.path().join("runner.cjs"),
                include_str!("node_batch.cjs"),
            )?;
            (
                run_bounded(
                    Command::new(binary)
                        .arg("--max-old-space-size=256")
                        .arg(dir.path().join("runner.cjs"))
                        .arg(dir.path()),
                    Duration::from_secs(45),
                )?,
                false,
            )
        }
        Engine::Chrome(binary) => {
            let setup = format!(
                r#"globalThis.__manglerObserver = {};
                globalThis.__manglerFailure = null;
                globalThis.__manglerRejections = new Map();
                addEventListener('error', e => {{ __manglerFailure = ['throw', __manglerObserver.thrown(e.error)]; }});
                addEventListener('unhandledrejection', e => {{ __manglerRejections.set(e.promise, e.reason); }});
                addEventListener('rejectionhandled', e => {{ __manglerRejections.delete(e.promise); }});"#,
                crate::eval::OBSERVER_SOURCE
            );
            std::fs::write(dir.path().join("setup.js"), setup)?;
            for i in 0..programs.len() {
                let finish = format!(
                    r#"setTimeout(function() {{
                    try {{
                        const report = {{ outcome: __manglerFailure || ['value', __manglerObserver.value(globalThis.__out)],
                            trace: __manglerObserver.trace(), rejections: Array.from(__manglerRejections.values(), e => __manglerObserver.thrown(e)) }};
                        parent.postMessage({{kind:'mangler-result', i:{i}, report}}, '*');
                    }} catch(e) {{ parent.postMessage({{kind:'mangler-result', i:{i}, report:{{incomplete:String(e)}}}}, '*'); }}
                }}, 0);"#
                );
                std::fs::write(dir.path().join(format!("finish-{i}.js")), finish)?;
                std::fs::write(
                    dir.path().join(format!("frame-{i}.html")),
                    format!(
                        "<!doctype html><meta charset=utf-8><script src=setup.js></script><script src=artifact-{i}.js></script><script src=finish-{i}.js></script>"
                    ),
                )?;
            }
            let mut page = format!(
                r#"<!doctype html><meta charset=utf-8><pre id=results>WAITING</pre><script>
                const reports = new Array({}); let count=0;
                addEventListener('message', e => {{
                    if (!e.data || e.data.kind !== 'mangler-result' || reports[e.data.i]) return;
                    reports[e.data.i] = e.data.report;
                    if (++count === reports.length) {{
                        document.getElementById('results').textContent = encodeURIComponent(JSON.stringify(reports));
                        document.body.appendChild(document.createComment('mangler-'+'complete'));
                    }}
                }});
                </script>"#,
                programs.len()
            );
            for i in 0..programs.len() {
                page.push_str(&format!("<iframe src=frame-{i}.html></iframe>"));
            }
            let page_path = dir.path().join("index.html");
            std::fs::write(&page_path, page)?;
            run_process(
                Command::new(binary)
                    .arg("--headless")
                    .arg("--disable-gpu")
                    .arg("--no-sandbox")
                    .arg("--disable-background-networking")
                    .arg("--disable-extensions")
                    .arg("--disable-background-timer-throttling")
                    .arg("--allow-file-access-from-files")
                    .arg("--no-first-run")
                    .arg(format!(
                        "--user-data-dir={}",
                        dir.path().join("profile").display()
                    ))
                    .arg("--virtual-time-budget=3000")
                    .arg("--dump-dom")
                    .arg(format!("file://{}", page_path.display())),
                Duration::from_secs(30),
                Some(b"<!--mangler-complete-->"),
            )?
        }
    };
    if !output.status.success() && !complete_browser_report {
        return Err(io::Error::other(format!(
            "{} failed: {}",
            engine.name(),
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    let text = String::from_utf8(output.stdout).map_err(io::Error::other)?;
    let json =
        match engine {
            Engine::Node(_) => text,
            Engine::Chrome(_) => {
                let encoded = text
                    .split("<pre id=\"results\">")
                    .nth(1)
                    .and_then(|s| s.split("</pre>").next())
                    .ok_or_else(|| {
                        io::Error::other(format!("Chrome did not return an observation: {text}"))
                    })?;
                let mut bytes = Vec::new();
                let mut chars = encoded.as_bytes().iter().copied();
                while let Some(b) = chars.next() {
                    if b == b'%' {
                        let hi = chars.next().and_then(|c| (c as char).to_digit(16));
                        let lo = chars.next().and_then(|c| (c as char).to_digit(16));
                        bytes.push(hi.zip(lo).map(|(h, l)| (h * 16 + l) as u8).ok_or_else(
                            || io::Error::other("invalid browser observation encoding"),
                        )?);
                    } else {
                        bytes.push(b);
                    }
                }
                String::from_utf8(bytes).map_err(io::Error::other)?
            }
        };
    let reports: Vec<serde_json::Value> = serde_json::from_str(&json).map_err(|e| {
        io::Error::other(format!(
            "{} observation incomplete: {e}; {json}",
            engine.name()
        ))
    })?;
    if reports.len() != programs.len()
        || reports
            .iter()
            .any(|r| r.get("incomplete").is_some() || r.get("outcome").is_none())
    {
        return Err(io::Error::other(format!(
            "{} returned incomplete observations: {reports:?}",
            engine.name()
        )));
    }
    Ok(reports)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn configured_node_has_a_real_deadline() {
        let Some(node) = node_path() else {
            return;
        };
        let error = run_bounded(
            Command::new(node).arg("-e").arg("while(true){}"),
            Duration::from_millis(100),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }
    #[test]
    fn real_node_encoder_distinguishes_async_and_primitive_values() {
        let Some(node) = node_path() else {
            return;
        };
        let values = evaluate_many(
            &Engine::Node(node),
            &[
                "globalThis.__out=true",
                "globalThis.__out='true'",
                "Promise.resolve().then(()=>globalThis.__out=42)",
            ],
        )
        .unwrap();
        assert_ne!(values[0], values[1]);
        assert_eq!(values[2]["outcome"][1], r#"["number","42"]"#);
    }
    #[test]
    fn inherited_pipes_cannot_outlive_the_deadline() {
        let Some(node) = node_path() else {
            return;
        };
        let started = Instant::now();
        let error = run_bounded(Command::new(node).arg("-e").arg(
            "require('node:child_process').spawn(process.execPath,['-e','setTimeout(()=>{},500)'],{detached:true,stdio:'inherit'}).unref()"), Duration::from_millis(100)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(1));
    }
    #[test]
    fn excessive_engine_output_is_rejected() {
        let Some(node) = node_path() else {
            return;
        };
        let error = run_bounded(
            Command::new(node)
                .arg("-e")
                .arg("process.stdout.write('x'.repeat(9*1024*1024))"),
            Duration::from_secs(2),
        )
        .unwrap_err();
        assert!(error.to_string().contains("output limit"));
    }
}
