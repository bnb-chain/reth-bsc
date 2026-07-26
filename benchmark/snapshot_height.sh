#!/usr/bin/env bash
# Report the head block number of a snapshot datadir.
#
# Use this to confirm all of a group's snapshots sit at the SAME height before
# benchmarking: the range is from..to and every config's node must start at
# exactly from-1.
#
# Default (offline) mode reads the stage checkpoints out of mdbx with
# `<binary> db list StageCheckpoints`. That path opens mdbx read-only, so it
# takes no storage lock, and it never touches the triedb/pathdb state backend -
# it returns in seconds regardless of backend, and it also reports when a
# snapshot's stages disagree (a half-unwound datadir).
#
# --via-rpc boots the node with p2p disabled and calls eth_blockNumber instead.
# That is far slower and more invasive: it takes the mdbx read-WRITE lock, and
# on a triedb snapshot the RocksDB open runs with max_open_files=-1, so it
# builds table readers for every SST before emitting a single log line - many
# minutes on a mainnet-sized datadir, with no progress output. Extra node args
# (e.g. --statedb.triedb) go after `--` and only apply in this mode.
#
# --stages dumps every stage checkpoint. Worth doing once per snapshot group:
# two datadirs at the same height but with different stage profiles (e.g. one
# fastnode, one not) are not a like-for-like benchmark.
#
# Usage:
#   benchmark/snapshot_height.sh <binary> <datadir> [--chain NAME] [--stages]
#       [--via-rpc] [--http-port N] [--wait-secs N] [-- <extra node args>]
#
# Examples:
#   benchmark/snapshot_height.sh ./reth-bsc /snapshots/legacy-mdbx
#   benchmark/snapshot_height.sh ./reth-bsc /snapshots/legacy-triedb --stages
#   benchmark/snapshot_height.sh ./reth-bsc /snapshots/legacy-triedb \
#       --via-rpc --wait-secs 1800 -- --statedb.triedb

set -euo pipefail

if [[ $# -lt 2 ]]; then
    grep '^#' "$0" | grep -v '^#!' | sed 's/^# \{0,1\}//'
    exit 1
fi

BINARY=$1
DATADIR=$2
shift 2

CHAIN=bsc
VIA_RPC=0
SHOW_STAGES=0
HTTP_PORT=8545
WAIT_SECS=300
EXTRA_ARGS=()
while [[ $# -gt 0 ]]; do
    case "$1" in
        --chain) CHAIN=$2; shift 2 ;;
        --stages) SHOW_STAGES=1; shift ;;
        --via-rpc) VIA_RPC=1; shift ;;
        --http-port) HTTP_PORT=$2; shift 2 ;;
        --wait-secs) WAIT_SECS=$2; shift 2 ;;
        --) shift; EXTRA_ARGS=("$@"); break ;;
        *) echo "unknown argument: $1" >&2; exit 1 ;;
    esac
done

[[ -x "$BINARY" ]] || { echo "error: binary not executable: $BINARY" >&2; exit 1; }
[[ -d "$DATADIR" ]] || { echo "error: datadir not found: $DATADIR" >&2; exit 1; }

# ---------------------------------------------------------------------------
# Who else is on this datadir?
#
# reth writes "<pid>\n<start_time>" to <datadir>/db/lock when it opens mdbx
# read-write, and refuses to start while a process with that exact pid AND
# start time is alive (crates/storage/db/src/lockfile.rs). A killed node leaves
# the file behind, which reth correctly ignores - so only report a holder whose
# pid still resolves to a live reth-ish process, otherwise a recycled pid would
# produce a bogus warning.
# ---------------------------------------------------------------------------
lock_holder_pid() {
    local lock_file="$DATADIR/db/lock" pid cmd
    [[ -f "$lock_file" ]] || return 0
    pid=$(head -n 1 "$lock_file" 2>/dev/null | tr -d '[:space:]')
    [[ "$pid" =~ ^[0-9]+$ ]] || return 0
    kill -0 "$pid" 2>/dev/null || return 0
    cmd=$(ps -o comm= -p "$pid" 2>/dev/null || true)
    [[ "$cmd" == *reth* ]] || return 0
    printf '%s' "$pid"
}

# ---------------------------------------------------------------------------
# Offline: read the stage checkpoints from mdbx.
#
# `db list ... --json` prints a JSON array of [stage_id, {block_number, ...}]
# pairs. --len must exceed the number of stages (default is 5) or the JSON is
# truncated to the first few.
#
# reth's tracing writes to STDOUT, not stderr, so its startup lines land in the
# middle of the JSON stream. --quiet turns the log layer off (LevelFilter::OFF,
# see Verbosity::directive); the parser below still scans past any leading
# noise in case a build ignores it.
# ---------------------------------------------------------------------------
read_offline() {
    local json_file err_file rc=0
    json_file=$(mktemp "${TMPDIR:-/tmp}/snapshot_height_json.XXXXXX")
    err_file=$(mktemp "${TMPDIR:-/tmp}/snapshot_height_err.XXXXXX")
    # shellcheck disable=SC2064
    trap "rm -f '$json_file' '$err_file'" RETURN

    echo "reading stage checkpoints from $DATADIR (mdbx, read-only)" >&2
    "$BINARY" db --quiet --chain "$CHAIN" --datadir "$DATADIR" \
        list StageCheckpoints --json --len 64 >"$json_file" 2>"$err_file" || rc=$?

    if (( rc != 0 )); then
        echo "error: '$BINARY db list StageCheckpoints' failed (exit $rc)." >&2
        local holder
        holder=$(lock_holder_pid)
        if [[ -n "$holder" ]]; then
            echo "A reth process (PID $holder) is live on this datadir; stop it first (kill $holder)." >&2
        fi
        echo "Last output lines:" >&2
        tail -n 20 "$err_file" >&2
        return 1
    fi

    if [[ ! -s "$json_file" ]]; then
        echo "error: no JSON on stdout - StageCheckpoints looks empty for this datadir." >&2
        tail -n 20 "$err_file" >&2
        return 1
    fi

    # Reports the Finish checkpoint, and flags any stage that lags behind it -
    # a snapshot whose stages disagree cannot be compared against another.
    SHOW_STAGES=$SHOW_STAGES python3 - "$json_file" <<'PY'
import json, os, sys

SHOW_STAGES = os.environ.get("SHOW_STAGES") == "1"

with open(sys.argv[1], errors="replace") as fh:
    raw = fh.read()

def to_stages(doc):
    """Pull {stage_id: block_number} out of a decoded db-list document.

    Tolerates both the [[key, value], ...] list-of-pairs shape and a plain
    object, and returns {} for anything that isn't stage checkpoints."""
    try:
        pairs = doc.items() if isinstance(doc, dict) else [tuple(r) for r in doc]
    except TypeError:
        return {}
    stages = {}
    for pair in pairs:
        if len(pair) != 2:
            continue
        key, value = pair
        if isinstance(value, dict) and isinstance(value.get("block_number"), int):
            stages[key] = value["block_number"]
    return stages


# reth logs to stdout, not stderr, so the payload can be surrounded by log
# lines. Scan for the first offset that both decodes as JSON and actually looks
# like stage checkpoints - a bare "[]" inside a log line must not win.
stages = {}
decoder = json.JSONDecoder()
for start in (i for i, ch in enumerate(raw) if ch in "[{"):
    try:
        doc, _ = decoder.raw_decode(raw, start)
    except ValueError:
        continue
    stages = to_stages(doc)
    if stages:
        break

if not stages:
    print("error: no stage checkpoints found in db output. First lines were:",
          file=sys.stderr)
    for line in raw.splitlines()[:15]:
        print(f"    {line}", file=sys.stderr)
    sys.exit(1)

head = stages.get("Finish")
if head is None:
    head = max(stages.values())
    print("warning: no Finish checkpoint; using max stage checkpoint", file=sys.stderr)

# Three distinct situations, which need distinct wording - reporting them all as
# "below head" once nearly caused a completed sync to be redone from scratch.
#
#   0            - the stage never advanced. A config/sync-path difference
#                  (fastnode drops the hashing stages; a live-synced datadir
#                  never runs some of these), not damage.
#   below head   - stopped part way. This is the one that means inconsistent.
#   above head   - the pipeline got that far but did not reach `Finish`, which
#                  is what `head` reports. An interrupted run, and re-running it
#                  usually just completes the trailing stages.
never_ran = sorted(k for k, v in stages.items() if v == 0 and head != 0)
behind = sorted(((k, v) for k, v in stages.items() if v != 0 and v < head),
                key=lambda kv: kv[1])
ahead = sorted(((k, v) for k, v in stages.items() if v > head), key=lambda kv: kv[1])

# Stages that legitimately sit below head because they are no-ops unless
# explicitly configured. Era only advances when [stages.era] names a path or
# url to import ERA1 archives from; on a normal datadir it never moves, and
# flagging it as a possible unwind is a false alarm.
INERT_WHEN_BEHIND = {"Era"}

if behind:
    real = [(k, v) for k, v in behind if k not in INERT_WHEN_BEHIND]
    inert = [(k, v) for k, v in behind if k in INERT_WHEN_BEHIND]
    if real:
        print(f"WARNING: {len(real)} stage(s) stopped below {head} - snapshot may be mid-unwind:",
              file=sys.stderr)
        for k, v in real:
            print(f"    {k:<24} {v}", file=sys.stderr)
    for k, v in inert:
        print(f"note: {k} is at {v}, below the head - expected, it is a no-op "
              "unless configured.", file=sys.stderr)

if ahead:
    top = max(v for _, v in ahead)
    print(f"note: {len(ahead)} stage(s) are AHEAD of the reported head, up to {top}.",
          file=sys.stderr)
    print(f"      The pipeline reached {top} but did not complete `Finish`, so the head",
          file=sys.stderr)
    print("      still reads lower. Re-running the same sync normally finishes in moments -",
          file=sys.stderr)
    print("      the expensive stages are already committed. Do not start over.",
          file=sys.stderr)
    for k, v in ahead:
        print(f"    {k:<24} {v}", file=sys.stderr)

if never_ran:
    print(f"note: {len(never_ran)} stage(s) never ran (checkpoint 0): {', '.join(never_ran)}",
          file=sys.stderr)
    print("      Expected for fastnode/live-synced datadirs; compare across snapshots",
          file=sys.stderr)
    print("      with --stages, since a differing stage profile is not a like-for-like benchmark.",
          file=sys.stderr)

if SHOW_STAGES:
    print(f"stage checkpoints ({len(stages)}):", file=sys.stderr)
    for k, v in sorted(stages.items()):
        print(f"    {k:<24} {v}", file=sys.stderr)

print(f"head block: {head} ({hex(head)})")
PY
}

# ---------------------------------------------------------------------------
# Via RPC: boot the node with p2p disabled and ask eth_blockNumber.
# ---------------------------------------------------------------------------
NODE_PID=""
cleanup() {
    if [[ -n "$NODE_PID" ]] && kill -0 "$NODE_PID" 2>/dev/null; then
        kill -INT "$NODE_PID" 2>/dev/null || true
        for _ in $(seq 1 60); do
            kill -0 "$NODE_PID" 2>/dev/null || break
            sleep 1
        done
        if kill -0 "$NODE_PID" 2>/dev/null; then
            kill -KILL "$NODE_PID" 2>/dev/null || true
            sleep 2
            # A node stuck in uninterruptible I/O outlives SIGKILL until the
            # syscall returns, and keeps holding the mdbx lock - which makes the
            # NEXT run fail with "storage directory is currently in use". Say so
            # here rather than letting that be a surprise later.
            if kill -0 "$NODE_PID" 2>/dev/null; then
                echo "WARNING: node PID $NODE_PID survived SIGKILL and still holds" >&2
                echo "  $DATADIR/db/lock - later runs on this datadir will fail until it exits." >&2
                echo "  Inspect with: ps -o pid,stat,wchan:24,etime,cmd -p $NODE_PID" >&2
            fi
        fi
    fi
}

read_via_rpc() {
    trap cleanup EXIT

    local holder
    holder=$(lock_holder_pid)
    if [[ -n "$holder" ]]; then
        echo "error: a reth process (PID $holder) already holds $DATADIR/db/lock." >&2
        echo "Stop it first (kill $holder), or use the default offline mode which needs no lock." >&2
        return 1
    fi

    local node_log
    node_log=$(mktemp "${TMPDIR:-/tmp}/snapshot_height_node.XXXXXX")
    echo "starting $BINARY on $DATADIR (p2p disabled); node log -> $node_log" >&2
    # The `+` guard keeps an empty EXTRA_ARGS from tripping `set -u` on bash < 4.4
    # (e.g. macOS system bash 3.2); it expands to zero words when unset.
    "$BINARY" node --chain "$CHAIN" --datadir "$DATADIR" \
        --http --http.port "$HTTP_PORT" \
        --disable-discovery --max-outbound-peers 0 --max-inbound-peers 0 \
        ${EXTRA_ARGS[@]+"${EXTRA_ARGS[@]}"} >"$node_log" 2>&1 &
    NODE_PID=$!

    # poll every 2s for up to WAIT_SECS
    local max_tries=$(( WAIT_SECS / 2 ))
    (( max_tries < 1 )) && max_tries=1
    local hex="" resp="" rc=0 i
    for i in $(seq 1 "$max_tries"); do
        if ! kill -0 "$NODE_PID" 2>/dev/null; then
            echo "error: node exited early. Last log lines:" >&2
            tail -n 20 "$node_log" >&2
            return 1
        fi
        # `|| true`: while the node is still opening its RPC port, curl exits
        # non-zero (connection refused). Without this the failed command
        # substitution would trip `set -e` and kill the script on the first try
        # instead of retrying.
        resp=$(curl -s "localhost:$HTTP_PORT" -X POST -H 'Content-Type: application/json' \
            -d '{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}') && rc=0 || rc=$?
        hex=$(printf '%s' "$resp" | sed -n 's/.*"result":"\(0x[0-9a-fA-F]\{1,\}\)".*/\1/p')
        [[ -n "$hex" ]] && break
        # Heartbeat that distinguishes the two failure shapes: curl couldn't
        # connect (RPC port not open yet) vs. it connected but the JSON had no
        # result (node up, eth_blockNumber still erroring during init).
        if (( i % 5 == 0 )); then
            if (( rc != 0 )); then
                echo "  ... waiting for RPC port $HTTP_PORT to open (curl rc=$rc, ${i}/${max_tries})" >&2
            else
                echo "  ... RPC is up but no block number yet (${i}/${max_tries}); last response: ${resp:0:200}" >&2
            fi
        fi
        sleep 2
    done

    if [[ -z "$hex" ]]; then
        echo "error: RPC on port $HTTP_PORT did not return a block number after ~$((max_tries * 2))s." >&2
        if (( rc != 0 )); then
            echo "The RPC port never opened (curl rc=$rc) - the node was still initializing." >&2
            echo "On a triedb snapshot this is usually the RocksDB open; raise --wait-secs, or" >&2
            echo "drop --via-rpc to read the height offline instead." >&2
        else
            echo "The RPC answered but without a result. Last response: $resp" >&2
        fi
        echo "Last node log lines:" >&2
        tail -n 20 "$node_log" >&2
        return 1
    fi

    echo "head block: $((hex)) ($hex)"
}

if (( VIA_RPC )); then
    read_via_rpc
else
    if (( ${#EXTRA_ARGS[@]} )); then
        echo "note: extra node args are ignored in offline mode (they only apply to --via-rpc)" >&2
    fi
    read_offline
fi
