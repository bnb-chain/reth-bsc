# Reuse the miner's parent-state overlay

## Problem

The miner creates an `OverlayManager` for every build. Consecutive builds therefore merge the
same unpersisted ancestor state repeatedly. In the captured block 34509517, merging 24 ancestors
took 180.54 ms of a 181 ms finish, despite zero user transactions. Nearby import tasks reused
their parent overlays and extended them by one block in 23–25 ms.

## Change

Keep a miner-owned manager across retries and linear parent-chain extensions. Insert only newly
observed executed ancestors. Keep the existing isolated miner worker pools and fresh sparse-trie
behavior; candidate output never advances the shared parent-state cache.

Retain at most two managers in LRU order across frontier changes and compatible chain
extensions. Admit at most 32 distinct `(snapshot anchor, parent)` views per manager;
repeated views remain reusable at capacity. Select a retained manager only if its block
graph covers the requested chain or its tip is an ancestor in the captured snapshot.
An empty snapshot retires all owned managers. These limits bound ownership and admissions,
not bytes or references held by in-flight tasks. Reth still keys each computed overlay by
its actual durable anchor and parent hash. See [overlay retention](miner-overlay-retention.md)
for selection metrics and validation details.

Never remove ancestors from a manager used by an in-flight task. Old provider factories and root
tasks retain their manager clones until completion/cancellation. Retired managers and unused
preserved sparse tries are released through the runtime drop worker rather than the miner's
critical path. The cache mutex protects selection/insertion only, not proof work or root waits.

## Reth v2.8 proof-provider cache

Proof workers share at most 16 completed `StateTrieOverlay` values, keyed by parent hash and
both durable frontier hashes (partial state/trie and Finish). Each value is built with trie
changesets enabled so cached proofs preserve exact trie masks. Execution overlays remain lazy
and are resolved by Reth against each provider's own database snapshot; no open transaction or
cursor is cached. An empty fallback may reuse a completed value but never waits for a normal
build to populate it. Failed construction leaves the entry retryable, and evicted entries stay
alive while existing workers hold them.

Each proof cache admits at most two queued or active retirements to its runtime's drop worker.
When both slots are occupied, it releases the evicted entry inline, after unlocking the cache.
The ordinary and private-child caches use separate runtimes and retirement budgets. These
limits bound retained entries, not allocated bytes or ownership held by in-flight readers.
`bsc_miner_proof_overlay_retirements_total{mode}` counts background and inline retirements;
`bsc_miner_proof_overlay_retire_seconds{mode,kind}` measures destruction on either path.
The existing `cache_retire` preparation phase measures caller latency, including submission
or inline cleanup, rather than background destruction time.

## Private-child parent reuse

A private child can extend an already completed ordinary proof overlay with its sealed private
parent's trie and hashed-state updates. The lookup requires the private parent's ancestor hash
and both durable frontier hashes to match the current database transaction. It uses a nonblocking
cache lookup and never waits for an ordinary computation. Missing, busy, or unfinished entries
fall back to the private manager's fresh overlay construction.

The extension runs on the existing private overlay worker pool. Copy-on-write preserves the
ordinary overlay; private blocks and derived results stay in the private manager/proof cache.
No database transaction, mutable sparse trie, or candidate state is shared with ordinary mining.
The existing private proof-cache bound and retirement budget also cover extended values.

`bsc_miner_private_overlay_total{result}` counts reuse and fallback reasons.
`bsc_miner_private_overlay_extend_seconds` measures extension work, and
`bsc_miner_private_overlay_total_seconds` includes the private worker queue wait.

## Validation

Local tests compare overlay contents and state roots against fresh managers for retries, linear
and skipped-head extensions, forks, rollback, and durable/in-memory anchor changes. A concurrent
old task must still finish correctly after its generation is replaced. Persistence-stall tests
check the generation limit. Tests use synthetic account and storage updates; they do not replace
a running-node reorg or performance test.

Repeat the existing 500M gas workload with the same reserve, logging, and transaction mix. Check
`parent_overlay=true` / `block_count=1` on steady linear progress, miner overlay job duration,
empty fallback submissions, canonical throughput, and RSS. The existing cache counters attribute
reuse/fills to `origin="miner"`; no per-transaction logging is added. Initial builds and anchor
changes can still require a full merge. No production speedup is claimed before that comparison.
