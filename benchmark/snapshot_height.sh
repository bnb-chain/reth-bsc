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

NODE_LOG=$(mktemp "${TMPDIR:-/tmp}/snapshot_height_node.XXXXXX")
echo "starting $BINARY on $DATADIR (p2p disabled); node log -> $NODE_LOG" >&2
# The `+` guard keeps an empty EXTRA_ARGS from tripping `set -u` on bash < 4.4
# (e.g. macOS system bash 3.2); it expands to zero words when unset.
"$BINARY" node --chain bsc --datadir "$DATADIR" \
    --http --http.port "$HTTP_PORT" \
    --disable-discovery --max-outbound-peers 0 --max-inbound-peers 0 \
    ${EXTRA_ARGS[@]+"${EXTRA_ARGS[@]}"} >"$NODE_LOG" 2>&1 &
NODE_PID=$!

MAX_TRIES=150
HEX=""
for i in $(seq 1 $MAX_TRIES); do
    if ! kill -0 "$NODE_PID" 2>/dev/null; then
        echo "error: node exited early. Last log lines:" >&2
        tail -n 20 "$NODE_LOG" >&2
        exit 1
    fi
    # `|| true`: while the node is still opening its RPC port, curl exits
    # non-zero (connection refused). Without this the failed command
    # substitution would trip `set -e` and kill the script on the first try
    # instead of retrying.
    HEX=$(curl -s "localhost:$HTTP_PORT" -X POST -H 'Content-Type: application/json' \
        -d '{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}' \
        | sed -n 's/.*"result":"\(0x[0-9a-fA-F]\{1,\}\)".*/\1/p') || true
    [[ -n "$HEX" ]] && break
    # periodic heartbeat so a slow-starting node isn't mistaken for a hang
    (( i % 5 == 0 )) && echo "  ... still waiting for RPC on port $HTTP_PORT (${i}/${MAX_TRIES})" >&2
    sleep 2
done

if [[ -z "$HEX" ]]; then
    echo "error: RPC on port $HTTP_PORT did not return a block number after ~$((MAX_TRIES * 2))s." >&2
    echo "Last log lines:" >&2
    tail -n 20 "$NODE_LOG" >&2
    exit 1
fi

DEC=$((HEX))
echo "head block: $DEC ($HEX)"
