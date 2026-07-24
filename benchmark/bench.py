#!/usr/bin/env python3
"""Benchmark matrix CLI.

  run      execute the (config x group) matrix, then analyze the results
  analyze  (re-)compute stats, summaries, and the chart from an existing results dir

Requires Python >= 3.11 (tomllib). Stdlib only.
"""

from __future__ import annotations

import argparse
import json
import sys
from datetime import datetime
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from matrix.analysis import (  # noqa: E402
    RunDataError,
    build_summary,
    compute_stats,
    load_cumulative_time_us,
    load_run,
    write_summary_csv,
    write_summary_json,
    write_summary_md,
)
from matrix.chart import write_chart  # noqa: E402
from matrix.config import ConfigError, load_config  # noqa: E402
from matrix.runner import Runner, preflight_binaries  # noqa: E402


def analyze(results_root: Path, warmup_blocks: int, title: str) -> int:
    meta_path = results_root / "meta.json"
    if not meta_path.is_file():
        print(f"error: {meta_path} not found — is this a results directory?", file=sys.stderr)
        return 1
    meta = json.loads(meta_path.read_text())
    config_order: list[str] = meta["configs"]
    group_names: list[str] = meta["groups"]
    group_labels: dict[str, str] = meta.get("group_labels", {})
    warmup = meta.get("warmup_blocks", warmup_blocks)

    matrix: dict[str, dict] = {}
    for group in group_names:
        matrix[group] = {}
        for config in config_order:
            run_dir = results_root / "runs" / group / config
            run_json = run_dir / "run.json"
            if run_json.is_file() and not json.loads(run_json.read_text()).get("valid", False):
                print(f"skipping invalid run: group={group} config={config}")
                matrix[group][config] = None
                continue
            try:
                records = load_run(run_dir)
                cumulative = load_cumulative_time_us(run_dir)
                matrix[group][config] = compute_stats(records, warmup, cumulative)
            except RunDataError as e:
                print(f"skipping unusable run (group={group} config={config}): {e}")
                matrix[group][config] = None

    summary = build_summary(matrix, group_labels, config_order)
    write_summary_json(summary, results_root / "summary.json")
    write_summary_csv(summary, results_root / "summary.csv")
    write_summary_md(summary, results_root / "summary.md")
    write_chart(summary, results_root / "chart.html", title=title)
    print(f"\nwrote {results_root}/summary.{{json,csv,md}} and {results_root}/chart.html")
    print((results_root / "summary.md").read_text())
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(prog="bench.py", description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)

    p_run = sub.add_parser("run", help="run the benchmark matrix")
    p_run.add_argument("--config", required=True, help="path to config.toml")
    p_run.add_argument("--configs", help="comma-separated subset of config names")
    p_run.add_argument("--groups", help="comma-separated subset of group names")
    p_run.add_argument(
        "--dry-run",
        action="store_true",
        help="print every command without executing anything",
    )
    p_run.add_argument(
        "--no-restore",
        action="store_true",
        help="skip the snapshot restore; the datadir must already be at from-1 "
        "(the head check still enforces this). Best combined with a single "
        "--configs/--groups cell, since a run leaves the datadir at `to`",
    )
    p_run.add_argument(
        "--results-dir",
        help="accumulate runs into this directory instead of a new timestamped one; "
        "combine with --configs/--groups to run one cell at a time and analyze "
        "the combined set at the end",
    )
    p_run.add_argument("--title", default="reth-bsc storage benchmark")

    p_an = sub.add_parser("analyze", help="analyze an existing results directory")
    p_an.add_argument("--results", required=True, help="path to results/<timestamp>")
    p_an.add_argument("--warmup-blocks", type=int, default=2)
    p_an.add_argument("--title", default="reth-bsc storage benchmark")

    args = parser.parse_args()

    if args.command == "analyze":
        return analyze(Path(args.results), args.warmup_blocks, args.title)

    try:
        cfg = load_config(args.config)
    except (ConfigError, FileNotFoundError) as e:
        print(f"config error: {e}", file=sys.stderr)
        return 1

    config_names = args.configs.split(",") if args.configs else None
    group_names = args.groups.split(",") if args.groups else None

    # Verify the prebuilt binaries the config points at before touching anything.
    selected = [cfg.config(n) for n in config_names] if config_names else cfg.configs
    binaries = [("bench_bin", cfg.global_.bench_bin)] + [
        (f"config '{c.name}'", c.binary) for c in selected
    ]
    problems, versions = preflight_binaries(binaries, probe_version=not args.dry_run)
    for label, version in versions.items():
        print(f"binary check {label}: {version}")
    if problems:
        for p in problems:
            print(f"{'warning' if args.dry_run else 'error'}: {p}", file=sys.stderr)
        if not args.dry_run:
            print(
                "point the config at binaries you have already built "
                "(node binaries need the bench-test feature; see benchmark/README.md)",
                file=sys.stderr,
            )
            return 1

    results_root = (
        Path(args.results_dir)
        if args.results_dir
        else Path(cfg.global_.output_dir) / datetime.now().strftime("%Y%m%d-%H%M%S")
    )
    runner = Runner(cfg, results_root, dry_run=args.dry_run, no_restore=args.no_restore)

    if not args.dry_run:
        results_root.mkdir(parents=True, exist_ok=True)

    results = runner.run_matrix(config_names, group_names)

    if args.dry_run:
        print("\ndry run complete; nothing was executed")
        return 0

    # record the verified binary versions for this invocation's cells
    meta_path = results_root / "meta.json"
    meta = json.loads(meta_path.read_text())
    meta["binary_versions"] = {**meta.get("binary_versions", {}), **versions}
    meta_path.write_text(json.dumps(meta, indent=2) + "\n")

    n_valid = sum(r.valid for r in results)
    print(f"\n{n_valid}/{len(results)} runs valid")
    rc = analyze(results_root, cfg.global_.warmup_blocks, args.title)
    return rc if rc else (0 if n_valid == len(results) else 2)


if __name__ == "__main__":
    sys.exit(main())
