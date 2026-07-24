# Storage benchmark matrix

Benchmarks block-execution + storage performance of `reth-bsc` across node
configurations (e.g. Storage V2 vs legacy TrieDB vs legacy MDBX) and produces a
reth-2.0-announcement-style comparison chart: P50 / P90 block latency and
throughput (Ggas/s) per block group, with % deltas vs a baseline.

It orchestrates the existing harness — a node built with `make bench-test`
(exposes `engine_forkchoiceUpdatedV1`) replayed by `bin/reth-bench`
(`reth-bench-bsc forkchoice-only`) — and adds run isolation, statistics,
summaries, and the chart. Stdlib-only Python >= 3.11.

## How it works

For each (config x group) cell:

1. Restore the config's datadir snapshot for that group (`rsync -a --delete`).
2. Start the node with that config's binary and flags (p2p-isolated so it
   cannot sync on its own).
3. Wait until `eth_blockNumber` reports `from - 1`.
4. Replay blocks `from..=to` via `reth-bench-bsc forkchoice-only`, which
   fetches each block from a synced source node's RPC and times each
   forkchoice update (the node must execute the block: EVM + state root).
5. Stop the node, validate the run (full block coverage, CSV consistency, no
   error-pattern hits in `node.log`, clean exit), write `run.json`.

Then it computes per-cell stats and writes `summary.{json,csv,md}` and
`chart.html` into the results directory.

Note on ranges: `reth-bench-bsc --from N --to M` only uses block N to seed its
loop; the blocks actually replayed are `N+1..=M`. This tool's config uses the
intuitive convention instead — group `from`/`to` are exactly the measured
blocks — and passes `--from from-1` internally. **Snapshots must therefore be
synced to exactly `from - 1`.**

## Usage

```bash
# copy and edit
cp benchmark/config.example.toml benchmark/config.toml

# sanity-check the config: prints every command without executing anything
python3 benchmark/bench.py run --config benchmark/config.toml --dry-run

# full matrix (or: make bench-matrix)
python3 benchmark/bench.py run --config benchmark/config.toml

# subset
python3 benchmark/bench.py run --config benchmark/config.toml --configs storage-v2 --groups normal

# one cell at a time, accumulated into one results dir (any order; the chart
# and summary update after each run, covering everything run so far)
python3 benchmark/bench.py run --config benchmark/config.toml \
    --results-dir benchmark/results/v2-vs-legacy --configs storage-v2   --groups normal
python3 benchmark/bench.py run --config benchmark/config.toml \
    --results-dir benchmark/results/v2-vs-legacy --configs legacy-mdbx  --groups normal
# re-running a cell into the same --results-dir replaces its previous result

# skip the snapshot restore when the datadir is already at from-1 (e.g. the
# first run right after preparing it); the head check still verifies the
# height, and note a run leaves the datadir at `to`, so the next run of that
# cell needs a restore again
python3 benchmark/bench.py run --config benchmark/config.toml \
    --no-restore --configs storage-v2 --groups normal

# re-analyze / re-chart an existing results dir without re-running
python3 benchmark/bench.py analyze --results benchmark/results/20260707-120000
```

Results land in `<output_dir>/<timestamp>/`:

```
meta.json
runs/<group>/<config>/{forkchoice_latency.csv,total_gas.csv,node.log,bench.log,run.json}
summary.json  summary.csv  summary.md  chart.html
```

Invalid runs (failed checks) are recorded but excluded from the chart.

## Prerequisites (per benchmark machine)

1. **Prebuilt binaries** — the tool never builds anything; you point the
   config at binaries you already have, wherever they live:

   - each `[[configs]].binary` → a `reth-bsc` node binary built **with the
     `bench-test` feature** (it exposes the `engine_forkchoiceUpdatedV1`
     endpoint the driver calls)
   - `[global].bench_bin` → a `reth-bench-bsc` driver binary

   `bench.py run` verifies each configured binary exists, is executable, and
   records its `--version` output into `meta.json` before starting anything.

   If you still need to produce them, this repo does it with `make bench-test`
   (node) and `make reth-bench` (driver). For a binary from a legacy branch,
   verify the feature exists on that branch first:

   ```bash
   git grep -n "forkchoiceUpdatedV1" -- src/node/engine_api/
   grep bench-test Cargo.toml
   ```

   If absent, backport is minimal: add `bench-test = []` to `[features]` and
   cherry-pick the `#[cfg(feature = "bench-test")]`-gated
   `engine_forkchoiceUpdatedV1` registration from the current
   `src/node/engine_api/mod.rs`.

2. **A synced source node** reachable at each group's `rpc_url`, covering
   blocks `[from - 65, to]` (the driver also fetches head-32 / head-64 for the
   safe / finalized hashes).

3. **One snapshot per (config, group)**: a datadir synced to exactly
   `from - 1` under the matching binary and backend. Recipe:

   ```bash
   # start the node for that config on an earlier datadir, then advance it:
   ./reth-bench-bsc forkchoice-only --rpc-url <source> \
       --from <head> --to <from-1> \
       --jwt-secret <jwt> --engine-rpc-url http://localhost:8551
   # stop the node, then:
   cp -a <datadir> /snapshots/<config>-<group>
   ```

   Snapshots **cannot be shared across backends** (`--statedb.triedb` is
   persisted into the datadir's config, which wins over the CLI flag) nor
   across binary revisions (storage schema drift between pinned reth revs).

4. A JWT secret file (hex) shared by the node and the driver.

## Interpreting the numbers

- **Latency** is the end-to-end forkchoice-update round trip per block:
  execution + state root + persistence as observed by the driver.
- The first `warmup_blocks` blocks of each run (default 2) are dropped from
  the stats: right after node start the driver retries on `SYNCING` in 200 ms
  steps, which inflates those samples.
- **Throughput** = total gas / total per-block latency over the measured
  blocks. The bench binary's own printed "Total Ggas/s" uses cumulative wall
  time instead (including its consumer-loop overhead); the summary flags any
  run where the two definitions diverge by more than 1%.
- Deltas compare each config against the **first** `[[configs]]` entry.

## Fairness notes

- Runs execute sequentially on the same machine; keep it otherwise idle.
- The node is started with p2p disabled (`isolation_args`) so it cannot sync
  past the snapshot head on its own; the run is invalidated if the head is
  already beyond `from - 1`.
- OS page cache is not dropped between runs by default. For strict cold-cache
  comparisons on Linux, run e.g.
  `sync && echo 3 | sudo tee /proc/sys/vm/drop_caches` between runs.
- Re-run the tool on the same range and compare summaries if you need
  variance estimates; results directories are timestamped and never
  overwritten.

## Development

```bash
python3 -m unittest discover -s benchmark/tests -v
```
