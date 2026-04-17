//! Fork recovery: ancestor-aware block pull that replaces the naive
//! `batch_request_range_and_await_import` call in the import service.
//!
//! See `docs/superpowers/specs/2026-04-17-p2p-fork-recovery-design.md`.

use std::time::Duration;

use alloy_primitives::B256;
use futures::future::BoxFuture;
use reth_network_api::PeerId;
use reth_primitives_traits::AlloyBlockHeader;
use reth_provider::{BlockHashReader, HeaderProvider};

use crate::BscBlock;

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

/// Abstraction over `GetBlocksByRange`. Tests substitute a fake; production
/// forwards to `bsc_protocol::registry::request_blocks_by_range`.
pub trait RangeFetcher: Send + Sync {
    /// Fetch up to `count` blocks starting at `(start_num, start_hash)` and
    /// walking backwards via `parent_hash`. Response is ordered
    /// **newest -> oldest**.
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
    /// Fork blocks **newest -> oldest**. Empty on `Shortcircuit`.
    pub fork_blocks: Vec<crate::BscBlock>,
    pub outcome: DiscoveryOutcome,
}

/// Walk backwards from `(head_num, head_hash)` via `parent_hash`-walked
/// `GetBlocksByRange` hops until a local-chain match is found or
/// `MAX_FORK_DEPTH` is exhausted.
pub async fn discover_fork_blocks<
    P: BlockHashReader + HeaderProvider<Header = alloy_consensus::Header>,
>(
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
        //
        // The side-chain hit (`provider.header(cursor_hash).is_some()`) is
        // safe to treat as "ancestor reached" because engine-tree only stores
        // a header when its parent was already Valid via a prior new_payload.
        // Transitively, any side-block in our provider is rooted at the
        // canonical chain, so no further walking is required.
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

        let resp = fetcher
            .fetch(peer, cursor_num, cursor_hash, count)
            .await
            .map_err(ForkRecoverError::FetchFailed)?;
        if resp.is_empty() {
            return Err(ForkRecoverError::EmptyResponse { num: cursor_num, hash: cursor_hash });
        }

        // Iterate newest -> oldest (the order we got them in).
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
            return Ok(Discovery { fork_blocks, outcome: DiscoveryOutcome::AncestorFound });
        }

        // Advance cursor to the block just below the oldest in this response.
        // saturating_sub: if we're already at block 0 with no match, the next
        // pre-hop check fails and `walked >= MAX_FORK_DEPTH` eventually trips
        // ForkTooDeep rather than panicking on underflow.
        let oldest = resp.last().unwrap();
        walked += resp.len() as u64;
        cursor_num = oldest.header.number.saturating_sub(1);
        cursor_hash = oldest.header.parent_hash;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::Header;
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
            Ok(self.canonical_by_num.get(&num).and_then(|h| self.headers_by_hash.get(h).cloned()))
        }
        fn header_td(&self, _: &B256) -> Result<Option<alloy_primitives::U256>, ProviderError> {
            Ok(None)
        }
        fn header_td_by_number(
            &self,
            _: u64,
        ) -> Result<Option<alloy_primitives::U256>, ProviderError> {
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
            Arc::new(Self { responses: Mutex::new(responses), requests: Mutex::new(vec![]) })
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
        Header { number, parent_hash, extra_data: vec![tag].into(), ..Default::default() }
    }

    fn make_block(header: Header) -> BscBlock {
        BscBlock { header, body: BscBlockBody::default() }
    }

    /// Build a linear chain starting from `genesis_parent` of length `len`.
    /// Returns `(headers, hashes)` in ascending height order.
    fn linear_chain(
        start_num: u64,
        len: u64,
        genesis_parent: B256,
        tag: u8,
    ) -> (Vec<Header>, Vec<B256>) {
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

    // ---- Spec test #1: head already on canonical (pre-hop short-circuit) ----
    #[tokio::test]
    async fn discover_head_on_canonical_no_fetch() {
        let mut provider = FakeProvider::default();
        let (chain, hashes) = linear_chain(0, 101, B256::ZERO, 0xC);
        for h in chain {
            provider.insert_canonical(h);
        }

        let fetcher = ScriptedFetcher::new(vec![]);
        let out = discover_fork_blocks(fake_peer(), hashes[100], 100, &provider, fetcher.as_ref())
            .await
            .unwrap();

        assert!(out.fork_blocks.is_empty(), "no blocks to import");
        assert!(matches!(out.outcome, DiscoveryOutcome::Shortcircuit));
        assert_eq!(fetcher.calls().len(), 0, "no network fetch should happen");
    }

    // ---- Spec test #2: simple linear-ahead, one hop, one extra pre-hop check ----
    #[tokio::test]
    async fn discover_linear_ahead_one_hop() {
        let mut provider = FakeProvider::default();
        let (local, local_hashes) = linear_chain(0, 101, B256::ZERO, 0xC); // canonical 0..=100
        for h in &local {
            provider.insert_canonical(h.clone());
        }

        // Peer extends with blocks 101..=104 parented on 100.
        let (peer_ext, _peer_hashes) = linear_chain(101, 4, local_hashes[100], 0xC);
        let hop1: Vec<BscBlock> = peer_ext.iter().cloned().rev().map(make_block).collect();
        let fetcher = ScriptedFetcher::new(vec![Ok(hop1)]);

        let head_hash = peer_ext.last().unwrap().hash_slow();
        let out = discover_fork_blocks(fake_peer(), head_hash, 104, &provider, fetcher.as_ref())
            .await
            .unwrap();

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
        for h in &shared {
            provider.insert_canonical(h.clone());
        }
        let ancestor_hash = shared_hashes[95];

        // Local fork X: 96X..=100X
        let (local_x, _) = linear_chain(96, 5, ancestor_hash, 0xA);
        for h in &local_x {
            provider.insert_canonical(h.clone());
        }

        // Peer fork Y: 96Y..=102Y
        let (peer_y, peer_y_hashes) = linear_chain(96, 7, ancestor_hash, 0xB);

        // Hop 1: server returns [102Y, 101Y, 100Y, 99Y] (newest→oldest).
        let hop1: Vec<BscBlock> = peer_y[3..=6].iter().cloned().rev().map(make_block).collect();
        // Hop 2: server returns [98Y, 97Y, 96Y, 95_shared].
        let mut hop2: Vec<BscBlock> = peer_y[0..=2].iter().cloned().rev().map(make_block).collect();
        hop2.push(make_block(shared[95].clone()));
        let fetcher = ScriptedFetcher::new(vec![Ok(hop1), Ok(hop2)]);

        let out =
            discover_fork_blocks(fake_peer(), peer_y_hashes[6], 102, &provider, fetcher.as_ref())
                .await
                .unwrap();

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
        for h in &shared {
            provider.insert_canonical(h.clone());
        }

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

        let err =
            discover_fork_blocks(fake_peer(), peer_hashes[299], 300, &provider, fetcher.as_ref())
                .await
                .unwrap_err();
        assert!(matches!(err, ForkRecoverError::ForkTooDeep));
        assert_eq!(fetcher.calls().len(), 64);
    }

    // ---- Spec test #7: head already present as side-chain (short-circuit, empty fork_blocks) ----
    #[tokio::test]
    async fn discover_head_side_chain_shortcircuit() {
        let mut provider = FakeProvider::default();
        let (shared, shared_hashes) = linear_chain(0, 96, B256::ZERO, 0xC);
        for h in &shared {
            provider.insert_canonical(h.clone());
        }
        // Insert a side-chain block at 96 (not canonical).
        let side_96 = make_header(96, shared_hashes[95], 0xB);
        let side_hash = side_96.hash_slow();
        provider.insert_side(side_96);

        let fetcher = ScriptedFetcher::new(vec![]);
        let out = discover_fork_blocks(fake_peer(), side_hash, 96, &provider, fetcher.as_ref())
            .await
            .unwrap();
        assert!(matches!(out.outcome, DiscoveryOutcome::Shortcircuit));
        assert!(out.fork_blocks.is_empty());
        assert_eq!(fetcher.calls().len(), 0);
    }

    // ---- Spec test #8: mid-chain side block already present, skipped ----
    #[tokio::test]
    async fn discover_mid_chain_side_block_skipped() {
        let mut provider = FakeProvider::default();
        let (shared, shared_hashes) = linear_chain(0, 96, B256::ZERO, 0xC);
        for h in &shared {
            provider.insert_canonical(h.clone());
        }
        let ancestor_hash = shared_hashes[95];

        // Peer fork Y: 96Y..=99Y (4 blocks).
        let (peer_y, peer_y_hashes) = linear_chain(96, 4, ancestor_hash, 0xB);
        // Register 97Y as an already-known side-chain block.
        provider.insert_side(peer_y[1].clone());

        // Hop 1: [99Y, 98Y, 97Y, 96Y].
        let hop1: Vec<BscBlock> = peer_y.iter().cloned().rev().map(make_block).collect();
        let fetcher = ScriptedFetcher::new(vec![Ok(hop1)]);

        let out =
            discover_fork_blocks(fake_peer(), peer_y_hashes[3], 99, &provider, fetcher.as_ref())
                .await
                .unwrap();

        assert!(matches!(out.outcome, DiscoveryOutcome::AncestorFound));
        let nums: Vec<u64> = out.fork_blocks.iter().map(|b| b.header.number).collect();
        // Non-contiguous fork_blocks: 97Y is omitted because it's already a
        // known side-block. Task 4's import loop must therefore tolerate gaps
        // — it imports [96, 98, 99] oldest-first and relies on engine-tree
        // already holding 97Y as Valid (which is why the side-block exists).
        assert_eq!(nums, vec![99, 98, 96], "97Y skipped because already on side-chain");
        assert_eq!(fetcher.calls().len(), 1);
    }
}
