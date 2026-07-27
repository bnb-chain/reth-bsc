#!/usr/bin/env python3
"""Check everything a benchmark cell depends on, before it runs.

A cell can take hours, runs on a datadir that is expensive to re-prepare, and
fails in ways that only surface once it is well underway - a source node that
no longer serves the range, a datadir at the wrong height, a stray node holding
the port. Each of those is cheap to check up front and costly to discover late.

    python3 benchmark/preflight.py --config benchmark/config.toml --group pilot
    python3 benchmark/preflight.py --config benchmark/config.toml --group main \
        --configs legacy-triedb

Exits non-zero if any check fails, so it can gate a run in a script.
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import urllib.error
import urllib.request
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from matrix.config import ConfigError, load_config  # noqa: E402

HERE = Path(__file__).resolve().parent

# The driver also fetches head-32 and head-64 for the safe/finalized hashes, so
# the source must serve a little before the first measured block.
LOOKBACK = 65


class Check:
    def __init__(self) -> None:
        self.failed = 0
        self.warned = 0

    def ok(self, what: str, detail: str = "") -> None:
        print(f"  \033[32mPASS\033[0m  {what}{'  ' + detail if detail else ''}")

    def fail(self, what: str, detail: str = "") -> None:
        self.failed += 1
        print(f"  \033[31mFAIL\033[0m  {what}{'  ' + detail if detail else ''}")

    def warn(self, what: str, detail: str = "") -> None:
        self.warned += 1
        print(f"  \033[33mWARN\033[0m  {what}{'  ' + detail if detail else ''}")


def rpc(url: str, method: str, params: list, timeout: int = 20):
    req = urllib.request.Request(
        url,
        data=json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode(),
        headers={"content-type": "application/json"},
    )
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        body = json.load(resp)
    if "error" in body:
        raise RuntimeError(body["error"].get("message", body["error"]))
    return body["result"]


def check_source(c: Check, rpc_url: str, from_block: int, to_block: int) -> None:
    """The source must serve every block the driver will ask for.

    Checked at both ends rather than the whole range: a pruned or partially
    synced source fails at one edge, and fetching thousands of blocks here would
    cost as much as the run.
    """
    try:
        head = int(rpc(rpc_url, "eth_blockNumber", []), 16)
        c.ok("source reachable", f"head {head}")
    except Exception as e:  # noqa: BLE001 - any failure means unusable
        c.fail("source reachable", f"{rpc_url}: {e}")
        return

    if head < to_block:
        c.fail("source has the range", f"head {head} < to {to_block}")
        return

    for label, n in (("oldest needed", from_block - LOOKBACK), ("last measured", to_block)):
        try:
            blk = rpc(rpc_url, "eth_getBlockByNumber", [hex(n), True])
            if blk is None:
                c.fail(f"source serves {label}", f"block {n} returned null - pruned?")
            else:
                c.ok(f"source serves {label}", f"block {n}, {len(blk['transactions'])} txs")
        except Exception as e:  # noqa: BLE001
            c.fail(f"source serves {label}", f"block {n}: {e}")


def datadir_head(binary: str, datadir: str, chain: str) -> int | None:
    """Read the datadir's head via snapshot_height.sh (read-only, no lock)."""
    script = HERE / "snapshot_height.sh"
    try:
        out = subprocess.run(
            [str(script), binary, datadir, "--chain", chain],
            capture_output=True, text=True, timeout=900,
        )
    except subprocess.TimeoutExpired:
        return None
    for line in out.stdout.splitlines():
        if line.startswith("head block:"):
            return int(line.split()[2])
    return None


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__,
                                formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--config", required=True)
    p.add_argument("--group", required=True)
    p.add_argument("--configs", help="comma-separated subset; default all")
    p.add_argument("--skip-height", action="store_true",
                   help="skip the datadir height check (slow on triedb - it opens RocksDB)")
    args = p.parse_args()

    try:
        cfg = load_config(args.config)
        group = cfg.group(args.group)
    except ConfigError as e:
        print(f"config error: {e}", file=sys.stderr)
        return 2

    wanted = args.configs.split(",") if args.configs else [c.name for c in cfg.configs]
    configs = [c for c in cfg.configs if c.name in wanted]
    missing = set(wanted) - {c.name for c in configs}
    if missing:
        print(f"unknown config(s): {sorted(missing)}", file=sys.stderr)
        return 2

    c = Check()
    expected_head = group.from_block - 1

    print(f"\ngroup '{group.name}': blocks {group.from_block}..{group.to_block} "
          f"({group.block_count}), datadirs must be at {expected_head}\n")

    print("source")
    check_source(c, group.rpc_url, group.from_block, group.to_block)

    print("\nharness")
    jwt = Path(cfg.global_.jwt_secret)
    (c.ok if jwt.is_file() else c.fail)("jwt secret", str(jwt))

    bench_bin = Path(cfg.global_.bench_bin)
    if not (bench_bin.is_file() and os.access(bench_bin, os.X_OK)):
        c.fail("driver binary", f"{bench_bin} missing or not executable")
    else:
        # A driver built before the new-payload work has no such subcommand, and
        # the failure would otherwise appear only once the cell starts.
        help_out = subprocess.run([str(bench_bin), "--help"], capture_output=True, text=True)
        blob = help_out.stdout + help_out.stderr
        if cfg.global_.bench_mode in blob:
            c.ok("driver supports mode", cfg.global_.bench_mode)
        else:
            c.fail("driver supports mode",
                   f"{cfg.global_.bench_mode} not in --help; stale binary?")

    # `pgrep -af` prints "<pid> <cmdline>" on Linux; BSD/macOS pgrep treats -a
    # differently and can emit bare pids. Keep only lines that actually name the
    # binary, so this reports the same thing on either platform.
    stray = subprocess.run(["pgrep", "-af", "reth-bsc"], capture_output=True, text=True).stdout
    stray = [l for l in stray.splitlines() if "reth-bsc" in l and "pgrep" not in l]
    if stray:
        # Not fatal: a rebuild or prune drain on another datadir is legitimate.
        # But it competes for CPU, page cache and disk, and this is a storage
        # benchmark - so it must be a deliberate choice, not a surprise.
        c.warn(f"{len(stray)} reth-bsc process(es) running", "measured runs want an idle box")
        for l in stray[:5]:
            print(f"          {l[:110]}")
    else:
        c.ok("no other reth-bsc processes")

    print("\ncells")
    for cell in configs:
        if not (Path(cell.binary).is_file() and os.access(cell.binary, os.X_OK)):
            c.fail(f"{cell.name}: binary", cell.binary)
            continue
        c.ok(f"{cell.name}: binary", Path(cell.binary).name)

        if args.skip_height:
            c.warn(f"{cell.name}: head", "skipped")
            continue
        head = datadir_head(cell.binary, cell.datadir, cfg.global_.chain)
        if head is None:
            c.fail(f"{cell.name}: head", f"could not read {cell.datadir}")
        elif head != expected_head:
            c.fail(f"{cell.name}: head", f"{head}, expected {expected_head}")
        else:
            c.ok(f"{cell.name}: head", str(head))

    print()
    if c.failed:
        print(f"\033[31m{c.failed} check(s) failed\033[0m - do not start the run\n")
        return 1
    if c.warned:
        print(f"\033[33mready, with {c.warned} warning(s)\033[0m\n")
    else:
        print("\033[32mready\033[0m\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
