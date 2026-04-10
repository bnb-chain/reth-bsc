#!/usr/bin/env python3
"""
Analyze state root computation timing from reth-bsc logs.

Usage:
  sed 's/\x1b\[[0-9;]*m//g' reth.log | python3 analyze_root_timing.py
  ... | python3 analyze_root_timing.py --min-tx 10
  ... | python3 analyze_root_timing.py --min-tx 10 --caller miner
  ... | python3 analyze_root_timing.py --last 200
"""

import sys
import re
import argparse
import statistics
from collections import defaultdict

ANSI_RE = re.compile(r'\x1b\[[0-9;]*m')
KV_RE = re.compile(r'(\w+)=(0x[0-9a-fA-F]+|"[^"]*"|[\w.]+)')


def parse_kv(line):
    kvs = {}
    for m in KV_RE.finditer(line):
        k, v = m.group(1), m.group(2).strip('"')
        try:
            kvs[k] = float(v) if '.' in v else int(v)
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
    return "avg={:.1f}  p50={:.1f}  p95={:.1f}  p99={:.1f}".format(
        statistics.mean(values), percentile(values, 50),
        percentile(values, 95), percentile(values, 99))


def extract(entries, key):
    return [e[key] for e in entries if key in e]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--last", type=int, default=0)
    parser.add_argument("--min-tx", type=int, default=0)
    parser.add_argument("--caller", choices=["miner", "import", "all"], default="all")
    args = parser.parse_args()

    finish_entries = []
    root_entries = []
    triedb_top = []
    triedb_inner = []
    commit_entries = []

    for line in sys.stdin:
        line = ANSI_RE.sub('', line)
        if "finish_with_difflayer breakdown" in line:
            finish_entries.append(parse_kv(line))
        elif "state root breakdown" in line:
            root_entries.append(parse_kv(line))
        elif "intermediate_and_commit breakdown" in line:
            triedb_top.append(parse_kv(line))
        elif "intermediate_inner breakdown" in line:
            triedb_inner.append(parse_kv(line))
        elif "commit_inner breakdown" in line:
            commit_entries.append(parse_kv(line))

    if args.caller != "all":
        triedb_top = [e for e in triedb_top if e.get('caller') == args.caller]
        triedb_inner = [e for e in triedb_inner if e.get('caller') == args.caller]
        commit_entries = [e for e in commit_entries if e.get('caller') == args.caller]

    if args.last > 0:
        finish_entries = finish_entries[-args.last:]
        root_entries = root_entries[-args.last:]
        triedb_top = triedb_top[-args.last:]
        triedb_inner = triedb_inner[-args.last:]
        commit_entries = commit_entries[-args.last:]

    if args.min_tx > 0:
        finish_entries = [e for e in finish_entries if e.get('user_tx_len', 0) >= args.min_tx]
        root_entries = [e for e in root_entries if e.get('user_tx_count', 0) >= args.min_tx]

    W = 70
    # === Builder ===
    print("=" * W)
    print("FINISH_WITH_DIFFLAYER  (n={})".format(len(finish_entries)))
    print("=" * W)
    if finish_entries:
        for k in ['finish_total_ms', 'executor_finish_ms', 'merge_transitions_ms', 'assemble_ms']:
            print("  {:24s}: {}".format(k, stats_str(extract(finish_entries, k))))
        print("  avg user_tx_len         : {:.0f}".format(
            statistics.mean(extract(finish_entries, 'user_tx_len')) if extract(finish_entries, 'user_tx_len') else 0))

    # === State Root ===
    print("\n" + "=" * W)
    print("STATE ROOT BREAKDOWN  (n={})".format(len(root_entries)))
    print("=" * W)
    if root_entries:
        for k in ['state_root_total_ms', 'hashed_post_state_ms', 'prefetcher_finish_ms',
                   'to_triedb_state_ms', 'triedb_calc_ms']:
            print("  {:24s}: {}".format(k, stats_str(extract(root_entries, k))))
        print()
        for k in ['hashed_accounts', 'hashed_storages', 'hashed_storage_slots']:
            v = extract(root_entries, k)
            print("  avg {:22s}: {:.0f}".format(k, statistics.mean(v) if v else 0))

    # === TrieDB top ===
    print("\n" + "=" * W)
    print("TRIEDB: intermediate_and_commit  (n={})".format(len(triedb_top)))
    print("=" * W)
    if triedb_top:
        for k in ['total_ms', 'state_at_ms', 'intermediate_inner_ms', 'commit_ms']:
            print("  {:24s}: {}".format(k, stats_str(extract(triedb_top, k))))
        print()
        for k in ['states_count', 'storage_states_count']:
            v = extract(triedb_top, k)
            print("  avg {:22s}: {:.0f}".format(k, statistics.mean(v) if v else 0))
        # Cache stats
        hits = extract(triedb_top, 'cache_hits')
        misses = extract(triedb_top, 'cache_misses')
        if hits and misses:
            print()
            print("  {:24s}: {}".format("cache_hits", stats_str(hits)))
            print("  {:24s}: {}".format("cache_misses", stats_str(misses)))
            total_h = sum(hits)
            total_m = sum(misses)
            rate = total_h / max(total_h + total_m, 1) * 100
            print("  overall hit rate        : {:.1f}%  ({} hits / {} total)".format(
                rate, total_h, total_h + total_m))

    # === intermediate_inner ===
    print("\n" + "=" * W)
    print("TRIEDB: intermediate_inner  (n={})".format(len(triedb_inner)))
    print("=" * W)
    if triedb_inner:
        for k in ['total_ms', 'update_state_objects_ms', 'update_account_trie_ms', 'account_hash_ms']:
            print("  {:24s}: {}".format(k, stats_str(extract(triedb_inner, k))))
        for k in ['account_count']:
            v = extract(triedb_inner, k)
            if v:
                print("  {:24s}: {}".format(k, stats_str(v)))

    # === commit_inner ===
    print("\n" + "=" * W)
    print("TRIEDB: commit_inner  (n={})".format(len(commit_entries)))
    print("=" * W)
    if commit_entries:
        print("  {:24s}: {}".format("commit_state_objects_ms",
              stats_str(extract(commit_entries, 'commit_state_objects_ms'))))
        v = extract(commit_entries, 'storage_tries_count')
        print("  avg storage_tries_count : {:.0f}".format(statistics.mean(v) if v else 0))

    # === Bottleneck ===
    print("\n" + "=" * W)
    print("BOTTLENECK SUMMARY")
    print("=" * W)

    def show_alloc(label, entries, fields):
        totals = {}
        for f in fields:
            vals = extract(entries, f)
            totals[f] = statistics.mean(vals) if vals else 0
        grand = sum(totals.values()) or 1
        print("\n  {}:".format(label))
        for f in sorted(totals, key=totals.get, reverse=True):
            pct = totals[f] / grand * 100
            bar = "#" * int(pct / 2)
            print("    {:25s}: {:6.1f}ms  ({:4.1f}%)  {}".format(f, totals[f], pct, bar))

    if root_entries:
        show_alloc("State root time", root_entries,
                   ['hashed_post_state_ms', 'prefetcher_finish_ms', 'to_triedb_state_ms', 'triedb_calc_ms'])
    if triedb_top:
        show_alloc("TrieDB internal time", triedb_top,
                   ['state_at_ms', 'intermediate_inner_ms', 'commit_ms'])
    if triedb_inner:
        show_alloc("intermediate_inner time", triedb_inner,
                   ['update_state_objects_ms', 'update_account_trie_ms', 'account_hash_ms'])


if __name__ == "__main__":
    main()
