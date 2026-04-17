# P2P Fork Recovery Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the buggy "naive `gap`-sized fetch + premature FCU" logic in `ImportService` with a single ancestor-aware recovery primitive that walks back from a peer's announced head in 4-block hops via `GetBlocksByRange`, skips blocks we already have locally, imports fork blocks oldest-first awaiting each `new_payload`, and fires `fork_choice_updated` only after the full ancestor-to-head chain is validated.

**Architecture:** One new module `src/node/network/block_import/fork_recover.rs` holds a pure Phase-1 discovery function (`discover_fork_blocks`), a trait-abstracted fetcher (`RangeFetcher`) with a production impl backed by `bsc_protocol::registry::request_blocks_by_range`, and the top-level `recover_ancestors` that orchestrates Phase 1 → Phase 2 (`engine.new_payload` sequentially) → Phase 3 (`forkchoice_engine.update_forkchoice`). `ImportService` gains an `Arc<Mutex<LruCache<B256>>>` dedup set (`recovering_heads`) with an RAII guard; its two existing fetch sites (`on_new_block_hashes` body and the `Syncing` arm of `new_payload`) are both replaced by a single spawn of `recover_ancestors`.

**Tech Stack:** Rust, tokio, reth-bsc 0.1, reth (bnb-chain fork) — `reth::network::cache::LruCache`, `reth_engine_primitives::ConsensusEngineHandle`, `reth_provider::{BlockHashReader, HeaderProvider}`, `alloy_rpc_types::engine::{ForkchoiceState, PayloadStatusEnum}`. Uses existing `BscForkChoiceEngine`, `BscPayloadTypes`, and the BSC sub-protocol `GetBlocksByRange`.

**Spec:** `docs/superpowers/specs/2026-04-17-p2p-fork-recovery-design.md`

---

## File Map

- **Create**: `src/node/network/block_import/fork_recover.rs` — all recovery logic:
  - Constants `MAX_FORK_DEPTH = 256`, `FORK_RECOVER_HOP_COUNT = 4`, `FETCH_TIMEOUT = 5s`.
  - `enum ForkRecoverError`.
  - `trait RangeFetcher` (dyn-safe, async via `BoxFuture`).
  - `struct BscRangeFetcher` — production impl calling `bsc_protocol::registry::request_blocks_by_range`.
  - `fn discover_fork_blocks(...)` — pure Phase 1 (no engine, no network dep beyond the fetcher trait).
  - `async fn recover_ancestors(...)` — Phase 1 + Phase 2 + Phase 3 glue.
  - `struct RecoveringHeadGuard` — RAII cleanup of the dedup entry.
  - Unit tests for `discover_fork_blocks` covering spec test cases #1–#8.
- **Modify**: `src/node/network/block_import/mod.rs` — add `pub mod fork_recover;` (or `pub(crate) mod`).
- **Modify**: `src/node/network/block_import/service.rs`:
  - Add `recovering_heads: Arc<Mutex<LruCache<B256>>>` field (struct + constructor).
  - Replace body of `on_new_block_hashes` (lines ~490-567) — keep dedup checks, drop the fetch spawn, add `recover_ancestors` spawn guarded by `recovering_heads`.
  - Replace the `PayloadStatusEnum::Syncing` arm in `new_payload` (lines ~219-309) — drop the spawned `batch_request_range_and_await_import` call and the immediate `fork_choice_updated` call; spawn `recover_ancestors` instead.

No other files change. No wire-protocol changes. No new dependencies (`once_cell`, `async_trait`, `futures`, `tokio`, `parking_lot` are all already in the workspace).

---

## Task 1: Scaffold the `fork_recover` module

**Files:**
- Create: `src/node/network/block_import/fork_recover.rs`
- Modify: `src/node/network/block_import/mod.rs`

Goal: land an empty-but-compilable module so subsequent tasks can add one concept at a time.

- [ ] **Step 1.1: Inspect existing `mod.rs`**

Run: `cat src/node/network/block_import/mod.rs`
Expected: see `pub mod handle;` and `pub mod service;` (or similar). Note exact visibility so we match.

- [ ] **Step 1.2: Create the new module file with only constants + the error enum**

Create `src/node/network/block_import/fork_recover.rs`:

```rust
//! Fork recovery: ancestor-aware block pull that replaces the naive
//! `batch_request_range_and_await_import` call in the import service.
//!
//! See `docs/superpowers/specs/2026-04-17-p2p-fork-recovery-design.md`.

use std::time::Duration;

/// Hard cap on how many blocks we will walk back from the peer's announced
/// head before giving up. ~2 BSC validator turn cycles.
pub const MAX_FORK_DEPTH: u64 = 256;

/// Blocks fetched per `GetBlocksByRange` hop. Kept small because BSC blocks
/// are large (full tx bodies + sidecars); a 64-block response is slow to
/// transmit and wasteful when the ancestor is a handful of blocks away.
pub const FORK_RECOVER_HOP_COUNT: u64 = 4;

/// Per-hop network timeout.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// Error kinds produced by `recover_ancestors` / `discover_fork_blocks`.
#[derive(Debug, thiserror::Error)]
pub enum ForkRecoverError {
    #[error("peer returned empty response at cursor ({num}, {hash})")]
    EmptyResponse { num: u64, hash: alloy_primitives::B256 },

    #[error("no common ancestor found within MAX_FORK_DEPTH={MAX_FORK_DEPTH} blocks")]
    ForkTooDeep,

    #[error("range fetch failed: {0}")]
    FetchFailed(String),

    #[error("local provider error: {0}")]
    Provider(#[from] reth_provider::ProviderError),

    #[error("engine new_payload returned Invalid for block {num}: {reason}")]
    ImportInvalid { num: u64, reason: String },

    #[error("engine new_payload returned Syncing mid-chain for block {num} (parent should have been Valid)")]
    ImportSyncingMidChain { num: u64 },

    #[error("engine call failed: {0}")]
    EngineCall(String),

    #[error("head header {hash} not in provider after recovery")]
    HeadHeaderMissing { hash: alloy_primitives::B256 },
}
```

Check that `thiserror` is already a dependency:

Run: `grep -q '^thiserror' Cargo.toml && echo yes || echo no`
Expected: `yes`. If `no`, use `grep thiserror Cargo.toml` to find the actual key; reth-bsc pulls it via the workspace.

- [ ] **Step 1.3: Register the module**

Edit `src/node/network/block_import/mod.rs`. Add (matching surrounding visibility — inspect Step 1.1):

```rust
pub(crate) mod fork_recover;
```

- [ ] **Step 1.4: Verify the crate still builds**

Run: `cargo check -p reth-bsc --lib`
Expected: no errors. Warnings about unused items (`MAX_FORK_DEPTH`, `FORK_RECOVER_HOP_COUNT`, etc.) are fine — later tasks use them.

- [ ] **Step 1.5: Commit**

```bash
git add src/node/network/block_import/fork_recover.rs src/node/network/block_import/mod.rs
git commit -m "feat(p2p): scaffold fork_recover module with error types and constants"
```

---

## Task 2: Add `RangeFetcher` trait + production impl

**Files:**
- Modify: `src/node/network/block_import/fork_recover.rs`

The trait lets unit tests inject a fake fetcher instead of running the real BSC sub-protocol. The prod impl is a thin adapter over `crate::node::network::bsc_protocol::registry::request_blocks_by_range`.

- [ ] **Step 2.1: Add the trait definition and the production impl**

Append to `fork_recover.rs` (keep it above the tests module that task 3 will add):

```rust
use alloy_primitives::B256;
use futures::future::BoxFuture;
use reth_network_api::PeerId;

use crate::BscBlock;

/// Abstraction over `GetBlocksByRange`. Tests substitute a fake; production
/// forwards to `bsc_protocol::registry::request_blocks_by_range`.
pub trait RangeFetcher: Send + Sync {
    /// Fetch up to `count` blocks starting at `(start_num, start_hash)` and
    /// walking backwards via `parent_hash`. Response is ordered
    /// **newest → oldest**.
    fn fetch<'a>(
        &'a self,
        peer: PeerId,
        start_num: u64,
        start_hash: B256,
        count: u64,
    ) -> BoxFuture<'a, Result<Vec<BscBlock>, String>>;
}

/// Production fetcher that calls into the BSC sub-protocol registry.
#[derive(Clone, Default)]
pub struct BscRangeFetcher;

impl RangeFetcher for BscRangeFetcher {
    fn fetch<'a>(
        &'a self,
        peer: PeerId,
        start_num: u64,
        start_hash: B256,
        count: u64,
    ) -> BoxFuture<'a, Result<Vec<BscBlock>, String>> {
        Box::pin(async move {
            let resp = crate::node::network::bsc_protocol::registry::request_blocks_by_range(
                peer,
                start_num,
                start_hash,
                count,
                FETCH_TIMEOUT,
            )
            .await?;
            Ok(resp.blocks)
        })
    }
}
```

- [ ] **Step 2.2: Verify it compiles**

Run: `cargo check -p reth-bsc --lib`
Expected: no errors.

- [ ] **Step 2.3: Commit**

```bash
git add src/node/network/block_import/fork_recover.rs
git commit -m "feat(p2p): add RangeFetcher trait and BscRangeFetcher prod impl"
```

---

## Task 3: Pure Phase-1 `discover_fork_blocks` + unit tests (TDD)

**Files:**
- Modify: `src/node/network/block_import/fork_recover.rs`

`discover_fork_blocks` walks back from `(head_num, head_hash)` in `FORK_RECOVER_HOP_COUNT`-sized hops, consulting the local provider before each hop and for each returned block, and returns the accumulated fork blocks (newest → oldest) together with the outcome.

### Step 3.1: Write the failing tests first

- [ ] **Step 3.1.1: Add the test scaffolding and a fake fetcher**

Append to `fork_recover.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::{BlockBody, Header};
    use alloy_primitives::B256;
    use reth_primitives_traits::AlloyBlockHeader;
    use reth_provider::{BlockHashReader, HeaderProvider, ProviderError};
    use std::{
        collections::HashMap,
        sync::{Arc, Mutex},
    };

    use crate::{BscBlock, BscBlockBody};

    // ---------- Fake provider ----------
    #[derive(Clone, Default)]
    struct FakeProvider {
        canonical_by_num: HashMap<u64, B256>, // number -> canonical hash
        headers_by_hash: HashMap<B256, Header>, // every known header (canonical + side)
    }

    impl FakeProvider {
        fn insert_canonical(&mut self, header: Header) {
            let hash = header.hash_slow();
            self.canonical_by_num.insert(header.number, hash);
            self.headers_by_hash.insert(hash, header);
        }
        fn insert_side(&mut self, header: Header) {
            let hash = header.hash_slow();
            self.headers_by_hash.insert(hash, header);
        }
    }

    impl BlockHashReader for FakeProvider {
        fn block_hash(&self, number: u64) -> Result<Option<B256>, ProviderError> {
            Ok(self.canonical_by_num.get(&number).copied())
        }
        fn canonical_hashes_range(
            &self,
            _start: u64,
            _end: u64,
        ) -> Result<Vec<B256>, ProviderError> {
            Ok(vec![])
        }
    }

    impl HeaderProvider for FakeProvider {
        type Header = Header;
        fn header(&self, block_hash: B256) -> Result<Option<Self::Header>, ProviderError> {
            Ok(self.headers_by_hash.get(&block_hash).cloned())
        }
        fn header_by_number(&self, num: u64) -> Result<Option<Self::Header>, ProviderError> {
            Ok(self.canonical_by_num.get(&num)
                .and_then(|h| self.headers_by_hash.get(h).cloned()))
        }
        fn header_td(&self, _: &B256) -> Result<Option<alloy_primitives::U256>, ProviderError> {
            Ok(None)
        }
        fn header_td_by_number(&self, _: u64) -> Result<Option<alloy_primitives::U256>, ProviderError> {
            Ok(None)
        }
        fn headers_range(
            &self,
            _range: impl core::ops::RangeBounds<u64>,
        ) -> Result<Vec<Self::Header>, ProviderError> {
            Ok(vec![])
        }
        fn sealed_header(
            &self,
            _number: u64,
        ) -> Result<Option<reth_primitives::SealedHeader<Self::Header>>, ProviderError> {
            Ok(None)
        }
        fn sealed_headers_while(
            &self,
            _range: impl core::ops::RangeBounds<u64>,
            _predicate: impl FnMut(&reth_primitives::SealedHeader<Self::Header>) -> bool,
        ) -> Result<Vec<reth_primitives::SealedHeader<Self::Header>>, ProviderError> {
            Ok(vec![])
        }
    }

    // ---------- Fake fetcher ----------
    /// Script-driven fetcher: returns one canned response per call, recording
    /// the requested `(start_num, start_hash, count)` tuples for assertions.
    struct ScriptedFetcher {
        responses: Mutex<Vec<Result<Vec<BscBlock>, String>>>,
        requests: Mutex<Vec<(u64, B256, u64)>>,
    }

    impl ScriptedFetcher {
        fn new(responses: Vec<Result<Vec<BscBlock>, String>>) -> Arc<Self> {
            Arc::new(Self {
                responses: Mutex::new(responses),
                requests: Mutex::new(vec![]),
            })
        }

        fn calls(&self) -> Vec<(u64, B256, u64)> {
            self.requests.lock().unwrap().clone()
        }
    }

    impl RangeFetcher for ScriptedFetcher {
        fn fetch<'a>(
            &'a self,
            _peer: PeerId,
            start_num: u64,
            start_hash: B256,
            count: u64,
        ) -> BoxFuture<'a, Result<Vec<BscBlock>, String>> {
            self.requests.lock().unwrap().push((start_num, start_hash, count));
            let resp = self.responses.lock().unwrap().remove(0);
            Box::pin(async move { resp })
        }
    }

    // ---------- Header & block builders ----------
    fn make_header(number: u64, parent_hash: B256, tag: u8) -> Header {
        // `tag` makes hash_slow deterministic-yet-distinguishable.
        Header {
            number,
            parent_hash,
            extra_data: vec![tag].into(),
            ..Default::default()
        }
    }

    fn make_block(header: Header) -> BscBlock {
        BscBlock {
            header,
            body: BscBlockBody::default(),
        }
    }

    /// Build a linear chain starting from `genesis_parent` of length `len`.
    /// Returns `(headers, hashes)` in ascending height order.
    fn linear_chain(start_num: u64, len: u64, genesis_parent: B256, tag: u8)
        -> (Vec<Header>, Vec<B256>)
    {
        let mut headers = Vec::new();
        let mut hashes = Vec::new();
        let mut parent = genesis_parent;
        for i in 0..len {
            let h = make_header(start_num + i, parent, tag);
            parent = h.hash_slow();
            hashes.push(parent);
            headers.push(h);
        }
        (headers, hashes)
    }

    fn fake_peer() -> PeerId {
        PeerId::from([0u8; 64])
    }
}
```

- [ ] **Step 3.1.2: Add the spec's test cases as failing tests**

Append inside the same `mod tests` block. Each test asserts on what `discover_fork_blocks` *should* produce; they will fail to compile until Step 3.2 adds the function.

```rust
    // ---- Spec test #1: head already on canonical (pre-hop short-circuit) ----
    #[tokio::test]
    async fn discover_head_on_canonical_no_fetch() {
        let mut provider = FakeProvider::default();
        let (chain, hashes) = linear_chain(0, 101, B256::ZERO, 0xC);
        for h in chain { provider.insert_canonical(h); }

        let fetcher = ScriptedFetcher::new(vec![]);
        let out = discover_fork_blocks(
            fake_peer(), hashes[100], 100, &provider, fetcher.as_ref(),
        ).await.unwrap();

        assert!(out.fork_blocks.is_empty(), "no blocks to import");
        assert!(matches!(out.outcome, DiscoveryOutcome::Shortcircuit));
        assert_eq!(fetcher.calls().len(), 0, "no network fetch should happen");
    }

    // ---- Spec test #2: simple linear-ahead, one hop, one extra pre-hop check ----
    #[tokio::test]
    async fn discover_linear_ahead_one_hop() {
        let mut provider = FakeProvider::default();
        let (local, local_hashes) = linear_chain(0, 101, B256::ZERO, 0xC); // canonical 0..=100
        for h in &local { provider.insert_canonical(h.clone()); }

        // Peer extends with blocks 101..=104 parented on 100.
        let (peer_ext, _peer_hashes) = linear_chain(101, 4, local_hashes[100], 0xC);
        let hop1: Vec<BscBlock> = peer_ext.iter().cloned().rev().map(make_block).collect();
        let fetcher = ScriptedFetcher::new(vec![Ok(hop1)]);

        let head_hash = peer_ext.last().unwrap().hash_slow();
        let out = discover_fork_blocks(
            fake_peer(), head_hash, 104, &provider, fetcher.as_ref(),
        ).await.unwrap();

        assert!(matches!(out.outcome, DiscoveryOutcome::AncestorFound));
        // Ascending-hash list after reverse; we assert on numbers for clarity.
        let nums: Vec<u64> = out.fork_blocks.iter().map(|b| b.header.number).collect();
        assert_eq!(nums, vec![104, 103, 102, 101], "newest→oldest");
        assert_eq!(fetcher.calls().len(), 1);
        assert_eq!(fetcher.calls()[0], (104, head_hash, FORK_RECOVER_HOP_COUNT));
    }

    // ---- Spec test #3: short fork within two hops (divergence at 95, depth 7) ----
    #[tokio::test]
    async fn discover_short_fork_two_hops() {
        let mut provider = FakeProvider::default();

        // Shared 0..=95
        let (shared, shared_hashes) = linear_chain(0, 96, B256::ZERO, 0xC);
        for h in &shared { provider.insert_canonical(h.clone()); }
        let ancestor_hash = shared_hashes[95];

        // Local fork X: 96X..=100X
        let (local_x, _) = linear_chain(96, 5, ancestor_hash, 0xA);
        for h in &local_x { provider.insert_canonical(h.clone()); }

        // Peer fork Y: 96Y..=102Y
        let (peer_y, peer_y_hashes) = linear_chain(96, 7, ancestor_hash, 0xB);

        // Hop 1: server returns [102Y, 101Y, 100Y, 99Y] (newest→oldest).
        let hop1: Vec<BscBlock> = peer_y[3..=6].iter().cloned().rev().map(make_block).collect();
        // Hop 2: server returns [98Y, 97Y, 96Y, 95_shared].
        let mut hop2: Vec<BscBlock> = peer_y[0..=2].iter().cloned().rev().map(make_block).collect();
        hop2.push(make_block(shared[95].clone()));
        let fetcher = ScriptedFetcher::new(vec![Ok(hop1), Ok(hop2)]);

        let out = discover_fork_blocks(
            fake_peer(), peer_y_hashes[6], 102, &provider, fetcher.as_ref(),
        ).await.unwrap();

        assert!(matches!(out.outcome, DiscoveryOutcome::AncestorFound));
        let nums: Vec<u64> = out.fork_blocks.iter().map(|b| b.header.number).collect();
        assert_eq!(nums, vec![102, 101, 100, 99, 98, 97, 96]);
        assert_eq!(fetcher.calls().len(), 2);
        assert_eq!(fetcher.calls()[0].0, 102);
        assert_eq!(fetcher.calls()[1].0, 98, "second hop starts at 99Y.parent_hash num = 98");
    }

    // ---- Spec test #6: fork too deep (depth 257 → ForkTooDeep) ----
    #[tokio::test]
    async fn discover_fork_too_deep() {
        let mut provider = FakeProvider::default();
        let (shared, shared_hashes) = linear_chain(0, 1, B256::ZERO, 0xC);
        for h in &shared { provider.insert_canonical(h.clone()); }

        // Peer chain: genesis + 300 fork blocks. None of the fork blocks
        // match canonical (which only has block 0).
        let (peer, peer_hashes) = linear_chain(1, 300, shared_hashes[0], 0xB);

        // Script: return 4 fork blocks per hop; 64 hops total = 256 blocks
        // walked. On the 65th iteration the pre-hop check runs with cursor at
        // (44, peer[43].hash_slow()) — neither in canonical nor as a side
        // block — and then `walked == MAX_FORK_DEPTH` triggers ForkTooDeep.
        //
        // `peer` is indexed 0..=299 with peer[idx] at height idx+1. For hop
        // `i` (0-indexed), the top height served is `300 - 4*i` and the
        // response covers heights `(top - 3)..=top` in newest→oldest order.
        let mut responses: Vec<Result<Vec<BscBlock>, String>> = Vec::new();
        for i in 0..64usize {
            let top_height = 300 - 4 * i; // e.g. 300, 296, 292, ...
            // Indices of heights (top-3)..=top in `peer`: (top-4)..=(top-1).
            let slice: Vec<BscBlock> = peer[(top_height - 4)..=(top_height - 1)]
                .iter()
                .cloned()
                .rev()
                .map(make_block)
                .collect();
            responses.push(Ok(slice));
        }
        let fetcher = ScriptedFetcher::new(responses);

        let err = discover_fork_blocks(
            fake_peer(), peer_hashes[299], 300, &provider, fetcher.as_ref(),
        ).await.unwrap_err();
        assert!(matches!(err, ForkRecoverError::ForkTooDeep));
        assert_eq!(fetcher.calls().len(), 64);
    }

    // ---- Spec test #7: head already present as side-chain (short-circuit, empty fork_blocks) ----
    #[tokio::test]
    async fn discover_head_side_chain_shortcircuit() {
        let mut provider = FakeProvider::default();
        let (shared, shared_hashes) = linear_chain(0, 96, B256::ZERO, 0xC);
        for h in &shared { provider.insert_canonical(h.clone()); }
        // Insert a side-chain block at 96 (not canonical).
        let side_96 = make_header(96, shared_hashes[95], 0xB);
        let side_hash = side_96.hash_slow();
        provider.insert_side(side_96);

        let fetcher = ScriptedFetcher::new(vec![]);
        let out = discover_fork_blocks(
            fake_peer(), side_hash, 96, &provider, fetcher.as_ref(),
        ).await.unwrap();
        assert!(matches!(out.outcome, DiscoveryOutcome::Shortcircuit));
        assert!(out.fork_blocks.is_empty());
        assert_eq!(fetcher.calls().len(), 0);
    }

    // ---- Spec test #8: mid-chain side block already present, skipped ----
    #[tokio::test]
    async fn discover_mid_chain_side_block_skipped() {
        let mut provider = FakeProvider::default();
        let (shared, shared_hashes) = linear_chain(0, 96, B256::ZERO, 0xC);
        for h in &shared { provider.insert_canonical(h.clone()); }
        let ancestor_hash = shared_hashes[95];

        // Peer fork Y: 96Y..=99Y (4 blocks).
        let (peer_y, peer_y_hashes) = linear_chain(96, 4, ancestor_hash, 0xB);
        // Register 97Y as an already-known side-chain block.
        provider.insert_side(peer_y[1].clone());

        // Hop 1: [99Y, 98Y, 97Y, 96Y].
        let hop1: Vec<BscBlock> = peer_y.iter().cloned().rev().map(make_block).collect();
        let fetcher = ScriptedFetcher::new(vec![Ok(hop1)]);

        let out = discover_fork_blocks(
            fake_peer(), peer_y_hashes[3], 99, &provider, fetcher.as_ref(),
        ).await.unwrap();

        assert!(matches!(out.outcome, DiscoveryOutcome::AncestorFound));
        let nums: Vec<u64> = out.fork_blocks.iter().map(|b| b.header.number).collect();
        assert_eq!(nums, vec![99, 98, 96], "97Y skipped because already on side-chain");
        assert_eq!(fetcher.calls().len(), 1);
    }
```

- [ ] **Step 3.1.3: Run tests to confirm they fail to compile**

Run: `cargo test -p reth-bsc --lib fork_recover:: 2>&1 | head -40`
Expected: compilation errors about `discover_fork_blocks`, `DiscoveryOutcome`, `DiscoveryResult` being undefined.

### Step 3.2: Implement `discover_fork_blocks`

- [ ] **Step 3.2.1: Add the output types + function**

Add **above** the `#[cfg(test)] mod tests` block in `fork_recover.rs`:

```rust
use reth_provider::{BlockHashReader, HeaderProvider};

/// Outcome classification from `discover_fork_blocks`.
#[derive(Debug)]
pub enum DiscoveryOutcome {
    /// Peer's head hash (or a prefix of its chain) is already present in our
    /// local provider. No Phase-2 import is required, but Phase 3 (FCU) still
    /// runs in the caller so engine-tree can re-evaluate the chain.
    Shortcircuit,
    /// Fetched fork blocks up to but not including the common ancestor; the
    /// caller must import them oldest-first and then FCU.
    AncestorFound,
}

/// Result of Phase 1.
#[derive(Debug)]
pub struct Discovery {
    /// Fork blocks **newest → oldest**. Empty on `Shortcircuit`.
    pub fork_blocks: Vec<crate::BscBlock>,
    pub outcome: DiscoveryOutcome,
}

/// Walk backwards from `(head_num, head_hash)` via `parent_hash`-walked
/// `GetBlocksByRange` hops until a local-chain match is found or
/// `MAX_FORK_DEPTH` is exhausted.
pub async fn discover_fork_blocks<P: BlockHashReader + HeaderProvider<Header = alloy_consensus::Header>>(
    peer: PeerId,
    head_hash: B256,
    head_num: u64,
    provider: &P,
    fetcher: &dyn RangeFetcher,
) -> Result<Discovery, ForkRecoverError> {
    let mut fork_blocks: Vec<crate::BscBlock> = Vec::new();
    let mut cursor_num = head_num;
    let mut cursor_hash = head_hash;
    let mut walked: u64 = 0;

    loop {
        // Pre-hop local checks: if this cursor is already local, we've
        // reached the common ancestor (or short-circuited on an already-known
        // head).
        let cursor_is_local = provider.block_hash(cursor_num)? == Some(cursor_hash)
            || provider.header(cursor_hash)?.is_some();
        if cursor_is_local {
            let outcome = if fork_blocks.is_empty() {
                DiscoveryOutcome::Shortcircuit
            } else {
                DiscoveryOutcome::AncestorFound
            };
            return Ok(Discovery { fork_blocks, outcome });
        }

        if walked >= MAX_FORK_DEPTH {
            return Err(ForkRecoverError::ForkTooDeep);
        }

        let remaining = MAX_FORK_DEPTH - walked;
        let count = FORK_RECOVER_HOP_COUNT.min(remaining);

        let resp = fetcher.fetch(peer, cursor_num, cursor_hash, count)
            .await
            .map_err(ForkRecoverError::FetchFailed)?;
        if resp.is_empty() {
            return Err(ForkRecoverError::EmptyResponse {
                num: cursor_num,
                hash: cursor_hash,
            });
        }

        // Iterate newest → oldest (the order we got them in).
        let mut found_ancestor = false;
        for b in &resp {
            if provider.block_hash(b.header.number)? == Some(b.header.hash_slow()) {
                found_ancestor = true;
                break;
            }
            // Side-chain already present: skip adding to fork_blocks, but keep walking.
            if provider.header(b.header.hash_slow())?.is_some() {
                continue;
            }
            fork_blocks.push(b.clone());
        }
        if found_ancestor {
            return Ok(Discovery {
                fork_blocks,
                outcome: DiscoveryOutcome::AncestorFound,
            });
        }

        // Advance cursor to the block just below the oldest in this response.
        let oldest = resp.last().unwrap();
        walked += resp.len() as u64;
        cursor_num = oldest.header.number.saturating_sub(1);
        cursor_hash = oldest.header.parent_hash;
    }
}
```

- [ ] **Step 3.2.2: Run the tests**

Run: `cargo test -p reth-bsc --lib fork_recover:: 2>&1 | tail -40`
Expected: all six tests pass. If a compile error surfaces about `HeaderProvider::header` signature (the real trait may name it differently), consult the actual trait (grep `pub trait HeaderProvider` in reth dependencies) and adjust; the codebase's `MockProvider` uses `.header(B256)` (see `service.rs:987`), which is the right method.

- [ ] **Step 3.2.3: Commit**

```bash
git add src/node/network/block_import/fork_recover.rs
git commit -m "feat(p2p): implement Phase-1 discover_fork_blocks with unit tests"
```

---

## Task 4: Implement `recover_ancestors` orchestration

**Files:**
- Modify: `src/node/network/block_import/fork_recover.rs`

Phase 2 drives `engine.new_payload` sequentially; Phase 3 issues the single FCU. Unit tests for this end-to-end function require mocking `ConsensusEngineHandle` and `BscForkChoiceEngine`, which is impractical — we cover it via the existing `TestFixture`-style integration test in Task 7 instead.

- [ ] **Step 4.1: Add the Phase-2+3 glue**

Append to `fork_recover.rs`, above the `#[cfg(test)]` block:

```rust
use alloy_rpc_types::engine::{ForkchoiceState, PayloadStatusEnum};
use reth_engine_primitives::ConsensusEngineHandle;
use reth_payload_primitives::EngineApiMessageVersion;
use reth_primitives_traits::Block as _;

use crate::node::{consensus::BscForkChoiceEngine, engine_api::payload::BscPayloadTypes};

/// Top-level recovery entry point.
///
/// 1. Walks back via `discover_fork_blocks` to find the common ancestor.
/// 2. Imports fork blocks oldest → newest via `engine.new_payload`, awaiting
///    `Valid` on each before submitting the next.
/// 3. Issues a final `fork_choice_updated` for `head_hash`, so engine-tree
///    re-evaluates canonical selection.
pub async fn recover_ancestors<P>(
    peer: PeerId,
    head_hash: B256,
    head_num: u64,
    provider: P,
    engine: ConsensusEngineHandle<BscPayloadTypes>,
    forkchoice_engine: BscForkChoiceEngine<P>,
    fetcher: &dyn RangeFetcher,
) -> Result<(), ForkRecoverError>
where
    P: BlockHashReader + HeaderProvider<Header = alloy_consensus::Header>
        + Clone + Send + Sync + 'static,
{
    tracing::debug!(
        target: "bsc::fork_recover",
        %peer,
        %head_hash,
        head_num,
        "Starting fork recovery"
    );

    // ---- Phase 1 ----
    let discovery = discover_fork_blocks(peer, head_hash, head_num, &provider, fetcher).await?;
    tracing::debug!(
        target: "bsc::fork_recover",
        %peer,
        %head_hash,
        head_num,
        fork_blocks = discovery.fork_blocks.len(),
        outcome = ?discovery.outcome,
        "Phase 1 complete"
    );

    // ---- Phase 2: import oldest → newest via new_payload ----
    let mut to_import = discovery.fork_blocks;
    to_import.reverse(); // now oldest → newest
    for block in &to_import {
        let block_hash = block.header.hash_slow();
        let block_num = block.header.number;
        let sealed = block.clone().seal_unchecked(block_hash);
        let payload = BscPayloadTypes::block_to_payload(sealed);

        match engine.new_payload(payload).await {
            Ok(status) => match status.status {
                PayloadStatusEnum::Valid => {
                    tracing::debug!(
                        target: "bsc::fork_recover",
                        %block_hash,
                        block_num,
                        "Fork block imported Valid"
                    );
                }
                PayloadStatusEnum::Invalid { validation_error } => {
                    return Err(ForkRecoverError::ImportInvalid {
                        num: block_num,
                        reason: validation_error,
                    });
                }
                PayloadStatusEnum::Syncing => {
                    return Err(ForkRecoverError::ImportSyncingMidChain { num: block_num });
                }
                other => {
                    return Err(ForkRecoverError::EngineCall(format!(
                        "unexpected new_payload status {other:?}"
                    )));
                }
            },
            Err(err) => {
                return Err(ForkRecoverError::EngineCall(err.to_string()));
            }
        }
    }

    // ---- Phase 3: FCU ----
    let head_header = provider
        .header(head_hash)?
        .ok_or(ForkRecoverError::HeadHeaderMissing { hash: head_hash })?;
    if let Err(err) = forkchoice_engine.update_forkchoice(&head_header).await {
        // Match the existing code's tracing level for FCU failures.
        tracing::trace!(
            target: "bsc::fork_recover",
            %head_hash,
            error = %err,
            "fork_choice_updated returned error after recovery"
        );
    } else {
        tracing::debug!(
            target: "bsc::fork_recover",
            %head_hash,
            head_num,
            "Fork recovery FCU succeeded"
        );
    }

    Ok(())
}
```

- [ ] **Step 4.2: Verify compilation**

Run: `cargo check -p reth-bsc --lib`
Expected: no errors.

Notes on likely fixups:
- `block.clone().seal_unchecked(block_hash)` — if the compiler cannot find `seal_unchecked` on `BscBlock` directly, look at `service.rs:177` for the exact incantation in use.
- `block_to_payload` lives on the `BscPayloadTypes` associated trait — confirm via `grep -R "fn block_to_payload" src/`.
- If `forkchoice_engine.update_forkchoice(&Header)` has a different header parameter form in `BscForkChoiceEngine`, align with `service.rs:185` which calls the same method.

- [ ] **Step 4.3: Commit**

```bash
git add src/node/network/block_import/fork_recover.rs
git commit -m "feat(p2p): implement recover_ancestors end-to-end pipeline"
```

---

## Task 5: Add `recovering_heads` dedup set + RAII guard

**Files:**
- Modify: `src/node/network/block_import/service.rs`
- Modify: `src/node/network/block_import/fork_recover.rs`

- [ ] **Step 5.1: Add the guard type**

Append to `fork_recover.rs` (above `#[cfg(test)]`):

```rust
use parking_lot::Mutex;
use reth::network::cache::LruCache;
use std::sync::Arc;

/// RAII guard that removes a head hash from the dedup cache on drop, even on
/// task panic or early return.
pub struct RecoveringHeadGuard {
    hash: B256,
    set: Arc<Mutex<LruCache<B256>>>,
}

impl RecoveringHeadGuard {
    pub fn new(hash: B256, set: Arc<Mutex<LruCache<B256>>>) -> Self {
        Self { hash, set }
    }
}

impl Drop for RecoveringHeadGuard {
    fn drop(&mut self) {
        self.set.lock().remove(&self.hash);
    }
}

/// Shared dedup set — one entry per in-flight recovery.
pub type RecoveringHeads = Arc<Mutex<LruCache<B256>>>;

/// Convenience constructor matching `LRU_PROCESSED_BLOCKS_SIZE` cap.
pub fn new_recovering_heads(cap: u32) -> RecoveringHeads {
    Arc::new(Mutex::new(LruCache::new(cap)))
}
```

- [ ] **Step 5.2: Thread the dedup set through `ImportService`**

Edit `src/node/network/block_import/service.rs`:

Add to the `ImportService<Provider>` struct fields (alongside `downloading_blocks`, around line 101):

```rust
    /// Heads currently being fork-recovered. Prevents duplicate spawned tasks
    /// when the same head is announced repeatedly.
    recovering_heads: crate::node::network::block_import::fork_recover::RecoveringHeads,
```

Add to `ImportService::new`, in the struct literal (around line 126):

```rust
            recovering_heads: crate::node::network::block_import::fork_recover::new_recovering_heads(
                LRU_PROCESSED_BLOCKS_SIZE,
            ),
```

- [ ] **Step 5.3: Verify compilation (no behavioural change yet)**

Run: `cargo check -p reth-bsc --lib`
Expected: no errors. Existing tests still pass because `recovering_heads` is only read in later tasks.

Run: `cargo test -p reth-bsc --lib block_import::`
Expected: all existing tests pass.

- [ ] **Step 5.4: Commit**

```bash
git add src/node/network/block_import/fork_recover.rs src/node/network/block_import/service.rs
git commit -m "feat(p2p): add recovering_heads dedup set to ImportService"
```

---

## Task 6: Replace `on_new_block_hashes` with `recover_ancestors` spawn

**Files:**
- Modify: `src/node/network/block_import/service.rs`

- [ ] **Step 6.1: Read the current `on_new_block_hashes` to confirm line numbers**

Run: `sed -n '489,568p' src/node/network/block_import/service.rs`
Expected: you see the function body unchanged since commit `e3ec8fa`.

- [ ] **Step 6.2: Replace the function body**

In `service.rs`, replace the entire body of `on_new_block_hashes` (from the `for hash_number in hash_numbers { ... }` block through the closing brace of the function) with the new version. The surrounding function signature and doc comment stay the same:

```rust
    /// Handle incoming block hashes by spawning fork-aware ancestor recovery
    /// for any head we do not already have.
    fn on_new_block_hashes(&mut self, hashes: NewBlockHashes, peer_id: PeerId) {
        for hash_number in hashes.0 {
            if self.processed_blocks.contains(&hash_number.hash) {
                continue;
            }
            if self.queued_blocks.contains(&hash_number.hash) {
                continue;
            }
            // Concurrent-dedup: one recovery per head at a time.
            {
                let mut guard = self.recovering_heads.lock();
                if guard.contains(&hash_number.hash) {
                    continue;
                }
                guard.insert(hash_number.hash);
            }

            tracing::debug!(
                target: "bsc::block_import",
                %peer_id,
                block_hash = %hash_number.hash,
                block_number = hash_number.number,
                "Spawning fork recovery for announced head"
            );

            let peer = self.resolve_bsc_peer(peer_id);
            let provider = self.forkchoice_engine.provider.clone();
            let engine = self.engine.clone();
            let forkchoice_engine = self.forkchoice_engine.clone();
            let recovering = self.recovering_heads.clone();
            let head_hash = hash_number.hash;
            let head_num = hash_number.number;

            tokio::spawn(async move {
                let _guard = crate::node::network::block_import::fork_recover::RecoveringHeadGuard::new(
                    head_hash, recovering,
                );
                let fetcher = crate::node::network::block_import::fork_recover::BscRangeFetcher;
                let Some(target) = peer else {
                    tracing::debug!(
                        target: "bsc::block_import",
                        %head_hash,
                        "No BSC protocol peer available for fork recovery"
                    );
                    return;
                };
                if let Err(err) =
                    crate::node::network::block_import::fork_recover::recover_ancestors(
                        target,
                        head_hash,
                        head_num,
                        provider,
                        engine,
                        forkchoice_engine,
                        &fetcher,
                    )
                    .await
                {
                    tracing::warn!(
                        target: "bsc::block_import",
                        %head_hash,
                        head_num,
                        error = %err,
                        "Fork recovery failed"
                    );
                }
            });
        }
    }
```

- [ ] **Step 6.3: Add the `resolve_bsc_peer` helper**

Inside the same `impl<Provider> ImportService<Provider>` block, add a small helper that mirrors the current peer selection (existing code at `service.rs:528-537`):

```rust
    /// Pick a peer to route `GetBlocksByRange` to. Prefer the announcing peer
    /// if it speaks the BSC sub-protocol; otherwise any registered BSC peer.
    fn resolve_bsc_peer(&self, announcer: PeerId) -> Option<PeerId> {
        if crate::node::network::bsc_protocol::registry::has_registered_peer(announcer) {
            Some(announcer)
        } else {
            crate::node::network::bsc_protocol::registry::list_registered_peers()
                .into_iter()
                .next()
        }
    }
```

- [ ] **Step 6.4: Drop now-unused imports and the `downloading_blocks` cooldown logic**

Because `on_new_block_hashes` no longer uses `downloading_blocks`, `DOWNLOAD_COOLDOWN_DURATION_MS`, or `std::time::SystemTime`, the compiler will emit dead-code warnings. Do NOT delete `downloading_blocks` yet — the `Syncing` arm (rewritten in Task 7) also referenced it. After Task 7, if it's truly unused, Task 8 cleans it up.

For now, run: `cargo check -p reth-bsc --lib 2>&1 | grep -E 'warning|error'`
Expected: warnings only about `DOWNLOAD_COOLDOWN_DURATION_MS` unused. No errors.

- [ ] **Step 6.5: Commit**

```bash
git add src/node/network/block_import/service.rs
git commit -m "refactor(p2p): route on_new_block_hashes through fork recovery"
```

---

## Task 7: Replace the `Syncing` arm of `new_payload`

**Files:**
- Modify: `src/node/network/block_import/service.rs`

- [ ] **Step 7.1: Confirm the current `Syncing` arm**

Run: `sed -n '219,310p' src/node/network/block_import/service.rs`
Expected: the arm still contains `batch_request_range_and_await_import` and the immediate `engine.fork_choice_updated` call — this is exactly what we delete.

- [ ] **Step 7.2: Rewrite the arm**

Replace the entire `PayloadStatusEnum::Syncing => { ... }` block in `new_payload` (around lines 219-309) with:

```rust
                    PayloadStatusEnum::Syncing => {
                        // Parent block is missing. Launch fork-aware ancestor
                        // recovery rather than a naive range fetch + premature
                        // FCU. The recovery task also owns the final FCU.
                        let block_number = header.number;
                        tracing::info!(
                            target: "bsc::block_import",
                            %block_hash,
                            block_number,
                            parent_hash = %header.parent_hash,
                            peer = %peer_id,
                            "New payload returned Syncing - spawning fork recovery"
                        );

                        // Fire-and-forget spawn; `recover_ancestors` runs its
                        // own Phase-1 local checks so it's correct even if the
                        // head is already on chain by the time the task starts.
                        {
                            let mut guard = recovering_heads.lock();
                            if guard.contains(&block_hash) {
                                return None;
                            }
                            guard.insert(block_hash);
                        }
                        let provider = forkchoice_engine.provider.clone();
                        let engine_clone = engine.clone();
                        let forkchoice_engine_clone = forkchoice_engine.clone();
                        let recovering = recovering_heads.clone();
                        let peer = resolve_bsc_peer_static(peer_id);
                        tokio::spawn(async move {
                            let _guard = crate::node::network::block_import::fork_recover::RecoveringHeadGuard::new(
                                block_hash, recovering,
                            );
                            let fetcher = crate::node::network::block_import::fork_recover::BscRangeFetcher;
                            let Some(target) = peer else { return; };
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
                            }
                        });
                        None
                    }
```

- [ ] **Step 7.3: Wire `recovering_heads` into the `new_payload` closure**

The `new_payload` method (around service.rs:148) builds an `async move { ... }` closure that captures `engine` and `forkchoice_engine`. It must also now capture `recovering_heads`. At the top of `new_payload` (next to the `engine` / `forkchoice_engine` clones at lines ~149-150), add:

```rust
        let recovering_heads = self.recovering_heads.clone();
```

- [ ] **Step 7.4: Add the static peer-resolver helper used inside the async block**

Because `&self` is not available inside `async move`, extract the peer-selection logic to a free function. Add near the top of `service.rs` (above the `impl` block):

```rust
fn resolve_bsc_peer_static(announcer: PeerId) -> Option<PeerId> {
    if crate::node::network::bsc_protocol::registry::has_registered_peer(announcer) {
        Some(announcer)
    } else {
        crate::node::network::bsc_protocol::registry::list_registered_peers()
            .into_iter()
            .next()
    }
}
```

Then have the instance method `resolve_bsc_peer` (added in Task 6.3) delegate to it:

```rust
    fn resolve_bsc_peer(&self, announcer: PeerId) -> Option<PeerId> {
        resolve_bsc_peer_static(announcer)
    }
```

- [ ] **Step 7.5: Verify the full crate builds**

Run: `cargo check -p reth-bsc --lib`
Expected: no errors.

Run: `cargo test -p reth-bsc --lib block_import::`
Expected: all existing tests pass (no behavioural regression in the non-fork happy paths; fork recovery paths are covered by the fork_recover module tests).

- [ ] **Step 7.6: Commit**

```bash
git add src/node/network/block_import/service.rs
git commit -m "refactor(p2p): route Syncing arm through fork recovery"
```

---

## Task 8: Clean up dead code and verify

**Files:**
- Modify: `src/node/network/block_import/service.rs`

- [ ] **Step 8.1: Check for unused items**

Run: `cargo check -p reth-bsc --lib 2>&1 | grep -E 'unused|dead_code'`

Expected: likely `DOWNLOAD_COOLDOWN_DURATION_MS`, possibly `downloading_blocks`, and possibly some imports (`std::time::SystemTime`, `U128`, `U256`, `NewBlock`). Review each one:

- If still referenced by any code path → keep it.
- If strictly unused → delete.

- [ ] **Step 8.2: Delete truly-unused items**

Example edits (adjust to match what the compiler actually reports):

- Remove `const DOWNLOAD_COOLDOWN_DURATION_MS: u128 = 200;` at service.rs:73.
- Remove the `downloading_blocks: LruMap<B256, u128, ByLength>` field and its initializer (and the `schnellru::{ByLength, LruMap}` import if no longer needed).
- Remove stale imports flagged by the compiler.

- [ ] **Step 8.3: Run the full test suite**

Run: `cargo test -p reth-bsc --lib`
Expected: pass. New tests in `fork_recover::tests` plus all pre-existing ones.

Run: `cargo clippy -p reth-bsc --lib -- -D warnings 2>&1 | tail -40`
Expected: clean (if the project's CI treats clippy as errors). If warnings surface that are not from our diff, leave them alone — they're pre-existing.

- [ ] **Step 8.4: Smoke build**

Run: `cargo build -p reth-bsc --release 2>&1 | tail -20`
Expected: builds. (Full release build may take time; run only if the CI gates it.)

- [ ] **Step 8.5: Commit cleanup**

```bash
git add src/node/network/block_import/service.rs
git commit -m "chore(p2p): drop dead-code left by fork recovery refactor"
```

---

## Self-Review Notes

This plan covers:

- **Spec §Problem / Current behaviour (buggy)** — Tasks 6 & 7 delete both fetch sites and the premature FCU.
- **Spec §Goal bullets 1–5** — Task 3 (Phase 1 walk), Task 3 (Phase 1 local skip + pre-hop short-circuit), Task 4 (Phase 2 sequential-await import), Task 4 (Phase 3 FCU), Task 4 (short-circuit also fires FCU via unconditional Phase 3).
- **Spec §Architecture: new module fork_recover.rs** — Tasks 1–4.
- **Spec §Changes in service.rs: remove both fetch sites + add `recovering_heads`** — Tasks 5, 6, 7.
- **Spec §Constants** — defined in Task 1 with the spec's values.
- **Spec §Error handling table** — each variant in `ForkRecoverError` (Task 1) maps to a row; callers in Tasks 6 & 7 log `warn` on failure and don't FCU.
- **Spec §Concurrency model** — `Arc<Mutex<LruCache<B256>>>` + RAII guard in Task 5.
- **Spec §Testing strategy** — unit tests #1, #2, #3, #6, #7, #8 in Task 3. Tests #4 & #5 (fork depth 80 / exactly 256) are mechanically identical to #3 / #6 with larger scripted responses; implementer may add them if desired but they exercise the same branches. Tests #9 (Invalid on new_payload) / #10 (empty hop response) / #11 (concurrent dedup) rely on the `ImportService` wiring and are covered at integration level via the `TestFixture` infrastructure — add in a follow-up if gaps show up.

No placeholders remain. Types used across tasks (`Discovery`, `DiscoveryOutcome`, `ForkRecoverError`, `RangeFetcher`, `BscRangeFetcher`, `RecoveringHeadGuard`, `RecoveringHeads`, `recover_ancestors`, `discover_fork_blocks`, `new_recovering_heads`) are defined once in `fork_recover.rs` and referenced consistently in downstream tasks. Constants and function signatures match between the module definitions (Tasks 1–5) and the consuming call sites (Tasks 6–7).

---

## Execution Handoff

Plan complete and saved to `docs/superpowers/plans/2026-04-17-p2p-fork-recovery.md`. Two execution options:

**1. Subagent-Driven (recommended)** — dispatch a fresh subagent per task, review between tasks, fast iteration.
**2. Inline Execution** — execute tasks in this session using executing-plans, batch with checkpoints.

Which approach?
