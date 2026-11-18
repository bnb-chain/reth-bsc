# BAL local cluster validation — 2026-11-13

Result: passed after fixing hint publication on the default mining path.

## Environment

- macOS arm64, Rust 1.99.0, debug build of `reth-bsc`.
- BSC branch `feat/bal-v2.8`, implementation `efe212a9f` plus the publication
  fix recorded with this report; companion Reth commit `9278c0cddb`.
- Three Reth validators using the existing three-validator Rialto fixture,
  chain ID 714. Nodes 0 and 1 enable `--bal.enabled`; node 2 omits the flag.
- One non-mining Geth observer, v1.8.1, commit `583be4db`, independently validates
  the chain. All four nodes connect directly with discovery disabled.
- Fresh experiment data, a separately funded ephemeral transaction wallet, HTTP
  ports 28450–28453 and P2P ports 32650–32653. Existing cluster data was preserved.
- The isolated genesis adds the BSC Osaka blob schedule. Geth receives explicit
  matching fork overrides, because Rialto mode otherwise replaces genesis times.

## Workload and assertions

Each run deploys a seven-byte EVM contract that writes calldata to storage slot 0,
then sends 36 writes, alternating submission among all three Reth validators.
The contract's final slot value is 36. This gives 74 successful user transactions
in total, including two deployments.

For every transaction, all four clients returned matching block placement, success
status, gas usage, cumulative gas usage, logs and contract address. At each checked
height, all four clients agreed on the block hash, parent hash, state root, receipt
root, transaction root, gas usage and transaction hashes. Contract bytecode and
storage values also matched on all four clients.

| Run | Submission mode | Canonical blocks compared | User transactions |
| --- | --- | --- | --- |
| Default path | `BSC_SUBMIT_BUILT_PAYLOAD=false` on all validators | 1–136 | 37 |
| Restart / pre-executed path | Node 0 restarted on the same database with `BSC_SUBMIT_BUILT_PAYLOAD=true`; nodes 1 and 2 remain false | 1–379 | 37 |

The final compared block was 379:

```text
hash:      0x2c51ea71d8b41e2748565235becff76f39dfd05788c97bbb2dd1f30a022b72f6
stateRoot: 0x7cbaff4392d2ea4adfe960ef13e56068e4339d937118a7cc6c0dad573acf0d83
```

All three validators produced canonical blocks. The BAL-disabled validator produced
137 of the first 379 blocks, exercising ordinary imports without hints from that
producer. Geth and the disabled Reth node stayed connected to BAL-enabled peers.
Only the two enabled validators advertised `bsc-bal/1`.

## Evidence that the hint path ran

| Snapshot | Node | Hints sent | Hints received | Lookup hits | Lookup misses |
| --- | --- | --- | --- | --- | --- |
| Default run | 0 | 35 | 35 | 34 | 71 |
| Default run | 1 | 35 | 35 | 35 | 70 |
| After restart run | 0 | 48 | 48 | 48 | 75 |
| After restart run | 1 | 109 | 103 | 103 | 247 |

Counters are per-process: node 0 resets on restart; node 1 does not. These are
snapshots while the chain was running. Misses also include self-produced blocks
and blocks from the disabled validator; they are not a hint-delivery failure rate.

Engine traces contained `Starting BAL prewarm` on both enabled nodes (108 and 103
occurrences across their retained logs), and none on node 2. Node 2 also exposed no
BAL counters. Thus the checks cover actual prefetch invocation, not just capability
advertisement. There were no error-level log entries after the corrected cluster
configuration and publication fix were started.

## Defect found and fixed

Initially hints were published only by `on_new_mined_block`, which is used when
`BSC_SUBMIT_BUILT_PAYLOAD=true`. The default false path dropped the collected hint
when submitting the ordinary block, so the live cluster saw no transmitted hints.

Publication now happens in the miner's common submission path, after final sealing
and double-sign checks and before either import channel is scheduled. The old
publication call was removed, avoiding duplicates. Both submission modes were
then exercised successfully. Prometheus names in the feature documentation were
also corrected to include the node's `reth_` prefix.

## Supporting checks and artifacts

Seven focused BSC BAL tests passed. The affected library and binary passed Clippy;
the changed Rust files passed rustfmt with the repository's Rust 2021 edition.
Existing dependency and future-compatibility warnings remain.

Local artifacts are retained in
`/Users/chee/Documents/repo/bal-local-cluster-2026-11-13/`:

- `cluster.py`: isolated startup, connection, restart and shutdown commands.
- `validate.py`: transaction workload and cross-client assertions.
- `results/default-path/validation.json` and
  `results/preexecuted-path/validation.json`: assertion summaries and metrics.
- Each phase also retains transaction receipts, checked canonical blocks, peer
  capabilities and metrics. `results/source.patch` identifies the tested Rust fix.
- Node logs and databases remain available for inspection. Test processes were
  stopped after validation. Wallet files stay outside the source repository.

This is a correctness smoke test with a small storage workload. It establishes
mixed-client compatibility and functioning advisory prefetch in this cluster;
it does not establish a throughput or latency benefit, mainnet readiness, or
resilience under sustained adversarial traffic.
