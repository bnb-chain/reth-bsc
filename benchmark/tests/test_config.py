import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from matrix.config import ConfigError, load_config

VALID = """
[global]
jwt_secret = "/tmp/jwt.hex"
output_dir = "results"
bench_bin  = "reth-bench-bsc"

[[configs]]
name    = "base"
binary  = "/bin/a"
datadir = "/data/d"
[configs.snapshots]
g1 = "/snap/base-g1"

[[configs]]
name    = "new"
binary  = "/bin/b"
datadir = "/data/d"
extra_node_args = ["--statedb.triedb"]

[[groups]]
name    = "g1"
label   = "Group One"
rpc_url = "http://src:8545"
from    = 100
to      = 110
"""


def load_str(content: str):
    with tempfile.NamedTemporaryFile("w", suffix=".toml", delete=False) as f:
        f.write(content)
        path = f.name
    try:
        return load_config(path)
    finally:
        Path(path).unlink()


class TestConfig(unittest.TestCase):
    def test_valid(self):
        cfg = load_str(VALID)
        self.assertEqual(cfg.baseline.name, "base")
        self.assertEqual(cfg.configs[1].extra_node_args, ["--statedb.triedb"])
        self.assertEqual(cfg.groups[0].block_count, 11)
        self.assertEqual(cfg.groups[0].label, "Group One")
        self.assertEqual(cfg.config("new").binary, "/bin/b")
        self.assertEqual(cfg.global_.authrpc_port, 8551)  # default
        self.assertEqual(cfg.configs[0].snapshots["g1"], "/snap/base-g1")

    def test_label_defaults_to_name(self):
        cfg = load_str(VALID.replace('label   = "Group One"\n', ""))
        self.assertEqual(cfg.groups[0].label, "g1")

    def test_missing_key(self):
        with self.assertRaises(ConfigError):
            load_str(VALID.replace('rpc_url = "http://src:8545"\n', ""))

    def test_duplicate_config_names(self):
        with self.assertRaises(ConfigError):
            load_str(VALID.replace('name    = "new"', 'name    = "base"'))

    def test_inverted_range(self):
        with self.assertRaises(ConfigError):
            load_str(VALID.replace("to      = 110", "to      = 10"))

    def test_no_groups(self):
        with self.assertRaises(ConfigError):
            load_str(VALID.split("[[groups]]")[0])

    def test_example_config_parses(self):
        example = Path(__file__).resolve().parents[1] / "config.example.toml"
        cfg = load_config(example)
        self.assertEqual([c.name for c in cfg.configs], ["legacy-mdbx", "legacy-triedb", "storage-v2"])
        self.assertEqual([g.name for g in cfg.groups], ["big", "normal"])
        for c in cfg.configs:
            for g in cfg.groups:
                self.assertIn(g.name, c.snapshots)


if __name__ == "__main__":
    unittest.main()
