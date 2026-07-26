"""TOML config loading and validation for the benchmark matrix."""

from __future__ import annotations

import tomllib
from dataclasses import dataclass, field
from pathlib import Path


class ConfigError(Exception):
    pass


@dataclass
class GlobalConfig:
    jwt_secret: str
    output_dir: str
    bench_bin: str
    chain: str = "bsc"
    authrpc_port: int = 8551
    http_port: int = 8545
    ipc_path: str = "/tmp/reth-bench.ipc"
    warmup_blocks: int = 2
    node_ready_timeout_secs: int = 300
    node_shutdown_timeout_secs: int = 120
    error_log_patterns: list[str] = field(default_factory=lambda: ["state root", "ERROR"])
    isolation_args: list[str] = field(
        default_factory=lambda: [
            "--disable-discovery",
            "--max-outbound-peers",
            "0",
            "--max-inbound-peers",
            "0",
        ]
    )


@dataclass
class NodeConfig:
    name: str
    binary: str
    datadir: str
    extra_node_args: list[str] = field(default_factory=list)
    snapshots: dict[str, str] = field(default_factory=dict)


@dataclass
class Group:
    name: str
    label: str
    rpc_url: str
    # Blocks measured are `from_block..=to_block` inclusive. The snapshot for this
    # group must be synced to exactly `from_block - 1`.
    from_block: int
    to_block: int

    @property
    def block_count(self) -> int:
        return self.to_block - self.from_block + 1


@dataclass
class MatrixConfig:
    global_: GlobalConfig
    configs: list[NodeConfig]
    groups: list[Group]

    @property
    def baseline(self) -> NodeConfig:
        return self.configs[0]

    def config(self, name: str) -> NodeConfig:
        for c in self.configs:
            if c.name == name:
                return c
        raise ConfigError(f"unknown config {name!r}")

    def group(self, name: str) -> Group:
        for g in self.groups:
            if g.name == name:
                return g
        raise ConfigError(f"unknown group {name!r}")


def _require(table: dict, key: str, where: str):
    if key not in table:
        raise ConfigError(f"missing required key {key!r} in {where}")
    return table[key]


def _contains(parent: Path, child: Path) -> bool:
    """True if `child` lies inside `parent`. Both must already be resolved."""
    return parent in child.parents


def load_config(path: str | Path) -> MatrixConfig:
    path = Path(path)
    with open(path, "rb") as f:
        raw = tomllib.load(f)

    g = raw.get("global", {})
    global_ = GlobalConfig(
        jwt_secret=_require(g, "jwt_secret", "[global]"),
        output_dir=_require(g, "output_dir", "[global]"),
        bench_bin=_require(g, "bench_bin", "[global]"),
        **{
            k: g[k]
            for k in (
                "chain",
                "authrpc_port",
                "http_port",
                "ipc_path",
                "warmup_blocks",
                "node_ready_timeout_secs",
                "node_shutdown_timeout_secs",
                "error_log_patterns",
                "isolation_args",
            )
            if k in g
        },
    )

    configs = []
    for i, c in enumerate(raw.get("configs", [])):
        where = f"[[configs]] #{i + 1}"
        configs.append(
            NodeConfig(
                name=_require(c, "name", where),
                binary=_require(c, "binary", where),
                datadir=_require(c, "datadir", where),
                extra_node_args=list(c.get("extra_node_args", [])),
                snapshots=dict(c.get("snapshots", {})),
            )
        )
    if not configs:
        raise ConfigError("at least one [[configs]] entry is required")
    names = [c.name for c in configs]
    if len(set(names)) != len(names):
        raise ConfigError(f"duplicate config names: {names}")

    # A snapshot must never be the working datadir, or live inside one.
    #
    # Restoring is `rsync -a --delete <snapshot>/ <datadir>/`, and a run leaves
    # the datadir advanced to the group's `to`. So pointing both at one path
    # silently consumes the snapshot on the first run - and once a second cell
    # restores over that path, --delete removes what's left. Preparing a
    # snapshot costs hours of resync, so refuse the config instead of
    # discovering it afterwards.
    for c in configs:
        datadir = Path(c.datadir).expanduser().resolve()
        for group_name, snapshot in c.snapshots.items():
            snap = Path(snapshot).expanduser().resolve()
            where = f"config '{c.name}' group '{group_name}'"
            if snap == datadir:
                raise ConfigError(
                    f"{where}: snapshot and datadir are the same path ({snap}). "
                    "A run advances the datadir, which would destroy the snapshot."
                )
            if _contains(snap, datadir) or _contains(datadir, snap):
                raise ConfigError(
                    f"{where}: snapshot ({snap}) and datadir ({datadir}) are nested. "
                    "The restore would delete one while reading the other."
                )

    groups = []
    for i, grp in enumerate(raw.get("groups", [])):
        where = f"[[groups]] #{i + 1}"
        from_block = _require(grp, "from", where)
        to_block = _require(grp, "to", where)
        if to_block < from_block:
            raise ConfigError(f"{where}: to ({to_block}) < from ({from_block})")
        groups.append(
            Group(
                name=_require(grp, "name", where),
                label=grp.get("label", grp["name"]),
                rpc_url=_require(grp, "rpc_url", where),
                from_block=from_block,
                to_block=to_block,
            )
        )
    if not groups:
        raise ConfigError("at least one [[groups]] entry is required")
    gnames = [grp.name for grp in groups]
    if len(set(gnames)) != len(gnames):
        raise ConfigError(f"duplicate group names: {gnames}")

    if global_.warmup_blocks < 0:
        raise ConfigError("warmup_blocks must be >= 0")

    return MatrixConfig(global_=global_, configs=configs, groups=groups)
