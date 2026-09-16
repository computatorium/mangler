#!/usr/bin/env python3
"""Build and verify matched native CLI, interpreter, and Wasm compiler assets."""
import argparse
import gzip
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import tempfile

ROOT = Path(__file__).resolve().parents[1]


def source_hash():
    paths = {ROOT / 'Cargo.toml', ROOT / 'Cargo.lock'}
    for directory in ('crates', '.cargo', 'vendor'):
        paths.update(path for path in (ROOT / directory).rglob('*') if path.is_file())
    paths.update(ROOT.glob('rust-toolchain*'))
    digest = hashlib.sha256()
    for path in sorted(paths):
        digest.update(str(path.relative_to(ROOT)).encode() + b'\0')
        digest.update(hashlib.sha256(path.read_bytes()).digest())
    return digest.hexdigest()


def run(command, **kwargs):
    print('+ ' + ' '.join(map(str, command)), file=sys.stderr, flush=True)
    return subprocess.run(command, cwd=ROOT, check=True, **kwargs)


def cargo_artifacts(command, destination):
    with destination.open('w') as stream:
        run([*command, '--message-format=json-render-diagnostics'], stdout=stream)
    return [json.loads(line) for line in destination.read_text().splitlines() if line.strip()]


def executable(artifacts, name):
    paths = {Path(row['executable']) for row in artifacts
             if row.get('reason') == 'compiler-artifact'
             and row['target']['name'] == name and row.get('executable')}
    if len(paths) != 1:
        raise RuntimeError(f'Expected exactly one {name} executable; found {paths}')
    return paths.pop()


def wasm_artifact(artifacts):
    paths = {Path(filename) for row in artifacts
             if row.get('reason') == 'compiler-artifact' and row['target']['name'] == 'mangler_eval'
             for filename in row['filenames'] if filename.endswith('.wasm')}
    if len(paths) != 1:
        raise RuntimeError(f'Expected exactly one compiler Wasm artifact; found {paths}')
    return paths.pop()


def verify(stage, node):
    wasm = stage / 'mangler-eval.wasm'
    if wasm.stat().st_size > 8 * 1024 * 1024:
        raise RuntimeError('Compiler exceeds the 8 MiB synchronous browser compilation budget; use the wasm-release profile')
    if wasm.read_bytes()[:8] != b'\0asm\x01\0\0\0':
        raise RuntimeError('Compiler artifact is not a version-1 WebAssembly module')
    checked = run([node, '-e', "const fs=require('node:fs');const m=new WebAssembly.Module(fs.readFileSync(process.argv[1]));"
         "if(WebAssembly.Module.imports(m).length)throw Error('Compiler requires host imports');"
         "const e=new WebAssembly.Instance(m).exports;"
         "if(e.mangler_abi_version()!==1)throw Error('Compiler ABI mismatch');"
         "if(typeof e.mangler_compiler_fingerprint!=='function')throw Error('Compiler fingerprint missing; rebuild matched assets');"
         "process.stdout.write(String(e.mangler_compiler_fingerprint()));", str(wasm)], timeout=60, capture_output=True, text=True)
    compiler_fingerprint = f'{int(checked.stdout) & ((1 << 64) - 1):016x}'
    # Exercise the packaged executable's adjacent-asset lookup, with no override.
    environment = dict(os.environ)
    environment.pop('MANGLER_EVAL_WASM', None)
    source = 'function pay(x){return eval("x+2")}globalThis.__out=pay(5);'
    with tempfile.TemporaryDirectory(prefix='mangler-package-smoke-') as temporary:
        standalone = Path(temporary) / 'standalone.cjs'
        standalone.write_bytes((stage / 'mangler-eval.js').read_bytes() + b'\n' +
                               (stage / 'smoke.js').read_bytes() + b'\n' +
                               b"globalThis.manglerSmokeIntrinsicEval=globalThis.eval;const bytes=require('node:fs').readFileSync(process.argv[2]);"
                               b"const results=[manglerCompilerSmoke(bytes),manglerCompilerMappedSmoke(bytes),manglerCompilerIntrinsicSmoke(bytes),manglerCompilerDynamicSmoke(bytes)];"
                               b"if(results.some(result=>!result.ok))throw Error('Standalone runtime smoke failed');")
        run([node, str(standalone), str(wasm)], timeout=60)
        run([node, '--expose-gc', str(ROOT / 'scripts/check-eval-template-sites.cjs'), str(stage)], timeout=60)
        protected = Path(temporary) / 'protected.js'
        with protected.open('w') as stream:
            run([str(stage / 'mangler'), '-', '--lang', 'js', '--preset', 'minify',
                 '--seed', '42', '--require-virtualized', 'pay', '--verify'],
                input=source, text=True, stdout=stream, env=environment, timeout=120)
        run([node, '-e', "const fs=require('node:fs'),vm=require('node:vm');"
             "const c=vm.createContext({atob,btoa,TextEncoder,TextDecoder});"
             "new vm.Script(fs.readFileSync(process.argv[1],'utf8')).runInContext(c,{timeout:30000});"
             "if(c.__out!==7)throw Error('Packaged direct eval returned '+c.__out);", str(protected)], timeout=60)
    return compiler_fingerprint


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('destination', nargs='?', type=Path, help='Legacy positional output directory')
    parser.add_argument('--output', type=Path, help='Artifacts directory; defaults to target/eval-runtime')
    parser.add_argument('--profile', default='release', help='Native Cargo build profile')
    parser.add_argument('--wasm-profile', default='wasm-release', help='Wasm Cargo build profile; defaults to size-optimized wasm-release')
    parser.add_argument('--node', default=os.environ.get('MANGLER_TESTKIT_NODE') or shutil.which('node'),
                        help='Node executable for mandatory packaged-asset verification')
    parser.add_argument('--package', action='store_true', help='Also create a portable host archive')
    parser.add_argument('--archive', type=Path, help='Archive path; implies --package')
    args = parser.parse_args()
    if args.output and args.destination:
        parser.error('Use either --output or the positional output directory')
    if not args.node:
        parser.error('Node is required for package verification; supply --node or MANGLER_TESTKIT_NODE')
    package = args.package or args.archive is not None
    output = (args.output or args.destination or ROOT / 'target' / ('distribution/mangler' if package else 'eval-runtime')).resolve()
    if output == ROOT or output in ROOT.parents or output == ROOT / 'crates' or ROOT / 'crates' in output.parents:
        parser.error('Output must not overwrite the checkout or its source tree')
    if output.exists() and (not output.is_dir() or (any(output.iterdir()) and not any((output / name).is_file() for name in ('manifest.json', 'sizes.json')))):
        parser.error(f'Refusing to replace non-artifact directory: {output}')
    archive = (args.archive or Path(str(output) + '.tar.gz')).resolve() if package else None
    if archive and (archive == output or output in archive.parents):
        parser.error('Archive must be outside the artifacts directory')
    output.parent.mkdir(parents=True, exist_ok=True)
    before = source_hash()
    with tempfile.TemporaryDirectory(prefix='.' + output.name + '-build-', dir=output.parent) as temporary:
        work = Path(temporary)
        stage = work / 'artifacts'
        stage.mkdir()
        # One host workspace feature graph generates both native executables.
        native = cargo_artifacts(['cargo', 'build', '--locked', '--workspace', '--bins', '--profile', args.profile], work / 'native.jsonl')
        cli = executable(native, 'mangler')
        interpreter = executable(native, 'mangler-eval-runtime')
        shutil.copy2(cli, stage / 'mangler')
        with (stage / 'interpreter.js').open('w') as stream:
            run([str(interpreter)], stdout=stream, timeout=60)
        wasm = cargo_artifacts(['cargo', 'build', '--locked', '-p', 'mangler-eval', '--lib',
                                '--profile', args.wasm_profile, '--target', 'wasm32-unknown-unknown'], work / 'wasm.jsonl')
        shutil.copy2(wasm_artifact(wasm), stage / 'mangler-eval.wasm')
        host = run([str(interpreter), '--host'], capture_output=True, timeout=60).stdout
        (stage / 'mangler-eval.js').write_bytes((stage / 'interpreter.js').read_bytes() + b'\n' + host)
        shutil.copy2(ROOT / 'crates/mangler-eval/host/smoke.js', stage / 'smoke.js')
        shutil.copy2(ROOT / 'vendor/LICENSE-SWC', stage / 'LICENSE-SWC')
        shutil.copy2(ROOT / 'vendor/README.md', stage / 'SWC-PATCHES.md')
        if source_hash() != before:
            raise RuntimeError('Compiler inputs changed during the build; rerun from a stable checkout')
        compiler_fingerprint = verify(stage, args.node)
        if source_hash() != before:
            raise RuntimeError('Compiler inputs changed during verification; package was not published')
        measure = {}
        for name in ('mangler', 'interpreter.js', 'mangler-eval.wasm', 'mangler-eval.js',
                     'smoke.js', 'LICENSE-SWC', 'SWC-PATCHES.md'):
            data = (stage / name).read_bytes()
            measure[name] = {'bytes': len(data), 'sha256': hashlib.sha256(data).hexdigest()}
            if name in ('mangler-eval.wasm', 'mangler-eval.js'):
                packed = gzip.compress(data, compresslevel=9, mtime=0)
                (stage / (name + '.gz')).write_bytes(packed)
                measure[name]['gzipBytes'] = len(packed)
        try:
            revision = run(['git', 'rev-parse', 'HEAD'], capture_output=True, text=True).stdout.strip()
        except (FileNotFoundError, subprocess.CalledProcessError):
            revision = None  # Source archives still have the full input fingerprint.
        rustc = run(['rustc', '-vV'], capture_output=True, text=True).stdout.strip()
        manifest = {'schema': 1, 'git_revision': revision, 'source_sha256': before,
                    'compiler_fingerprint': compiler_fingerprint,
                    'profile': args.profile, 'wasm_profile': args.wasm_profile, 'rustc': rustc, 'verification': {'wasm_imports': 0, 'wasm_browser_size_budget': 8 * 1024 * 1024, 'compiler_abi': 1, 'standalone_smoke': 'pass', 'eval_template_identity_and_lifetime': 'pass', 'packaged_direct_eval': 'pass'},
                    'files': measure}
        (stage / 'manifest.json').write_text(json.dumps(manifest, indent=2) + '\n')
        (stage / 'sizes.json').write_text(json.dumps(measure, indent=2) + '\n')
        # All outputs are staged; a failed build leaves the previous set intact.
        previous = work / 'previous'
        if output.exists():
            output.rename(previous)
        try:
            stage.rename(output)
        except BaseException:
            if previous.exists():
                previous.rename(output)
            raise
    if archive:
        archive.parent.mkdir(parents=True, exist_ok=True)
        with tempfile.NamedTemporaryFile(prefix='.' + archive.name, dir=archive.parent, delete=False) as stream:
            temporary_archive = Path(stream.name)
            try:
                with gzip.GzipFile(fileobj=stream, mode='wb', mtime=0, filename='') as compressed:
                    with tarfile.open(fileobj=compressed, mode='w') as bundle:
                        for name in sorted(path.name for path in output.iterdir() if path.is_file()):
                            entry = bundle.gettarinfo(str(output / name), arcname='mangler/' + name)
                            entry.uid = entry.gid = entry.mtime = 0
                            entry.uname = entry.gname = ''
                            entry.mode = 0o755 if name == 'mangler' else 0o644
                            with (output / name).open('rb') as content:
                                bundle.addfile(entry, content)
                stream.flush()
                temporary_archive.replace(archive)
            finally:
                temporary_archive.unlink(missing_ok=True)
    print(json.dumps({'artifacts': str(output), 'archive': str(archive) if archive else None, 'files': measure}, indent=2))


if __name__ == '__main__':
    try:
        main()
    except (OSError, RuntimeError, subprocess.SubprocessError) as error:
        raise SystemExit(str(error)) from error
