#!/usr/bin/env python3
"""Print a Cargo path-patch config for local review of the paired Reth changes.

Usage: python3 scripts/perf-diag-local-config.py ../reth-perf-diag > /tmp/reth-local.toml
       cargo check --lib --config /tmp/reth-local.toml
Cargo will adjust Cargo.lock for local paths; retain the original lock for a published patch.
"""
import argparse
import json
from pathlib import Path
import re
import subprocess

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("reth_directory", type=Path)
args = parser.parse_args()
root = args.reth_directory.resolve()
bsc = Path(__file__).resolve().parents[1]
lock = subprocess.check_output(["git", "show", "HEAD:Cargo.lock"], cwd=bsc, text=True)
required = set()
for package in lock.split("[[package]]"):
    if re.search(r'^source = "git\+https://github.com/bnb-chain/reth\.git\?', package, re.M):
        required.add(re.search(r'^name = "([^"]+)"', package, re.M).group(1))
paths = {}
for directory in [root / "bin", root / "crates", root / "testing"]:
    for manifest in directory.rglob("Cargo.toml"):
        name = re.search(r'^name\s*=\s*"([^"]+)"', manifest.read_text(), re.M)
        if name and name.group(1) in required:
            paths[name.group(1)] = manifest.parent
missing = required - paths.keys()
if missing:
    raise SystemExit(f"Missing local crates: {sorted(missing)}")
print('[patch."https://github.com/bnb-chain/reth.git"]')
for name, path in sorted(paths.items()):
    print(f'{name} = {{ path = {json.dumps(str(path))} }}')
