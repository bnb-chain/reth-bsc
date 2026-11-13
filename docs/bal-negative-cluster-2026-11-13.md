# Bad BAL local-cluster tests — 2026-11-13

All **13 live fault cases passed** against the optional `bsc-bal/1` read-hint
implementation. All four clients agreed on **534 consecutive blocks (1192–1725)**
and **634 successful workload transactions**. Incorrect hints did not corrupt
execution state or make the valid blocks invalid. Oversized transport frames
disconnected the sender; normal hint exchange resumed after reconnection.

## Setup and provenance

- BSC branch `feat/bal-v2.8`, source HEAD `1f29a71c64bc722140d39d26ff0b9aa5bc181d40`;
  companion Reth `9278c0cddbd86dc9c85ee0b8a2d530eb037faaae`.
- Node 0: BAL-enabled validator with a test-only outgoing-frame mutation hook in
  detached checkout `reth-bsc-bal-negative`. It still produced ordinary valid blocks.
- Node 1: unmodified BAL-enabled receiver/validator. Node 2: unmodified BAL-disabled
  validator. Node 3: BSC Geth 1.8.1 observer, commit `583be4db`.
- Receivers used the same debug binary validated in the preceding common suite,
  matching Rust source at `5c9da45a8`; the later commit changes documentation only.
  Its embedded version reports `efe212a9f-dirty` because it was built before the
  publication fix was committed. Exact binary hashes and verbatim version output
  are preserved in `results/provenance.json`.
- Isolated APFS copies of the stopped common-suite chain-714 databases and genesis;
  matching forks through Jenner. Original data was preserved. HTTP ports
  28470–28473, P2P ports 32670–32673, metrics ports 29670–29672.
- Default `BSC_SUBMIT_BUILT_PAYLOAD=false` on all validators. Faults were inserted
  after normal producer-side bounds, so oversized/malformed messages reached the
  actual transport or receiver checks instead of being suppressed at publication.

## Live cases

Every row passed receipt, canonical block and contract-state comparisons.
Transaction counts below total 633 storage writes; the deployment makes 634.

| Fault | Writes | Observed behavior on stock receiver |
| --- | ---: | --- |
| Malformed inner BAL (`ff`) | 18 | Cache lookup reached the engine; zero BAL prewarm invocations. Ordinary execution succeeded. |
| Empty BAL (`c0`) | 64 | Cache hits but zero BAL prewarm invocations; ordinary execution succeeded. |
| I/O budget exceeded | 68 | One account plus 20,001 slots, 59,653 encoded bytes, exceeded the 40-million-gas block's 20,000-item limit; zero BAL prewarm invocations. |
| Wrong block hash | 19 | Hints received; zero BAL prewarm invocations; valid blocks still imported. |
| Wrong parent hash | 68 | Hints received; no matching lookup hits or BAL prewarm invocations. |
| Wrong block number | 68 | Hints received; zero BAL prewarm invocations; valid blocks still imported. |
| Irrelevant account/slot, omitting real accesses | 18 | Nine BAL prewarm invocations; execution loaded the actual required state normally. |
| Forged storage, balance, nonce and code | 73 | Ten BAL prewarm invocations; all advertised values were ignored. Actual state remained correct. |
| Malformed outer envelope | 72 | No BAL prewarm invocations; chain progressed. |
| Trailing data after envelope | 21 | No BAL prewarm invocations; chain progressed. |
| Inner BAL over 1 MiB | 74 | 1,048,577-byte BAL inside a transport-sized frame; no received-hint counter increase and no BAL prewarm invocations. |
| Burst of 64 frames | 12 | Bounded acceptance, no observed disconnection or chain divergence. Counter interval included nine received hints and recovery traffic. |
| Transport frame over limit | 58 | 1,049,678-byte frame exceeded 1 MiB + 128 bytes; sender disconnected and `reth_p2pstream_subprotocol_message_too_big` incremented to 1. |

Counter intervals can include in-flight normal/recovery messages. A lookup hit is
measured before engine decoding and does not prove that a hint was usable. Actual
`Starting BAL prewarm` traces distinguish prefetch invocation from cache arrival.
The burst test does not establish an exact per-second rate from aggregate counters
or exercise every queue-overflow condition; source limits remain eight frames per
peer per second, with separate ingress queue limits.

For forged values, the peer advertised slot 0 and balance as `U256::MAX`, nonce
999,999 and code `0xfe`. The deployed contract at
`0x6b8218e3f2a694f40056f4c7ba2300de4be7d385` instead retained code
`0x60003560005500`, zero balance, nonce 1 and the legitimate sequential counter
value (633 at completion). These values matched on every client. Read hints select
reads from the receiver's local parent-state database; advertised values are not
execution inputs or trie updates.

## Recovery and final verification

After restoring normal outgoing messages and requesting peer connections, the
producer reconnected, the receiver's normal hint-hit counter advanced, and every
node advanced another 12 blocks. All blocks 1192–1725 were then compared across all
four clients: hash, parent, state/receipt/transaction roots, gas used, transaction
list and requests hash. All 634 receipts were rechecked against their original
block placement, status, gas and logs; contract state was checked at the common
settled height.

```text
block:     1725
hash:      0x81409f82a0c1ccd0bd2ab4d4d674c2fd37281819602d48b40cb0e92de1e3c870
stateRoot: 0x40d30547407be87c1759722ed6fa1a8b582063ed3bc4a1324098604603cc7c86
```

Focused tests also passed: seven BSC BAL tests and four engine read-hint tests,
including local-value prefetch, malformed/empty/oversized rejection, resource
budgets, block binding, cache bounds, disable flags and serial transaction order.

This tests advisory hints. There is no BAL consensus commitment or BAL-specific
slashing in this path. Blocks still undergo normal BSC execution and validation.
These bounded fault cases do not prove sustained DoS resistance or performance
improvement. The five application scenario groups were run separately in the
[common suite](bal-common-suite-2026-11-13.md); this fault run used a storage workload.

## Artifacts and reproduction

Artifacts remain in `/Users/chee/Documents/repo/bal-negative-cluster-2026-11-13/`:

- `cluster.py`, `negative.py`, `verify_negative.py`: isolated setup, live faults,
  receipt/state assertions and post-fault verification.
- `results/producer-only-fault.patch`: the exact outgoing-wire hook, restricted
  to the detached test producer checkout. `bin/` retains both tested binaries.
- `results/negative-report.json`, `negative.log`: all cases, injected block
  identifiers/frame sizes, transactions, receipts and per-case observations.
- `results/verification.json`, `verified-blocks.json`, `provenance.json`, node logs
  and metrics: full comparison and recovery evidence. `configuration.json` was
  inherited from the seed run; provenance and process records identify this run.
- `results/bsc-negative-tests.log`, `engine-negative-tests.log`: focused test results.
- `results/harness-setup-failure.*`: retained first-attempt harness failure from
  address formatting, corrected before the successful full run.

Inspect/adapt ports, fixture paths and selected revisions before a new run. Start
the owned cluster, execute `negative.py` using the existing
`node-deploy-bsc/.venv/bin/python` (eth-account dependency), run
`python3 verify_negative.py`, then stop it with `python3 cluster.py stop`.
Use a fresh output directory or archive prior reports before repeating.

All four owned processes were stopped. The regular target binary was restored and
its hash verified against the stock binary. No fault hook was added to the feature
branch. The reusable local-cluster skill now includes the negative-BAL procedure.

## Local rerun

Repeated on 2026-11-13 from branch HEAD `0831b7c66d5c2d1f502bc3fd0a590289def0e73e`.
The Rust source and companion Reth revision were unchanged; both reused binaries
were checked against their recorded SHA-256 hashes. An isolated copy of the stopped
negative-test chain used HTTP ports 28480–28483 and P2P ports 32680–32683.

All **13 fault cases passed again**, with **830 successful workload transactions**
(one deployment and 829 storage writes). All **694 blocks from 1835 through 2528**
matched across the four clients, as did every workload receipt and the final
contract state. Irrelevant-access and forged-value hints each invoked BAL prewarm
ten times without affecting correctness. The 64-frame burst caused no observed
disconnect; the oversized transport frame disconnected the producer and incremented
the receiver's oversize counter once. Reconnection and resumed normal hint hits
were verified before all owned processes were stopped.

```text
block:     2528
hash:      0xf11e40c5472d089b64e2343fc920c5a8618216e70d39ac94a08cf9b24e8328a1
stateRoot: 0x6b9e8a5f64c52ba2ed78fbbbf6513e02186839793d9b7e8d54727d7312f20ac0
```

The first rerun attempt stopped during deployment receipt comparison, before any
fault injection. Logs showed startup reorganizations after the copied validators
connected. The harness previously treated peer connectivity as sufficient readiness
and compared receipts before settlement. It now requires 24 new blocks and matching
settled roots before deploying, waits 12 blocks before comparing receipts, and
fetches those receipts again after the wait. The original setup transaction was
also verified successfully with identical receipts on all four clients after
convergence. No client Rust changes were needed.

Artifacts are retained in
`/Users/chee/Documents/repo/bal-negative-cluster-2026-11-13-rerun/`:
`results/negative-report.json`, `verification.json`, `verified-blocks.json`,
`provenance.json`, `run-status.json`, node logs and metrics. The initial failure
and its unmodified harness are preserved under `startup-attempt-1/`.
`results/harness-settlement.patch` records the scoped harness correction;
`run_once.py` bounds each phase and stops owned processes on success or failure.
The reusable skill now documents the startup/receipt-settlement requirement.
