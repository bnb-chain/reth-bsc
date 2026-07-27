#!/usr/bin/env bash
# Run one benchmark cell, after checking everything it depends on.
#
# A cell runs for hours on a datadir that is expensive to re-prepare, so this
# gates the run on preflight.py: source node serves the range, datadir is at the
# right height, driver supports the configured mode, no stray nodes competing.
#
# Usage:
#   benchmark/run_cell.sh <config> <group> [extra bench.py args...]
#
# Examples:
#   benchmark/run_cell.sh legacy-mdbx pilot
#   benchmark/run_cell.sh legacy-triedb main --results-dir /server/bench-results/main
#
# Env:
#   BENCH_CONFIG   path to config.toml (default: benchmark/config.toml)
#   SKIP_PREFLIGHT set to 1 to bypass the checks (not advised)

set -euo pipefail

if [[ $# -lt 2 ]]; then
    grep '^#' "$0" | grep -v '^#!' | sed 's/^# \{0,1\}//'
    exit 1
fi

CONFIG_NAME=$1
GROUP=$2
shift 2

REPO_ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
BENCH_CONFIG=${BENCH_CONFIG:-$REPO_ROOT/benchmark/config.toml}

[[ -f "$BENCH_CONFIG" ]] || { echo "error: no config at $BENCH_CONFIG" >&2; exit 1; }

if [[ "${SKIP_PREFLIGHT:-0}" != "1" ]]; then
    echo "=== preflight: $CONFIG_NAME / $GROUP ==="
    # --skip-height would defeat the point: the wrong head is the failure this
    # is most likely to catch, and the harness would otherwise sit in its
    # node_ready timeout for up to two hours before giving up.
    python3 "$REPO_ROOT/benchmark/preflight.py" \
        --config "$BENCH_CONFIG" --group "$GROUP" --configs "$CONFIG_NAME"
fi

echo "=== running: $CONFIG_NAME / $GROUP ==="
# --no-restore: snapshots are multi-TB here, so cells run directly on their
# prepared datadirs rather than being rsynced from a pristine copy.
exec python3 "$REPO_ROOT/benchmark/bench.py" run \
    --config "$BENCH_CONFIG" \
    --no-restore \
    --configs "$CONFIG_NAME" \
    --groups "$GROUP" \
    "$@"
