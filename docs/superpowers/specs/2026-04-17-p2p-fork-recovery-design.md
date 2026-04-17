# P2P Fork Recovery Design

## Background

This is the second half of the BSC validator livelock fix. The first half — periodic head announcement — is already merged; see [`2026-04-17-fix-p2p-livelock-design.md`](./2026-04-17-fix-p2p-livelock-design.md).

That fix solves *"peer doesn't know my head exists"*. It does not solve *"peer knows my head but can't merge into it"*. When two validators drift onto different fork chains, periodic announce lets them see each other's heads — but the code that reacts to those announcements still can't pull the missing blocks on the far side of the divergence point.

## Problem

### Current behaviour (buggy)

`src/node/network/block_import/service.rs` handles peer head announcements in two places, both with the same flawed pattern:

**1. `on_new_block_hashes` (lines 490-567)** — receives `NewBlockHashes(peer_head_hash, peer_head_num)`:

```
gap   = peer_head_num.saturating_sub(local_tip)
count = gap.clamp(1, 64)
spawn batch_request_range_and_await_import(peer, peer_head_num, peer_head_hash, count)
```

**2. `Syncing` branch of `new_payload` (lines 219-273)** — fires when a just-imported peer block has a missing parent. Duplicates the same formula, then calls `engine.fork_choice_updated(peer_head)` immediately after spawning the fetch.

### Why this fails on forks

The server side of `GetBlocksByRange` (`src/node/network/blocks_by_range.rs:78-118`) walks backwards via `parent_hash` from `start_hash` for `count` blocks. So a request for `(peer_head, count=gap)` returns only blocks at heights `[peer_head - gap + 1 .. peer_head]` on the **peer's fork**.

When the local tip and peer head are on different forks (common ancestor at some height `D < local_tip`):
- `gap = peer_head_num - local_tip` is small (often 0 or 1).
- The fetched range stops far above the divergence point `D`.
- The oldest fetched block's parent is on the peer's fork, which we don't have.
- `new_payload` returns `Syncing`. The `Syncing` branch triggers another identical fetch. The 200ms `DOWNLOAD_COOLDOWN_DURATION_MS` suppresses the retry. Stuck.

### Secondary bug in the `Syncing` branch

`batch_request_range_and_await_import` (`src/node/network/bsc_protocol/registry.rs:169-216`) is misleadingly named. It calls `request_blocks_by_range`, then enqueues each returned block into `block_import_sender` and returns `Ok(())` immediately. **It does not await import completion.** The `Syncing` branch then calls `engine.fork_choice_updated(peer_head)` straight away, while the fetched blocks are still pending `new_payload`. FCU returns `Syncing`; the reorg never lands.

Even with fork recovery, this race has to be eliminated — parallel `pending_imports` futures in `ImportService` (`service.rs:486`) give no per-block ordering guarantee, so an FCU issued while ancestors are still validating is always premature.

## Goal

Replace both fetch sites with a single fork-aware recovery primitive that:

1. Walks back from `peer_head` via `parent_hash` chains, one `GetBlocksByRange` hop at a time, until it finds a block already on the local chain (the common ancestor) or hits a depth cap.
2. Skips blocks we already have (canonical or side-chain), avoiding redundant downloads and redundant `new_payload` calls.
3. Imports fork blocks in strict oldest-first order, **awaiting each `new_payload` to return `Valid`** before proceeding.
4. Issues `fork_choice_updated(peer_head)` only after the complete ancestor → head chain has been validated.
5. Fires an FCU on short-circuit hits too (peer head already present as a side-chain block), so previously-introduced-but-never-applied forks get re-evaluated.

## Non-Goals

- No TD-based peer gating. Engine-tree already rejects reorgs whose combined difficulty loses; an occasional wasted import is cheaper than plumbing reliable peer TD tracking through the stack.
- No new wire messages. `GetBlocksByRange` / `BlocksByRange` cover everything we need.
- No peer-switching retries. If a single recovery attempt fails mid-way (peer unreachable, truncated response), the next periodic announce retriggers it.
- No fallback to staged sync. The 256-block depth cap bounds recovery to fork depths that should be resolved via the fast path only.

## Design Principles

1. **Single primitive, two callers.** The same `recover_ancestors` function is invoked from `on_new_block_hashes` and from the `Syncing` branch. The current duplicate fetch logic is removed.
2. **Local-first checks.** Before every hop, and for every returned block, we consult `BlockHashReader` / `HeaderProvider` to avoid re-fetching or re-importing blocks we already have.
3. **Drive import directly via `engine.new_payload`.** The recovery task does not go through `block_import_sender`, because the main `ImportService` loop drives `pending_imports` in parallel and provides no ordering guarantee. Sequential `.await` on `new_payload` is the only way to guarantee "parent Valid before child submitted."
4. **FCU last, always.** No FCU fires until the full ancestor → head chain has been validated. This eliminates the existing FCU-before-import race.
5. **Bounded work.** `MAX_FORK_DEPTH = 256`, implying at most `ceil(256 / MAX_REQUEST_RANGE_BLOCKS_COUNT) = 4` hops and 4 bounded `new_payload` rounds. Recovery either succeeds or gives up within this envelope; it never loops unbounded.

## Architecture

### New module: `src/node/network/block_import/fork_recover.rs`

Exports one free async function:

```rust
pub async fn recover_ancestors<Provider>(
    peer_id: PeerId,
    head_hash: B256,
    head_num: u64,
    engine: BeaconConsensusEngineHandle<BscEngineTypes>,
    forkchoice_engine: BscForkChoiceEngine<Provider>,
) -> Result<(), ForkRecoverError>
where
    Provider: BlockNumReader + BlockHashReader + HeaderProvider<Header = Header>
              + Clone + Send + Sync + 'static,
```

(`Provider` matches the bounds already used on `ImportService<Provider>` at `service.rs:108`; `forkchoice_engine.provider` is the `BlockHashReader` / `HeaderProvider` we consult for local-presence checks; `engine` is the `BeaconConsensusEngineHandle` already cloned inside `ImportService::new_payload` at `service.rs:149`. Exact concrete type names will be confirmed during implementation, but the bounds stay as stated.)

It owns three phases:

**Phase 1 — discover and fetch.** Hop-back loop keyed on a `cursor: (num, hash)` that starts at `(head_num, head_hash)`:

```
fork_blocks: Vec<BscBlock> = []          // newest → oldest during accumulation
walked = 0
loop:
    // Pre-hop: is cursor already local? Then ancestor is at or above it.
    // This runs every iteration BEFORE the bound check, so the final cursor
    // advance (after walked hits MAX_FORK_DEPTH) still gets one check against
    // the ancestor-just-below-the-walked-range case.
    if provider.block_hash(cursor.num)? == Some(cursor.hash): break Found
    if provider.header_by_hash(cursor.hash)?.is_some():       break Found

    if walked >= MAX_FORK_DEPTH: return Err(ForkTooDeep)

    count = min(MAX_REQUEST_RANGE_BLOCKS_COUNT = 64, MAX_FORK_DEPTH - walked)
    resp  = request_blocks_by_range(peer_id, cursor.num, cursor.hash,
                                    count, FETCH_TIMEOUT).await?
    if resp.blocks.is_empty(): return Err(EmptyResponse)

    for b in &resp.blocks:                 // already newest → oldest
        if provider.block_hash(b.number)? == Some(b.hash): break 'outer Found
        if provider.header_by_hash(b.hash)?.is_some(): continue  // side-chain already
        fork_blocks.push(b.clone())

    oldest = resp.blocks.last()
    walked += resp.blocks.len()
    cursor = (oldest.number - 1, oldest.header.parent_hash)
```

With this structure, `MAX_FORK_DEPTH = 256` means any fork depth ≤ 256 is recoverable: hop 4 walks 256 blocks, then the next iteration's pre-hop check covers the ancestor sitting exactly at `peer_head - 256`. Fork depth 257+ returns `ForkTooDeep`.

The loop uses **`request_blocks_by_range`** (`registry.rs:140-165`), which only performs the wire round-trip. It does not use `batch_request_range_and_await_import`, which would enqueue blocks into the import pipeline and defeat our ordering.

**Phase 2 — import oldest-first with per-block await.**

```
fork_blocks.reverse()                      // oldest → newest
for b in fork_blocks:
    let payload = b.to_execution_payload()
    match engine.new_payload(payload).await? {
        Valid    => continue,
        Invalid  => log::warn, return Err(ImportInvalid)
        Syncing  => log::warn "unexpected mid-chain Syncing", return Err(ImportSyncing)
    }
```

Any `Invalid` or mid-chain `Syncing` aborts recovery. (`Invalid` should be impossible if peer gave us a consistent chain; `Syncing` should be impossible because we've established the parent is local before submitting.)

**Phase 3 — final FCU.**

```
let fc_state = ForkchoiceState {
    head_block_hash: head_hash,
    safe_block_hash: B256::ZERO,
    finalized_block_hash: B256::ZERO,
};
engine.fork_choice_updated(fc_state, None, EngineApiMessageVersion::V1).await?;
```

This runs in both branches:
- When Phase 1 discovered new fork blocks and Phase 2 imported them.
- **Also when Phase 1 short-circuited on `header_by_hash(head_hash).is_some()`** — the block was already in the tree as a side-chain from a previous recovery that didn't win TD at that time, but conditions may have changed. Re-issuing FCU is idempotent when already canonical and harmless otherwise.

### Changes in `src/node/network/block_import/service.rs`

**Remove** the existing fetch-and-spawn logic in two places:
- Inside `on_new_block_hashes` (lines 523-565), delete the gap/count math, target-peer selection, and the spawned `batch_request_range_and_await_import` call.
- Inside the `Syncing` arm of `new_payload` (lines 219-309), delete the spawned fetch, the gap/count math, *and the immediate `fork_choice_updated` call*.

**Replace** with short calls to `recover_ancestors`. The three existing LRU checks (`processed_blocks`, `queued_blocks`, plus the new `recovering_heads`) are sufficient — no DB lookup is needed on the hot path. `recover_ancestors`' Phase 1 pre-hop check handles the "already canonical" case inside the spawned task.

```rust
// on_new_block_hashes — inside the loop over NewBlockHashes
if self.processed_blocks.contains(&hash_number.hash) { continue; }
if self.queued_blocks.contains(&hash_number.hash)    { continue; }
if self.recovering_heads.contains(&hash_number.hash) { continue; }
self.recovering_heads.insert(hash_number.hash);
tokio::spawn(recover_ancestors_task(peer_id, hash_number.hash, hash_number.number, ...));
```

```rust
// Syncing arm of new_payload — after logging
if recovering_heads.contains(&block_hash) { return None; }
recovering_heads.insert(block_hash);
tokio::spawn(recover_ancestors_task(peer_id, block_hash, block_number, ...));
None
```

Where `recover_ancestors_task` is a thin wrapper that calls `recover_ancestors`, logs the outcome, and removes the hash from `recovering_heads` in both success and failure paths (use a guard / `defer`-style cleanup).

**Add** a new field `recovering_heads: LruCache<B256>` alongside the existing `processed_blocks` / `queued_blocks` (same `reth::network::cache::LruCache` type, cap `LRU_PROCESSED_BLOCKS_SIZE` for consistency). Mutation from the detached task happens under `Arc<Mutex<LruCache<B256>>>`; lock scope is a single `insert` or `remove`, so contention is negligible.

### Peer selection

Unchanged from the current code: try the announcing peer first via `has_registered_peer(peer_id)`, otherwise pick any peer from `list_registered_peers()`. If neither has the block, the hop fails with `EmptyResponse` and the next periodic announce gives us another shot.

## Constants

| Constant | Value | Rationale |
|----------|-------|-----------|
| `MAX_FORK_DEPTH` | 256 blocks | ~2 validator turn cycles on BSC; sufficient for all realistic validator livelocks while bounding work. |
| `MAX_REQUEST_RANGE_BLOCKS_COUNT` | 64 (existing) | Per-request cap enforced by server; recovery uses this to size each hop. |
| `FETCH_TIMEOUT` | 5s (existing for `batch_request_range_and_await_import`) | Matches existing range-fetch timeout. |
| `RECOVERING_HEADS_CAP` | `LRU_PROCESSED_BLOCKS_SIZE` (=100) | Reuses the existing constant (`service.rs:70`) so all three head-dedup caches share sizing. 100 is ample: in-flight recoveries typically number in the low single digits. |

## Error handling

| Failure | Response |
|---------|----------|
| `EmptyResponse` from any hop | Log `debug`, drop the recovery; next announce retries. |
| `ForkTooDeep` (walked ≥ 256 without ancestor) | Log `warn` with peer, head_hash, head_num; drop recovery. A fork deeper than 256 is either a chain split requiring operator action or a misbehaving peer — neither is something the import path should paper over. |
| `new_payload` returns `Invalid` | Log `warn`; abort subsequent imports in this recovery; do not FCU. No peer reputation penalty (consistent with existing `new_payload` Invalid handling). |
| `new_payload` returns `Syncing` mid-chain | Log `warn` (unexpected — our pre-checks should have prevented this). Abort recovery, no FCU. |
| `fork_choice_updated` returns error | Log `trace` (matches existing behaviour in the code we removed). |
| Task panic / network disconnect | `recovering_heads` cleanup guard ensures the hash is removed so the next announce can retry. |

## Concurrency model

- Each announcement of a unique `head_hash` spawns at most one detached recovery task, enforced by `recovering_heads` LRU.
- Different heads recover concurrently; there is no global lock.
- Recovery tasks own clones of `engine` and `forkchoice_engine` (the provider handle lives inside `forkchoice_engine.provider`, which is already `Clone`). They do not hold any other `ImportService` state.
- `recovering_heads` lives as `Arc<Mutex<LruCache<B256>>>` shared between the main task (insert before `tokio::spawn`; `contains` check on dedup) and the detached task (remove on completion via RAII guard). Lock scope is a single `insert` / `remove` / `contains` call, so contention is negligible.

## Testing strategy

Unit tests in `fork_recover.rs`:

1. **Single-hop happy path.** Mock provider with canonical `[0..100]`; mock peer serving canonical `[0..100]`. Announce head 100. Expect: no fetch issued (cursor matches local via `block_hash`), zero imports, single FCU.
2. **Simple linear-ahead.** Mock canonical `[0..100]`; peer has `[0..101]`. Announce head 101. Expect: one hop fetching `[101]`; one `new_payload(101)` returning Valid; FCU(101).
3. **Short fork within one hop.** Local canonical `[0..=95, 96X..=100X]`; peer chain `[0..=95, 96Y..=102Y]` (divergence at 95, fork depth 7). Announce `(hash=102Y, num=102)`. Expect: one hop with `count=64` returning `[102Y, 101Y, ..., 96Y, 95, 94, ..., 39]`; per-block loop pushes `[102Y..=96Y]` into `fork_blocks`, hits `block_hash(95) == 95_hash` on the 8th element and breaks; reverses to `[96Y..=102Y]` and imports in order; FCU(102Y).
4. **Fork deeper than one hop.** Local canonical `[0..=100]`, peer head at 200 with divergence at 120 (fork depth 80). Expect two hops (first covers `[200..=137]`, second covers `[136..=120]` with ancestor at 120 hit mid-batch). Verify hop count and final import order `[121Y..=200Y]`.
5. **Fork at exactly `MAX_FORK_DEPTH`.** Fork depth 256 (ancestor at `peer_head - 256`). Expect success via the post-loop pre-hop check on cursor `(peer_head - 256, ancestor_hash)`; 4 full hops + 1 extra pre-hop lookup; no 5th fetch issued.
6. **Fork deeper than `MAX_FORK_DEPTH`.** Fork depth 257. Expect `ForkTooDeep`, no imports, no FCU.
7. **Head already on canonical.** `block_hash(head_num) == head_hash`. Expect zero network requests, zero imports, FCU(head).
8. **Head already in tree as side-chain.** `header_by_hash(head_hash).is_some()` but not canonical. Expect zero requests, zero imports, **FCU fired anyway** (re-evaluation requirement from §Goal #5).
9. **Mid-chain already-present side block.** Fork chain contains one block whose hash we already have from a prior recovery; verify it's skipped in the `fork_blocks` accumulation but ancestor walk continues correctly.
10. **`new_payload` returns Invalid on one block.** Verify abort, no FCU, no further imports.
11. **Empty hop response.** Peer returns zero blocks. Verify `EmptyResponse` error path cleans up `recovering_heads`.
12. **Concurrent announces for same head.** Second announce arrives while first recovery is in flight. Verify only one task spawned, second announce is a no-op.

Integration testing — minimum: a two-node testbed where node A and node B produce divergent chains (simulated by freezing peer visibility during fork generation), then reconnected. Verify both converge on the higher-TD tip within 2× the announce interval.

## Migration / rollout

- No wire-protocol change; no handshake change.
- All changes localized to `src/node/network/block_import/service.rs` and the new `fork_recover.rs`.
- Feature behaviour is on by default; no flag needed. The prior code path was broken on fork, so this is strictly an improvement.
- Metrics: add counters for `fork_recover_started`, `fork_recover_succeeded`, `fork_recover_too_deep`, `fork_recover_failed`. Depth histogram (`fork_depth_blocks`) is useful for tuning `MAX_FORK_DEPTH` in follow-ups.

## Open questions

None gating implementation. TD-based peer gating and fork-depth histogram tuning are explicit follow-ups.
