#!/usr/bin/env python3
"""
Analyze reth-bsc miner timing logs to identify state root computation bottlenecks.

Usage:
    python3 scripts/analyze_timing.py <logfile>
    python3 scripts/analyze_timing.py <logfile> --last 100   # only last N blocks
    cat reth.log | grep -E "timing|payload_builder|tx_timing" | python3 scripts/analyze_timing.py -

Expects log lines containing these targets:
  - "payload_builder"        → Block payload built successfully (overall build timing)
  - "bsc::builder::timing"   → finish_with_difflayer breakdown
  - "bsc::miner::tx_timing"  → per-block tx execution summary + slow tx details
  - "triedb::timing"         → intermediate_and_commit, intermediate_inner, update_state_objects, commit_inner
"""

import sys
import re
import json
import argparse
from collections import defaultdict
from dataclasses import dataclass, field
from typing import Optional


@dataclass
class BlockTiming:
    block_number: int = 0
    tx_count: int = 0

    # payload.rs level
    prepare_ms: int = 0
    exec_ms: int = 0
    trie_root_ms: int = 0  # finalize_elapsed = finish_with_difflayer total
    build_ms: int = 0
    avg_tx_us: int = 0

    # builder.rs level (inside finish_with_difflayer)
    executor_finish_ms: int = 0
    merge_transitions_ms: int = 0
    hashed_post_state_ms: int = 0
    to_triedb_state_ms: int = 0
    triedb_calc_ms: int = 0
    hashed_accounts: int = 0
    hashed_storages: int = 0
    hashed_storage_slots: int = 0
    system_tx_count: int = 0

    # triedb: intermediate_and_commit breakdown
    triedb_state_at_ms: int = 0
    triedb_intermediate_inner_ms: int = 0
    triedb_commit_ms: int = 0

    # triedb: intermediate_inner breakdown
    update_state_objects_ms: int = 0
    update_account_trie_ms: int = 0
    account_hash_ms: int = 0

    # triedb: commit_inner breakdown
    commit_state_objects_ms: int = 0
    storage_tries_count: int = 0

    # per-tx execution breakdown
    tx_p50_us: int = 0
    tx_p95_us: int = 0
    tx_p99_us: int = 0
    tx_max_us: int = 0
    tx_min_us: int = 0
    tx_avg_us: int = 0
    slow_tx_1ms: int = 0
    very_slow_tx_5ms: int = 0
    total_gas: int = 0
    avg_gas: int = 0
    max_gas: int = 0


def extract_kv(line: str) -> dict:
    """Extract key=value pairs from a structured tracing log line."""
    kv = {}
    # Match key=value patterns. Order matters:
    #   1. 0x-prefixed hex strings (hashes, addresses)
    #   2. Quoted strings
    #   3. true/false
    #   4. Numbers (int or float)
    #   5. Any non-whitespace token
    for m in re.finditer(r'(\w+)=(0x[0-9a-fA-F]+|"[^"]*"|true|false|[\d.]+|\S+)', line):
        k, v = m.group(1), m.group(2)
        v = v.strip('"')
        if v.startswith('0x'):
            kv[k] = v  # keep hex as string
        elif v in ('true', 'false'):
            kv[k] = v == 'true'
        else:
            try:
                if '.' in v:
                    kv[k] = float(v)
                else:
                    kv[k] = int(v)
            except (ValueError, TypeError):
                kv[k] = v
    return kv


def parse_logs(lines, last_n=None):
    """Parse log lines into BlockTiming records, matched by block_number."""
    blocks = {}  # block_number -> BlockTiming

    for line in lines:
        line = line.strip()
        if not line:
            continue

        kv = extract_kv(line)

        # --- payload_builder: Block payload built successfully ---
        if "Block payload built successfully" in line:
            bn = kv.get("block_number", 0)
            bt = blocks.setdefault(bn, BlockTiming(block_number=bn))
            bt.tx_count = kv.get("tx_count", 0)
            bt.prepare_ms = kv.get("prepare_duration_ms", 0)
            bt.exec_ms = kv.get("exec_duration_ms", 0)
            bt.trie_root_ms = kv.get("trie_root_duration_ms", 0)
            bt.build_ms = kv.get("build_duration_ms", 0)
            bt.avg_tx_us = kv.get("avg_tx_duration_micros", 0)

        # --- bsc::builder::timing: finish_with_difflayer breakdown ---
        elif "finish_with_difflayer timing breakdown" in line:
            bn = kv.get("block_number", 0)
            bt = blocks.setdefault(bn, BlockTiming(block_number=bn))
            bt.executor_finish_ms = kv.get("executor_finish_ms", 0)
            bt.merge_transitions_ms = kv.get("merge_transitions_ms", 0)
            bt.hashed_post_state_ms = kv.get("hashed_post_state_ms", 0)
            bt.to_triedb_state_ms = kv.get("to_triedb_state_ms", 0)
            bt.triedb_calc_ms = kv.get("triedb_calc_ms", 0)
            bt.hashed_accounts = kv.get("hashed_accounts", 0)
            bt.hashed_storages = kv.get("hashed_storages", 0)
            bt.hashed_storage_slots = kv.get("hashed_storage_slots", 0)
            bt.system_tx_count = kv.get("system_tx_count", 0)

        # --- bsc::miner::tx_timing: per-block tx execution summary ---
        elif "per-block tx execution summary" in line:
            bn = kv.get("block_number", 0)
            bt = blocks.setdefault(bn, BlockTiming(block_number=bn))
            bt.tx_p50_us = kv.get("p50_us", 0)
            bt.tx_p95_us = kv.get("p95_us", 0)
            bt.tx_p99_us = kv.get("p99_us", 0)
            bt.tx_max_us = kv.get("max_us", 0)
            bt.tx_min_us = kv.get("min_us", 0)
            bt.tx_avg_us = kv.get("avg_us", 0)
            bt.slow_tx_1ms = kv.get("slow_tx_1ms", 0)
            bt.very_slow_tx_5ms = kv.get("very_slow_tx_5ms", 0)
            bt.total_gas = kv.get("total_gas", 0)
            bt.avg_gas = kv.get("avg_gas", 0)
            bt.max_gas = kv.get("max_gas", 0)

        # --- triedb::timing: intermediate_and_commit breakdown ---
        elif "intermediate_and_commit_hashed_post_state breakdown" in line:
            # Match to the most recent block
            bn = max(blocks.keys()) if blocks else 0
            bt = blocks.setdefault(bn, BlockTiming(block_number=bn))
            bt.triedb_state_at_ms = kv.get("state_at_ms", 0)
            bt.triedb_intermediate_inner_ms = kv.get("intermediate_inner_ms", 0)
            bt.triedb_commit_ms = kv.get("commit_ms", 0)

        # --- triedb::timing: intermediate_inner breakdown ---
        elif "intermediate_inner breakdown" in line:
            bn = max(blocks.keys()) if blocks else 0
            bt = blocks.setdefault(bn, BlockTiming(block_number=bn))
            bt.update_state_objects_ms = kv.get("update_state_objects_ms", 0)
            bt.update_account_trie_ms = kv.get("update_account_trie_ms", 0)
            bt.account_hash_ms = kv.get("account_hash_ms", 0)

        # --- triedb::timing: commit_inner breakdown ---
        elif "commit_inner breakdown" in line:
            bn = max(blocks.keys()) if blocks else 0
            bt = blocks.setdefault(bn, BlockTiming(block_number=bn))
            bt.commit_state_objects_ms = kv.get("commit_state_objects_ms", 0)
            bt.storage_tries_count = kv.get("storage_tries_count", 0)

    result = sorted(blocks.values(), key=lambda b: b.block_number)
    if last_n and len(result) > last_n:
        result = result[-last_n:]
    return result


def percentile(values, p):
    if not values:
        return 0
    s = sorted(values)
    idx = int(len(s) * p / 100)
    return s[min(idx, len(s) - 1)]


def stats(values):
    if not values:
        return {"avg": 0, "p50": 0, "p95": 0, "p99": 0, "max": 0, "min": 0}
    return {
        "avg": sum(values) / len(values),
        "p50": percentile(values, 50),
        "p95": percentile(values, 95),
        "p99": percentile(values, 99),
        "max": max(values),
        "min": min(values),
    }


def print_stats_table(label, s, unit="ms"):
    print(f"  {label:.<40s} avg={s['avg']:>7.1f}{unit}  p50={s['p50']:>6}{unit}  p95={s['p95']:>6}{unit}  p99={s['p99']:>6}{unit}  max={s['max']:>6}{unit}")


def analyze(blocks):
    if not blocks:
        print("No blocks found in log.")
        return

    n = len(blocks)
    print(f"\n{'='*80}")
    print(f"  MINER TIMING ANALYSIS  ({n} blocks)")
    print(f"  Block range: {blocks[0].block_number} - {blocks[-1].block_number}")
    print(f"{'='*80}\n")

    # --- Overall ---
    print("1. OVERALL BUILD TIMING")
    print("-" * 80)
    print_stats_table("tx_count", stats([b.tx_count for b in blocks]), unit="")
    print_stats_table("build_duration", stats([b.build_ms for b in blocks]))
    print_stats_table("  prepare_duration", stats([b.prepare_ms for b in blocks]))
    print_stats_table("  exec_duration", stats([b.exec_ms for b in blocks]))
    print_stats_table("  trie_root_duration (finalize)", stats([b.trie_root_ms for b in blocks]))
    print_stats_table("avg_tx_duration", stats([b.avg_tx_us for b in blocks]), unit="us")
    print()

    # --- finish_with_difflayer breakdown ---
    has_builder = any(b.executor_finish_ms > 0 or b.triedb_calc_ms > 0 for b in blocks)
    if has_builder:
        print("2. FINISH_WITH_DIFFLAYER BREAKDOWN")
        print("-" * 80)
        print_stats_table("  executor_finish (system txs)", stats([b.executor_finish_ms for b in blocks]))
        print_stats_table("  merge_transitions", stats([b.merge_transitions_ms for b in blocks]))
        print_stats_table("  hashed_post_state", stats([b.hashed_post_state_ms for b in blocks]))
        print_stats_table("  to_triedb_state", stats([b.to_triedb_state_ms for b in blocks]))
        print_stats_table("  triedb_calc (total)", stats([b.triedb_calc_ms for b in blocks]))
        print_stats_table("  hashed_accounts", stats([b.hashed_accounts for b in blocks]), unit="")
        print_stats_table("  hashed_storages", stats([b.hashed_storages for b in blocks]), unit="")
        print_stats_table("  hashed_storage_slots", stats([b.hashed_storage_slots for b in blocks]), unit="")
        print_stats_table("  system_tx_count", stats([b.system_tx_count for b in blocks]), unit="")
        print()

    # --- triedb breakdown ---
    has_triedb = any(b.triedb_state_at_ms > 0 or b.triedb_intermediate_inner_ms > 0 or b.triedb_commit_ms > 0 for b in blocks)
    if has_triedb:
        print("3. TRIEDB: intermediate_and_commit BREAKDOWN")
        print("-" * 80)
        print_stats_table("  state_at", stats([b.triedb_state_at_ms for b in blocks]))
        print_stats_table("  intermediate_inner", stats([b.triedb_intermediate_inner_ms for b in blocks]))
        print_stats_table("  commit", stats([b.triedb_commit_ms for b in blocks]))
        print()

    has_inner = any(b.update_state_objects_ms > 0 for b in blocks)
    if has_inner:
        print("4. TRIEDB: intermediate_inner BREAKDOWN")
        print("-" * 80)
        print_stats_table("  update_state_objects", stats([b.update_state_objects_ms for b in blocks]))
        print_stats_table("  update_account_trie (serial)", stats([b.update_account_trie_ms for b in blocks]))
        print_stats_table("  account_trie.hash()", stats([b.account_hash_ms for b in blocks]))
        print()

    has_commit = any(b.commit_state_objects_ms > 0 for b in blocks)
    if has_commit:
        print("5. TRIEDB: commit_inner BREAKDOWN")
        print("-" * 80)
        print_stats_table("  commit_state_objects", stats([b.commit_state_objects_ms for b in blocks]))
        print_stats_table("  storage_tries_count", stats([b.storage_tries_count for b in blocks]), unit="")
        print()

    # --- Per-tx execution analysis ---
    has_tx = any(b.tx_p50_us > 0 for b in blocks)
    if has_tx:
        print("6. PER-TX EXECUTION ANALYSIS")
        print("-" * 80)
        print_stats_table("  tx duration p50", stats([b.tx_p50_us for b in blocks]), unit="us")
        print_stats_table("  tx duration p95", stats([b.tx_p95_us for b in blocks]), unit="us")
        print_stats_table("  tx duration p99", stats([b.tx_p99_us for b in blocks]), unit="us")
        print_stats_table("  tx duration max", stats([b.tx_max_us for b in blocks]), unit="us")
        print_stats_table("  tx duration min", stats([b.tx_min_us for b in blocks]), unit="us")
        print_stats_table("  slow txs (>1ms)", stats([b.slow_tx_1ms for b in blocks]), unit="")
        print_stats_table("  very slow txs (>5ms)", stats([b.very_slow_tx_5ms for b in blocks]), unit="")
        print()
        print_stats_table("  total gas / block", stats([b.total_gas for b in blocks]), unit="")
        print_stats_table("  avg gas / tx", stats([b.avg_gas for b in blocks]), unit="")
        print_stats_table("  max gas / tx (in block)", stats([b.max_gas for b in blocks]), unit="")
        print()

        # Slow tx impact analysis
        blocks_with_slow = [b for b in blocks if b.very_slow_tx_5ms > 0]
        blocks_without_slow = [b for b in blocks if b.very_slow_tx_5ms == 0]
        if blocks_with_slow and blocks_without_slow:
            avg_exec_with = sum(b.exec_ms for b in blocks_with_slow) / len(blocks_with_slow)
            avg_exec_without = sum(b.exec_ms for b in blocks_without_slow) / len(blocks_without_slow)
            avg_txcount_with = sum(b.tx_count for b in blocks_with_slow) / len(blocks_with_slow)
            avg_txcount_without = sum(b.tx_count for b in blocks_without_slow) / len(blocks_without_slow)
            print(f"  SLOW TX IMPACT:")
            print(f"    Blocks with >5ms txs:    {len(blocks_with_slow):>4} blocks, avg exec={avg_exec_with:.0f}ms, avg tx_count={avg_txcount_with:.0f}")
            print(f"    Blocks without >5ms txs: {len(blocks_without_slow):>4} blocks, avg exec={avg_exec_without:.0f}ms, avg tx_count={avg_txcount_without:.0f}")
            if avg_txcount_with > 0 and avg_txcount_without > 0:
                per_tx_with = avg_exec_with / avg_txcount_with * 1000
                per_tx_without = avg_exec_without / avg_txcount_without * 1000
                print(f"    Per-tx avg (with slow):  {per_tx_with:.0f}μs")
                print(f"    Per-tx avg (without):    {per_tx_without:.0f}μs")
                print(f"    Slow tx overhead:        +{per_tx_with - per_tx_without:.0f}μs/tx")
            print()

    # --- Bottleneck analysis ---
    print("7. BOTTLENECK ANALYSIS")
    print("-" * 80)

    # Compute average percentages of build_ms
    avg_build = sum(b.build_ms for b in blocks) / n if n else 1
    avg_exec = sum(b.exec_ms for b in blocks) / n
    avg_trie = sum(b.trie_root_ms for b in blocks) / n

    components = []
    if avg_build > 0:
        components.append(("exec_duration", avg_exec, avg_exec / avg_build * 100))
        components.append(("trie_root_duration", avg_trie, avg_trie / avg_build * 100))

    # Sub-components of trie_root
    if has_builder:
        avg_ef = sum(b.executor_finish_ms for b in blocks) / n
        avg_mt = sum(b.merge_transitions_ms for b in blocks) / n
        avg_hp = sum(b.hashed_post_state_ms for b in blocks) / n
        avg_tt = sum(b.to_triedb_state_ms for b in blocks) / n
        avg_tc = sum(b.triedb_calc_ms for b in blocks) / n
        sub = [
            ("  executor_finish", avg_ef),
            ("  merge_transitions", avg_mt),
            ("  hashed_post_state", avg_hp),
            ("  to_triedb_state", avg_tt),
            ("  triedb_calc", avg_tc),
        ]
        # Sub-sub of triedb_calc
        if has_triedb:
            avg_sa = sum(b.triedb_state_at_ms for b in blocks) / n
            avg_ii = sum(b.triedb_intermediate_inner_ms for b in blocks) / n
            avg_cm = sum(b.triedb_commit_ms for b in blocks) / n
            sub.extend([
                ("    state_at", avg_sa),
                ("    intermediate_inner", avg_ii),
                ("    commit", avg_cm),
            ])
        if has_inner:
            avg_uso = sum(b.update_state_objects_ms for b in blocks) / n
            avg_uat = sum(b.update_account_trie_ms for b in blocks) / n
            avg_ah = sum(b.account_hash_ms for b in blocks) / n
            sub.extend([
                ("      update_state_objects", avg_uso),
                ("      update_account_trie", avg_uat),
                ("      account_hash", avg_ah),
            ])

    print(f"\n  Average build_duration: {avg_build:.0f}ms")
    print(f"  ├── exec_duration: {avg_exec:.0f}ms ({avg_exec/max(avg_build,1)*100:.0f}%)")
    print(f"  └── trie_root_duration: {avg_trie:.0f}ms ({avg_trie/max(avg_build,1)*100:.0f}%)")

    if has_builder:
        print(f"       ├── executor_finish: {avg_ef:.0f}ms")
        print(f"       ├── merge_transitions: {avg_mt:.0f}ms")
        print(f"       ├── hashed_post_state: {avg_hp:.0f}ms")
        print(f"       ├── to_triedb_state: {avg_tt:.0f}ms")
        print(f"       └── triedb_calc: {avg_tc:.0f}ms")

    if has_triedb:
        print(f"            ├── state_at: {avg_sa:.0f}ms")
        print(f"            ├── intermediate_inner: {avg_ii:.0f}ms")
        if has_inner:
            print(f"            │    ├── update_state_objects: {avg_uso:.0f}ms")
            print(f"            │    ├── update_account_trie: {avg_uat:.0f}ms")
            print(f"            │    └── account_hash: {avg_ah:.0f}ms")
        print(f"            └── commit: {avg_cm:.0f}ms")

    # Identify top bottleneck
    print(f"\n  TOP BOTTLENECK:")
    all_stages = [
        ("Transaction execution", avg_exec),
    ]
    if has_builder:
        all_stages.extend([
            ("executor_finish (system txs)", avg_ef),
            ("merge_transitions", avg_mt),
            ("hashed_post_state", avg_hp),
            ("to_triedb_state", avg_tt),
        ])
    if has_inner:
        all_stages.extend([
            ("update_state_objects (storage tries)", avg_uso),
            ("update_account_trie (serial)", avg_uat),
            ("account_trie.hash()", avg_ah),
        ])
    if has_commit:
        avg_cso = sum(b.commit_state_objects_ms for b in blocks) / n
        all_stages.append(("commit_state_objects", avg_cso))
    if has_triedb:
        all_stages.append(("state_at", avg_sa))

    all_stages.sort(key=lambda x: x[1], reverse=True)
    for i, (name, avg) in enumerate(all_stages[:5]):
        pct = avg / max(avg_build, 1) * 100
        marker = " <<<" if i == 0 else ""
        print(f"  {i+1}. {name}: {avg:.0f}ms ({pct:.0f}% of build){marker}")

    print(f"\n{'='*80}\n")


def main():
    parser = argparse.ArgumentParser(description="Analyze reth-bsc miner timing logs")
    parser.add_argument("logfile", help="Log file path, or '-' for stdin")
    parser.add_argument("--last", type=int, default=None, help="Analyze only the last N blocks")
    parser.add_argument("--min-tx", type=int, default=0, help="Only include blocks with >= N transactions")
    args = parser.parse_args()

    if args.logfile == "-":
        lines = sys.stdin.readlines()
    else:
        with open(args.logfile) as f:
            lines = f.readlines()

    blocks = parse_logs(lines, last_n=args.last)
    if args.min_tx > 0:
        before = len(blocks)
        blocks = [b for b in blocks if b.tx_count >= args.min_tx]
        print(f"  [filter: --min-tx {args.min_tx} kept {len(blocks)}/{before} blocks]")
    analyze(blocks)


if __name__ == "__main__":
    main()
