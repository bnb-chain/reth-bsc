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

Rotate the manager on an in-memory anchor change, fork/rollback, missing parent snapshot, or after
32 distinct parent selections in a generation. The limit bounds retained cached parent views
when persistence stalls; it does not cap the ancestor snapshot needed to build a valid root.
Repeated selection of the same parent does not consume the limit. Reth still keys each cached
overlay by its actual durable anchor and parent hash, including when persistence advances
before the in-memory snapshot is pruned.

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
