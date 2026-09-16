#!/usr/bin/env python3
"""Exercise the same Wasm compiler and static VM in Node and a CSP browser."""
import argparse
import functools
import hashlib
import http.server
import json
import os
import pathlib
import subprocess
import tempfile
import threading
import signal
import time

ROOT = pathlib.Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--artifacts', type=pathlib.Path, default=ROOT / 'target' / 'eval-runtime')
parser.add_argument('--output', type=pathlib.Path, help='Verification report; defaults to the artifacts directory')
args = parser.parse_args()
ARTIFACTS = args.artifacts.resolve()
NODE = os.environ.get('MANGLER_TESTKIT_NODE')
CHROME = os.environ.get('MANGLER_TESTKIT_CHROME')
if not NODE or not CHROME:
    raise SystemExit('Set MANGLER_TESTKIT_NODE and MANGLER_TESTKIT_CHROME to the engine binaries.')

with tempfile.TemporaryDirectory(prefix='mangler-eval-check-') as work:
    work = pathlib.Path(work)
    wasm = (ARTIFACTS / 'mangler-eval.wasm').read_bytes()
    runtime = (ARTIFACTS / 'mangler-eval.js').read_text()
    smoke = (ARTIFACTS / 'smoke.js').read_text()
    node_source = runtime + '\n' + smoke + '\n' + "globalThis.manglerSmokeIntrinsicEval=globalThis.eval;globalThis.eval=()=>{throw new Error('eval disabled')};globalThis.Function=()=>{throw new Error('Function disabled')};var bytes=require('fs').readFileSync(process.argv[2]);var result=manglerCompilerSmoke(bytes);result.mappedArguments=manglerCompilerMappedSmoke(bytes);result.dynamicSource=manglerCompilerDynamicSmoke(bytes);result.intrinsicIsolation=manglerCompilerIntrinsicSmoke(bytes);console.log(JSON.stringify(result));\n"
    (work / 'node.cjs').write_text(node_source)
    node = subprocess.run([NODE, str(work / 'node.cjs'), str(ARTIFACTS / 'mangler-eval.wasm')], capture_output=True, text=True, timeout=60)
    if node.returncode:
        raise SystemExit('Node failed:\n' + node.stderr)
    print('Node ' + node.stdout.strip())
    node_result = json.loads(node.stdout)
    assert node_result['ok'] and not node_result['imports']
    assert all(node_result[group]['ok'] for group in ('mappedArguments', 'dynamicSource', 'intrinsicIsolation'))
    (work / 'mangler-eval.wasm').write_bytes(wasm)
    (work / 'mangler-eval.js').write_text(runtime)
    (work / 'smoke.js').write_text(smoke)
    (work / 'browser.js').write_text("fetch('mangler-eval.wasm').then(r=>r.arrayBuffer()).then(bytes=>{globalThis.manglerSmokeIntrinsicEval=globalThis.eval;globalThis.eval=()=>{throw Error('eval disabled')};globalThis.Function=()=>{throw Error('Function disabled')};return manglerCompilerSmoke(bytes)}).then(result=>{document.body.dataset.complete='1';document.getElementById('result').textContent=JSON.stringify(result)}).catch(error=>{document.body.dataset.complete='1';document.getElementById('result').textContent=JSON.stringify({ok:false,error:String(error),stack:error.stack})});")
    (work / 'index.html').write_text('<!doctype html><meta charset="utf-8"><pre id="result"></pre><script src="mangler-eval.js"></script><script src="smoke.js"></script><script src="browser.js"></script>')
    # Native argument shells and dynamically compiled class/suspension support
    # need the host's dynamic-code permission. Test that mode separately while
    # retaining the restrictive CSP guarantee for the plain bytecode path.
    (work / 'browser-full.js').write_text("fetch('mangler-eval.wasm').then(r=>r.arrayBuffer()).then(bytes=>{globalThis.manglerSmokeIntrinsicEval=globalThis.eval;globalThis.eval=()=>{throw Error('eval disabled')};globalThis.Function=()=>{throw Error('Function disabled')};const result=manglerCompilerSmoke(bytes);result.mappedArguments=manglerCompilerMappedSmoke(bytes);result.dynamicSource=manglerCompilerDynamicSmoke(bytes);result.intrinsicIsolation=manglerCompilerIntrinsicSmoke(bytes);return result}).then(result=>{document.body.dataset.complete='1';document.getElementById('result').textContent=JSON.stringify(result)}).catch(error=>{document.body.dataset.complete='1';document.getElementById('result').textContent=JSON.stringify({ok:false,error:String(error),stack:error.stack})});")
    (work / 'full.html').write_text((work / 'index.html').read_text().replace('browser.js', 'browser-full.js'))

    class Handler(http.server.SimpleHTTPRequestHandler):
        def end_headers(self):
            permission = "'unsafe-eval'" if self.path == '/full.html' else "'wasm-unsafe-eval'"
            self.send_header('Content-Security-Policy', f"default-src 'self'; script-src 'self' {permission}; object-src 'none'")
            super().end_headers()
        def log_message(self, *args):
            pass

    server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), functools.partial(Handler, directory=str(work)))
    threading.Thread(target=server.serve_forever, daemon=True).start()
    def check_browser(page, label):
        url = f'http://127.0.0.1:{server.server_port}/{page}'
        command = [CHROME, '--headless', '--disable-gpu', '--no-sandbox', '--disable-background-networking', '--disable-extensions', '--disable-background-timer-throttling', '--no-first-run', '--no-default-browser-check', f'--user-data-dir={work / ("profile-" + page)}', '--virtual-time-budget=3000', '--dump-dom', url]
        process = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE, stdin=subprocess.DEVNULL, start_new_session=True)
        stdout, stderr = bytearray(), bytearray()
        complete = threading.Event()
        def reader(stream, output, is_stdout):
            while chunk := stream.read1(8192):
                output.extend(chunk)
                if len(output) > 8 * 1024 * 1024:
                    complete.set()
                    return
                if is_stdout and b'data-complete="1"' in output and b'</html>' in output:
                    complete.set()
        threads = [threading.Thread(target=reader, args=(process.stdout, stdout, True)), threading.Thread(target=reader, args=(process.stderr, stderr, False))]
        for thread in threads: thread.start()
        deadline = time.monotonic() + 30
        while process.poll() is None and not complete.wait(.05) and time.monotonic() < deadline:
            pass
        if process.poll() is None:
            os.killpg(process.pid, signal.SIGKILL)
        process.wait()
        for thread in threads: thread.join(timeout=2)
        chrome = subprocess.CompletedProcess(command, process.returncode, stdout.decode(errors='replace'), stderr.decode(errors='replace'))
        import html
        import re
        found = re.search(r'<pre id="result">(.*?)</pre>', chrome.stdout, re.S)
        if not found:
            raise SystemExit('Chrome did not report a result:\n' + chrome.stdout + '\n' + chrome.stderr)
        browser_result = json.loads(html.unescape(found.group(1)))
        print(label + ' ' + json.dumps(browser_result))
        assert browser_result['ok'] and not browser_result['imports'], browser_result
        return browser_result

    try:
        browser_result = check_browser('index.html', 'Chrome CSP')
        browser_full = check_browser('full.html', 'Chrome full')
        assert all(browser_full[group]['ok'] for group in ('mappedArguments', 'dynamicSource', 'intrinsicIsolation'))
    finally:
        server.shutdown()
    (args.output or ARTIFACTS / 'verification.json').write_text(json.dumps({'node': node_result, 'chromeCsp': browser_result, 'chromeFull': browser_full, 'assetsSha256': {'wasm': hashlib.sha256(wasm).hexdigest(), 'javascript': hashlib.sha256(runtime.encode()).hexdigest()}}, indent=2)+'\n')
