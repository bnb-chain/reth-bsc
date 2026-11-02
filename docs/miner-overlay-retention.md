# Miner overlay retention experiment

A persistence frontier change previously retired the miner's entire `OverlayManager`, even when
its append-only block graph still covered the requested parent. Returning to an earlier captured
snapshot or branch also lost that manager's completed overlays.

Keep at most two managers in LRU order. Reuse only a manager that covers the requested chain, or
whose tip is an ancestor in the captured snapshot. Admit at most 32 distinct `(snapshot anchor,
parent)` views per manager; repeated views remain reusable at capacity. Never prune a manager in
place, and retire evicted references off the mining thread so older proof tasks remain valid.
The Reth dependency and build deadlines are unchanged.

This reuses managers, not flattened overlays across different anchors. Reth still keys computed
results by `(durable anchor, parent)` and still controls parent lookup and merge/copy costs.

## Metrics

Names below have the exported `reth_` prefix. All labels use fixed values, never block hashes.

| Metric | Meaning |
| --- | --- |
| `bsc_miner_overlay_cache_selections_total{result}` | Manager selections: `repeat` same view, `extend` append ancestors, `covered` existing graph/new view, `retained` select older manager, `new` allocate, `empty` no in-memory snapshot. This is **not** the flattened-overlay cache hit rate. |
| `bsc_miner_overlay_cache_generations_total{reason}` | New managers due to `cold`, compatible manager `capacity`, or `chain_miss`. |
| `bsc_miner_overlay_cache_evictions_total{reason}` | Owner references retired for `capacity` or `empty_snapshot`; in-flight tasks can retain clones. |
| `bsc_miner_overlay_cache_generations` | Managers owned by this cache, at most two. |
| `bsc_miner_overlay_cache_views` | Distinct snapshot anchor/parent views admitted to owned managers, at most 64. Does not count underlying proof-factory keys. |
| `bsc_miner_overlay_cache_block_references` | Block entries inserted into owned managers, summed across managers. Shared blocks can count twice. |
| `bsc_miner_overlay_cache_inserted_blocks` | Number of block entries inserted by each selection, zero on repeat/covered reuse. |
| `bsc_miner_overlay_cache_selection_seconds` | Selection/insertion wall time inside the cache lock; excludes acquiring the lock, actual flattened-overlay computation, proof waits, and asynchronous retirement. |

Admission and generation counts bound cache lifetime/entries, **not bytes**. Required captured
chains and references held by old workers can exceed these ownership counts. Compare RSS/anonymous
memory, existing `proof_overlay_prepare` phases, account-session initialization, root wait and chain
empty rate; more manager hits alone do not establish a performance improvement.

Local tests compare fresh/reused roots with account and storage changes across extension, skipped
heads, frontier changes, reorg/rollback, retained branches, capacity eviction and in-flight readers.
