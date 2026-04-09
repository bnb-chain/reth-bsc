#!/usr/bin/env python3
"""
Analyze state root computation timing from reth-bsc logs.

Parses structured tracing logs from:
  - bsc::builder::timing  (finish_with_difflayer + state root breakdown)
  - triedb::timing        (triedb internal breakdown)

Usage:
  # From log file (strip ANSI codes first):
  sed 's/\x1b\[[0-9;]*m//g' reth.log | python3 scripts/analyze_root_timing.py

  # Filter to last N blocks:
  ... | python3 scripts/analyze_root_timing.py --last 200

  # Filter blocks with at least N user txs:
  ... | python3 scripts/analyze_root_timing.py --min-tx 10
"""

import sys
import re
import argparse
from collections import defaultdict
import statistics

# Strip ANSI escape codes
ANSI_RE = re.compile(r'\x1b\[[0-9;]*m')

# Match key=value pairs (handle hex values, quoted strings, plain values)
KV_RE = re.compile(r'(\w+)=(0x[0-9a-fA-F]+|"[^"]*"|[\w.]+)')


def parse_kv(line):
    """Extract key=value pairs from a log line."""
    kvs = {}
    for m in KV_RE.finditer(line):
        k, v = m.group(1), m.group(2)
        v = v.strip('"')
        try:
            if '.' in v:
                kvs[k] = float(v)
            else:
                kvs[k] = int(v)
        except ValueError:
            kvs[k] = v
    return kvs


def percentile(data, p):
    if not data:
        return 0
    s = sorted(data)
    k = (len(s) - 1) * p / 100.0
    f = int(k)
    c = f + 1 if f + 1 < len(s) else f
    return s[f] + (s[c] - s[f]) * (k - f)


def stats_str(values):
    if not values:
        return "n/a"
    p50 = percentile(values, 50)
    p95 = percentile(values, 95)
    p99 = percentile(values, 99)
    avg = statistics.mean(values)
    return f"avg={avg:.1f}  p50={p50:.1f}  p95={p95:.1f}  p99={p99:.1f}"


def main():
    parser = argparse.ArgumentParser(description="Analyze state root timing")
    parser.add_argument("--last", type=int, default=0, help="Only analyze last N blocks")
    parser.add_argument("--min-tx", type=int, default=0, help="Only analyze blocks with >= N user txs")
    parser.add_argument("--caller", choices=["miner", "import", "all"], default="all",
                        help="Filter triedb entries by caller (miner/import/all)")
    args = parser.parse_args()

    # Collect log entries by type
    finish_entries = []       # finish_with_difflayer breakdown
    root_entries = []         # state root breakdown
    triedb_top_entries = []   # intermediate_and_commit breakdown
    triedb_inner_entries = [] # intermediate_inner breakdown
    commit_entries = []       # commit_inner breakdown
    slow_storage = []         # slow storage trie updates

    for line in sys.stdin:
        line = ANSI_RE.sub('', line)

        if "finish_with_difflayer breakdown" in line:
            finish_entries.append(parse_kv(line))
        elif "state root breakdown" in line:
            root_entries.append(parse_kv(line))
        elif "intermediate_and_commit_hashed_post_state breakdown" in line:
            triedb_top_entries.append(parse_kv(line))
        elif "intermediate_inner breakdown" in line:
            triedb_inner_entries.append(parse_kv(line))
        elif "commit_inner breakdown" in line:
            commit_entries.append(parse_kv(line))
        elif "slow storage trie update" in line:
            slow_storage.append(parse_kv(line))

    # Apply --caller filter to triedb entries
    if args.caller != "all":
        triedb_top_entries = [e for e in triedb_top_entries if e.get('caller') == args.caller]
        triedb_inner_entries = [e for e in triedb_inner_entries if e.get('caller') == args.caller]
        commit_entries = [e for e in commit_entries if e.get('caller') == args.caller]

    # Apply --last filter
    if args.last > 0:
        finish_entries = finish_entries[-args.last:]
        root_entries = root_entries[-args.last:]
        triedb_top_entries = triedb_top_entries[-args.last:]
        triedb_inner_entries = triedb_inner_entries[-args.last:]
        commit_entries = commit_entries[-args.last:]

    # Apply --min-tx filter
    if args.min_tx > 0:
        finish_entries = [e for e in finish_entries if e.get('user_tx_len', 0) >= args.min_tx]
        root_entries = [e for e in root_entries if e.get('user_tx_count', 0) >= args.min_tx]

    def extract(entries, key):
        return [e[key] for e in entries if key in e]

    # ===== Section 1: finish_with_difflayer =====
    print("=" * 70)
    print(f"FINISH_WITH_DIFFLAYER BREAKDOWN  (n={len(finish_entries)})")
    print("=" * 70)
    if finish_entries:
        print(f"  finish_total_ms     : {stats_str(extract(finish_entries, 'finish_total_ms'))}")
        print(f"  executor_finish_ms  : {stats_str(extract(finish_entries, 'executor_finish_ms'))}")
        print(f"  merge_transitions_ms: {stats_str(extract(finish_entries, 'merge_transitions_ms'))}")
        print(f"  assemble_ms         : {stats_str(extract(finish_entries, 'assemble_ms'))}")
        avg_tx = statistics.mean(extract(finish_entries, 'user_tx_len')) if extract(finish_entries, 'user_tx_len') else 0
        print(f"  avg user_tx_len     : {avg_tx:.0f}")

    # ===== Section 2: State Root Breakdown =====
    print()
    print("=" * 70)
    print(f"STATE ROOT BREAKDOWN  (n={len(root_entries)})")
    print("=" * 70)
    if root_entries:
        print(f"  state_root_total_ms   : {stats_str(extract(root_entries, 'state_root_total_ms'))}")
        print(f"  hashed_post_state_ms  : {stats_str(extract(root_entries, 'hashed_post_state_ms'))}")
        print(f"  prefetcher_finish_ms  : {stats_str(extract(root_entries, 'prefetcher_finish_ms'))}")
        print(f"  to_triedb_state_ms    : {stats_str(extract(root_entries, 'to_triedb_state_ms'))}")
        print(f"  triedb_calc_ms        : {stats_str(extract(root_entries, 'triedb_calc_ms'))}")
        print()
        avg_accts = statistics.mean(extract(root_entries, 'hashed_accounts')) if extract(root_entries, 'hashed_accounts') else 0
        avg_storages = statistics.mean(extract(root_entries, 'hashed_storages')) if extract(root_entries, 'hashed_storages') else 0
        avg_slots = statistics.mean(extract(root_entries, 'hashed_storage_slots')) if extract(root_entries, 'hashed_storage_slots') else 0
        print(f"  avg hashed_accounts     : {avg_accts:.0f}")
        print(f"  avg hashed_storages     : {avg_storages:.0f}")
        print(f"  avg hashed_storage_slots: {avg_slots:.0f}")

    # ===== Section 3: TrieDB Top-level =====
    print()
    print("=" * 70)
    print(f"TRIEDB: intermediate_and_commit  (n={len(triedb_top_entries)})")
    print("=" * 70)
    if triedb_top_entries:
        print(f"  total_ms              : {stats_str(extract(triedb_top_entries, 'total_ms'))}")
        print(f"  state_at_ms           : {stats_str(extract(triedb_top_entries, 'state_at_ms'))}")
        print(f"  intermediate_inner_ms : {stats_str(extract(triedb_top_entries, 'intermediate_inner_ms'))}")
        print(f"  commit_ms             : {stats_str(extract(triedb_top_entries, 'commit_ms'))}")
        print()
        avg_states = statistics.mean(extract(triedb_top_entries, 'states_count')) if extract(triedb_top_entries, 'states_count') else 0
        avg_storage = statistics.mean(extract(triedb_top_entries, 'storage_states_count')) if extract(triedb_top_entries, 'storage_states_count') else 0
        print(f"  avg states_count          : {avg_states:.0f}")
        print(f"  avg storage_states_count  : {avg_storage:.0f}")

    # ===== Section 4: TrieDB intermediate_inner =====
    print()
    print("=" * 70)
    print(f"TRIEDB: intermediate_inner  (n={len(triedb_inner_entries)})")
    print("=" * 70)
    if triedb_inner_entries:
        print(f"  total_ms                : {stats_str(extract(triedb_inner_entries, 'total_ms'))}")
        print(f"  update_state_objects_ms : {stats_str(extract(triedb_inner_entries, 'update_state_objects_ms'))}")
        print(f"  update_account_trie_ms  : {stats_str(extract(triedb_inner_entries, 'update_account_trie_ms'))}")
        print(f"  account_hash_ms         : {stats_str(extract(triedb_inner_entries, 'account_hash_ms'))}")

    # ===== Section 5: TrieDB commit =====
    print()
    print("=" * 70)
    print(f"TRIEDB: commit_inner  (n={len(commit_entries)})")
    print("=" * 70)
    if commit_entries:
        print(f"  commit_state_objects_ms : {stats_str(extract(commit_entries, 'commit_state_objects_ms'))}")
        avg_tries = statistics.mean(extract(commit_entries, 'storage_tries_count')) if extract(commit_entries, 'storage_tries_count') else 0
        print(f"  avg storage_tries_count : {avg_tries:.0f}")

    # ===== Section 6: Slow storage accounts =====
    if slow_storage:
        print()
        print("=" * 70)
        print(f"SLOW STORAGE TRIE UPDATES (>5ms)  (n={len(slow_storage)})")
        print("=" * 70)
        print(f"  acct_ms   : {stats_str(extract(slow_storage, 'acct_ms'))}")
        avg_slots = statistics.mean(extract(slow_storage, 'slot_count')) if extract(slow_storage, 'slot_count') else 0
        print(f"  avg slots : {avg_slots:.0f}")
        # Show top repeat offenders
        addr_counts = defaultdict(list)
        for e in slow_storage:
            addr = e.get('hashed_address', 'unknown')
            addr_counts[addr].append(e.get('acct_ms', 0))
        top = sorted(addr_counts.items(), key=lambda x: len(x[1]), reverse=True)[:10]
        print(f"\n  Top slow accounts:")
        for addr, times in top:
            print(f"    {addr[:16]}...  count={len(times)}  avg_ms={statistics.mean(times):.1f}")

    # ===== Section 7: Bottleneck Summary =====
    print()
    print("=" * 70)
    print("BOTTLENECK SUMMARY")
    print("=" * 70)
    if root_entries:
        fields = ['hashed_post_state_ms', 'prefetcher_finish_ms', 'to_triedb_state_ms', 'triedb_calc_ms']
        totals = {}
        for f in fields:
            vals = extract(root_entries, f)
            totals[f] = statistics.mean(vals) if vals else 0
        grand = sum(totals.values()) or 1
        print("  State root avg time allocation:")
        for f in sorted(totals, key=totals.get, reverse=True):
            pct = totals[f] / grand * 100
            bar = "#" * int(pct / 2)
            print(f"    {f:25s}: {totals[f]:6.1f}ms  ({pct:4.1f}%)  {bar}")

    if triedb_top_entries:
        fields = ['state_at_ms', 'intermediate_inner_ms', 'commit_ms']
        totals = {}
        for f in fields:
            vals = extract(triedb_top_entries, f)
            totals[f] = statistics.mean(vals) if vals else 0
        grand = sum(totals.values()) or 1
        print("\n  TrieDB internal avg time allocation:")
        for f in sorted(totals, key=totals.get, reverse=True):
            pct = totals[f] / grand * 100
            bar = "#" * int(pct / 2)
            print(f"    {f:25s}: {totals[f]:6.1f}ms  ({pct:4.1f}%)  {bar}")

    if triedb_inner_entries:
        fields = ['update_state_objects_ms', 'update_account_trie_ms', 'account_hash_ms']
        totals = {}
        for f in fields:
            vals = extract(triedb_inner_entries, f)
            totals[f] = statistics.mean(vals) if vals else 0
        grand = sum(totals.values()) or 1
        print("\n  intermediate_inner avg time allocation:")
        for f in sorted(totals, key=totals.get, reverse=True):
            pct = totals[f] / grand * 100
            bar = "#" * int(pct / 2)
            print(f"    {f:25s}: {totals[f]:6.1f}ms  ({pct:4.1f}%)  {bar}")


if __name__ == "__main__":
    main()
