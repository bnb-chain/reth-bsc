# Optional BAL read-hint prefetching

This experimental path uses block access lists to prefetch account and storage data
from the node's own parent-state database. Enable it with `--bal.enabled` or
`BSC_BAL_ENABLED=true`. It is disabled by default.

## Build

Two local branches are required, with these sibling checkout names:

| Checkout | Branch | Base |
| --- | --- | --- |
| `reth-bsc-bal-v2.8` | `feat/bal-v2.8` | reth-bsc `feat/upgrade-reth-v2.8`, `c3efa9ba41d6f0faa0ad0ec978ee56016d24fa73` |
| `reth-bal-v2.8` | `feat/bal-prefetch-v2.8` | bnb-chain/reth `5ab86a20b9c8e74200ae036bf33bafa97cf79ef7` |

The Cargo source patches select the sibling Reth checkout consistently for the
entire dependency graph, including the execution-spec test package. These are local
development branches; the build does not assume they have been published remotely.
The tested companion implementation is commit `9278c0cddb`.
Use Rust 1.95 or newer (local validation uses 1.99.0).

```sh
cd reth-bsc-bal-v2.8
cargo +1.99.0 build --release --bin reth-bsc
./target/release/reth-bsc node --chain bsc --bal.enabled
```

Append the flag to the node's existing launch arguments. No genesis, hardfork, or
chain-ID change is required. To roll back the feature, restart without the flag and
unset `BSC_BAL_ENABLED` if it was set. The existing database remains compatible.

## How it works

1. A participating producer records accesses while building a block, including
   BSC's deferred system transactions. It discards all recorded values and transaction
   indexes, retaining account addresses and storage keys.
2. Once the block has its final hash, the producer queues a hint for peers advertising
   the experimental `bsc-bal/1` RLPx capability. Ordinary BSC block diffusion proceeds
   immediately; delivery order across the two capabilities is not guaranteed.
3. On import, a participating peer consumes an already-arrived hint matching the block
   hash, parent hash, and number. It never waits for a hint or requests a missing one.
4. The Reth engine uses its BAL I/O pool to load the selected accounts and slots from
   local parent state into the shared execution cache. Claimed values received from
   peers are ignored, even if a peer includes them.
5. Normal BSC transaction execution, system transactions, receipts, state-root checks,
   and Parlia validation determine the result. The hint is never passed as a consensus
   BAL, a transaction execution input, or a source of trie updates.

This enables parallel **state prefetching**, not parallel transaction execution or
Ethereum EIP-7928 consensus rules. Blocks do not gain a BAL header commitment. The
standard `eth` and `bsc` protocol encodings, including the legacy BEP-592 handling,
remain unchanged. Peers without `bsc-bal/1` remain connected normally.

## Fallbacks and limits

- Missing, late, empty, undecodable, oversized, or over-budget hints fall back to the
  ordinary execution/prewarming path. Incomplete or incorrect address lists affect
  cache hit rates only; normal execution loads anything else it needs.
- `--engine.disable-prewarming`, `--engine.disable-state-cache`, and
  `--engine.disable-bal-batch-io` suppress hint prefetching.
- Each hint is capped at 1 MiB. The receiver retains at most 32 entries and 16 MiB
  of encoded BAL data for 10 seconds. Hints are consumed once. Each peer is limited
  to eight received hints per second, with at most eight frames processed per poll.
  The outgoing broadcast ring retains at most eight hints.
  RLPx also caps each peer's pending hint frames at eight messages and roughly 2 MiB;
  violations of transport frame/queue limits use the normal peer-disconnection policy.
- The engine additionally caps accounts plus storage slots by
  `min(block_gas_limit, 100_000_000) / 2_000`, independently of header validity.
  These are local resource limits, not consensus rules.
- Producers send hints for local builds and legacy simulated `SendBid` payloads.
  BEP-675 zero-simulation bids do not currently supply hints. Remote hints are not
  relayed, so useful hints require a direct connection to a participating producer.
- Collection, transmission, and speculative I/O have costs. No throughput or latency
  improvement has been established; benchmark representative block replay before
  selecting this option for a deployment.

## Metrics and validation

`reth_bsc_bal_hints_sent_total` and `reth_bsc_bal_hints_received_total` count hint frames
in the node's Prometheus endpoint.
`reth_bsc_bal_hint_lookup_total{result="hit"|"miss"}` reports arrival before validation;
a hit can still be rejected by the engine's decoder or resource limits.
`reth_bsc_bal_hints_lagged_total` counts outgoing subscriber lag. Existing engine cache
and BAL-prefetch metrics measure cache activity.

Focused checks:

```sh
cargo +1.99.0 test -p reth_bsc --lib node::bal::tests -- --test-threads=1
cargo +1.99.0 clippy --workspace --tests --all-features
cd ../reth-bal-v2.8
cargo +1.99.0 test -p reth-engine-tree --lib read_hint --target-dir ../reth-bsc-bal-v2.8/target -- --test-threads=1
cargo +1.99.0 clippy -p reth-engine-tree --tests --target-dir ../reth-bsc-bal-v2.8/target
```

The regression tests cover frame bounds, cache eviction/expiry and block binding,
unsupported peers, stripping advertised values, malformed-hint fallback, resource
limits, local-state cache contents, disable flags, and transaction ordering.
They also compare BSC transaction results and state with collection enabled and disabled.
Live multi-node propagation and mixed-client correctness were also checked in the
[local cluster run](bal-local-cluster-2026-11-13.md). Performance benchmarks remain
separate validation work.
The subsequent [common suite](bal-common-suite-2026-11-13.md) also passed BEP-675,
CAS20, ERC20, BNB transfer and Uniswap V2 scenarios on a mixed-client BAL cluster.

Validated locally with Rust 1.99.0 on 2026-11-13: all seven BSC BAL tests and all four
engine read-hint tests passed. The full BSC library run had 781 passes, one ignored
test, and four gRPC tests blocked by sandbox socket restrictions; all four passed
when rerun with local socket access. Both Clippy commands above passed, with existing
dependency/future-compatibility warnings.
