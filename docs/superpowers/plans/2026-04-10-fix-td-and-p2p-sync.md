# Fix TD and P2P Block Sync Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix three P2P sync issues that cause a cluster-wide mining deadlock after simultaneous validator restart: (1) eth status TD is always 0, (2) no active block fetching when parent is missing, (3) no periodic head announcement as fallback.

**Architecture:** The fix spans two repos: `reth` (bnb-chain fork) for the TD bug in the engine launch loop, and `reth-bsc` for the active sync and periodic announce logic. The changes are additive — no existing behavior is removed, only gaps are filled.

**Tech Stack:** Rust, reth engine-tree, eth P2P protocol, BSC `GetBlocksByRange` subprotocol.

---

## File Map

| Repo | File | Change | Purpose |
|------|------|--------|---------|
| reth | `crates/node/builder/src/launch/engine.rs` | Modify lines 393-395 | Fix TD=0 in status update |
| reth | `crates/node/builder/src/launch/engine.rs` | Modify imports (line 33-36) | Add `HeaderProvider` import |
| reth-bsc | `src/node/network/block_import/service.rs` | Modify `new_payload` Syncing handler (lines 210-257) | Trigger range fetch on missing parent |
| reth-bsc | `src/node/network/block_import/service.rs` | Modify `on_new_block_hashes` (line 504) | Request more blocks (count=64 instead of 1) |
| reth-bsc | `src/node/network/block_import/service.rs` | Add periodic announce in `poll()` | Periodically broadcast head as NewBlock |
| reth-bsc | `src/node/network/block_import/service.rs` | Add field `last_announce_time` to struct | Track announce interval |

---

### Task 1: Fix TD in eth status update (reth repo)

**Files:**
- Modify: `/Users/jiaqiwang/workspace/reth/crates/node/builder/src/launch/engine.rs:33-36` (imports)
- Modify: `/Users/jiaqiwang/workspace/reth/crates/node/builder/src/launch/engine.rs:393-395` (TD calculation)

The bug: `chainspec.final_paris_total_difficulty()` returns `None` for BSC (no Paris/Merge fork), so `unwrap_or_default()` yields 0. The `provider` at this scope is a `BlockchainProvider` which implements `HeaderProvider` with `header_td_by_number`.

- [ ] **Step 1: Add `HeaderProvider` to imports**

In `/Users/jiaqiwang/workspace/reth/crates/node/builder/src/launch/engine.rs`, change the `reth_provider` import block:

```rust
// Line 33-36: change from
use reth_provider::{
    providers::{BlockchainProvider, NodeTypesForProvider},
    BlockNumReader, MetadataProvider,
};

// to
use reth_provider::{
    providers::{BlockchainProvider, NodeTypesForProvider},
    BlockNumReader, HeaderProvider, MetadataProvider,
};
```

- [ ] **Step 2: Fix TD calculation**

In the same file, change lines 393-395 from:

```rust
total_difficulty: chainspec.final_paris_total_difficulty()
    .filter(|_| chainspec.is_paris_active_at_block(head.number()))
    .unwrap_or_default(),
```

to:

```rust
total_difficulty: chainspec.final_paris_total_difficulty()
    .filter(|_| chainspec.is_paris_active_at_block(head.number()))
    .or_else(|| {
        provider.header_td_by_number(head.number()).ok().flatten()
    })
    .unwrap_or_default(),
```

This preserves the original PoS behavior (uses Paris TD if active), but falls back to reading actual cumulative TD from the database for non-PoS chains like BSC.

- [ ] **Step 3: Verify compilation**

Run: `cd /Users/jiaqiwang/workspace/reth && cargo check -p reth-node-builder 2>&1 | tail -5`
Expected: compilation succeeds (or only unrelated warnings)

- [ ] **Step 4: Commit in reth repo**

```bash
cd /Users/jiaqiwang/workspace/reth
git add crates/node/builder/src/launch/engine.rs
git commit -m "fix: use actual TD from provider for non-PoS chains in eth status update

For chains without Paris/Merge (like BSC), the total_difficulty in eth
status was always 0 because final_paris_total_difficulty() returns None.
This broke TD-based peer comparison and prevented P2P sync from working.

Now falls back to reading the real cumulative TD from the database."
```

---

### Task 2: Trigger range fetch when `new_payload` returns Syncing (reth-bsc repo)

**Files:**
- Modify: `/Users/jiaqiwang/workspace/reth-bsc/src/node/network/block_import/service.rs:210-257` (Syncing handler in `new_payload`)

When `new_payload` returns `Syncing`, the parent block is missing. Currently only an FCU is sent. We add a range fetch via the BSC subprotocol to actively pull ancestor blocks from the peer.

The `new_payload` closure captures `peer_id` from the outer `on_new_block` call, so we have access to the originating peer. We also have `block.block.0.block.header.number` for the block number.

- [ ] **Step 1: Modify the Syncing handler to trigger a range fetch**

In `/Users/jiaqiwang/workspace/reth-bsc/src/node/network/block_import/service.rs`, replace the `PayloadStatusEnum::Syncing` arm (lines 210-257) with:

```rust
PayloadStatusEnum::Syncing => {
    // Parent block is missing. Actively fetch ancestor blocks
    // from the announcing peer via BSC GetBlocksByRange to bridge
    // the gap, rather than waiting passively.
    let block_number = header.number;
    let parent_hash = header.parent_hash();
    tracing::info!(
        target: "bsc::block_import",
        block_hash = %block_hash,
        block_number = block_number,
        parent_hash = %parent_hash,
        peer = %peer_id,
        "New payload returned Syncing - fetching ancestor blocks from peer"
    );

    // Determine how many blocks to request.
    // Use best_block_number as reference for the gap size.
    let local_tip = forkchoice_engine.provider
        .best_block_number()
        .unwrap_or(0);
    let gap = block_number.saturating_sub(local_tip);
    let count = gap.clamp(1, 64);

    // Spawn async range fetch from the originating peer
    let fetch_peer = peer_id;
    tokio::spawn(async move {
        let target_peer = if crate::node::network::bsc_protocol::registry::has_registered_peer(fetch_peer) {
            Some(fetch_peer)
        } else {
            crate::node::network::bsc_protocol::registry::list_registered_peers()
                .into_iter()
                .next()
        };
        if let Some(bsc_peer) = target_peer {
            tracing::debug!(
                target: "bsc::block_import",
                peer = %bsc_peer,
                block_hash = %block_hash,
                block_number = block_number,
                count = count,
                "Requesting ancestor block range for syncing block"
            );
            let _ = crate::node::network::bsc_protocol::registry::batch_request_range_and_await_import(
                bsc_peer,
                block_number,
                block_hash,
                count,
                std::time::Duration::from_secs(5),
            ).await;
        } else {
            tracing::debug!(
                target: "bsc::block_import",
                "No BSC protocol peer available for ancestor fetch"
            );
        }
    });

    // Also send FCU to inform the engine-tree about the new head,
    // which may trigger its own download mechanism via
    // BasicBlockDownloader if the eth P2P layer has suitable peers.
    let forkchoice_state = alloy_rpc_types::engine::ForkchoiceState {
        head_block_hash: block_hash,
        safe_block_hash: alloy_primitives::B256::ZERO,
        finalized_block_hash: alloy_primitives::B256::ZERO,
    };
    match engine
        .fork_choice_updated(
            forkchoice_state,
            None,
            reth_payload_primitives::EngineApiMessageVersion::V1,
        )
        .await
    {
        Ok(result) => {
            tracing::debug!(
                target: "bsc::block_import",
                block_hash = %block_hash,
                block_number = block_number,
                status = ?result.payload_status.status,
                "FCU result for syncing block"
            );
        }
        Err(err) => {
            tracing::trace!(
                target: "bsc::block_import",
                block_hash = %block_hash,
                block_number = block_number,
                error = %err,
                "Failed to update fork choice for syncing block"
            );
        }
    }
    None
}
```

- [ ] **Step 2: Verify compilation**

Run: `cd /Users/jiaqiwang/workspace/reth-bsc && cargo check -p reth_bsc 2>&1 | tail -5`
Expected: compilation succeeds

- [ ] **Step 3: Commit**

```bash
cd /Users/jiaqiwang/workspace/reth-bsc
git add src/node/network/block_import/service.rs
git commit -m "fix: actively fetch ancestor blocks when new_payload returns Syncing

When a received block's parent is missing, actively request up to 64
ancestor blocks from the announcing peer via BSC GetBlocksByRange,
instead of only sending an FCU and hoping the engine-tree handles it.
This bridges fork gaps during simultaneous validator restarts."
```

---

### Task 3: Increase block count in `on_new_block_hashes` range request (reth-bsc repo)

**Files:**
- Modify: `/Users/jiaqiwang/workspace/reth-bsc/src/node/network/block_import/service.rs:494-506` (range request in `on_new_block_hashes`)

Currently `on_new_block_hashes` requests only 1 block. When a peer announces a block far ahead, we need to fetch enough blocks to bridge the gap.

- [ ] **Step 1: Add local tip lookup and dynamic count**

In `/Users/jiaqiwang/workspace/reth-bsc/src/node/network/block_import/service.rs`, replace the spawn block inside `on_new_block_hashes` (approximately lines 494-507):

```rust
// From:
tokio::spawn(async move {
    use std::time::Duration;
    let req_timeout = Duration::from_millis(1000);
    let _ = crate::node::network::bsc_protocol::registry::batch_request_range_and_await_import(
        bsc_peer,
        start_height,
        start_hash,
        1,
        req_timeout,
    ).await;
});

// To:
let local_tip = self.forkchoice_engine.provider
    .best_block_number()
    .unwrap_or(0);
let gap = start_height.saturating_sub(local_tip);
let count = gap.clamp(1, 64);
tokio::spawn(async move {
    use std::time::Duration;
    // Scale timeout with the number of blocks requested
    let req_timeout = Duration::from_secs(5);
    let _ = crate::node::network::bsc_protocol::registry::batch_request_range_and_await_import(
        bsc_peer,
        start_height,
        start_hash,
        count,
        req_timeout,
    ).await;
});
```

- [ ] **Step 2: Verify compilation**

Run: `cd /Users/jiaqiwang/workspace/reth-bsc && cargo check -p reth_bsc 2>&1 | tail -5`
Expected: compilation succeeds

- [ ] **Step 3: Commit**

```bash
cd /Users/jiaqiwang/workspace/reth-bsc
git add src/node/network/block_import/service.rs
git commit -m "fix: request up to 64 blocks in on_new_block_hashes instead of 1

When receiving NewBlockHashes for a block far ahead of the local tip,
request enough blocks (up to 64) to bridge the gap in one round-trip
via BSC GetBlocksByRange, instead of only fetching 1 block."
```

---

### Task 4: Add periodic head announcement as fallback (reth-bsc repo)

**Files:**
- Modify: `/Users/jiaqiwang/workspace/reth-bsc/src/node/network/block_import/service.rs` (add field to `ImportService`, modify `poll()`)

After restart with no new blocks being produced, no announcements are sent. A periodic broadcast of the current head ensures peers discover our chain state and can push their (potentially longer) chain back to us.

We use `announce_block` (which sends a full `NewBlock` to sqrt(N) peers + `NewBlockHashes` to the rest) with the correct TD. This is done in the `ImportService::poll()` method since it already runs as a long-lived Future.

- [ ] **Step 1: Add `last_announce` field to `ImportService`**

In `/Users/jiaqiwang/workspace/reth-bsc/src/node/network/block_import/service.rs`, add a field to the struct (after `downloading_blocks`):

```rust
pub struct ImportService<Provider>
where
    Provider: BlockNumReader + HeaderProvider + Clone,
{
    // ... existing fields ...
    /// Cache of downloading block hashes to avoid re-downloading the same block.
    downloading_blocks: LruMap<B256, u128, ByLength>,
    /// Last time we announced our head to the network.
    last_announce: std::time::Instant,
}
```

Update the constructor to initialize it:

```rust
Self {
    engine,
    forkchoice_engine,
    from_network,
    from_builder,
    from_hashes,
    to_network,
    pending_imports: FuturesUnordered::new(),
    processed_blocks: LruCache::new(LRU_PROCESSED_BLOCKS_SIZE),
    queued_blocks: LruCache::new(LRU_PROCESSED_BLOCKS_SIZE),
    downloading_blocks: LruMap::new(ByLength::new(LRU_PROCESSED_BLOCKS_SIZE)),
    last_announce: std::time::Instant::now(),
}
```

- [ ] **Step 2: Add periodic announce logic in `poll()`**

In the `Future::poll` impl for `ImportService`, add the announce logic **at the end** (before `Poll::Pending`):

```rust
// Periodic head announcement (every 5s) so that peers learn about our chain.
// After restart when no new blocks are produced, this ensures peers discover
// our tip and can push their (potentially longer) chain back to us.
const ANNOUNCE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
if this.last_announce.elapsed() >= ANNOUNCE_INTERVAL {
    this.last_announce = std::time::Instant::now();
    if let Some(net) = crate::shared::get_network_handle() {
        if let Ok(info) = this.forkchoice_engine.provider.chain_info() {
            if let Ok(Some(header)) = this.forkchoice_engine.provider.header_by_number(info.best_number) {
                let hash = header.hash_slow();
                let td = this.forkchoice_engine.provider
                    .header_td_by_number(info.best_number)
                    .ok()
                    .flatten()
                    .unwrap_or_default();
                let block = crate::node::primitives::BscBlock {
                    header: header.clone(),
                    body: crate::node::primitives::BscBlockBody {
                        inner: reth_ethereum_primitives::BlockBody::default(),
                        sidecars: None,
                    },
                };
                let new_block = crate::node::network::BscNewBlock(
                    reth_eth_wire::NewBlock {
                        block,
                        td: alloy_primitives::U128::from(td.to::<u128>()),
                    },
                );
                tracing::debug!(
                    target: "bsc::block_import",
                    number = info.best_number,
                    hash = %hash,
                    td = %td,
                    "Periodic head announcement"
                );
                net.announce_block(new_block, hash, Some(td));
            }
        }
    }
}

Poll::Pending
```

- [ ] **Step 3: Verify compilation**

Run: `cd /Users/jiaqiwang/workspace/reth-bsc && cargo check -p reth_bsc 2>&1 | tail -5`
Expected: compilation succeeds

- [ ] **Step 4: Run existing tests**

Run: `cd /Users/jiaqiwang/workspace/reth-bsc && cargo test -p reth_bsc 2>&1 | tail -10`
Expected: all tests pass

- [ ] **Step 5: Commit**

```bash
cd /Users/jiaqiwang/workspace/reth-bsc
git add src/node/network/block_import/service.rs
git commit -m "feat: periodically announce head block to peers every 5s

After restart when no new blocks are produced, peers have no way to
learn about our chain tip. Periodically broadcasting our head as a
NewBlock message (with correct TD) ensures peers can compare chains
and push their longer chain back to us, breaking the deadlock."
```

---

### Task 5: Verify end-to-end and final commit

- [ ] **Step 1: Build both repos**

```bash
cd /Users/jiaqiwang/workspace/reth && cargo check -p reth-node-builder 2>&1 | tail -5
cd /Users/jiaqiwang/workspace/reth-bsc && cargo check -p reth_bsc 2>&1 | tail -5
```

Expected: both compile clean

- [ ] **Step 2: Run reth-bsc tests**

```bash
cd /Users/jiaqiwang/workspace/reth-bsc && cargo test -p reth_bsc 2>&1 | tail -15
```

Expected: all tests pass

- [ ] **Step 3: Verify the changes are consistent**

Review checklist:
1. `engine.rs`: TD falls back to `provider.header_td_by_number()` for non-PoS chains
2. `service.rs` Syncing handler: spawns `batch_request_range_and_await_import` with dynamic count
3. `service.rs` `on_new_block_hashes`: uses dynamic count (1-64) based on gap
4. `service.rs` `poll()`: periodic announce every 5s with correct TD from provider
5. No hardcoded `td: U256::ZERO` in new code paths

---

## Summary of changes

```
reth repo:
  engine.rs  — TD: Paris fallback → provider.header_td_by_number()

reth-bsc repo:
  service.rs — Syncing: passive FCU → active range fetch (up to 64 blocks)
  service.rs — on_new_block_hashes: count=1 → count=min(gap, 64)
  service.rs — poll(): add periodic head announce every 5s with real TD
```
