# Fork Recovery Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Eliminate the `EmptyResponse`-induced recovery loop observed after shipping fork recovery (commit `0745554`) by (a) making the `reth-bsc` `GetBlocksByRange` server consult persistent storage when the in-memory body cache misses, and (b) making the client-side fork-recovery resilient to empty / timed-out responses by rotating peers and cooling down recently-failed heads.

**Architecture:**
- **Peer-side (server) fix.** `build_blocks_by_range_response` currently reads only `BODY_CACHE` (a 512-entry broadcast-only LRU). We add a `FULL_BLOCK_PROVIDER` fallback path that reads from `BlockReader`, and we tighten the existing `CachedFullBlockProvider<P>` in `network/mod.rs` so it returns **full blocks with bodies** instead of header-only shells.
- **Client-side (requester) hardening.**
  1. `BscRangeFetcher::fetch` (and the underlying `registry::request_blocks_by_range`) gain a failover loop: on `EmptyResponse` / timeout / IO error the fetcher rotates through other registered BSC peers up to `MAX_PEER_ATTEMPTS`.
  2. `ImportService` grows a `recent_failed_heads` cooler (LRU + per-entry deadline). After `recover_ancestors` fails for a head, the cooler suppresses re-spawning recovery for that head until the cooldown elapses, so the 3s head-announce tick doesn't storm.

**Tech Stack:** Rust, tokio, reth-bsc 0.1, `parking_lot::Mutex`, `alloy_primitives::B256`, `reth_provider::{BlockReader, HeaderProvider, BlockNumReader}`, the existing `reth::network::cache::LruCache`.

**Spec (inline):** see the Background/Problem/Design sections below. No separate design doc is warranted — this plan bolts onto the already-shipped primitives from `docs/superpowers/specs/2026-04-17-p2p-fork-recovery-design.md`.

---

## Background

After Tasks 1–8 of `docs/superpowers/plans/2026-04-17-p2p-fork-recovery.md` landed, production logs show `recover_ancestors` aborting during Phase 1 with:

```
error=peer returned empty response at cursor (19194099, 0x26a4a9acdb8b317776…)
```

Tracing both sides of the request-response:

1. Our client (`fork_recover::discover_fork_blocks`) sends `GetBlocksByRange(start=peer_head_num, hash=peer_head_hash, count=4)` and treats an empty `blocks` vec as a fatal `ForkRecoverError::EmptyResponse` (`src/node/network/block_import/fork_recover.rs:168-175`).
2. On the peer side, `build_blocks_by_range_response` (`src/node/network/blocks_by_range.rs:78-118`) **only** consults `crate::shared::{get_cached_block_by_hash, get_cached_block_by_number}`, which read the 512-entry in-memory `BODY_CACHE` populated exclusively from `NewBlock` broadcast paths (`src/node/network/block_import/service.rs:304, 724`). Blocks acquired via staged sync are **never** inserted there.
3. Go-bsc's equivalent handler (`bsc/eth/protocols/bsc/handler.go:179-181`) returns `error` and drops the peer when the start block is missing — it never returns an empty list. So the observed empty response must be coming from a `reth-bsc` peer with a cache miss.

Because the 3-second periodic head announce (from the livelock fix) keeps re-announcing the same stuck head, `on_new_block_hashes` re-spawns `recover_ancestors`, which fails the same way every tick. `recovering_heads` only dedups *concurrent* attempts; it doesn't add a cool-off.

## Problem

### Peer-side bug
`build_blocks_by_range_response` never falls back to persistent storage. A `FullBlockProvider` trait and a global installation slot already exist in `src/shared.rs:200-206` and `src/node/network/mod.rs:256-307`, but:

- The range-builder doesn't consult `FULL_BLOCK_PROVIDER`.
- `CachedFullBlockProvider<P>` returns blocks with `BlockBody::default()` (empty txs), so even if it were consulted it would hand out unverifiable shells.

### Client-side fragility
- `BscRangeFetcher::fetch` asks exactly one peer and surfaces any error / empty as fatal.
- `discover_fork_blocks` treats `Ok(Vec::new())` as `EmptyResponse` and aborts the entire Phase-1 walk.
- `ImportService::on_new_block_hashes` re-spawns recovery on every periodic announce after a failure, so the same broken head produces N failures per peer per minute.

## Goal

1. A `reth-bsc` peer that has a requested block in its DB (canonical or side-chain) **must** include full bodies in its `BlocksByRange` response.
2. A client encountering an empty/timed-out response from the announcing peer **must** attempt at least one other BSC peer before declaring the recovery a failure.
3. A head that has failed recovery **must** be suppressed from re-recovery for `FAILED_HEAD_COOLDOWN` (default 30s).

## Non-Goals

- No change to the `GetBlocksByRange` wire format or message IDs.
- No change to the discovery/Phase-1 walk algorithm itself (still backwards via `parent_hash`).
- No cross-peer result merging. Failover picks a single peer's response per hop.
- No persistence for the failed-heads cooler: it's process-lifetime only.

---

## File Map

- **Modify** `src/shared.rs`
  - Add `pub fn get_full_block_provider() -> Option<Arc<dyn FullBlockProvider + Send + Sync>>` (~below line 460).
- **Modify** `src/node/network/blocks_by_range.rs`
  - Rewrite `build_blocks_by_range_response` so cache misses fall back to `FULL_BLOCK_PROVIDER`.
  - Add `#[cfg(test)] mod tests` covering cache-hit, provider-fallback, and nothing-available paths.
- **Modify** `src/node/network/mod.rs`
  - Replace `CachedFullBlockProvider<P>` with one bounded by `BlockReader<Block = BscBlock>` so it returns full blocks with bodies.
- **Modify** `src/node/network/bsc_protocol/registry.rs`
  - Add `pub async fn request_blocks_by_range_with_failover(preferred: PeerId, …, max_attempts: usize) -> Result<BlocksByRangePacket, String>` that rotates through other registered BSC peers on empty/timeout/IO errors.
  - Expose an internal pure helper `plan_failover_peers(preferred, registered, max_attempts)` for unit testing.
- **Modify** `src/node/network/block_import/fork_recover.rs`
  - `BscRangeFetcher::fetch` calls the new `…_with_failover` helper (no signature change).
  - Add one unit test proving that a fake `RangeFetcher` returning `Ok(vec![])` once then a non-empty response lets `discover_fork_blocks` complete — but only via the fetcher-level retry (we keep `EmptyResponse` as a real error if **all** attempts return empty).
  - Add public constants `MAX_PEER_ATTEMPTS: usize = 3` and `FAILED_HEAD_COOLDOWN: Duration = Duration::from_secs(30)`.
  - Add a small `FailedHeadsCooler` type + `new_failed_heads_cooler()` factory (mirroring `RecoveringHeads` / `new_recovering_heads`).
- **Modify** `src/node/network/block_import/service.rs`
  - Add `failed_heads: FailedHeadsCooler` to `ImportService` and initialise in `new()`.
  - Gate the spawn in `on_new_block_hashes` (line ~490) and the `Syncing` arm in `new_payload` (line ~258) on `!failed_heads.is_cooling(head)`.
  - Inside each spawned task, call `failed_heads.mark_failed(head)` on `recover_ancestors` `Err(_)`.

---

## Task 1: Surface `FULL_BLOCK_PROVIDER` via an accessor

**Files:**
- Modify: `src/shared.rs:200-206, 456-470`

- [ ] **Step 1.1: Read existing setter/trait site to confirm type**

Run: `rg -n "FULL_BLOCK_PROVIDER|pub fn set_full_block_provider\(" src/shared.rs`
Expected: matches at `static FULL_BLOCK_PROVIDER` (line 206) and `pub fn set_full_block_provider` (line 456).

- [ ] **Step 1.2: Add the getter directly under `set_full_block_provider`**

In `src/shared.rs`, after the `pub fn set_full_block_provider` function body, add:

```rust
/// Get a clone of the installed [`FullBlockProvider`], if any.
pub fn get_full_block_provider() -> Option<Arc<dyn FullBlockProvider + Send + Sync>> {
    FULL_BLOCK_PROVIDER.get().cloned()
}
```

- [ ] **Step 1.3: Verify it compiles**

Run: `cargo check -p reth_bsc`
Expected: PASS (no new warnings introduced).

- [ ] **Step 1.4: Commit**

```bash
git add src/shared.rs
git commit -m "feat(shared): expose get_full_block_provider accessor

Needed by the GetBlocksByRange responder so it can fall back to the
installed full-block provider when the in-memory BODY_CACHE misses."
```

---

## Task 2: Upgrade the installed `CachedFullBlockProvider` to return full blocks with bodies

**Files:**
- Modify: `src/node/network/mod.rs:256-307`

**Context:** the current impl satisfies only `HeaderProvider + BlockNumReader`, so its fallback hands out `BscBlock` with `BlockBody::default()` (empty txs/sidecars). That is useless to a peer that will try to `new_payload` the block. `ctx.provider()` in reth-bsc satisfies `BlockReader<Block = BscBlock>` — confirmed by `src/rpc/blob.rs:11,237`.

- [ ] **Step 2.1: Read the current impl**

Run: `sed -n '253,307p' src/node/network/mod.rs` (via Read tool).
Expected: sees `struct CachedFullBlockProvider<P>` and two `.map(|h| BscBlock { header: h, body: default })` branches.

- [ ] **Step 2.2: Rewrite the struct and impl**

Replace the entire block from line 253 (`// Install a cached…`) through the `set_full_block_provider` call at 306 with:

```rust
// Install a cached full block provider so BSC BlocksByRange replies can include
// full bodies when blocks are in the provider (DB + canonical in-memory state),
// not just the broadcast-populated BODY_CACHE.
{
    use reth_provider::BlockReader;
    struct CachedFullBlockProvider<P> {
        inner: P,
    }
    impl<P> crate::shared::FullBlockProvider for CachedFullBlockProvider<P>
    where
        P: BlockReader<Block = crate::node::primitives::BscBlock>
            + Clone
            + Send
            + Sync
            + 'static,
    {
        fn block_by_hash(
            &self,
            hash: &alloy_primitives::B256,
        ) -> Option<crate::node::primitives::BscBlock> {
            crate::shared::get_cached_block_by_hash(hash)
                .or_else(|| self.inner.block_by_hash(*hash).ok().flatten())
        }
        fn block_by_number(
            &self,
            number: u64,
        ) -> Option<crate::node::primitives::BscBlock> {
            crate::shared::get_cached_block_by_number(number)
                .or_else(|| self.inner.block_by_number(number).ok().flatten())
        }
    }

    let _ = crate::shared::set_full_block_provider(Arc::new(CachedFullBlockProvider {
        inner: provider.clone(),
    }));
}
```

- [ ] **Step 2.3: Verify compile**

Run: `cargo check -p reth_bsc`
Expected: PASS. If it fails with "the trait `BlockReader` is not implemented for `ProviderFactory<…>`", inspect the concrete type of `ctx.provider()` — reth's `BlockchainProvider` and `DatabaseProvider` both impl `BlockReader<Block = NodePrimitives::Block>` which is `BscBlock` for this node.

- [ ] **Step 2.4: Commit**

```bash
git add src/node/network/mod.rs
git commit -m "feat(network): CachedFullBlockProvider returns full blocks from BlockReader

Previously it returned header-only shells with empty bodies, so even
installing it didn't help peers complete a GetBlocksByRange response.
Bounding on BlockReader<Block=BscBlock> lets the fallback hand out
the full block (tx bodies + sidecars) from the DB / canonical state."
```

---

## Task 3: Route `build_blocks_by_range_response` through the provider fallback (TDD)

**Files:**
- Modify: `src/node/network/blocks_by_range.rs:78-118` and append a `#[cfg(test)] mod tests`
- Test: `src/node/network/blocks_by_range.rs` (tests co-located)

The existing `build_blocks_by_range_response` hard-wires `crate::shared::get_cached_block_by_{hash,number}`. We introduce an internal pure helper parameterised by a `BlockLookup` callback, then let the public function call it with the real lookup that tries the cache and then `get_full_block_provider`.

- [ ] **Step 3.1: Write the failing test (cache-miss + provider-fallback)**

Append to the bottom of `src/node/network/blocks_by_range.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::primitives::{BscBlock, BscBlockBody};
    use alloy_consensus::Header;
    use alloy_primitives::B256;
    use reth_ethereum_primitives::BlockBody;
    use std::collections::HashMap;

    fn mk_block(number: u64, parent: B256) -> BscBlock {
        BscBlock {
            header: Header { number, parent_hash: parent, ..Default::default() },
            body: BscBlockBody { inner: BlockBody::default(), sidecars: None },
        }
    }

    #[test]
    fn build_response_falls_back_to_provider_lookup_on_cache_miss() {
        // Simulate a 4-block parent chain only available via the "provider" map.
        let b3 = mk_block(3, B256::ZERO);
        let b2 = mk_block(2, b3.header.hash_slow());
        let b1 = mk_block(1, b2.header.hash_slow());
        let b0 = mk_block(0, b1.header.hash_slow());

        let mut by_hash: HashMap<B256, BscBlock> = HashMap::new();
        by_hash.insert(b0.header.hash_slow(), b0.clone());
        by_hash.insert(b1.header.hash_slow(), b1.clone());
        by_hash.insert(b2.header.hash_slow(), b2.clone());
        by_hash.insert(b3.header.hash_slow(), b3.clone());

        let req = GetBlocksByRangePacket {
            request_id: 42,
            start_block_height: 0,
            start_block_hash: b0.header.hash_slow(),
            count: 4,
        };
        let resp = build_blocks_by_range_response_with(&req, |h, _| {
            by_hash.get(h).cloned()
        });

        assert_eq!(resp.request_id, 42);
        assert_eq!(resp.blocks.len(), 4, "should walk the full chain via provider fallback");
        assert_eq!(resp.blocks[0].header.number, 0);
        assert_eq!(resp.blocks[3].header.number, 3);
    }

    #[test]
    fn build_response_returns_empty_when_start_block_unknown() {
        let req = GetBlocksByRangePacket {
            request_id: 7,
            start_block_height: 0,
            start_block_hash: B256::repeat_byte(0xaa),
            count: 4,
        };
        let resp = build_blocks_by_range_response_with(&req, |_, _| None);
        assert!(resp.blocks.is_empty());
    }

    #[test]
    fn build_response_looks_up_by_number_when_hash_is_zero() {
        let b0 = mk_block(10, B256::ZERO);
        let req = GetBlocksByRangePacket {
            request_id: 9,
            start_block_height: 10,
            start_block_hash: B256::ZERO,
            count: 1,
        };
        let b0_clone = b0.clone();
        let resp = build_blocks_by_range_response_with(&req, move |_, n| {
            if n == Some(10) { Some(b0_clone.clone()) } else { None }
        });
        assert_eq!(resp.blocks.len(), 1);
        assert_eq!(resp.blocks[0].header.number, 10);
    }
}
```

- [ ] **Step 3.2: Run test to verify it fails**

Run: `cargo test -p reth_bsc --lib -- --nocapture node::network::blocks_by_range::tests`
Expected: FAIL with "cannot find function `build_blocks_by_range_response_with` in this module".

- [ ] **Step 3.3: Implement the parameterised helper**

Replace the body of `build_blocks_by_range_response` in `src/node/network/blocks_by_range.rs` with:

```rust
/// Pure variant of [`build_blocks_by_range_response`] parameterised by a block
/// lookup. `lookup(hash, number)` returns `Some(block)` when the block is
/// available; the caller decides whether to consult a cache, a provider, or
/// both. Exposed `pub(crate)` for testing.
pub(crate) fn build_blocks_by_range_response_with<F>(
    req: &GetBlocksByRangePacket,
    mut lookup: F,
) -> BlocksByRangePacket
where
    F: FnMut(&B256, Option<u64>) -> Option<BscBlock>,
{
    let mut blocks: Vec<BscBlock> = Vec::new();

    let mut current_block: Option<BscBlock> = if req.start_block_hash != B256::ZERO {
        lookup(&req.start_block_hash, None)
    } else {
        lookup(&B256::ZERO, Some(req.start_block_height))
    };

    let mut remaining = req.count.min(MAX_REQUEST_RANGE_BLOCKS_COUNT);
    while let (Some(block), r) = (current_block.clone(), remaining) {
        if r == 0 {
            break;
        }
        blocks.push(block.clone());

        let parent_hash = block.header.parent_hash;
        current_block = if parent_hash != B256::ZERO { lookup(&parent_hash, None) } else { None };
        remaining -= 1;
    }

    let requested = req.count.min(MAX_REQUEST_RANGE_BLOCKS_COUNT) as usize;
    if blocks.len() < requested {
        tracing::debug!(
            target: "bsc_protocol",
            request_id = req.request_id,
            requested = requested,
            produced = blocks.len(),
            "Truncated BlocksByRange due to missing parent/body"
        );
    }

    BlocksByRangePacket { request_id: req.request_id, blocks }
}

/// Build a response for a `GetBlocksByRange` request using the BODY_CACHE
/// first, then the installed [`FullBlockProvider`] (DB / canonical state).
pub fn build_blocks_by_range_response(req: &GetBlocksByRangePacket) -> BlocksByRangePacket {
    use crate::shared::{
        get_cached_block_by_hash, get_cached_block_by_number, get_full_block_provider,
    };

    let provider = get_full_block_provider();
    build_blocks_by_range_response_with(req, |hash, number_opt| match number_opt {
        Some(num) => get_cached_block_by_number(num)
            .or_else(|| provider.as_ref().and_then(|p| p.block_by_number(num))),
        None => get_cached_block_by_hash(hash)
            .or_else(|| provider.as_ref().and_then(|p| p.block_by_hash(hash))),
    })
}
```

- [ ] **Step 3.4: Run tests to verify they pass**

Run: `cargo test -p reth_bsc --lib -- node::network::blocks_by_range::tests`
Expected: 3 tests pass.

- [ ] **Step 3.5: Commit**

```bash
git add src/node/network/blocks_by_range.rs
git commit -m "fix(bsc-proto): GetBlocksByRange falls back to FullBlockProvider on cache miss

build_blocks_by_range_response used to consult only the 512-entry
broadcast-populated BODY_CACHE. Blocks acquired via staged sync were
invisible, so fork-recovery peers got empty responses for historical
blocks that were sitting in the DB. Thread a lookup closure through a
pure helper and chain BODY_CACHE -> FULL_BLOCK_PROVIDER."
```

---

## Task 4: Add multi-peer failover to the BSC range request (TDD)

**Files:**
- Modify: `src/node/network/bsc_protocol/registry.rs`
- Modify: `src/node/network/block_import/fork_recover.rs`

The failover policy is simple: try `preferred`, then up to `MAX_PEER_ATTEMPTS - 1` additional registered BSC peers (excluding `preferred` and excluding any already tried). Failover triggers on `Err(_)` from `request_blocks_by_range` **or** on `Ok(resp)` with `resp.blocks.is_empty()`. The last response wins.

- [ ] **Step 4.1: Add pure peer-selection helper with unit tests in `registry.rs`**

Append to the bottom of `src/node/network/bsc_protocol/registry.rs`:

```rust
/// Compute a failover peer ordering: `preferred` first, then up to
/// `max_attempts - 1` other peers from `registered`, preserving order and
/// deduplicating. Returns at most `max_attempts` entries.
pub(crate) fn plan_failover_peers(
    preferred: PeerId,
    registered: Vec<PeerId>,
    max_attempts: usize,
) -> Vec<PeerId> {
    if max_attempts == 0 {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(max_attempts);
    out.push(preferred);
    for p in registered {
        if out.len() >= max_attempts {
            break;
        }
        if !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

#[cfg(test)]
mod failover_tests {
    use super::*;
    use alloy_primitives::B512;

    fn pid(byte: u8) -> PeerId {
        B512::repeat_byte(byte)
    }

    #[test]
    fn plan_puts_preferred_first_and_dedups() {
        let plan = plan_failover_peers(pid(1), vec![pid(2), pid(1), pid(3)], 3);
        assert_eq!(plan, vec![pid(1), pid(2), pid(3)]);
    }

    #[test]
    fn plan_respects_max_attempts() {
        let plan = plan_failover_peers(pid(1), vec![pid(2), pid(3), pid(4)], 2);
        assert_eq!(plan, vec![pid(1), pid(2)]);
    }

    #[test]
    fn plan_handles_zero_attempts() {
        let plan = plan_failover_peers(pid(1), vec![pid(2)], 0);
        assert!(plan.is_empty());
    }

    #[test]
    fn plan_handles_empty_registered() {
        let plan = plan_failover_peers(pid(1), vec![], 5);
        assert_eq!(plan, vec![pid(1)]);
    }
}
```

- [ ] **Step 4.2: Run the new tests**

Run: `cargo test -p reth_bsc --lib -- node::network::bsc_protocol::registry::failover_tests`
Expected: 4 tests pass.

- [ ] **Step 4.3: Add the failover-aware request function**

Below `plan_failover_peers` in `src/node/network/bsc_protocol/registry.rs`, add:

```rust
/// Like [`request_blocks_by_range`], but rotates through other registered BSC
/// peers on `Err` or empty response. Returns the first non-empty success,
/// otherwise the last seen result (preserving the original error for
/// diagnostics).
pub async fn request_blocks_by_range_with_failover(
    preferred: PeerId,
    start_height: u64,
    start_hash: B256,
    count: u64,
    timeout_dur: Duration,
    max_attempts: usize,
) -> Result<BlocksByRangePacket, String> {
    let peers = plan_failover_peers(preferred, list_registered_peers(), max_attempts);
    if peers.is_empty() {
        return Err("no BSC peers available for range request".to_string());
    }

    let mut last: Result<BlocksByRangePacket, String> =
        Err("uninitialised failover".to_string());
    for (idx, peer) in peers.iter().enumerate() {
        match request_blocks_by_range(*peer, start_height, start_hash, count, timeout_dur).await {
            Ok(resp) if !resp.blocks.is_empty() => return Ok(resp),
            Ok(empty_resp) => {
                tracing::debug!(
                    target: "bsc_protocol",
                    %peer,
                    attempt = idx + 1,
                    total = peers.len(),
                    start_height,
                    %start_hash,
                    "Empty BlocksByRange response, trying next peer"
                );
                last = Ok(empty_resp);
            }
            Err(err) => {
                tracing::debug!(
                    target: "bsc_protocol",
                    %peer,
                    attempt = idx + 1,
                    total = peers.len(),
                    start_height,
                    %start_hash,
                    %err,
                    "BlocksByRange request failed, trying next peer"
                );
                last = Err(err);
            }
        }
    }
    last
}
```

- [ ] **Step 4.4: Verify compile**

Run: `cargo check -p reth_bsc`
Expected: PASS.

- [ ] **Step 4.5: Wire it into `BscRangeFetcher::fetch`**

In `src/node/network/block_import/fork_recover.rs`, at the top of the `impl RangeFetcher for BscRangeFetcher` block (~line 83), add a constant and update the call:

```rust
/// Max peer attempts per hop before `BscRangeFetcher` gives up. The
/// announcing peer is tried first; failover rotates through registered
/// BSC peers.
pub const MAX_PEER_ATTEMPTS: usize = 3;
```

Then replace the body of `BscRangeFetcher::fetch` with:

```rust
impl RangeFetcher for BscRangeFetcher {
    fn fetch<'a>(
        &'a self,
        peer: PeerId,
        start_num: u64,
        start_hash: B256,
        count: u64,
    ) -> BoxFuture<'a, Result<Vec<BscBlock>, String>> {
        Box::pin(async move {
            let resp = crate::node::network::bsc_protocol::registry::request_blocks_by_range_with_failover(
                peer,
                start_num,
                start_hash,
                count,
                FETCH_TIMEOUT,
                MAX_PEER_ATTEMPTS,
            )
            .await?;
            Ok(resp.blocks)
        })
    }
}
```

- [ ] **Step 4.6: Verify the fetcher still compiles and existing tests still pass**

Run: `cargo test -p reth_bsc --lib -- node::network::block_import::fork_recover`
Expected: all existing `fork_recover` unit tests pass (they use a fake `RangeFetcher`, so the registry change doesn't touch them).

- [ ] **Step 4.7: Commit**

```bash
git add src/node/network/bsc_protocol/registry.rs src/node/network/block_import/fork_recover.rs
git commit -m "feat(fork-recover): failover through BSC peers on empty/err GetBlocksByRange

A single reth-bsc peer with a BODY_CACHE miss used to abort the entire
recovery with EmptyResponse. We now rotate through up to 3 registered
BSC peers per hop, picking the first non-empty response. Peer-ordering
logic is a pure function covered by unit tests; the async failover
function is glue over the existing request_blocks_by_range primitive."
```

---

## Task 5: Add a recently-failed-heads cooler to suppress retry storms (TDD)

**Files:**
- Modify: `src/node/network/block_import/fork_recover.rs`
- Test: same file (new `mod tests` section for the cooler)

- [ ] **Step 5.1: Write failing tests for the cooler**

Append at the bottom of `src/node/network/block_import/fork_recover.rs` (inside the existing `#[cfg(test)] mod tests` block, or a new `mod cooler_tests` next to it):

```rust
#[cfg(test)]
mod cooler_tests {
    use super::{FailedHeadsCooler, FAILED_HEAD_COOLDOWN};
    use alloy_primitives::B256;
    use std::time::Duration;

    #[test]
    fn is_cooling_is_false_before_mark_failed() {
        let cooler = FailedHeadsCooler::new(8, FAILED_HEAD_COOLDOWN);
        assert!(!cooler.is_cooling(&B256::repeat_byte(0x11)));
    }

    #[test]
    fn is_cooling_is_true_right_after_mark_failed() {
        let cooler = FailedHeadsCooler::new(8, FAILED_HEAD_COOLDOWN);
        let h = B256::repeat_byte(0x22);
        cooler.mark_failed(h);
        assert!(cooler.is_cooling(&h));
    }

    #[test]
    fn cooldown_expires_after_duration() {
        // Use a 0-length cooldown so expiry fires immediately.
        let cooler = FailedHeadsCooler::new(8, Duration::from_millis(0));
        let h = B256::repeat_byte(0x33);
        cooler.mark_failed(h);
        // With 0ms cooldown the next check must not consider it cooling.
        std::thread::sleep(Duration::from_millis(5));
        assert!(!cooler.is_cooling(&h));
    }

    #[test]
    fn capacity_evicts_oldest_entries() {
        let cooler = FailedHeadsCooler::new(2, Duration::from_secs(60));
        let a = B256::repeat_byte(0xaa);
        let b = B256::repeat_byte(0xbb);
        let c = B256::repeat_byte(0xcc);
        cooler.mark_failed(a);
        cooler.mark_failed(b);
        cooler.mark_failed(c);
        // `a` must be evicted; `b` and `c` must remain.
        assert!(!cooler.is_cooling(&a));
        assert!(cooler.is_cooling(&b));
        assert!(cooler.is_cooling(&c));
    }
}
```

- [ ] **Step 5.2: Run tests to verify they fail**

Run: `cargo test -p reth_bsc --lib -- node::network::block_import::fork_recover::cooler_tests`
Expected: FAIL with "cannot find type `FailedHeadsCooler` / const `FAILED_HEAD_COOLDOWN`".

- [ ] **Step 5.3: Implement the cooler using `schnellru::LruMap`**

In `src/node/network/block_import/fork_recover.rs`, near the `MAX_FORK_DEPTH` / `MAX_PEER_ATTEMPTS` constants at the top of the file, add:

```rust
/// How long a head stays suppressed after `recover_ancestors` fails.
/// Prevents the 3s periodic head-announce tick from re-spawning a
/// doomed recovery every loop.
pub const FAILED_HEAD_COOLDOWN: Duration = Duration::from_secs(30);
```

Above the existing `RecoveringHeadGuard` definition (search for it with `rg -n "RecoveringHeadGuard" src/node/network/block_import/fork_recover.rs`), add:

```rust
/// Bounded LRU of recently-failed recovery heads with per-entry deadlines.
///
/// `is_cooling` returns true only for entries whose deadline has not yet
/// expired. Expired entries are lazily removed on access. Capacity eviction
/// is handled by the underlying `schnellru::LruMap` and matches the
/// `BODY_CACHE` / `RecoveringHeads` pattern elsewhere in the codebase.
#[derive(Clone)]
pub struct FailedHeadsCooler {
    inner: Arc<parking_lot::Mutex<schnellru::LruMap<B256, std::time::Instant, schnellru::ByLength>>>,
    cooldown: Duration,
}

impl FailedHeadsCooler {
    pub fn new(capacity: u32, cooldown: Duration) -> Self {
        Self {
            inner: Arc::new(parking_lot::Mutex::new(schnellru::LruMap::new(
                schnellru::ByLength::new(capacity),
            ))),
            cooldown,
        }
    }

    pub fn mark_failed(&self, head: B256) {
        let mut g = self.inner.lock();
        g.insert(head, std::time::Instant::now() + self.cooldown);
    }

    pub fn is_cooling(&self, head: &B256) -> bool {
        let mut g = self.inner.lock();
        match g.get(head).copied() {
            None => false,
            Some(deadline) if std::time::Instant::now() >= deadline => {
                g.remove(head);
                false
            }
            Some(_) => true,
        }
    }
}

/// Factory matching the shape of `new_recovering_heads`.
pub fn new_failed_heads_cooler(capacity: u32) -> FailedHeadsCooler {
    FailedHeadsCooler::new(capacity, FAILED_HEAD_COOLDOWN)
}
```

Note: `schnellru` is already a workspace dependency (see `src/shared.rs:24`), so no Cargo.toml change is required. If the compiler complains that the import isn't visible from this module, add `use schnellru;` at the top of `fork_recover.rs`, or qualify each use with `::schnellru::…` (the crate is in the workspace prelude).

- [ ] **Step 5.4: Run cooler tests**

Run: `cargo test -p reth_bsc --lib -- node::network::block_import::fork_recover::cooler_tests`
Expected: 4 tests pass.

- [ ] **Step 5.5: Commit**

```bash
git add src/node/network/block_import/fork_recover.rs
git commit -m "feat(fork-recover): add FailedHeadsCooler LRU for retry suppression

Adds a bounded LRU + per-entry Instant deadline map used by
ImportService to suppress re-spawning recovery for a head that just
failed. Cooldown is process-lifetime only; defaults to 30s.

Pure type with no network dependency; covered by 4 unit tests."
```

---

## Task 6: Gate fork-recovery spawn sites on the cooler

**Files:**
- Modify: `src/node/network/block_import/service.rs`

- [ ] **Step 6.1: Add the field and constructor wiring**

In `src/node/network/block_import/service.rs`:

1. Below the existing `recovering_heads` field (line ~98), add:

```rust
    /// Heads whose most recent recovery attempt failed. Suppresses
    /// re-spawning recovery until the cooldown elapses. Prevents storm
    /// behaviour when the 3s head-announce tick re-announces the same
    /// unreachable head.
    failed_heads: crate::node::network::block_import::fork_recover::FailedHeadsCooler,
```

2. Inside `ImportService::new(...)` (line ~133), next to the `recovering_heads: …new_recovering_heads(LRU_PROCESSED_BLOCKS_SIZE)` initialiser, add:

```rust
            failed_heads: crate::node::network::block_import::fork_recover::new_failed_heads_cooler(
                LRU_PROCESSED_BLOCKS_SIZE,
            ),
```

- [ ] **Step 6.2: Gate `on_new_block_hashes` on the cooler**

In `fn on_new_block_hashes`, directly after the `processed_blocks` / `queued_blocks` checks (around line 475, before the `recovering_heads.lock()` block), insert:

```rust
            if self.failed_heads.is_cooling(&hash_number.hash) {
                tracing::debug!(
                    target: "bsc::block_import",
                    block_hash = %hash_number.hash,
                    block_number = hash_number.number,
                    "Skipping fork recovery: head is in cooldown after recent failure"
                );
                continue;
            }
```

Then clone the cooler into the spawned task: before `tokio::spawn(async move { … })` (line ~501), add:

```rust
            let failed_heads = self.failed_heads.clone();
```

Inside the spawned block, after the existing `if let Err(err) = …recover_ancestors(…)` logging, add before the closing `}` of the `if let Err` arm:

```rust
                    failed_heads.mark_failed(head_hash);
```

- [ ] **Step 6.3: Gate the `Syncing` arm in `new_payload`**

In `fn new_payload` (the `PayloadStatusEnum::Syncing =>` arm, around line 229-286):

1. Directly after the `let block_number = header.number;` / `tracing::info!` "spawning fork recovery" lines (line ~234-241) and **before** the `let mut guard = recovering_heads.lock();` block at line ~247, add:

```rust
                        if self.failed_heads.is_cooling(&block_hash) {
                            tracing::debug!(
                                target: "bsc::block_import",
                                %block_hash,
                                block_number,
                                "Skipping fork recovery: head is in cooldown after recent failure"
                            );
                            return None;
                        }
```

   Note: this site is inside `fn new_payload`, which already has `&self` — `self.failed_heads` is reachable. Ensure the `let recovering_heads = self.recovering_heads.clone();` line above also clones `failed_heads`:

   Change `let recovering_heads = self.recovering_heads.clone();` (line ~160) to:

```rust
        let recovering_heads = self.recovering_heads.clone();
        let failed_heads = self.failed_heads.clone();
```

2. Inside the `tokio::spawn(async move { … })` body, where the existing `if let Err(err) = …recover_ancestors(…)` logs a warning, append a `failed_heads.mark_failed(block_hash);` call so failures are recorded.

```rust
                            if let Err(err) =
                                crate::node::network::block_import::fork_recover::recover_ancestors(
                                    target,
                                    block_hash,
                                    block_number,
                                    provider,
                                    engine_clone,
                                    forkchoice_engine_clone,
                                    &fetcher,
                                )
                                .await
                            {
                                tracing::warn!(
                                    target: "bsc::block_import",
                                    %block_hash,
                                    block_number,
                                    error = %err,
                                    "Fork recovery failed (Syncing path)"
                                );
                                failed_heads.mark_failed(block_hash);
                            }
```

- [ ] **Step 6.4: Build and run existing import-service tests**

Run: `cargo check -p reth_bsc && cargo test -p reth_bsc --lib -- node::network::block_import`
Expected: PASS.

- [ ] **Step 6.5: Commit**

```bash
git add src/node/network/block_import/service.rs
git commit -m "feat(block-import): gate fork-recovery spawns on FailedHeadsCooler

The livelock fix announces heads every 3 seconds. Without a cool-off,
a head whose recovery failed (peer missing the block, timeout, etc.)
triggers a new recover_ancestors spawn on every tick. This wires the
new FailedHeadsCooler into both spawn sites (on_new_block_hashes and
the Syncing arm of new_payload) and records failures on recover_ancestors
Err so the same head isn't retried for 30s."
```

---

## Task 7: Final verification

**Files:** none modified.

- [ ] **Step 7.1: Full build**

Run: `cargo check -p reth_bsc --all-targets`
Expected: PASS, no new warnings.

- [ ] **Step 7.2: Targeted test suites**

Run: `cargo test -p reth_bsc --lib -- node::network::blocks_by_range node::network::bsc_protocol::registry node::network::block_import::fork_recover`
Expected: all tests pass (Task 3: 3 new; Task 4: 4 new; Task 5: 4 new; pre-existing fork_recover tests unchanged).

- [ ] **Step 7.3: Formatting and clippy**

Run: `cargo +nightly fmt --all` then `RUSTFLAGS="-D warnings" cargo clippy -p reth_bsc --all-targets --locked`
Expected: no diff after fmt, no clippy warnings.

- [ ] **Step 7.4: Smoke-check log expectations**

Re-read the changed spawn sites and confirm the log message wording now present in the code:
- `"Skipping fork recovery: head is in cooldown after recent failure"` appears exactly twice (on_new_block_hashes and the Syncing arm).
- `"Empty BlocksByRange response, trying next peer"` appears in registry.rs.
- `"BlocksByRange request failed, trying next peer"` appears in registry.rs.
- `"Truncated BlocksByRange due to missing parent/body"` is unchanged.

Run: `rg -n "Skipping fork recovery|trying next peer|Truncated BlocksByRange" src/`
Expected: 4 matches, one per message above — counting both `Skipping fork recovery` occurrences as two.

- [ ] **Step 7.5: Commit any fmt drift**

```bash
git status
# If fmt produced changes:
git add -u && git commit -m "chore: cargo +nightly fmt"
```

---

## Self-Review Checklist (run after drafting, not by the executing agent)

1. **Spec coverage.** Goals 1, 2, 3 map to Tasks 2+3, 4, 5+6 respectively. Non-goals are honoured: wire format untouched, no cross-peer merging, no persistence.
2. **Placeholder scan.** No "TBD", "add appropriate …", or naked "implement later" strings. All code blocks are complete.
3. **Type consistency.** `FailedHeadsCooler` / `FAILED_HEAD_COOLDOWN` / `MAX_PEER_ATTEMPTS` / `new_failed_heads_cooler` / `plan_failover_peers` / `request_blocks_by_range_with_failover` / `build_blocks_by_range_response_with` are defined in exactly one task and referenced by name in later tasks.
4. **Test isolation.** `plan_failover_peers` is pure; cooler tests use `Instant`-based timings with a 0-ms cooldown path to avoid flaky sleeps; the range-response test uses an in-memory `HashMap` lookup, no globals.
