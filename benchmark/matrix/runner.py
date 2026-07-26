"""Run orchestration: snapshot restore, node lifecycle, bench invocation, validation.

One "run" is a (config, group) cell. The measured blocks are
`group.from_block ..= group.to_block`; the snapshot must be synced to exactly
`from_block - 1`, and the bench binary is invoked with `--from from_block - 1`
because reth-bench-bsc only uses the first block of its range to seed the loop
(blocks actually replayed are `--from + 1 ..= --to`).
"""

from __future__ import annotations

import hashlib
import json
import os
import re
import signal
import socket
import subprocess
import time
import urllib.request
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path

from .analysis import load_run
from .config import Group, MatrixConfig, NodeConfig


class RunError(Exception):
    pass


def preflight_binaries(
    binaries: list[tuple[str, str]], probe_version: bool = True
) -> tuple[list[str], dict[str, str]]:
    """Check that the binaries the config points at exist and are executable.

    `binaries` is a list of (label, path). Returns (problems, versions) where
    versions maps label -> first line of `<binary> --version` output.
    """
    problems: list[str] = []
    versions: dict[str, str] = {}
    for label, path in binaries:
        p = Path(path)
        if not p.is_file():
            problems.append(f"{label}: {path} does not exist")
            continue
        if not os.access(p, os.X_OK):
            problems.append(f"{label}: {path} is not executable")
            continue
        if probe_version:
            try:
                out = subprocess.run(
                    [str(p), "--version"], capture_output=True, text=True, timeout=15
                )
                if out.returncode != 0:
                    # Not every binary implements --version: reth-bench-bsc
                    # doesn't, and clap answers with a usage error. Record that
                    # plainly rather than storing the error text in meta.json
                    # where a version string is expected. Not a hard failure -
                    # the binary exists and runs, which is what preflight is for.
                    versions[label] = f"unknown (--version exited {out.returncode})"
                else:
                    first = (out.stdout or out.stderr).strip().splitlines()
                    versions[label] = first[0] if first else "unknown (no output)"
            except (subprocess.TimeoutExpired, OSError) as e:
                problems.append(f"{label}: {path} failed to run --version: {e}")
    return problems, versions


@dataclass
class RunResult:
    config: str
    group: str
    run_dir: Path
    valid: bool
    checks: dict[str, str] = field(default_factory=dict)  # check name -> "ok" | reason


def _now() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="seconds")


def _sha256(path: Path) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def _port_open(port: int, host: str = "127.0.0.1") -> bool:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.settimeout(0.5)
        return s.connect_ex((host, port)) == 0


def _rpc_block_number(port: int, timeout: float = 2.0) -> int | None:
    req = urllib.request.Request(
        f"http://127.0.0.1:{port}",
        data=json.dumps(
            {"jsonrpc": "2.0", "id": 1, "method": "eth_blockNumber", "params": []}
        ).encode(),
        headers={"Content-Type": "application/json"},
    )
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            body = json.load(resp)
        return int(body["result"], 16)
    except Exception:
        return None


class Runner:
    def __init__(
        self,
        cfg: MatrixConfig,
        results_root: Path,
        dry_run: bool = False,
        no_restore: bool = False,
    ):
        self.cfg = cfg
        self.g = cfg.global_
        self.results_root = results_root
        self.dry_run = dry_run
        # Skip snapshot restore; the datadir must already be at from-1 (the
        # head check in wait_ready still enforces this).
        self.no_restore = no_restore
        self.pidfile = results_root / "node.pid"

    # -- helpers ---------------------------------------------------------------

    def _log(self, msg: str) -> None:
        print(f"[{datetime.now().strftime('%H:%M:%S')}] {msg}", flush=True)

    def _exec(self, argv: list[str], **kwargs) -> subprocess.CompletedProcess | None:
        if self.dry_run:
            self._log(f"DRY-RUN exec: {' '.join(argv)}")
            return None
        return subprocess.run(argv, **kwargs)

    def node_argv(self, config: NodeConfig, group: Group) -> list[str]:
        return [
            config.binary,
            "node",
            "--chain",
            self.g.chain,
            "--datadir",
            config.datadir,
            "--http",
            "--http.port",
            str(self.g.http_port),
            "--authrpc.port",
            str(self.g.authrpc_port),
            "--authrpc.jwtsecret",
            self.g.jwt_secret,
            # Note: `--ipcpath`, not `--ipc.path`. reth derives this flag from
            # the struct field name (RpcServerArgs::ipcpath) with no `long =`
            # override, so the dotted form is rejected outright by every
            # revision, old and new.
            "--ipcpath",
            self.g.ipc_path,
            *self.g.isolation_args,
            *config.extra_node_args,
        ]

    def bench_argv(self, group: Group, run_dir: Path) -> list[str]:
        return [
            self.g.bench_bin,
            "forkchoice-only",
            "--rpc-url",
            group.rpc_url,
            # reth-bench replays --from+1 ..= --to; measured blocks are from..=to.
            "--from",
            str(group.from_block - 1),
            "--to",
            str(group.to_block),
            "--jwt-secret",
            self.g.jwt_secret,
            "--engine-rpc-url",
            f"http://127.0.0.1:{self.g.authrpc_port}",
            "--output",
            str(run_dir),
        ]

    # -- lifecycle steps -------------------------------------------------------

    def kill_leftover(self) -> None:
        """Kill a node we started in a previous (failed) invocation, if any."""
        if self.dry_run:
            self._log("checking for leftover node (pidfile + port probe)")
            return
        if self.pidfile.exists():
            try:
                pid = int(self.pidfile.read_text().strip())
                os.kill(pid, signal.SIGKILL)
                self._log(f"killed leftover node pid {pid} from previous run")
                time.sleep(2)
            except (ValueError, ProcessLookupError):
                pass
            self.pidfile.unlink(missing_ok=True)
        for port in (self.g.http_port, self.g.authrpc_port):
            if _port_open(port):
                raise RunError(
                    f"port {port} is already in use by a process this tool did not start; "
                    "stop it or change the port in the config"
                )

    def restore_snapshot(self, snapshot: str, datadir: str) -> None:
        if not self.dry_run:
            if not Path(snapshot).is_dir():
                raise RunError(f"snapshot directory does not exist: {snapshot}")
            Path(datadir).mkdir(parents=True, exist_ok=True)
        rsync = ["rsync", "-a", "--delete", f"{snapshot.rstrip('/')}/", f"{datadir.rstrip('/')}/"]
        self._log(f"restoring snapshot: {' '.join(rsync)}")
        proc = self._exec(rsync)
        if proc is not None and proc.returncode != 0:
            raise RunError(f"rsync failed with exit code {proc.returncode}")

    def start_node(self, config: NodeConfig, group: Group, node_log: Path) -> subprocess.Popen | None:
        argv = self.node_argv(config, group)
        self._log(f"starting node: {' '.join(argv)}")
        if self.dry_run:
            self._log(f"DRY-RUN node log would be: {node_log}")
            return None
        log_f = open(node_log, "wb")
        proc = subprocess.Popen(argv, stdout=log_f, stderr=subprocess.STDOUT)
        self.pidfile.write_text(str(proc.pid))
        return proc

    def wait_ready(self, proc: subprocess.Popen | None, expected_head: int) -> None:
        self._log(
            f"waiting for node RPC on port {self.g.http_port} to report head {expected_head} "
            f"(timeout {self.g.node_ready_timeout_secs}s)"
        )
        if self.dry_run:
            return
        deadline = time.monotonic() + self.g.node_ready_timeout_secs
        while time.monotonic() < deadline:
            if proc is not None and proc.poll() is not None:
                raise RunError(f"node exited during startup with code {proc.returncode}")
            head = _rpc_block_number(self.g.http_port)
            if head is not None:
                if head == expected_head:
                    self._log(f"node ready at head {head}")
                    return
                if head > expected_head:
                    raise RunError(
                        f"node head {head} is beyond expected {expected_head}: "
                        "snapshot was synced past the benchmark range"
                    )
            time.sleep(2)
        raise RunError(f"node did not reach head {expected_head} within timeout")

    def run_bench(self, group: Group, run_dir: Path, bench_log: Path) -> None:
        argv = self.bench_argv(group, run_dir)
        self._log(f"running bench: {' '.join(argv)}")
        if self.dry_run:
            return
        with open(bench_log, "wb") as log_f:
            proc = subprocess.run(argv, stdout=log_f, stderr=subprocess.STDOUT)
        if proc.returncode != 0:
            raise RunError(f"bench exited with code {proc.returncode}; see {bench_log}")

    def stop_node(self, proc: subprocess.Popen | None) -> bool:
        """SIGINT the node and wait; returns True on clean exit."""
        if self.dry_run or proc is None:
            self._log("stopping node (SIGINT)")
            return True
        if proc.poll() is not None:
            self.pidfile.unlink(missing_ok=True)
            return proc.returncode == 0
        self._log("stopping node (SIGINT)")
        proc.send_signal(signal.SIGINT)
        try:
            proc.wait(timeout=self.g.node_shutdown_timeout_secs)
            clean = proc.returncode == 0
        except subprocess.TimeoutExpired:
            self._log("node did not exit in time; sending SIGKILL")
            proc.kill()
            proc.wait()
            clean = False
        self.pidfile.unlink(missing_ok=True)
        return clean

    # -- validation --------------------------------------------------------------

    def validate(self, group: Group, run_dir: Path, node_log: Path, clean_exit: bool) -> dict[str, str]:
        checks: dict[str, str] = {}

        try:
            records = load_run(run_dir)
            checks["csv_join"] = "ok"
            expected = group.block_count
            if len(records) != expected:
                checks["block_coverage"] = (
                    f"expected {expected} rows, got {len(records)}"
                )
            elif records[0].block_number != group.from_block:
                checks["block_coverage"] = (
                    f"first block {records[0].block_number} != {group.from_block}"
                )
            elif records[-1].block_number != group.to_block:
                checks["block_coverage"] = (
                    f"last block {records[-1].block_number} != {group.to_block}"
                )
            else:
                checks["block_coverage"] = "ok"
        except Exception as e:
            checks["csv_join"] = str(e)
            checks["block_coverage"] = "skipped (csv_join failed)"

        patterns = [re.compile(p) for p in self.g.error_log_patterns]
        matches = []
        if node_log.is_file():
            with open(node_log, errors="replace") as f:
                for line in f:
                    if any(p.search(line) for p in patterns):
                        matches.append(line.strip())
                        if len(matches) >= 5:
                            break
            checks["node_log"] = "ok" if not matches else f"matched: {matches[:5]}"
        else:
            checks["node_log"] = "node.log missing"

        checks["clean_exit"] = "ok" if clean_exit else "node did not exit cleanly"
        return checks

    # -- one cell ------------------------------------------------------------------

    def run_one(self, config: NodeConfig, group: Group) -> RunResult:
        run_dir = self.results_root / "runs" / group.name / config.name
        if not self.dry_run:
            run_dir.mkdir(parents=True, exist_ok=True)
        node_log = run_dir / "node.log"
        bench_log = run_dir / "bench.log"
        started = _now()
        self._log(f"=== run: group={group.name} config={config.name} ===")

        snapshot = config.snapshots.get(group.name)
        if snapshot is None and not self.no_restore:
            raise RunError(
                f"config {config.name!r} has no snapshot for group {group.name!r} "
                "(or pass --no-restore if the datadir is already at from-1)"
            )

        proc = None
        clean_exit = False
        try:
            self.kill_leftover()
            if self.no_restore:
                self._log(
                    f"snapshot restore skipped (--no-restore); datadir {config.datadir} "
                    f"must already be at block {group.from_block - 1}"
                )
            else:
                self.restore_snapshot(snapshot, config.datadir)
            proc = self.start_node(config, group, node_log)
            self.wait_ready(proc, expected_head=group.from_block - 1)
            self.run_bench(group, run_dir, bench_log)
        finally:
            clean_exit = self.stop_node(proc)

        if self.dry_run:
            return RunResult(config.name, group.name, run_dir, valid=False,
                             checks={"dry_run": "no checks executed"})

        checks = self.validate(group, run_dir, node_log, clean_exit)
        valid = all(v == "ok" for v in checks.values())

        run_meta = {
            "config": config.name,
            "group": group.name,
            "valid": valid,
            "checks": checks,
            "binary": config.binary,
            "binary_sha256": _sha256(Path(config.binary)) if Path(config.binary).is_file() else None,
            "node_args": self.node_argv(config, group),
            "bench_args": self.bench_argv(group, run_dir),
            "snapshot": None if self.no_restore else snapshot,
            "restore_skipped": self.no_restore,
            "started": started,
            "finished": _now(),
        }
        (run_dir / "run.json").write_text(json.dumps(run_meta, indent=2) + "\n")
        self._log(f"run {'VALID' if valid else 'INVALID'}: {json.dumps(checks)}")
        return RunResult(config.name, group.name, run_dir, valid, checks)

    # -- matrix ------------------------------------------------------------------

    def _update_meta(self, configs: list[NodeConfig], groups: list[Group]) -> None:
        """Merge this invocation's cells into meta.json.

        Runs can be accumulated one at a time into the same results dir; the
        config/group order always follows the config file so the baseline (the
        first [[configs]] entry) is stable regardless of run order.
        """
        meta_path = self.results_root / "meta.json"
        prev = json.loads(meta_path.read_text()) if meta_path.is_file() else {}
        ran_configs = set(prev.get("configs", [])) | {c.name for c in configs}
        ran_groups = set(prev.get("groups", [])) | {g.name for g in groups}
        meta = {
            "started": prev.get("started", _now()),
            "updated": _now(),
            "configs": [c.name for c in self.cfg.configs if c.name in ran_configs],
            "groups": [g.name for g in self.cfg.groups if g.name in ran_groups],
            "warmup_blocks": self.g.warmup_blocks,
            "chain": self.g.chain,
            "group_labels": {g.name: g.label for g in self.cfg.groups},
            "binary_versions": prev.get("binary_versions", {}),
        }
        meta_path.write_text(json.dumps(meta, indent=2) + "\n")

    def run_matrix(
        self, config_names: list[str] | None = None, group_names: list[str] | None = None
    ) -> list[RunResult]:
        configs = [self.cfg.config(n) for n in config_names] if config_names else self.cfg.configs
        groups = [self.cfg.group(n) for n in group_names] if group_names else self.cfg.groups

        if not self.dry_run:
            self.results_root.mkdir(parents=True, exist_ok=True)
            self._update_meta(configs, groups)

        results = []
        for group in groups:
            for config in configs:
                try:
                    results.append(self.run_one(config, group))
                except RunError as e:
                    self._log(f"run FAILED (group={group.name} config={config.name}): {e}")
                    results.append(
                        RunResult(
                            config.name,
                            group.name,
                            self.results_root / "runs" / group.name / config.name,
                            valid=False,
                            checks={"run_error": str(e)},
                        )
                    )
        return results
