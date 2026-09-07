"""Enforce rustfmt on changed files without reformatting unrelated legacy code."""
import os
from pathlib import Path
import subprocess
import sys

base = os.environ.get("BASE_SHA", "HEAD")
if not base or set(base) == {"0"}:
    base = "HEAD^"
if subprocess.run(["git", "rev-parse", "--verify", base], capture_output=True).returncode:
    base = "HEAD"
changed = subprocess.check_output(["git", "diff", "--name-only", "--diff-filter=ACMR", base, "--", "*.rs"], text=True)
paths = [name for name in changed.splitlines() if Path(name).is_file()]
failed = False
for path in paths:
    result = subprocess.run(["rustfmt", "--edition", "2024", "--check", "--config", "skip_children=true", path])
    failed |= result.returncode != 0
print(f"Checked formatting of {len(paths)} changed Rust files.")
sys.exit(1 if failed else 0)
