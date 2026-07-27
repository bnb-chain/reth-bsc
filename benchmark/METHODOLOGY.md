# Storage backend benchmark — methodology

Reference for the triedb / mdbx-v1 / mdbx-v2 comparison run on BSC mainnet data.
Read alongside [`README.md`](README.md), which covers operating the harness;
this document covers *what is being measured and what to distrust*.

Status: methodology fixed and validated; results not yet collected at time of
writing. Everything marked **unverified** below was not measured.

---

## 1. Goal

Compare block-execution and storage performance of three reth-bsc storage
configurations on identical mainnet blocks:

| cell | backend | binary |
|---|---|---|
| `legacy-mdbx` | mdbx, legacy layout (v1) | reth-bsc `v0.0.9-beta` |
| `legacy-triedb` | triedb (RocksDB pathdb) | reth-bsc `v0.0.9-beta` + `--statedb.triedb` |
| `storage-v2` | mdbx, storage-v2 layout | reth-bsc `v0.1.0` |

The question: **for the same 20,000 mainnet blocks, how does per-block execution
latency and gas throughput differ between the three backends?**

Note the comparison is not single-variable. `storage-v2` also changes binary
(and therefore reth revision); triedb vs mdbx-v1 is the only pair that isolates
the backend alone. See §9.

---

## 2. Environment

**Benchmark host** — `devops-bot-m7i`

- `/server` on `/dev/nvme0n1`, 30 TB, 7.6 TB free at setup time
- 61 GB RAM
- All three datadirs and the benchmark run on this one host

**Block source** — `devops-bot-i7i`, `http://10.179.41.225:9545`

A synced reth-bsc node (`v0.0.10-beta`, 258 peers) reachable over the LAN. The
driver reads blocks from it via `eth_getBlockByNumber`. It is *only* a data
source — it is never the node under test, and its performance does not enter the
measurement.

Public RPC (Alchemy) was tried first and rejected: it rate-limits
(`429 … exceeded its compute units per second capacity`) partway through a
multi-thousand-block fetch, and the driver has no retry layer.

**Datadir sizes** (measured)

| datadir | size |
|---|---|
| mdbx v1 | 5.4 TB |
| mdbx v2 | 3.3 TB |
| triedb `rust_eth_triedb/` alone | 1013 GB across 14,568 SST files |

The triedb figure is the RocksDB store only; the full datadir (mdbx + static
files + RocksDB) is larger and was not measured separately.

---

## 3. Binaries

Both node binaries are **faithful rebuilds of the shipped releases plus the
`bench-test` feature** — same source revision, same reth pin, same profile.
Verified by comparing `--version` output against the shipped binaries.

| benchmark binary | reth-bsc source | reth pin | verified `Commit SHA` |
|---|---|---|---|
| `reth-bsc-v0.0.9-beta-np` | tag `v0.0.9-beta` (`aaec8d756`) | tag `v0.0.9` | `95d649fe0b75ba06e23c317fd75e07b422fdebd0` |
| `reth-bsc-v0.1.0-np` | tag `v0.1.0` (`7a9045281`) | rev `0dea17d2` | `0dea17d20f358768ac2d458bef170d8f0ab1df59` |

Built with `make bench-test` — profile `maxperf` (fat LTO, single codegen unit,
`-C target-cpu=native`), features `jemalloc,asm-keccak,bench-test`. The shipped
binaries also report `Build Profile: maxperf` and the same feature set, so the
optimisation profile matches.

`--version` reports the **reth dependency** SHA, not the reth-bsc commit. That is
how the two were correlated back to their source tags.

The `bench-test` feature touches exactly one file — `src/node/engine_api/mod.rs`
— and only adds RPC method registrations. It does not alter execution, storage,
or consensus. That is what makes it safe to benchmark with.

Driver: `bin/reth-bench/target/release/reth-bench-bsc`, built with
`make reth-bench` from `feat/bench-newpayload`.

---

## 4. The newPayload prototype

This is the central methodological decision and the main deviation from the
harness as originally designed.

### The problem

`reth-bench-bsc forkchoice-only` sends only `engine_forkchoiceUpdatedV1` — it
tells the node *which block to adopt* but never sends the block. The node must
obtain the block itself, and its only channel for that is p2p.

**The legacy binaries cannot peer with the current BSC network.** Verified
directly: every peer is rejected at handshake with

```
net: fork id mismatch, removing peer
  remote_fork_id=ForkId { hash: ForkHash("22d523b2"), next: 0 }
  our_fork_id=ForkId { hash: ForkHash("098d24ac"), next: 1768357800 }
```

Confirmed against public bootnodes and against the LAN node. With zero peers the
node answers `SYNCING` to every forkchoice update and the run hangs — observed
before the fix, with the node log showing
`Received forkchoice updated message when syncing`.

There is no RPC method on a stock reth-bsc node that accepts a block: BSC has no
production engine API, and `BscEngineApi::into_rpc_module` returns an empty
`RpcModule` unless `bench-test` is enabled.

### The fix

Add a second `bench-test`-gated engine method that accepts a whole block, so the
driver pushes blocks in rather than the node fetching them.

**Node side** — `src/node/engine_api/mod.rs:128` registers
`engine_newPayloadBscV1`, alongside the pre-existing
`engine_forkchoiceUpdatedV1` at line 68.

- Parameter: a single `0x`-prefixed hex string, the RLP encoding of a standard
  consensus block (`alloy_consensus::Block<TransactionSigned>`).
- Decoded at `mod.rs:149`, then wrapped as `BscBlock` with
  `sidecars: None` (`mod.rs:163`) and handed to `engine_handle.new_payload`.
- Returns `PayloadStatus` verbatim for every status, including `SYNCING` and
  `INVALID`. The caller decides what counts as failure.

Deliberately **not** named `engine_newPayloadV1`: the parameter is a consensus
block, not a spec `ExecutionPayloadV1`, and should not be mistaken for it.

*Why RLP and not JSON:* `bin/reth-bench` is a separate cargo workspace pinned to
a different reth revision and cannot depend on `reth_bsc` types. It therefore
sends an Ethereum-shaped block and the BSC wrapper is attached node-side. RLP is
canonical, which avoids a class of serde-shape mismatches between the two
crates.

*Why `sidecars: None` is sound:* blob sidecars are a data-availability concern
and expire from the network. The p2p path already executes these historical
blocks without them — the node logs
`blob_store_read: block has blob txs but no sidecars found!` while executing
normally. Execution needs only the versioned hashes, which the blob transactions
themselves carry. Validated empirically on mainnet blocks containing blob
transactions (§5).

**Driver side** — `bin/reth-bench/src/bench/new_payload_fcu.rs`, subcommand
`new-payload-fcu` (registered at `bench/mod.rs:36`). Per block it:

1. fetches the block from `--rpc-url` in a prefetch task,
2. converts it to a consensus block and RLP-encodes it **in that task**,
3. sends `engine_newPayloadBscV1` — *this call is the timed window*,
4. checks the returned status is VALID, then sends the forkchoice update.

### Consequence

The node under test runs with **zero peers**. This is stricter isolation than the
original harness intended: no peer traffic, no live-head announcements, no second
node competing for page cache. `isolation_args` in `config.toml`
(`--disable-discovery --max-outbound-peers 0 --max-inbound-peers 0`) is correct
under this mode — under `forkchoice-only` those same flags guarantee a hang.

---

## 5. Validation of the prototype

**Devnet** (`node-deploy-bsc`, chain id 714), fresh empty datadir, zero peers,
blocks pushed from a geth node's RPC:

| binary | blocks accepted | stopped at | reason |
|---|---|---|---|
| `v0.1.0` + newPayload | 354 (0 → 354) | block 355 | `failed to decode deposit requests from receipts: BSC validation error: unexpected system tx` |
| `v0.0.9-beta` + newPayload | 10 (0 → 10) | block 11 | `mismatched block state root` |

Both stopping points are **pre-existing chain/binary incompatibilities, not
prototype faults**. A state-root mismatch means the block was decoded and
executed — the node disagreed with the chain about the *result*. The devnet runs
every fork active from genesis, so the April `v0.0.9-beta` binary diverges early.
Corroborating: the devnet's own reth nodes (a different binary, receiving blocks
over p2p) fail with the same error class at block 52 and have been wedged since
2026-07-24.

**Mainnet** — the decisive test. `v0.0.9-beta` + newPayload, on the real mdbx v1
datadir, blocks 84295565–84296084:

- **520+ blocks accepted**, no rejections
- `net_peerCount` = `0x0` throughout — blocks could only have come from the driver
- block hashes on the node identical to the source node's
- the range contains blob transactions, so `sidecars: None` is validated on real
  data

Since the node computed matching state roots for every block, the RLP round-trip
preserves the header exactly and the mechanism is sound.

---

## 6. Snapshot preparation

All cells must start at the same block, since the harness replays the same range
against each. Target height for this run: **84296084** (`from - 1`).

### Why not the staged pipeline

The obvious route — start the node with `--debug.tip` / `--debug.max-block` and
let the pipeline sync forward — was abandoned after measuring it:

- Advancing mdbx v1 by 8,678 blocks took ~4 hours, dominated by `MerkleExecute`.
- That stage runs the whole range in a **single mdbx write transaction**
  (chunked internally at `incremental_threshold = 7000`, but committing once),
  producing 67 GiB of write amplification and no crash-resume granularity.
- It also requires peers, which the legacy binaries do not have.

`new-payload-fcu` avoids all of it: the live path computes state roots
incrementally per block, commits as it goes, and needs no peers. **Use it for
snapshot advancement, not just for measurement.**

### Prune backlog

The snapshots arrived carrying a large prune debt — `StorageHistory` was ~205,000
blocks behind its target. Advancing the tip is what makes the pruner act on it.

This must be drained **before** benchmarking. `block_interval = 5` means the
pruner fires throughout a run; a backlog grinding mid-measurement would distort
the numbers far more than anything else in this document.

Two findings:

- **Do not remove the `[prune]` section to skip it.** Tried; the node then fails
  with `trying to append row to Receipts at index #10267341240 but expected
  index #10267098003`. Receipt bookkeeping depends on the pruner's checkpoints;
  the config is not optional on an already-pruned datadir.
- **Raise `[stages.prune] commit_threshold`** from its default `1000000`
  (`reth crates/config/src/config.rs`, `PruneStageConfig::default`) to
  `10000000`. Measured effect on mdbx v1:

  | threshold | blocks/pass | entries/pass | pass time | rate |
  |---|---|---|---|---|
  | 1,000,000 | ~1,308 | ~588,000 | ~93s | 9.4 blocks/s |
  | 10,000,000 | ~15,000–19,400 | ~5,800,000 | 420–630s | **~34 blocks/s** |

  A 3.6× speedup, not 10× — commit cost scales with batch size. Applied to all
  three datadirs. It changes batch size only, not what gets pruned.

Progress signal: `Pruner interrupted, has more data … StorageHistory[<block>]`
in the debug log; completion is the `Prune` stage checkpoint jumping to the
target.

### mdbx v2 requires a full trie rebuild

`reth-bsc db migration-v2` **deletes** the trie and index tables and zeroes their
stage checkpoints. From `reth crates/cli/commands/src/db/migrate_v2.rs`:

- `clear_recomputable_tables` is called unconditionally at line 101 (the only
  early return, line 60, is for datadirs already on v2)
- `AccountsTrie` and `StoragesTrie` cleared at lines 311–312, alongside
  `TransactionSenders`, `TransactionHashNumbers`, `AccountsHistory`,
  `StoragesHistory`, `PlainAccountState`, `PlainStorageState`
- six stage checkpoints reset to 0 at line 314 onward: `SenderRecovery`,
  `TransactionLookup`, `IndexAccountHistory`, `IndexStorageHistory`,
  `MerkleExecute`, `MerkleUnwind`

So a migrated datadir is **not usable until the pipeline rebuilds all of it**.
Until then the engine runs backfill and answers `SYNCING` to every payload — the
symptom we hit when first starting mdbx v2.

Measured on this datadir: reth's own `stage_eta` for `MerkleExecute`
(checkpoint 0 → target 84286886) reported **~21.5 hours**. The full-rebuild
branch is resumable, unlike the incremental path.

`migration-v2` is therefore a conversion **plus** roughly a day of mandatory
rebuild on mainnet, even though the command itself returns quickly.

---

## 7. Harness and configuration

`benchmark/bench.py` with `benchmark/config.toml` (gitignored). Key settings:

```toml
bench_mode    = "new-payload-fcu"   # not the default forkchoice-only
warmup_blocks = 200
authrpc_port  = 8601
http_port     = 8600
node_ready_timeout_secs = 7200      # triedb's RocksDB open measured at 41m30s
isolation_args = ["--disable-discovery", "--max-outbound-peers", "0",
                  "--max-inbound-peers", "0"]
```

`bench_mode` is validated against `BENCH_MODES` (`matrix/config.py:103`) and
selects the driver subcommand at `matrix/runner.py`.

Ports are deliberately non-default (8600/8601) so the harness does not collide
with other nodes on the box — the mdbx v2 rebuild occupies 8545/8551 for a day.

### `--no-restore`, and why

The harness normally does `rsync -a --delete <snapshot>/ <datadir>/` before each
cell, giving byte-identical starting state. **That is not viable at these
sizes**: the mdbx v1 snapshot is 5.4 TB against 7.6 TB free, so a scratch copy
barely fits and would take hours to write per cell.

Instead each config's `datadir` points at its prepared directory, `snapshots` is
omitted, and the matrix runs with `--no-restore`.

**Consequence: runs are one-shot.** After a cell completes its datadir sits at
`to`, and there is no cheap way back — re-preparing means repeating §6. Validate
the harness with a short pilot group before committing the prepared state:
every cell ends at the same height, so the real group simply starts where the
pilot left off.

A config guard rejects `datadir` equal to or nested inside a snapshot path
(`matrix/config.py:183`), since that misconfiguration silently consumes the
snapshot on the first run.

---

## 8. What is measured

The timed window is the **`engine_newPayloadBscV1` call**
(`new_payload_fcu.rs:168`). That covers, on the node:

- transaction execution
- state root computation
- storage writes
- **pruning** (`block_interval = 5`, so it fires throughout a run)

Explicitly **outside** the window: fetching the block from the source RPC and
RLP-encoding it, both of which happen in the driver's prefetch task. This is an
improvement over `forkchoice-only`, where peer download time sits inside the
measurement.

The subsequent forkchoice update is issued but not included in the reported
latency.

**Pruning is included deliberately.** It could have been excluded by widening
the prune distances, but the backends differ in *where* pruning writes —
storage-v2 relocates history to RocksDB while v1 keeps it in mdbx — so excluding
it would hide a genuine behavioural difference. Note the consequence for
interpretation: reported Ggas/s is "execution + state root + pruning", not pure
execution. Over a 20,000-block range with `distance = 10064` the run prunes
roughly as many blocks as it executes.

Output: `forkchoice_latency.csv` (`gas_used`, `latency`) and `total_gas.csv` per
cell. **`latency` is serialised in microseconds** (`bench/output.rs:53`), not
nanoseconds — easy to misread by 1000×.

---

## 9. Known caveats

Listed in rough order of how much they could mislead.

**Warmup is large and may differ per backend.** Measured on mdbx v1 over 500
blocks from a cold node:

```
blocks   0- 49   6.1 MGas/s
blocks 150-199  15.0 MGas/s
blocks 300-349  15.3 MGas/s
blocks 450-499  20.0 MGas/s
```

Still climbing at block 500 — steady state was not reached in that sample.
`warmup_blocks = 200` is a judgement call, not a measured plateau. triedb and
mdbx cache very differently, so if warmup is not excluded properly the
comparison partly measures *how fast each backend warms*. **Unverified:** the
warmup curve for triedb and mdbx v2.

**mdbx v2 runs on a freshly rebuilt trie.** Its trie was regenerated in one pass
by `MerkleExecute` (§6), while mdbx v1 and triedb have tries built incrementally
over months of syncing. A freshly built trie is likely better laid out on disk.
This favours mdbx v2 for reasons unrelated to the storage format. **Not
quantified.**

**The triedb datadir could not be brought to the benchmark height.** Two
independent blockers, both reproducible:

1. **State-root divergence at block 84295819.** With `--statedb.triedb` the node
   computes `0x56d3e258…` where the canonical root is `0x16c9545b…`. Parent
   84295818 validates and the 254 preceding blocks executed correctly, so the
   pre-state was canonically correct going in. The same binary on an mdbx
   datadir executes the same block correctly via the same driver, which rules
   out the EVM, consensus, and the push mechanism. Deterministic across two
   runs. Ruled out as triggers: blob transactions (none in the block), contract
   creation, withdrawals, and EIP-7702 (7 of 22 sampled earlier blocks contain
   type-0x4 transactions and all executed correctly). **Cause not localised** -
   that needs the `--debug.invalid-block-hook witness` diff, which was not
   captured.

2. **The trie unwind exceeds available memory.** `stage unwind to-block` reports
   success but leaves `MerkleExecute` and `MerkleUnwind` at the pre-unwind
   height; the node then attempts that reconciliation itself at startup, and the
   storage-trie unwind builds an uncommitted mdbx write transaction that grew
   past 45 GB on a 61 GB machine and was OOM-killed. Nothing is written until it
   commits, so an OOM discards the entire attempt. `v0.0.9-beta` pins triedb
   `v0.0.2`, which has no `RETHBSC_ROCKSDB_*` overrides, so the 16 GB block
   cache cannot be reduced without rebuilding.

Consequence: the comparison lost its only single-variable pair. triedb vs
mdbx-v1 shares a binary and a reth revision, so it isolates the backend;
mdbx-v1 vs mdbx-v2 does not.

**Preliminary triedb figures, not results.** Over the 254 blocks it did execute:
~59 ms/block at ~71 MGas/s, against mdbx v1's steady-state ~480 ms at ~16
MGas/s. Recorded only because the run ended in a state-root divergence and the
sample is far too small - it is not a performance finding.

**triedb pays a ~40-minute RocksDB open on every node start.** Measured on a
cold page cache against a 1013 GB store of 14,568 SSTs: **41m30s** from
`Opening database` (17:11:46) to `RPC HTTP server started` (17:53:16), reading
at a sustained 128 MB/s for a cumulative **~330 GB**. `PathDB::new` opens the
database twice (once to enumerate column families, once with descriptors), and
`max_open_files = -1` loads table readers for every SST up front. mdbx opens in
constant time by comparison — seconds, regardless of its 5.4 TB size.

This is a real operational property of triedb, not a benchmark artifact, and
worth reporting as a result in its own right: restarting a 1 TB triedb node
costs 40 minutes. It also means every triedb cell carries that setup before its
first block executes, which is why `node_ready_timeout_secs` is 7200.

**Unverified:** whether a warm-page-cache restart is materially faster. The
measurement above was cold and with an unrelated trie rebuild running
concurrently (that job was doing only ~2 MB/s, so contention was probably
minor). A back-to-back restart would answer it.

**The three datadirs have different provenance.** mdbx v1 came from a full
staged-sync pipeline (all 15 stage checkpoints at head); mdbx v2 came from
`migration-v2` of mdbx v1 followed by a rebuild; triedb came from its own sync.
They are not clones of one another.

**A periodic throughput dip, undiagnosed.** In the 500-block mdbx v1 sample,
throughput halved for a ~100-block band (15.0 → 7.5 MGas/s at blocks 200–299)
and recovered. Gas per block was flat across the band, so it is not block weight.
Engine persistence flushing batched blocks is a plausible cause but **was not
investigated**. Unknown whether it is periodic; if it is, it may land differently
in each cell.

**The comparison is not single-variable.** `storage-v2` changes both the storage
layout and the binary (reth `0dea17d2` vs reth `v0.0.9`). Only triedb vs mdbx-v1
isolates the backend. A fourth cell — mdbx v1 on the `v0.1.0` binary — would
separate the two, and was considered and declined.

**Isolation flags are not always honoured.** During pipeline syncs a node started
with `--disable-discovery --max-outbound-peers 0 --max-inbound-peers 0` was
observed with `connected_peers=12`. Under `new-payload-fcu` the measured runs did
show `net_peerCount = 0`, but the flags should not be assumed sufficient without
checking.

---

## 10. Repositories, branches, and source references

**https://github.com/bnb-chain/reth-bsc**

| branch | contents |
|---|---|
| `feat/bench-newpayload` | driver `new-payload-fcu`, harness changes, this document |
| `bench/newpayload-v0.1.0` | tag `v0.1.0` + the engine method — builds the storage-v2 node binary |
| `bench/newpayload-v0.0.9-beta` | tag `v0.0.9-beta` + the engine method — builds the legacy node binary |

The two `bench/*` branches exist because the node binaries must be built from
their own release tags; the engine-method commit is cherry-picked onto each.
`v0.0.9-beta` needed a manual conflict fix — that revision's
`fork_choice_updated` takes an extra `EngineApiMessageVersion` argument.

**https://github.com/bnb-chain/reth** — reth tag `v0.0.9` (`95d649fe`), tag
`v0.0.10`, rev `0dea17d2`.

Key files:

| path | what |
|---|---|
| `src/node/engine_api/mod.rs` | both bench-test engine methods; `:128` newPayload, `:163` `sidecars: None` |
| `bin/reth-bench/src/bench/new_payload_fcu.rs` | driver mode; `:44` method name, `:168` the timed call |
| `bin/reth-bench/src/bench/output.rs` | CSV serialisation; `:53` microsecond units |
| `benchmark/matrix/config.py` | config schema, `bench_mode` validation, snapshot/datadir guard |
| `benchmark/matrix/runner.py` | per-cell orchestration, argv construction |
| `benchmark/snapshot_height.sh` | offline height check; reads stage checkpoints without starting a node |

`snapshot_height.sh` is the tool for verifying alignment. It reads
`StageCheckpoints` read-only via `db list`, so it takes no storage lock, never
opens the state backend, and returns in seconds — unlike booting a node, which
on triedb costs hours. It also distinguishes stages *behind* head (possible
mid-unwind) from stages *ahead* of head (interrupted pipeline run) from stages
that never ran.

---

## 11. Reproduction

Assumes prepared datadirs. To prepare from scratch, see §6.

```bash
# 1. binaries
cd /server/reth-bsc && git fetch
git checkout feat/bench-newpayload && make reth-bench     # driver
git worktree add /server/build-np009 bench/newpayload-v0.0.9-beta
git worktree add /server/build-np010 bench/newpayload-v0.1.0
(cd /server/build-np009 && make bench-test)
(cd /server/build-np010 && make bench-test)
cp /server/build-np009/target/maxperf/reth-bsc /server/binaries/reth-bsc-v0.0.9-beta-np
cp /server/build-np010/target/maxperf/reth-bsc /server/binaries/reth-bsc-v0.1.0-np

# 2. verify the rebuilds match the shipped binaries
/server/binaries/reth-bsc-v0.0.9-beta-np --version    # Commit SHA 95d649fe...
/server/binaries/reth-bsc-v0.1.0-np      --version    # Commit SHA 0dea17d2...

# 3. verify alignment — all cells must report the same head
for d in mdbx_data/data-seed triedb_data/data-seed; do
  ./reth-bsc/benchmark/snapshot_height.sh \
      /server/binaries/reth-bsc-v0.0.9-beta-np /server/all_data/$d/data_dir
done
./reth-bsc/benchmark/snapshot_height.sh \
    /server/binaries/reth-bsc-v0.1.0-np /server/all_data/mdbx_v2/data_dir

# 4. config, then dry run (prints every command, executes nothing)
python3 benchmark/bench.py run --config benchmark/config.toml --dry-run --no-restore

# 5. pilot on a short group, then the real range
python3 benchmark/bench.py run --config benchmark/config.toml --no-restore

# 6. re-analyse an existing results dir without re-running
python3 benchmark/bench.py analyze --results /server/bench-results/<timestamp>
```

Requires Python ≥ 3.11, or 3.9 with `tomli` installed (RHEL 9 ships 3.9).

Advancing a datadir to a target height, when needed:

```bash
# node with zero peers
/server/binaries/<binary> node --chain bsc --datadir <datadir> \
    --http --http.port 8600 --authrpc.port 8601 --authrpc.jwtsecret "$JWT" \
    --ipcpath /tmp/np.ipc \
    --disable-discovery --max-outbound-peers 0 --max-inbound-peers 0 &

# wait for eth_blockNumber to report the datadir's current head, then push
/server/reth-bsc/bin/reth-bench/target/release/reth-bench-bsc new-payload-fcu \
    --rpc-url "http://10.179.41.225:9545" \
    --from <current-head> --to <target> \
    --jwt-secret "$JWT" --engine-rpc-url http://127.0.0.1:8601 \
    --output /tmp/advance
```

`--from` is the datadir's current head — the driver replays `from+1 ..= to`.
