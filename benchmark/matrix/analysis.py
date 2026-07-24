"""Per-run CSV parsing and matrix statistics.

Input per run (written by `reth-bench-bsc forkchoice-only --output <dir>`):
  - forkchoice_latency.csv: gas_used,latency        (latency in microseconds)
  - total_gas.csv:          block_number,gas_used,time (time = cumulative
    benchmark microseconds, block-fetch waits already excluded)

Both files are serialized from the same in-order results vec, so rows
correspond positionally; per-row gas_used equality is asserted as a check.
"""

from __future__ import annotations

import csv
import json
from dataclasses import dataclass
from pathlib import Path

GIGAGAS = 1_000_000_000

LATENCY_CSV = "forkchoice_latency.csv"
GAS_CSV = "total_gas.csv"


class RunDataError(Exception):
    pass


@dataclass
class BlockRecord:
    block_number: int
    gas_used: int
    latency_us: int


@dataclass
class RunStats:
    n_blocks: int  # blocks included in stats (after warmup drop)
    warmup_dropped: int
    total_gas: int
    p50_ms: float
    p90_ms: float
    # Sum(gas) / Sum(per-block FCU latency) over the non-warmup blocks.
    throughput_ggas_s: float
    # The bench binary's own aggregate definition: Sum(all gas) / last cumulative
    # `time` (includes consumer-loop overhead and warmup blocks).
    bench_style_ggas_s: float


def load_run(run_dir: str | Path) -> list[BlockRecord]:
    """Positionally join the two CSVs into per-block records."""
    run_dir = Path(run_dir)
    lat_path = run_dir / LATENCY_CSV
    gas_path = run_dir / GAS_CSV
    for p in (lat_path, gas_path):
        if not p.is_file():
            raise RunDataError(f"missing {p}")

    with open(lat_path, newline="") as f:
        lat_rows = list(csv.DictReader(f))
    with open(gas_path, newline="") as f:
        gas_rows = list(csv.DictReader(f))

    if len(lat_rows) != len(gas_rows):
        raise RunDataError(
            f"row count mismatch: {lat_path.name} has {len(lat_rows)}, "
            f"{gas_path.name} has {len(gas_rows)}"
        )
    if not lat_rows:
        raise RunDataError(f"{lat_path} is empty")

    records = []
    for i, (lat, gas) in enumerate(zip(lat_rows, gas_rows)):
        if lat["gas_used"] != gas["gas_used"]:
            raise RunDataError(
                f"gas_used mismatch at row {i}: "
                f"{lat['gas_used']} (latency csv) != {gas['gas_used']} (gas csv)"
            )
        records.append(
            BlockRecord(
                block_number=int(gas["block_number"]),
                gas_used=int(gas["gas_used"]),
                latency_us=int(lat["latency"]),
            )
        )
    return records


def load_cumulative_time_us(run_dir: str | Path) -> int:
    """Last cumulative `time` value from total_gas.csv (microseconds)."""
    with open(Path(run_dir) / GAS_CSV, newline="") as f:
        rows = list(csv.DictReader(f))
    if not rows:
        raise RunDataError(f"{GAS_CSV} is empty")
    return int(rows[-1]["time"])


def percentile(sorted_values: list[float], q: float) -> float:
    """Linear-interpolation percentile (numpy 'linear' method). q in [0, 100]."""
    if not sorted_values:
        raise ValueError("empty input")
    n = len(sorted_values)
    if n == 1:
        return sorted_values[0]
    rank = (n - 1) * (q / 100.0)
    lo = int(rank)
    hi = min(lo + 1, n - 1)
    frac = rank - lo
    return sorted_values[lo] * (1 - frac) + sorted_values[hi] * frac


def compute_stats(
    records: list[BlockRecord], warmup_blocks: int, cumulative_time_us: int
) -> RunStats:
    if warmup_blocks >= len(records):
        raise RunDataError(
            f"warmup_blocks ({warmup_blocks}) leaves no blocks out of {len(records)}"
        )
    measured = records[warmup_blocks:]
    latencies_ms = sorted(r.latency_us / 1000.0 for r in measured)
    total_gas = sum(r.gas_used for r in measured)
    total_latency_s = sum(r.latency_us for r in measured) / 1_000_000.0
    all_gas = sum(r.gas_used for r in records)
    return RunStats(
        n_blocks=len(measured),
        warmup_dropped=warmup_blocks,
        total_gas=total_gas,
        p50_ms=percentile(latencies_ms, 50),
        p90_ms=percentile(latencies_ms, 90),
        throughput_ggas_s=total_gas / total_latency_s / GIGAGAS,
        bench_style_ggas_s=all_gas / (cumulative_time_us / 1_000_000.0) / GIGAGAS,
    )


# --- matrix summary ---------------------------------------------------------

# Metric direction: True means lower is better.
METRICS = [
    ("p50_ms", "P50 (ms)", True),
    ("p90_ms", "P90 (ms)", True),
    ("throughput_ggas_s", "Throughput (Ggas/s)", False),
]


def delta_pct(value: float, baseline: float) -> float:
    """Raw percent change vs baseline: (value/baseline - 1) * 100."""
    return (value / baseline - 1.0) * 100.0


def delta_caption(metric_key: str, pct: float) -> str:
    """Reference-image style caption, e.g. '34% lower' / '53% higher'."""
    word = "higher" if pct > 0 else "lower"
    return f"{abs(pct):.0f}% {word}"


def is_improvement(metric_key: str, pct: float) -> bool:
    lower_is_better = next(low for key, _, low in METRICS if key == metric_key)
    return pct < 0 if lower_is_better else pct > 0


def build_summary(
    matrix: dict[str, dict[str, RunStats | None]],
    group_labels: dict[str, str],
    config_order: list[str],
) -> dict:
    """Build the summary structure consumed by the writers and the chart.

    `matrix[group_name][config_name]` is a RunStats, or None for a missing or
    invalid run. `config_order[0]` is the baseline.
    """
    baseline_name = config_order[0]
    groups_out = []
    for group_name, per_config in matrix.items():
        base = per_config.get(baseline_name)
        configs_out = []
        for config_name in config_order:
            stats = per_config.get(config_name)
            if stats is None:
                configs_out.append({"name": config_name, "valid": False})
                continue
            entry = {
                "name": config_name,
                "valid": True,
                "n_blocks": stats.n_blocks,
                "warmup_dropped": stats.warmup_dropped,
                "total_gas": stats.total_gas,
                "p50_ms": stats.p50_ms,
                "p90_ms": stats.p90_ms,
                "throughput_ggas_s": stats.throughput_ggas_s,
                "bench_style_ggas_s": stats.bench_style_ggas_s,
                "deltas": None,
            }
            if base is not None and config_name != baseline_name:
                entry["deltas"] = {
                    key: delta_pct(getattr(stats, key), getattr(base, key))
                    for key, _, _ in METRICS
                }
            configs_out.append(entry)
        groups_out.append(
            {
                "name": group_name,
                "label": group_labels.get(group_name, group_name),
                "configs": configs_out,
            }
        )
    return {"baseline": baseline_name, "groups": groups_out}


# --- writers -----------------------------------------------------------------


def write_summary_json(summary: dict, path: str | Path) -> None:
    Path(path).write_text(json.dumps(summary, indent=2) + "\n")


def write_summary_csv(summary: dict, path: str | Path) -> None:
    fields = [
        "group",
        "config",
        "n_blocks",
        "warmup_dropped",
        "total_gas",
        "p50_ms",
        "p90_ms",
        "throughput_ggas_s",
        "bench_style_ggas_s",
        "p50_delta_pct",
        "p90_delta_pct",
        "throughput_delta_pct",
    ]
    with open(path, "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(fields)
        for group in summary["groups"]:
            for c in group["configs"]:
                if not c["valid"]:
                    w.writerow([group["name"], c["name"]] + ["invalid"] * (len(fields) - 2))
                    continue
                d = c["deltas"] or {}
                w.writerow(
                    [
                        group["name"],
                        c["name"],
                        c["n_blocks"],
                        c["warmup_dropped"],
                        c["total_gas"],
                        f"{c['p50_ms']:.2f}",
                        f"{c['p90_ms']:.2f}",
                        f"{c['throughput_ggas_s']:.4f}",
                        f"{c['bench_style_ggas_s']:.4f}",
                        f"{d['p50_ms']:.1f}" if d else "",
                        f"{d['p90_ms']:.1f}" if d else "",
                        f"{d['throughput_ggas_s']:.1f}" if d else "",
                    ]
                )


def write_summary_md(summary: dict, path: str | Path) -> None:
    lines = ["# Benchmark summary", ""]
    lines.append(f"Baseline: `{summary['baseline']}` (deltas are vs baseline)")
    lines.append("")
    for group in summary["groups"]:
        lines.append(f"## {group['label']}")
        lines.append("")
        lines.append(
            "| Config | P50 (ms) | P90 (ms) | Throughput (Ggas/s) | Blocks | Total gas |"
        )
        lines.append("|---|---|---|---|---|---|")
        for c in group["configs"]:
            if not c["valid"]:
                lines.append(f"| {c['name']} | invalid run | | | | |")
                continue
            d = c["deltas"]

            def cell(key: str, fmt: str) -> str:
                val = format(c[key], fmt)
                if d is None:
                    return val
                return f"{val} ({d[key]:+.1f}%)"

            name = c["name"] + (" (baseline)" if d is None else "")
            lines.append(
                f"| {name} | {cell('p50_ms', '.2f')} | {cell('p90_ms', '.2f')} "
                f"| {cell('throughput_ggas_s', '.4f')} | {c['n_blocks']} | {c['total_gas']:,} |"
            )
        lines.append("")
        drops = {c["warmup_dropped"] for c in group["configs"] if c["valid"]}
        if drops:
            lines.append(
                f"_First {max(drops)} block(s) of each run dropped as warmup "
                "(node-start SYNCING retries inflate their latency)._"
            )
            lines.append("")
        # Flag runs where the two throughput definitions diverge noticeably.
        for c in group["configs"]:
            if c["valid"] and c["throughput_ggas_s"] > 0:
                ratio = c["bench_style_ggas_s"] / c["throughput_ggas_s"]
                if abs(ratio - 1.0) > 0.01:
                    lines.append(
                        f"_Note: `{c['name']}` bench-binary aggregate is "
                        f"{c['bench_style_ggas_s']:.4f} Ggas/s "
                        f"({(ratio - 1) * 100:+.1f}% vs the per-block-latency definition), "
                        "due to consumer-loop overhead and warmup blocks._"
                    )
        lines.append("")
    Path(path).write_text("\n".join(lines))
