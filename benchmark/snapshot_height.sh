#!/usr/bin/env bash
# Report the head block number of a snapshot datadir, by briefly starting the
# node on it (p2p disabled so it cannot advance) and querying eth_blockNumber.
#
# Use this to confirm all of a group's snapshots sit at the SAME height before
# benchmarking: the range is from..to and every config's node must start at
# exactly from-1. Run it once per (binary, snapshot); extra node args (e.g.
# --statedb.triedb) go after `--`.
#
# Usage:
#   benchmark/snapshot_height.sh <binary> <datadir> [--http-port N] [-- <extra node args>]
#
# Examples:
#   benchmark/snapshot_height.sh ./reth-bsc /snapshots/legacy-mdbx
#   benchmark/snapshot_height.sh ./reth-bsc /snapshots/legacy-triedb -- --statedb.triedb

set -euo pipefail

if [[ $# -lt 2 ]]; then
    grep '^#' "$0" | grep -v '^#!' | sed 's/^# \{0,1\}//'
    exit 1
fi

BINARY=$1
DATADIR=$2
shift 2

HTTP_PORT=8545
EXTRA_ARGS=()
while [[ $# -gt 0 ]]; do
    case "$1" in
        --http-port) HTTP_PORT=$2; shift 2 ;;
        --) shift; EXTRA_ARGS=("$@"); break ;;
        *) echo "unknown argument: $1" >&2; exit 1 ;;
    esac
done

[[ -x "$BINARY" ]] || { echo "error: binary not executable: $BINARY" >&2; exit 1; }
[[ -d "$DATADIR" ]] || { echo "error: datadir not found: $DATADIR" >&2; exit 1; }

NODE_PID=""
cleanup() {
    if [[ -n "$NODE_PID" ]] && kill -0 "$NODE_PID" 2>/dev/null; then
        kill -INT "$NODE_PID" 2>/dev/null || true
        for _ in $(seq 1 60); do
            kill -0 "$NODE_PID" 2>/dev/null || break
            sleep 1
        done
        kill -0 "$NODE_PID" 2>/dev/null && kill -KILL "$NODE_PID" 2>/dev/null || true
    fi
}
trap cleanup EXIT

echo "starting $BINARY on $DATADIR (p2p disabled) ..." >&2
"$BINARY" node --chain bsc --datadir "$DATADIR" \
    --http --http.port "$HTTP_PORT" \
    --disable-discovery --max-outbound-peers 0 --max-inbound-peers 0 \
    "${EXTRA_ARGS[@]}" >/tmp/snapshot_height_node.log 2>&1 &
NODE_PID=$!

HEX=""
for _ in $(seq 1 150); do
    kill -0 "$NODE_PID" 2>/dev/null || { echo "error: node exited early; see /tmp/snapshot_height_node.log" >&2; exit 1; }
    HEX=$(curl -s "localhost:$HTTP_PORT" -X POST -H 'Content-Type: application/json' \
        -d '{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}' \
        | sed -n 's/.*"result":"\(0x[0-9a-fA-F]*\)".*/\1/p')
    [[ -n "$HEX" ]] && break
    sleep 2
done

[[ -n "$HEX" ]] || { echo "error: RPC did not return a block number in time" >&2; exit 1; }

DEC=$((HEX))
echo "head block: $DEC ($HEX)"
