//! Fork recovery: ancestor-aware block pull that replaces the naive
//! batch range-request call in the import service.

use std::{sync::Arc, time::Duration};

use alloy_primitives::B256;
use alloy_rpc_types_engine::PayloadStatusEnum;
use futures::future::BoxFuture;
use parking_lot::Mutex;
use reth::network::cache::LruCache;
use reth_engine_primitives::ConsensusEngineHandle;
use reth_network_api::PeerId;
use reth_payload_primitives::PayloadTypes;
use reth_primitives_traits::{AlloyBlockHeader, Block as _};
use reth_provider::{BlockHashReader, BlockNumReader, HeaderProvider};

use crate::{
    node::{consensus::BscForkChoiceEngine, engine_api::payload::BscPayloadTypes},
    BscBlock,
};

/// Hard cap on how many blocks we will walk back from the peer's announced
/// head before giving up.
pub const MAX_FORK_DEPTH: u64 = 2048;

/// Blocks fetched per `GetBlocksByRange` hop. Kept small because BSC blocks
/// are large (full tx bodies + sidecars).
pub const FORK_RECOVER_HOP_COUNT: u64 = 4;

/// Per-hop network timeout.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// Max peer attempts per hop before `BscRangeFetcher` gives up. Attempts are
/// sequential, so worst-case per-hop stall is
/// `MAX_PEER_ATTEMPTS * FETCH_TIMEOUT` (~15s at current values); a full
/// recovery is bounded by
/// `FORK_RECOVER_HOP_COUNT * MAX_PEER_ATTEMPTS * FETCH_TIMEOUT` in adversarial
/// conditions. The announcing peer is tried first; failover rotates through
/// other registered BSC peers.
pub const MAX_PEER_ATTEMPTS: usize = 3;

/// How long a head stays suppressed after `recover_ancestors` fails.
/// Prevents the 3s periodic head-announce tick from re-spawning a
/// doomed recovery every loop.
pub const FAILED_HEAD_COOLDOWN: Duration = Duration::from_secs(30);

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

    #[error("fcu_target_header.hash {got} ≠ fcu_target_hash {expected}")]
    FcuTargetHeaderMismatch { expected: alloy_primitives::B256, got: alloy_primitives::B256 },
}

/// Abstraction over `GetBlocksByRange`. Tests substitute a fake; production
/// forwards to `bsc_protocol::registry::request_blocks_by_range_with_failover`,
/// which rotates through registered BSC peers on empty/error responses.
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

/// Walk backwards from `(start_num, start_hash)` via `parent_hash`-walked
/// `GetBlocksByRange` hops until a local-chain match is found or
/// `MAX_FORK_DEPTH` is exhausted.
///
/// The starting cursor is the *first block we want to learn about*, not
/// necessarily the announced head: the parent-start path (`on_new_block`)
/// passes `parent_hash` here while the legacy path (`on_new_block_hashes`)
/// passes the announced head itself.
pub async fn discover_fork_blocks<
    P: BlockHashReader + HeaderProvider<Header = alloy_consensus::Header>,
>(
    peer: PeerId,
    start_hash: B256,
    start_num: u64,
    provider: &P,
    fetcher: &dyn RangeFetcher,
) -> Result<Discovery, ForkRecoverError> {
    let mut fork_blocks: Vec<crate::BscBlock> = Vec::new();
    let mut cursor_num = start_num;
    let mut cursor_hash = start_hash;
    let mut walked: u64 = 0;

    loop {
        // Pre-hop local checks: if this cursor is already local, we've
        // reached the common ancestor (or short-circuited because the
        // starting cursor was itself locally known).
        //
        // The side-chain hit (`provider.header(cursor_hash).is_some()`) is
        // safe to treat as "ancestor reached" because engine-tree only stores
        // a header when its parent was already Valid via a prior new_payload.
        // Transitively, any side-block in our provider is rooted at the
        // canonical chain, so no further walking is required.
        let cursor_is_local = provider.block_hash(cursor_num)? == Some(cursor_hash) ||
            provider.header(cursor_hash)?.is_some();
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

/// Inputs to [`recover_ancestors`]. The discovery start and FCU target are
/// kept separate so callers with the full block payload (`on_new_block`) can
/// start the backward walk from `parent_hash` — avoiding BSC's
/// broadcast-before-commit race — while still flipping canonical to the
/// announced head. Construct via [`RecoverTarget::single_pair`] or
/// [`RecoverTarget::from_parent`].
#[derive(Debug, Clone)]
pub struct RecoverTarget {
    /// Cursor for the backward walk. Parent of the announced block on the
    /// `on_new_block` path; the head itself on header-only paths.
    pub fetch_start_hash: B256,
    /// Block number paired with `fetch_start_hash`.
    pub fetch_start_num: u64,
    /// Block we want canonical at the end of recovery.
    pub fcu_target_hash: B256,
    /// Block number paired with `fcu_target_hash`.
    pub fcu_target_num: u64,
    /// Header for the FCU target. `Some` when the caller already has it
    /// decoded — lets Phase 3 skip a `provider.header(fcu_target_hash)` lookup
    /// that may miss a block engine-tree just unbuffered but hasn't persisted.
    /// `None` falls back to phase-2 tail / provider.
    pub fcu_target_header: Option<alloy_consensus::Header>,
}

impl RecoverTarget {
    /// Fetch from and FCU to the same `(hash, num)`. Used by header-only
    /// paths (`on_new_block_hashes`) where we can't compute `parent_hash`;
    /// stays exposed to a small broadcast-before-commit race window.
    pub fn single_pair(hash: B256, num: u64) -> Self {
        Self {
            fetch_start_hash: hash,
            fetch_start_num: num,
            fcu_target_hash: hash,
            fcu_target_num: num,
            fcu_target_header: None,
        }
    }

    /// Fetch from `(parent_hash, parent_num)`, FCU to `(target_hash,
    /// target_num)`. Used by `on_new_block` to dodge the broadcast-before-
    /// commit race: `parent_hash` is always committed on the source peer
    /// (since `target` was built on top of it).
    ///
    /// Caller must already have called `engine.new_payload(target)` so the
    /// block is buffered in engine-tree; Phase 2's parent import unbuffers
    /// and imports it via `try_connect_buffered_blocks`.
    pub fn from_parent(
        parent_hash: B256,
        parent_num: u64,
        target_hash: B256,
        target_num: u64,
        target_header: alloy_consensus::Header,
    ) -> Self {
        Self {
            fetch_start_hash: parent_hash,
            fetch_start_num: parent_num,
            fcu_target_hash: target_hash,
            fcu_target_num: target_num,
            fcu_target_header: Some(target_header),
        }
    }
}

/// Three-phase ancestor-aware recovery:
///
/// 1. `discover_fork_blocks` walks back from `target.fetch_start_*` to the common ancestor.
/// 2. Imports fork blocks oldest → newest via `engine.new_payload`, awaiting `Valid` on each before
///    submitting the next.
/// 3. `fork_choice_updated` for `target.fcu_target_*` so engine-tree re-evaluates canonical
///    selection.
///
/// See [`RecoverTarget`] for the parent-start vs single-pair design.
pub async fn recover_ancestors<P>(
    peer: PeerId,
    target: RecoverTarget,
    provider: P,
    engine: ConsensusEngineHandle<BscPayloadTypes>,
    forkchoice_engine: BscForkChoiceEngine<P>,
    fetcher: &dyn RangeFetcher,
) -> Result<(), ForkRecoverError>
where
    P: BlockHashReader
        + BlockNumReader
        + HeaderProvider<Header = alloy_consensus::Header>
        + Clone
        + Send
        + Sync
        + 'static,
{
    let RecoverTarget {
        fetch_start_hash,
        fetch_start_num,
        fcu_target_hash,
        fcu_target_num,
        fcu_target_header,
    } = target;

    tracing::info!(
        target: "bsc::fork_recover",
        %peer,
        %fetch_start_hash,
        fetch_start_num,
        %fcu_target_hash,
        fcu_target_num,
        "Starting fork recovery"
    );

    // ---- Phase 1 ----
    let discovery =
        discover_fork_blocks(peer, fetch_start_hash, fetch_start_num, &provider, fetcher).await?;
    tracing::debug!(
        target: "bsc::fork_recover",
        %peer,
        %fetch_start_hash,
        fetch_start_num,
        %fcu_target_hash,
        fcu_target_num,
        fork_blocks = discovery.fork_blocks.len(),
        outcome = ?discovery.outcome,
        "Phase 1 complete"
    );

    // ---- Phase 2: import oldest → newest via new_payload ----
    let mut to_import = discovery.fork_blocks;
    to_import.reverse();
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
                    // Sequencing guarantees parents were already Valid, so
                    // Syncing here means a parent failed silently.
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

    // ---- Phase 3: FCU so engine-tree re-evaluates canonical head ----
    let head_header = resolve_fcu_head_header(
        &provider,
        fcu_target_hash,
        fcu_target_header.as_ref(),
        to_import.last(),
    )?;
    if let Err(err) = forkchoice_engine.update_forkchoice(&head_header).await {
        // FCU failure is recoverable (engine-tree may retry on next import);
        // surface at warn level to match `service.rs` convention.
        tracing::warn!(
            target: "bsc::fork_recover",
            %fcu_target_hash,
            error = %err,
            "fork_choice_updated returned error after recovery"
        );
    } else {
        tracing::info!(
            target: "bsc::fork_recover",
            %fcu_target_hash,
            fcu_target_num,
            "Fork recovery FCU succeeded"
        );
    }

    Ok(())
}

/// Resolve the FCU target header in priority order:
///
/// 1. Caller override (`fcu_target_header`) — used by `on_new_block` to skip a provider lookup for
///    a block engine-tree just unbuffered.
/// 2. Phase-2 tail iff its hash equals `fcu_target_hash` — single-pair callers; sidesteps the qanet
///    livelock where the DB-backed provider misses freshly-imported blocks.
/// 3. Provider — Shortcircuit path or last resort; missing → `HeadHeaderMissing`.
fn resolve_fcu_head_header<P>(
    provider: &P,
    fcu_target_hash: B256,
    fcu_target_header: Option<&alloy_consensus::Header>,
    phase_2_tail: Option<&crate::BscBlock>,
) -> Result<alloy_consensus::Header, ForkRecoverError>
where
    P: HeaderProvider<Header = alloy_consensus::Header>,
{
    // 1. Override. Hard-fail on hash mismatch: silently accepting a forged
    // header would flip canonical to the wrong head.
    if let Some(h) = fcu_target_header {
        let got = h.hash_slow();
        if got != fcu_target_hash {
            return Err(ForkRecoverError::FcuTargetHeaderMismatch {
                expected: fcu_target_hash,
                got,
            });
        }
        return Ok(h.clone());
    }

    // 2. Phase-2 tail iff it equals the FCU target.
    if let Some(last) = phase_2_tail {
        if last.header.hash_slow() == fcu_target_hash {
            return Ok(last.header.clone());
        }
    }

    // 3. Provider.
    provider
        .header(fcu_target_hash)?
        .ok_or(ForkRecoverError::HeadHeaderMissing { hash: fcu_target_hash })
}

/// Bounded LRU of recently-failed recovery heads with per-entry deadlines.
///
/// `is_cooling` returns true only for entries whose deadline has not yet
/// expired. Expired entries are lazily removed on access. Capacity eviction
/// is handled by the underlying `schnellru::LruMap` and matches the
/// `BODY_CACHE` / `RecoveringHeads` pattern elsewhere in the codebase.
#[derive(Clone)]
pub struct FailedHeadsCooler {
    inner: Arc<Mutex<schnellru::LruMap<B256, std::time::Instant, schnellru::ByLength>>>,
    cooldown: Duration,
}

impl FailedHeadsCooler {
    pub fn new(capacity: u32, cooldown: Duration) -> Self {
        Self {
            inner: Arc::new(Mutex::new(schnellru::LruMap::new(schnellru::ByLength::new(capacity)))),
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
        ) -> Result<Option<reth_primitives_traits::SealedHeader<Self::Header>>, ProviderError>
        {
            Ok(None)
        }
        fn sealed_headers_while(
            &self,
            _range: impl core::ops::RangeBounds<u64>,
            _predicate: impl FnMut(&reth_primitives_traits::SealedHeader<Self::Header>) -> bool,
        ) -> Result<Vec<reth_primitives_traits::SealedHeader<Self::Header>>, ProviderError>
        {
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

    // ---- Spec test #6: fork too deep (depth MAX_FORK_DEPTH + 1 → ForkTooDeep) ----
    #[tokio::test]
    async fn discover_fork_too_deep() {
        let mut provider = FakeProvider::default();
        let (shared, shared_hashes) = linear_chain(0, 1, B256::ZERO, 0xC);
        for h in &shared {
            provider.insert_canonical(h.clone());
        }

        // Peer chain: genesis + (MAX_FORK_DEPTH + FORK_RECOVER_HOP_COUNT) fork
        // blocks. None match canonical (which only has block 0).
        let peer_len = MAX_FORK_DEPTH + FORK_RECOVER_HOP_COUNT;
        let (peer, peer_hashes) = linear_chain(1, peer_len, shared_hashes[0], 0xB);

        // Script: return FORK_RECOVER_HOP_COUNT fork blocks per hop; after
        // `MAX_FORK_DEPTH / FORK_RECOVER_HOP_COUNT` hops, `walked ==
        // MAX_FORK_DEPTH`, so the next iteration's pre-hop check trips
        // ForkTooDeep.
        //
        // `peer` is indexed `0..=peer_len-1` with peer[idx] at height idx+1.
        // For hop `i` (0-indexed), the top height served is
        // `peer_len - FORK_RECOVER_HOP_COUNT * i` and the response covers
        // heights `(top - FORK_RECOVER_HOP_COUNT + 1)..=top` newest→oldest.
        let hops = (MAX_FORK_DEPTH / FORK_RECOVER_HOP_COUNT) as usize;
        let hop = FORK_RECOVER_HOP_COUNT as usize;
        let peer_len_usize = peer_len as usize;
        let mut responses: Vec<Result<Vec<BscBlock>, String>> = Vec::new();
        for i in 0..hops {
            let top_height = peer_len_usize - hop * i;
            let slice: Vec<BscBlock> = peer[(top_height - hop)..=(top_height - 1)]
                .iter()
                .cloned()
                .rev()
                .map(make_block)
                .collect();
            responses.push(Ok(slice));
        }
        let fetcher = ScriptedFetcher::new(responses);

        let err = discover_fork_blocks(
            fake_peer(),
            peer_hashes[peer_len_usize - 1],
            peer_len,
            &provider,
            fetcher.as_ref(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ForkRecoverError::ForkTooDeep));
        assert_eq!(fetcher.calls().len(), hops);
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

    // ---- RecoverTarget constructor tests ----
    //
    // Trivial field mapping, pinned to catch future swaps of `parent_*` ↔
    // `target_*` (would type-check but reintroduce the race).

    #[test]
    fn recover_target_single_pair_collapses_fetch_and_fcu() {
        let h = B256::repeat_byte(0xAB);
        let n = 42;
        let t = super::RecoverTarget::single_pair(h, n);
        assert_eq!(t.fetch_start_hash, h);
        assert_eq!(t.fetch_start_num, n);
        assert_eq!(t.fcu_target_hash, h);
        assert_eq!(t.fcu_target_num, n);
        assert!(t.fcu_target_header.is_none());
    }

    #[test]
    fn recover_target_from_parent_keeps_pairs_distinct() {
        let parent_hash = B256::repeat_byte(0x11);
        let parent_num = 99;
        let target_hash = B256::repeat_byte(0x22);
        let target_num = 100;
        let target_header = make_header(target_num, parent_hash, 0x1);
        let target_header_hash = target_header.hash_slow();

        let t = super::RecoverTarget::from_parent(
            parent_hash,
            parent_num,
            target_hash,
            target_num,
            target_header,
        );
        assert_eq!(t.fetch_start_hash, parent_hash);
        assert_eq!(t.fetch_start_num, parent_num);
        assert_eq!(t.fcu_target_hash, target_hash);
        assert_eq!(t.fcu_target_num, target_num);
        let stored = t.fcu_target_header.expect("from_parent must populate the header");
        assert_eq!(stored.hash_slow(), target_header_hash);
        assert_eq!(stored.number, target_num);
    }

    // ---- Phase-3 head-header resolver tests ----
    //
    // These pin the qanet livelock fix: Phase 2 imports the peer's head via
    // `engine.new_payload` (Valid), but the DB-backed provider does not see
    // that block until engine-tree persists it. The resolver must use the
    // Phase-2 tail, not the provider, on the AncestorFound path.

    #[test]
    fn resolve_fcu_head_header_uses_phase2_tail_when_provider_misses() {
        // Provider has the shared ancestor but not the peer's head (simulates
        // the just-imported-but-not-persisted engine-tree state that caused
        // the two-validator fork livelock).
        let mut provider = FakeProvider::default();
        let (shared, shared_hashes) = linear_chain(0, 1, B256::ZERO, 0xC);
        for h in &shared {
            provider.insert_canonical(h.clone());
        }

        // Peer fork: one block on top of the ancestor.
        let head = make_header(1, shared_hashes[0], 0xB);
        let head_hash = head.hash_slow();
        let tail_block = make_block(head.clone());

        // Sanity: provider can't see the head (neither canonical nor side).
        assert!(
            BlockHashReader::block_hash(&provider, 1).unwrap() != Some(head_hash),
            "precondition: head not canonical in provider",
        );
        assert!(
            HeaderProvider::header(&provider, head_hash).unwrap().is_none(),
            "precondition: head not as side-chain in provider",
        );

        // Legacy single-pair path: fcu_target_header = None, phase_2_tail.hash == fcu_target_hash.
        let resolved =
            super::resolve_fcu_head_header(&provider, head_hash, None, Some(&tail_block)).unwrap();
        assert_eq!(
            resolved.hash_slow(),
            head_hash,
            "must return the Phase-2 tail header verbatim, never consult provider",
        );
        assert_eq!(resolved.number, 1);
    }

    #[test]
    fn resolve_fcu_head_header_shortcircuit_falls_back_to_provider() {
        // Shortcircuit: fork_blocks empty, head already known locally
        // (side-chain block the provider can serve).
        let mut provider = FakeProvider::default();
        let (shared, shared_hashes) = linear_chain(0, 1, B256::ZERO, 0xC);
        for h in &shared {
            provider.insert_canonical(h.clone());
        }
        let side = make_header(1, shared_hashes[0], 0xB);
        let side_hash = side.hash_slow();
        provider.insert_side(side);

        let resolved = super::resolve_fcu_head_header(&provider, side_hash, None, None).unwrap();
        assert_eq!(resolved.hash_slow(), side_hash);
    }

    #[test]
    fn resolve_fcu_head_header_shortcircuit_missing_yields_error() {
        // Defensive path: if Shortcircuit somehow fires but the provider no
        // longer has the head (pruning race / DB corruption), surface
        // `HeadHeaderMissing` rather than forging an FCU target.
        let provider = FakeProvider::default();
        let head_hash = B256::repeat_byte(0xAB);

        let err = super::resolve_fcu_head_header(&provider, head_hash, None, None).unwrap_err();
        match err {
            ForkRecoverError::HeadHeaderMissing { hash } => assert_eq!(hash, head_hash),
            other => panic!("expected HeadHeaderMissing, got {other:?}"),
        }
    }

    // ---- Override / parent-start path tests ----

    #[test]
    fn resolve_fcu_head_header_explicit_override_wins_over_phase2_tail() {
        // Override (priority 1) wins over phase-2 tail (priority 2), even
        // when both are present.
        let provider = FakeProvider::default();

        let target = make_header(10, B256::repeat_byte(0xAA), 0x1);
        let target_hash = target.hash_slow();

        // Phase-2 tail is N-1 in the parent-start path; its hash differs.
        let tail_header = make_header(9, B256::repeat_byte(0xBB), 0x2);
        let tail_block = make_block(tail_header);

        let resolved = super::resolve_fcu_head_header(
            &provider,
            target_hash,
            Some(&target),
            Some(&tail_block),
        )
        .unwrap();
        assert_eq!(resolved.hash_slow(), target_hash);
        assert_eq!(resolved.number, 10);
    }

    #[test]
    fn resolve_fcu_head_header_mismatched_override_yields_error() {
        // Hash mismatch must surface as error, not silently flip canonical
        // to a forged head.
        let provider = FakeProvider::default();
        let supplied = make_header(10, B256::repeat_byte(0xAA), 0x1);
        let supplied_hash = supplied.hash_slow();
        let claimed_target_hash = B256::repeat_byte(0x99); // ≠ supplied_hash

        let err =
            super::resolve_fcu_head_header(&provider, claimed_target_hash, Some(&supplied), None)
                .unwrap_err();
        match err {
            ForkRecoverError::FcuTargetHeaderMismatch { expected, got } => {
                assert_eq!(expected, claimed_target_hash);
                assert_eq!(got, supplied_hash);
            }
            other => panic!("expected FcuTargetHeaderMismatch, got {other:?}"),
        }
    }

    #[test]
    fn resolve_fcu_head_header_parent_start_falls_back_to_provider_when_no_override() {
        // Parent-start: tail.hash (N-1) ≠ fcu_target_hash (N). Tail is
        // skipped, provider answers.
        let mut provider = FakeProvider::default();
        let parent = make_header(9, B256::repeat_byte(0xCC), 0x1);
        let parent_hash = parent.hash_slow();
        provider.insert_canonical(parent.clone());

        let target = make_header(10, parent_hash, 0x1);
        let target_hash = target.hash_slow();
        // Provider DOES know N (canonical) — pretend persistence raced ahead.
        provider.insert_canonical(target.clone());

        let tail_block = make_block(parent);

        let resolved =
            super::resolve_fcu_head_header(&provider, target_hash, None, Some(&tail_block))
                .unwrap();
        assert_eq!(resolved.hash_slow(), target_hash);
        assert_eq!(resolved.number, 10);
    }

    #[test]
    fn resolve_fcu_head_header_parent_start_missing_provider_yields_error() {
        // Parent-start with no override and provider also missing N → must
        // surface HeadHeaderMissing rather than forge from the N-1 tail.
        let provider = FakeProvider::default();
        let parent_hash = B256::repeat_byte(0xCC);
        let target_hash = B256::repeat_byte(0xDD);

        let tail_block = make_block(make_header(9, parent_hash, 0x1));

        let err = super::resolve_fcu_head_header(&provider, target_hash, None, Some(&tail_block))
            .unwrap_err();
        match err {
            ForkRecoverError::HeadHeaderMissing { hash } => assert_eq!(hash, target_hash),
            other => panic!("expected HeadHeaderMissing, got {other:?}"),
        }
    }
}

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
