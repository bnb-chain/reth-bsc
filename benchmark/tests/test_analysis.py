import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from matrix.analysis import (
    BlockRecord,
    RunDataError,
    build_summary,
    compute_stats,
    delta_caption,
    delta_pct,
    is_improvement,
    load_cumulative_time_us,
    load_run,
    percentile,
)

LATENCY_CSV = "gas_used,latency\n100,1000\n200,2000\n300,3000\n"
GAS_CSV = "block_number,gas_used,time\n11,100,1000\n12,200,3100\n13,300,6300\n"


def write_run(dirpath: Path, latency: str = LATENCY_CSV, gas: str = GAS_CSV) -> Path:
    (dirpath / "forkchoice_latency.csv").write_text(latency)
    (dirpath / "total_gas.csv").write_text(gas)
    return dirpath


class TestLoadRun(unittest.TestCase):
    def test_join(self):
        with tempfile.TemporaryDirectory() as d:
            records = load_run(write_run(Path(d)))
        self.assertEqual(
            records,
            [
                BlockRecord(11, 100, 1000),
                BlockRecord(12, 200, 2000),
                BlockRecord(13, 300, 3000),
            ],
        )

    def test_gas_mismatch_rejected(self):
        bad_gas = GAS_CSV.replace("12,200,", "12,999,")
        with tempfile.TemporaryDirectory() as d:
            with self.assertRaises(RunDataError):
                load_run(write_run(Path(d), gas=bad_gas))

    def test_row_count_mismatch_rejected(self):
        with tempfile.TemporaryDirectory() as d:
            with self.assertRaises(RunDataError):
                load_run(write_run(Path(d), latency="gas_used,latency\n100,1000\n"))

    def test_missing_file_rejected(self):
        with tempfile.TemporaryDirectory() as d:
            (Path(d) / "forkchoice_latency.csv").write_text(LATENCY_CSV)
            with self.assertRaises(RunDataError):
                load_run(d)

    def test_cumulative_time(self):
        with tempfile.TemporaryDirectory() as d:
            self.assertEqual(load_cumulative_time_us(write_run(Path(d))), 6300)


class TestPercentile(unittest.TestCase):
    def test_single(self):
        self.assertEqual(percentile([5.0], 50), 5.0)

    def test_median_even(self):
        self.assertEqual(percentile([1.0, 2.0, 3.0, 4.0], 50), 2.5)

    def test_p90_interpolated(self):
        # numpy.percentile([10,20,...,100], 90) == 91.0 (linear method)
        vals = [float(x) for x in range(10, 101, 10)]
        self.assertAlmostEqual(percentile(vals, 90), 91.0)

    def test_extremes(self):
        vals = [1.0, 2.0, 3.0]
        self.assertEqual(percentile(vals, 0), 1.0)
        self.assertEqual(percentile(vals, 100), 3.0)


class TestComputeStats(unittest.TestCase):
    def records(self):
        # 5 blocks; first has an absurd warmup latency
        return [
            BlockRecord(10, 1_000_000, 5_000_000),
            BlockRecord(11, 100_000_000, 100_000),  # 100ms
            BlockRecord(12, 200_000_000, 200_000),
            BlockRecord(13, 300_000_000, 300_000),
            BlockRecord(14, 400_000_000, 400_000),
        ]

    def test_warmup_dropped(self):
        stats = compute_stats(self.records(), warmup_blocks=1, cumulative_time_us=6_000_000)
        self.assertEqual(stats.n_blocks, 4)
        self.assertEqual(stats.warmup_dropped, 1)
        self.assertEqual(stats.total_gas, 1_000_000_000)
        self.assertAlmostEqual(stats.p50_ms, 250.0)
        # p90 of [100, 200, 300, 400] -> 370.0
        self.assertAlmostEqual(stats.p90_ms, 370.0)
        # 1 Ggas over 1.0s of latency
        self.assertAlmostEqual(stats.throughput_ggas_s, 1.0)
        # bench style: all 5 blocks' gas over cumulative 6s
        self.assertAlmostEqual(stats.bench_style_ggas_s, 1.001 / 6.0)

    def test_no_warmup(self):
        stats = compute_stats(self.records(), warmup_blocks=0, cumulative_time_us=6_000_000)
        self.assertEqual(stats.n_blocks, 5)
        # warmup block drags p90 up
        self.assertGreater(stats.p90_ms, 400.0)

    def test_warmup_exhausts_records(self):
        with self.assertRaises(RunDataError):
            compute_stats(self.records(), warmup_blocks=5, cumulative_time_us=1)


class TestDeltas(unittest.TestCase):
    def test_pct(self):
        self.assertAlmostEqual(delta_pct(556.38, 843.94), -34.07, places=1)
        self.assertAlmostEqual(delta_pct(1.70, 1.10), 54.5, places=1)

    def test_caption(self):
        self.assertEqual(delta_caption("p50_ms", -34.07), "34% lower")
        self.assertEqual(delta_caption("throughput_ggas_s", 53.2), "53% higher")

    def test_direction(self):
        self.assertTrue(is_improvement("p50_ms", -10))
        self.assertFalse(is_improvement("p50_ms", 10))
        self.assertTrue(is_improvement("throughput_ggas_s", 10))
        self.assertFalse(is_improvement("throughput_ggas_s", -10))


class TestBuildSummary(unittest.TestCase):
    def test_summary_with_missing_run(self):
        stats = compute_stats(
            [
                BlockRecord(11, 100_000_000, 100_000),
                BlockRecord(12, 200_000_000, 200_000),
            ],
            warmup_blocks=0,
            cumulative_time_us=300_000,
        )
        faster = compute_stats(
            [
                BlockRecord(11, 100_000_000, 50_000),
                BlockRecord(12, 200_000_000, 100_000),
            ],
            warmup_blocks=0,
            cumulative_time_us=150_000,
        )
        summary = build_summary(
            {"normal": {"base": stats, "new": faster, "broken": None}},
            {"normal": "Normal Blocks"},
            ["base", "new", "broken"],
        )
        self.assertEqual(summary["baseline"], "base")
        group = summary["groups"][0]
        self.assertEqual(group["label"], "Normal Blocks")
        base, new, broken = group["configs"]
        self.assertIsNone(base["deltas"])
        self.assertAlmostEqual(new["deltas"]["p50_ms"], -50.0)
        self.assertAlmostEqual(new["deltas"]["throughput_ggas_s"], 100.0)
        self.assertFalse(broken["valid"])


if __name__ == "__main__":
    unittest.main()
