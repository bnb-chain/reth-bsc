import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from matrix.chart import render_chart_html


def make_summary(n_configs: int = 3) -> dict:
    names = ["legacy-mdbx", "legacy-triedb", "storage-v2"][:n_configs]
    configs = []
    for i, name in enumerate(names):
        entry = {
            "name": name,
            "valid": True,
            "n_blocks": 498,
            "warmup_dropped": 2,
            "total_gas": 42_000_000_000,
            "p50_ms": 33.79 - i * 5,
            "p90_ms": 52.10 - i * 8,
            "throughput_ggas_s": 0.83 + i * 0.2,
            "bench_style_ggas_s": 0.82 + i * 0.2,
            "deltas": None
            if i == 0
            else {
                "p50_ms": -10.0 * i,
                "p90_ms": -12.0 * i,
                "throughput_ggas_s": 20.0 * i,
            },
        }
        configs.append(entry)
    return {
        "baseline": names[0],
        "groups": [
            {"name": "normal", "label": "Normal Blocks: 500 blocks", "configs": configs},
            {"name": "big", "label": "Big Blocks: 48 blocks ~1 Ggas", "configs": configs},
        ],
    }


class TestChart(unittest.TestCase):
    def test_three_configs(self):
        html = render_chart_html(make_summary(3), title="test chart")
        self.assertIn("<!doctype html>", html)
        self.assertIn("test chart", html)
        for name in ("legacy-mdbx", "legacy-triedb", "storage-v2"):
            self.assertIn(name, html)
        self.assertIn("Normal Blocks: 500 blocks", html)
        self.assertIn("Big Blocks: 48 blocks ~1 Ggas", html)
        self.assertIn("% lower", html)
        self.assertIn("% higher", html)
        self.assertIn("prefers-color-scheme: dark", html)
        # accent green and baseline grey both present as bar fills
        self.assertIn("#008300", html)
        self.assertIn("#c3c2b7", html)

    def test_two_configs(self):
        html = render_chart_html(make_summary(2))
        self.assertIn("legacy-triedb", html)
        self.assertNotIn("storage-v2", html)

    def test_invalid_config_rendered(self):
        summary = make_summary(3)
        summary["groups"][0]["configs"][2] = {"name": "storage-v2", "valid": False}
        html = render_chart_html(summary)
        self.assertIn("invalid run", html)

    def test_escaping(self):
        summary = make_summary(2)
        summary["groups"][0]["label"] = 'Blocks <script>alert("x")</script>'
        html = render_chart_html(summary)
        self.assertNotIn('<script>alert("x")</script>', html)


if __name__ == "__main__":
    unittest.main()
