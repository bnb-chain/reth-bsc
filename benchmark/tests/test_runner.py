import os
import stat
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import json

from matrix.config import GlobalConfig, Group, MatrixConfig, NodeConfig
from matrix.runner import Runner, preflight_binaries


class TestPreflight(unittest.TestCase):
    def test_missing_binary(self):
        problems, versions = preflight_binaries(
            [("bench_bin", "/nonexistent/reth-bench-bsc")], probe_version=False
        )
        self.assertEqual(len(problems), 1)
        self.assertIn("does not exist", problems[0])
        self.assertEqual(versions, {})

    def test_not_executable(self):
        with tempfile.NamedTemporaryFile(delete=False) as f:
            path = f.name
        os.chmod(path, stat.S_IRUSR)
        try:
            problems, _ = preflight_binaries([("config 'x'", path)], probe_version=False)
        finally:
            Path(path).unlink()
        self.assertEqual(len(problems), 1)
        self.assertIn("not executable", problems[0])

    def test_ok_with_version(self):
        with tempfile.NamedTemporaryFile("w", delete=False, suffix=".sh") as f:
            f.write("#!/bin/sh\necho fake-reth 1.2.3\n")
            path = f.name
        os.chmod(path, 0o755)
        try:
            problems, versions = preflight_binaries([("config 'a'", path)])
        finally:
            Path(path).unlink()
        self.assertEqual(problems, [])
        self.assertEqual(versions["config 'a'"], "fake-reth 1.2.3")


class TestNoRestore(unittest.TestCase):
    def make_cfg(self, with_snapshot: bool) -> MatrixConfig:
        snapshots = {"g1": "/snap/a-g1"} if with_snapshot else {}
        return MatrixConfig(
            global_=GlobalConfig(jwt_secret="j", output_dir="o", bench_bin="b"),
            configs=[NodeConfig("a", "/bin/a", "/data", snapshots=snapshots)],
            groups=[Group("g1", "Group One", "http://x", 100, 110)],
        )

    def test_missing_snapshot_rejected_without_flag(self):
        from matrix.runner import RunError

        cfg = self.make_cfg(with_snapshot=False)
        with tempfile.TemporaryDirectory() as d:
            runner = Runner(cfg, Path(d), dry_run=True)
            with self.assertRaises(RunError):
                runner.run_one(cfg.configs[0], cfg.groups[0])

    def test_missing_snapshot_ok_with_no_restore(self):
        cfg = self.make_cfg(with_snapshot=False)
        with tempfile.TemporaryDirectory() as d:
            runner = Runner(cfg, Path(d), dry_run=True, no_restore=True)
            result = runner.run_one(cfg.configs[0], cfg.groups[0])
        self.assertIn("dry_run", result.checks)


class TestArgv(unittest.TestCase):
    """Guards the exact spelling of node/driver flags.

    These are passed straight to binaries we don't build here, so a wrong
    spelling only surfaces as a failed run on the benchmark machine. The IPC
    flag already cost one: `--ipc.path` is rejected by every reth revision,
    which derives it from the field name as `--ipcpath`.
    """

    def make_runner(self):
        cfg = MatrixConfig(
            global_=GlobalConfig(
                jwt_secret="/jwt.hex",
                output_dir="o",
                bench_bin="/bin/bench",
                ipc_path="/tmp/reth-bench.ipc",
                http_port=8545,
                authrpc_port=8551,
            ),
            configs=[NodeConfig("a", "/bin/a", "/data", extra_node_args=["--statedb.triedb"])],
            groups=[Group("g1", "Group One", "http://x", 100, 110)],
        )
        return Runner(cfg, Path("/tmp"), dry_run=True), cfg

    def test_node_argv_ipc_flag_spelling(self):
        runner, cfg = self.make_runner()
        argv = runner.node_argv(cfg.configs[0], cfg.groups[0])
        self.assertIn("--ipcpath", argv)
        self.assertNotIn("--ipc.path", argv)
        self.assertEqual(argv[argv.index("--ipcpath") + 1], "/tmp/reth-bench.ipc")

    def test_node_argv_carries_extra_args_last(self):
        runner, cfg = self.make_runner()
        argv = runner.node_argv(cfg.configs[0], cfg.groups[0])
        self.assertEqual(argv[-1], "--statedb.triedb")

    def test_bench_argv_replays_from_minus_one(self):
        # reth-bench replays --from+1 ..= --to, so --from must be from_block-1
        # or the first measured block is silently skipped.
        runner, cfg = self.make_runner()
        argv = runner.bench_argv(cfg.groups[0], Path("/tmp/run"))
        self.assertEqual(argv[argv.index("--from") + 1], "99")
        self.assertEqual(argv[argv.index("--to") + 1], "110")


class TestMetaMerge(unittest.TestCase):
    def make_cfg(self) -> MatrixConfig:
        return MatrixConfig(
            global_=GlobalConfig(jwt_secret="j", output_dir="o", bench_bin="b"),
            configs=[
                NodeConfig("baseline", "/bin/a", "/data"),
                NodeConfig("new", "/bin/b", "/data"),
            ],
            groups=[
                Group("g1", "Group One", "http://x", 1, 2),
                Group("g2", "Group Two", "http://x", 3, 4),
            ],
        )

    def test_one_cell_at_a_time_merges_in_config_order(self):
        cfg = self.make_cfg()
        with tempfile.TemporaryDirectory() as d:
            runner = Runner(cfg, Path(d))
            # run the *last* config first: order in meta must still follow the
            # config file so the baseline stays stable
            runner._update_meta([cfg.configs[1]], [cfg.groups[1]])
            meta = json.loads((Path(d) / "meta.json").read_text())
            self.assertEqual(meta["configs"], ["new"])
            self.assertEqual(meta["groups"], ["g2"])

            runner._update_meta([cfg.configs[0]], [cfg.groups[0]])
            meta = json.loads((Path(d) / "meta.json").read_text())
            self.assertEqual(meta["configs"], ["baseline", "new"])
            self.assertEqual(meta["groups"], ["g1", "g2"])
            self.assertEqual(meta["group_labels"]["g1"], "Group One")

    def test_rerun_is_idempotent(self):
        cfg = self.make_cfg()
        with tempfile.TemporaryDirectory() as d:
            runner = Runner(cfg, Path(d))
            runner._update_meta(cfg.configs, cfg.groups)
            runner._update_meta([cfg.configs[0]], [cfg.groups[0]])
            meta = json.loads((Path(d) / "meta.json").read_text())
            self.assertEqual(meta["configs"], ["baseline", "new"])
            self.assertEqual(meta["groups"], ["g1", "g2"])


if __name__ == "__main__":
    unittest.main()
