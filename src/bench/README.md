# Miner Benchmark

This directory contains the `miner-bench` CLI, which is a local benchmark harness for
measuring block building, state-root computation, and post-build persistence costs.

## What It Measures

The main supported command on this branch is `run`, which executes a direct block-building
microbenchmark:

- initializes a temporary MDBX database from genesis
- optionally enables trieDB before genesis
- builds a setup block that deploys and distributes an ERC20 token
- executes a configurable number of benchmark blocks with synthetic ERC20 transfers
- finalizes the block
- persists block/state changes and reports timing breakdowns

The reported persistence bucket is real local work. It includes:

- block insert
- state write
- triedb flush
- database commit

## Supported Commands

### `run`

This is the supported benchmark mode on this branch.

Example:

```bash
cargo run --features bench-test --bin miner-bench -- run \
  --num-blocks 3 \
  --txs-per-block 500 \
  --funded-accounts 500 \
  --background-accounts 1000000 \
  --storage-slots-per-account 10 \
  --triedb \
  --output benchmark.csv
```

### `payload-job-run`

This command is currently unavailable on this branch. The upstream refactor removed the
`BscPayloadJob` / wait-slice scheduling APIs that the original payload-job benchmark depended on.
The command remains reserved so the CLI shape is explicit, but it returns an error if invoked.

## trieDB Flag

The benchmark CLI uses `--triedb`.

That is the benchmark-side equivalent of running the node with:

```bash
reth-bsc node --statedb.triedb
```

When `--triedb` is enabled, the benchmark initializes the global triedb manager before genesis so
the builder/finalization flow runs against triedb-backed state logic on this branch.

## Branch-Specific Caveats

- `--chain-difflayers` is ignored by the direct benchmark on this branch.
  The current builder API does not expose difflayer-returning finalize hooks here.
- The direct benchmark is still a microbenchmark, not a full node simulation.
- Transactions are pre-recovered during pool generation so the measured execution loop reflects
  block-building work more closely than signer recovery overhead.
- The workload is synthetic ERC20 traffic, not a live heterogeneous mempool.

## Interpreting Results

Useful high-level buckets:

- `tx_execution_us`: aggregate transaction execution time
- `execute_only_us`: time spent inside `execute_transaction()`
- `finish_us`: post-execution finalize work, including state-root computation
- `commit_us`: aggregate persistence work
- `insert_block_us`, `write_state_us`, `triedb_flush_us`, `provider_commit_us`:
  persistence subphases

In practice on this branch:

- `finish_us` is the relevant builder/finalize number
- `commit_us` is the relevant post-build persistence number
- the benchmark is useful for tuning builder/root/persistence behavior
- it should not be described as a full real-miner simulation
