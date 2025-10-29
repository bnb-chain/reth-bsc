use alloy_consensus::{constants::ETH_TO_WEI, Header};
use alloy_primitives::{Address, B256, BlockHash, BlockNumber, U256, address};
use parking_lot::RwLock;
use rand::Rng;
use reth_engine_primitives::BeaconConsensusEngineHandle;
use schnellru::{ByLength, LruMap};
use std::sync::Arc;
use tracing::{info, warn};

use crate::{chainspec::BscChainSpec, hardforks::BscHardforks, node::engine_api::payload::BscPayloadTypes, shared};
use reth_provider::{BlockNumReader, HeaderProvider, ProviderError};
use std::cmp::Ordering;

pub const SYSTEM_ADDRESS: Address = address!("0xfffffffffffffffffffffffffffffffffffffffe");
/// The reward percent to system
pub const SYSTEM_REWARD_PERCENT: usize = 4;
/// The max reward in system reward contract
pub const MAX_SYSTEM_REWARD: u128 = 100 * ETH_TO_WEI;

/// Errors that can occur in Parlia consensus
#[derive(Debug, thiserror::Error)]
pub enum ParliaConsensusErr {
    /// Error from the provider
    #[error(transparent)]
    Provider(#[from] ProviderError),
    /// Head block hash not found
    #[error("Head block hash not found")]
    HeadHashNotFound,
    /// Fork choice update error
    #[error("Fork choice update error: {0}")]
    ForkChoiceUpdateError(String),
    /// Unknown total difficulty
    #[error("Unknown total difficulty for block {0} at number {1}")]
    UnknownTotalDifficulty(B256, u64),
    /// Internal error
    #[error(transparent)]
    Internal(Box<dyn core::error::Error + Send + Sync>),
}

impl ParliaConsensusErr {
    /// Create a new internal error.
    pub fn internal<E: core::error::Error + Send + Sync + 'static>(e: E) -> Self {
        Self::Internal(Box::new(e))
    }
}

/// Parlia consensus implementation
/// TODO: parlia may need maintentain a fork chain state, and in memory TD store. 
/// the reth is hard to interact with fork logic.
pub struct ParliaConsensus<P> {
    /// The provider for reading block information
    pub provider: P,
    /// Chain spec is required to determine hardfork activation and parse attestations.
    pub chain_spec: Arc<BscChainSpec>,
    /// collect all forks headers and TDs.
    pub header_td_cache: RwLock<LruMap<BlockHash, Option<U256>, ByLength>>,
}

impl<P> ParliaConsensus<P>
where
    P: BlockNumReader + HeaderProvider<Header = Header> + Clone,
{
    pub fn new(provider: P, chain_spec: Arc<BscChainSpec>) -> Self {
        Self {
            provider,
            chain_spec,
            header_td_cache: RwLock::new(LruMap::new(ByLength::new(128))),
        }
    }

    /// Determines the head block hash according to Parlia consensus rules:
    /// 1. Follow the highest block number
    /// 2. For same height blocks, pick the one with lower hash
    pub(crate) fn canonical_head<'a>(&self, incoming: (&'a Header, Option<U256>), current: (&'a Header, Option<U256>)) -> Result<&'a Header, ParliaConsensusErr> {
        tracing::debug!(target: "parlia", "Canonical head: incoming = ({:?}, {:?}, {:?}), current = ({:?}, {:?}, {:?})", 
            incoming.0.number, incoming.0.hash_slow(), incoming.1, current.0.number, current.0.hash_slow(), current.1);
        if self.head_choice_with_fast_finality(incoming, current)? {
            Ok(incoming.0)
        } else {
            Ok(current.0)
        }
    }

    /// Implements BSC fast finality fork choice similar to geth's
    /// `ReorgNeededWithFastFinality`.
    /// ref: https://github.com/bnb-chain/bsc/blob/3f345c855ebceb14cca98dc3776718185ba2014a/core/forkchoice.go#L129
    ///
    /// Returns `Some((head, current))` if a decision could be made using fast finality. If fast
    /// finality is not applicable (pre-Plato or missing headers), returns `None` so that caller can
    /// fallback to default selection.
    pub(crate) fn head_choice_with_fast_finality(
        &self,
        incoming: (&Header, Option<U256>),
        current: (&Header, Option<U256>),
    ) -> Result<bool, ParliaConsensusErr> {
        // Get justified numbers from snapshots at parent blocks
        let (incoming_justified_num, _) = if self.chain_spec.is_plato_active_at_block(incoming.0.number) {
            self.get_justified_number_and_hash(incoming.0).unwrap_or((0, B256::ZERO))
        } else {
            (0, B256::ZERO)
        };

        let (current_justified_num, _) = if self.chain_spec.is_plato_active_at_block(current.0.number) {
            self.get_justified_number_and_hash(current.0).unwrap_or((0, B256::ZERO))
        } else {
            (0, B256::ZERO)
        };

        tracing::debug!(target: "parlia", "Head choice with fast finality: incoming_justified_num = {:?}, current_justified_num = {:?}", incoming_justified_num, current_justified_num);
        // If equal, fast finality can't decide: let caller fallback
        if incoming_justified_num == current_justified_num {
            return self.head_choice_with_td(incoming, current);
        }

        if incoming_justified_num > current_justified_num && incoming.0.number <= current.0.number {
            info!(
                target: "forkchoice",
                fromHeight = current.0.number,
                fromHash = ?current.0.hash_slow(),
                toHeight = incoming.0.number,
                toHash = ?incoming.0.hash_slow(),
                fromJustified = current_justified_num,
                toJustified = incoming_justified_num,
                "Chain find higher justifiedNumber"
            );
        }
        Ok(incoming_justified_num > current_justified_num)
    }


    /// Implements BSC fork choice similar to geth's `ReorgNeeded`.
    /// ref: https://github.com/bnb-chain/bsc/blob/3f345c855ebceb14cca98dc3776718185ba2014a/core/forkchoice.go#L76
    pub(crate) fn head_choice_with_td(
        &self,
        incoming: (&Header, Option<U256>),
        current: (&Header, Option<U256>),
    ) -> Result<bool, ParliaConsensusErr> { 
        let current_td = current.1
            .ok_or(ParliaConsensusErr::UnknownTotalDifficulty(current.0.hash_slow(), current.0.number))?;
        let incoming_td = incoming.1
            .ok_or(ParliaConsensusErr::UnknownTotalDifficulty(incoming.0.hash_slow(), incoming.0.number))?;

        let (incoming, current) = (incoming.0, current.0);
        tracing::debug!(target: "parlia", "Head choice with TD: incoming = ({:?}, {:?}, {:?}), current = ({:?}, {:?}, {:?})", 
            incoming.number, incoming.hash_slow(), incoming_td, current.number, current.hash_slow(), current_td);
        // there no need to check eth's TerminalTotalDifficulty here.
        // If the total difficulty is higher than our known, add it to the canonical chain
        match incoming_td.cmp(&current_td) {
            Ordering::Greater => Ok(true),
            Ordering::Less => Ok(false),
            Ordering::Equal => {
                // Local and external difficulty is identical.
	            // Second clause in the if statement reduces the vulnerability to selfish mining.
	            // Please refer to http://www.cs.cornell.edu/~ie53/publications/btcProcFC.pdf
                let reorg = if incoming.number < current.number {
                    true
                } else if incoming.number > current.number {
                    false
                } else {
                    // handle incoming_number == current_number case here.
                    if incoming.timestamp == current.timestamp {
                        if incoming.beneficiary == current.beneficiary {
                            incoming.hash_slow() < current.hash_slow()
                        } else {
                            // just rand select a fork.
                            // ref: https://github.com/bnb-chain/bsc/blob/3f345c855ebceb14cca98dc3776718185ba2014a/core/forkchoice.go#L118
                            rand::rng().random::<f64>() < 0.5
                        }
                    } else {
                        incoming.timestamp < current.timestamp
                    }
                };
                Ok(reorg)
            }
        }
        
    }

    pub(crate) fn get_justified_number_and_hash(
        &self,
        header: &Header,
    ) -> Option<(u64, B256)> {
        if !self.chain_spec.is_luban_active_at_block(header.number) {
            return None;
        }
        // Decode vote attestations from headers (post-Luban) to extract justified numbers.
        // Use Parlia helper that understands header layout across hardforks.
        // Fast finality must use snapshot provider; if unavailable, skip FF.
        let sp = match shared::get_snapshot_provider() {
            Some(sp) => sp,
            None => {
                warn!(target: "parlia", header_hash = ?header.hash_slow(), "Snapshot provider not set when get justified number and hash");
                return None;
            }
        };
        
        match sp.snapshot_by_hash(&header.hash_slow()) {
            Some(snap) => Some((snap.vote_data.target_number, snap.vote_data.target_hash)),
            None => {
                warn!(target: "parlia", header_hash = ?header.hash_slow(), "Missing snapshot for header when get justified number and hash");
                None
            }
        }
    }

    pub(crate) fn get_finalized_number_and_hash(
        &self,
        header: &Header,
    ) -> Option<(u64, B256)> {
        if !self.chain_spec.is_plato_active_at_block(header.number) {
            return None;
        }
        // Decode vote attestations from headers (post-Luban) to extract finalized numbers.
        // Use Parlia helper that understands header layout across hardforks.
        // Fast finality must use snapshot provider; if unavailable, skip FF.
        let sp = match shared::get_snapshot_provider() {
            Some(sp) => sp,
            None => {
                warn!(target: "parlia", header_hash = ?header.hash_slow(), "Snapshot provider not set when get finalized number and hash");
                return None;
            }
        };
        
        match sp.snapshot_by_hash(&header.hash_slow()) {
            Some(snap) => Some((snap.vote_data.source_number, snap.vote_data.source_hash)),
            None => {
                warn!(target: "parlia", header_hash = ?header.hash_slow(), "Missing snapshot for header when get finalized number and hash");
                None
            }
        }
    }
    
    pub(crate) async fn header_td_fcu(
        &self,
        engine: &BeaconConsensusEngineHandle<BscPayloadTypes>,
        incoming: &Header,
        current: &Header,
    ) -> Result<(Option<U256>, Option<U256>), ParliaConsensusErr> {
        let current_td = self.header_td(engine, current.number, current.hash_slow()).await?;
        let incoming_td = match self.header_td(engine, incoming.number, incoming.hash_slow()).await {
            Ok(td) => td,
            Err(e) => {
                tracing::debug!(target: "parlia", "Failed to get incoming header TD: {:?}, try to query parent block TD", e);
                match self.header_td(engine, incoming.number - 1, incoming.parent_hash).await? {
                    Some(td) => Some(td + incoming.difficulty),
                    None => {
                        tracing::debug!(target: "parlia", "Failed to get parent header TD, return None");
                        None
                    }
                }
                
            },
        };
        Ok((incoming_td, current_td))
    }
    pub(crate) async fn header_td(&self, engine: &BeaconConsensusEngineHandle<BscPayloadTypes>, number: BlockNumber, hash: BlockHash) -> Result<Option<U256>, ParliaConsensusErr> {
        if let Some(td) = self.header_td_cache.write().get(&hash) {
            return Ok(*td);
        }
        let td = engine.query_td(number, hash).await.map_err(ParliaConsensusErr::internal)?;
        self.header_td_cache.write().insert(hash, td);
        Ok(td)
    }
}

pub mod parlia;

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{BlockHash, BlockNumber, U256};
    use reth_chainspec::ChainInfo;
    use reth_primitives::SealedHeader;
    use reth_provider::BlockHashReader;
    use std::collections::HashMap;
    use crate::chainspec::bsc_rialto::bsc_qanet;
    use crate::consensus::parlia::vote::{VoteAttestation, VoteData};
    use crate::consensus::parlia::constants::{EXTRA_SEAL_LEN, EXTRA_VANITY_LEN};
    use crate::consensus::parlia::Snapshot;
    use crate::consensus::parlia::provider::SnapshotProvider as ParliaSnapshotProvider;
    use std::sync::Arc;

    use std::collections::HashMap as StdHashMap;
    use std::sync::RwLock;

    #[derive(Debug)]
    struct DummySnapProvider {
        snap_store: RwLock<StdHashMap<u64, Snapshot>>,
        snap_store_by_hash: RwLock<StdHashMap<BlockHash, Snapshot>>,
    }
    impl DummySnapProvider {
        fn new() -> Self {
            Self { snap_store: RwLock::new(StdHashMap::new()), snap_store_by_hash: RwLock::new(StdHashMap::new()) }
        }
    }
    impl ParliaSnapshotProvider for DummySnapProvider {
        fn snapshot_by_hash(&self, block_hash: &BlockHash) -> Option<Snapshot> {
            self.snap_store_by_hash.read().ok().and_then(|m| m.get(block_hash).cloned())
        }
        fn insert(&self, snapshot: Snapshot) {
            if let Ok(mut m) = self.snap_store.write() {
                m.insert(snapshot.block_number, snapshot.clone());
            }
            if let Ok(mut m) = self.snap_store_by_hash.write() {
                m.insert(snapshot.block_hash, snapshot.clone());
            }
        }
    }

    fn ensure_snapshot_provider() {
        if crate::shared::get_snapshot_provider().is_none() {
            let _ = crate::shared::set_snapshot_provider(Arc::new(DummySnapProvider::new()));
        }
    }

    #[derive(Clone)]
    struct MockProvider {
        headers_by_number: HashMap<BlockNumber, Header>,
        headers_by_hash: HashMap<BlockHash, Header>,
        td_by_hash: HashMap<BlockHash, U256>,
        head_number: BlockNumber,
        head_hash: BlockHash,
    }

    impl MockProvider {
        fn new() -> Self {
            let headers_by_number = HashMap::new();
            let headers_by_hash = HashMap::new();
            let td_by_hash = HashMap::new();
            Self { headers_by_number, headers_by_hash, td_by_hash, head_number: 0, head_hash: BlockHash::ZERO }
        }

        fn insert(&mut self, header: Header, td: U256) {
            self.headers_by_number.insert(header.number, header.clone());
            self.headers_by_hash.insert(header.hash_slow(), header.clone());
            self.td_by_hash.insert(header.hash_slow(), td);
            if header.number > self.head_number {
                self.head_number = header.number;
                self.head_hash = header.hash_slow();
            }
        }
    }

    impl BlockHashReader for MockProvider {
        fn block_hash(&self, number: BlockNumber) -> Result<Option<B256>, ProviderError> {
            Ok(self.headers_by_number.get(&number).map(|h| h.hash_slow()))
        }

        fn canonical_hashes_range(&self, _start: BlockNumber, _end: BlockNumber) -> Result<Vec<B256>, ProviderError> {
            Ok(vec![])
        }
    }

    impl BlockNumReader for MockProvider {
        fn chain_info(&self) -> Result<ChainInfo, ProviderError> {
            Ok(ChainInfo { best_hash: self.head_hash, best_number: self.head_number })
        }

        fn best_block_number(&self) -> Result<BlockNumber, ProviderError> {
            Ok(self.head_number)
        }

        fn last_block_number(&self) -> Result<BlockNumber, ProviderError> {
            Ok(self.head_number)
        }

        fn block_number(&self, hash: B256) -> Result<Option<BlockNumber>, ProviderError> {
            Ok(self.headers_by_hash.get(&hash).map(|h| h.number))
        }
    }

    impl HeaderProvider for MockProvider {
        type Header = Header;

        fn header(&self, block_hash: &B256) -> Result<Option<Self::Header>, ProviderError> {
            Ok(self.headers_by_hash.get(block_hash).cloned())
        }

        fn header_by_number(&self, num: u64) -> Result<Option<Self::Header>, ProviderError> {
            Ok(self.headers_by_number.get(&num).cloned())
        }

        fn header_td(&self, hash: &B256) -> Result<Option<U256>, ProviderError> {
            Ok(self.td_by_hash.get(hash).cloned())
        }

        fn header_td_by_number(&self, number: BlockNumber) -> Result<Option<U256>, ProviderError> {
            if let Some(h) = self.headers_by_number.get(&number) {
                Ok(self.td_by_hash.get(&h.hash_slow()).cloned())
            } else {
                Ok(None)
            }
        }

        fn headers_range(
            &self,
            range: impl core::ops::RangeBounds<BlockNumber>,
        ) -> Result<Vec<Self::Header>, ProviderError> {
            use std::ops::Bound::*;
            let start = match range.start_bound() { Included(&s) => s, Excluded(&s) => s + 1, Unbounded => 0 };
            let end = match range.end_bound() { Included(&e) => e, Excluded(&e) => e - 1, Unbounded => self.head_number };
            let mut out = Vec::new();
            for n in start..=end {
                if let Some(h) = self.headers_by_number.get(&n) {
                    out.push(h.clone());
                }
            }
            Ok(out)
        }

        fn sealed_header(&self, number: BlockNumber) -> Result<Option<SealedHeader<Self::Header>>, ProviderError> {
            Ok(self.headers_by_number.get(&number).cloned().map(SealedHeader::seal_slow))
        }

        fn sealed_headers_while(
            &self,
            range: impl core::ops::RangeBounds<BlockNumber>,
            mut predicate: impl FnMut(&SealedHeader<Self::Header>) -> bool,
        ) -> Result<Vec<SealedHeader<Self::Header>>, ProviderError> {
            let hs = self.headers_range(range)?;
            let mut out = Vec::new();
            for h in hs {
                let sh = SealedHeader::seal_slow(h);
                if !predicate(&sh) { break; }
                out.push(sh);
            }
            Ok(out)
        }
    }

    #[test]
    fn test_canonical_head() {
        let test_cases = [
            // ((current_number, current_td, vote_source_num, vote_target_num), (new_number, new_td, vote_source_num, vote_target_num), reorg)
            ((1, 2, 0, 0), (2, 4, 0, 0), true), // Higher block wins by TD
            ((1, 2, 0, 0), (2, 1, 0, 0), false), // Lower block stays by TD
            ((1, 2, 0, 0), (2, 2, 0, 0), false), // Same TD, select the higher number
            // ((1, 2, 0, 0), (1, 2, 0, 0), false), // Same TD, same number, random select a fork
        ];

        for ((curr_number, curr_td, curr_vote_source_num, curr_vote_target_num), (new_number, new_td, new_vote_source_num, new_vote_target_num), reorg) in test_cases {
            let mut provider = MockProvider::new();
            let curr_header = header_with_attestation(curr_number, curr_vote_source_num, curr_vote_target_num);
            provider.insert(curr_header.clone(), U256::from(curr_td));
            let new_header = header_with_attestation(new_number, new_vote_source_num, new_vote_target_num);
            provider.insert(new_header.clone(), U256::from(new_td));
            let consensus = ParliaConsensus::new(
                provider,
                Arc::new(crate::chainspec::BscChainSpec::from(crate::chainspec::bsc::bsc_mainnet())),
            );
            let canonical_head = consensus.canonical_head((&new_header, Some(U256::from(new_td))), (&curr_header, Some(U256::from(curr_td)))).unwrap();
            if reorg {
                assert_eq!(canonical_head, &new_header);
            } else {
                assert_eq!(canonical_head, &curr_header);
            }
        }
    }

    /// Helper: create a header with an embedded VoteAttestation in extra_data
    fn header_with_attestation(number: u64, source_number: u64, target_number: u64) -> alloy_consensus::Header {
        use alloy_consensus::Header as H;
        // craft a minimal attestation
        let att = VoteAttestation {
            vote_address_set: 0,
            agg_signature: Default::default(),
            data: VoteData {
                source_number,
                source_hash: B256::ZERO,
                target_number,
                target_hash: B256::ZERO,
            },
            extra: bytes::Bytes::new(),
        };
        let mut extra = vec![0u8; EXTRA_VANITY_LEN];
        extra.extend_from_slice(alloy_rlp::encode(&att).as_ref());
        extra.extend_from_slice(&[0u8; EXTRA_SEAL_LEN]);
        H { number, extra_data: alloy_primitives::Bytes::from(extra), ..Default::default() }
    }

    #[test]
    fn test_fast_finality_head_choice() {
        ensure_snapshot_provider();
        let chain_spec = Arc::new(crate::chainspec::BscChainSpec::from(bsc_qanet()));
        // Provide snapshots with equal justified numbers at head blocks
        let sp = crate::shared::get_snapshot_provider().unwrap().clone();

        let cases = [
            // ((current_number, current_td, current_vote_source_num, current_vote_target_num), (incoming_number, incoming_td, incoming_vote_source_num, incoming_vote_target_num), reorg)
            ((10, 20, 8, 9), (11, 22, 9, 10), true), // reorg to incoming (higher justified)
            ((20, 40, 18, 19), (21, 40, 18, 19), false), // no reorg (equal justified, equal TD)
            ((20, 40, 18, 19), (21, 42, 18, 19), true), // no reorg (equal justified, higher TD)
            ((30, 60, 28, 29), (31, 62, 27, 28), false), // no reorg (lower justified)
        ];
        for ((current_number, current_td, current_vote_source_num, current_vote_target_num), (incoming_number, incoming_td, incoming_vote_source_num, incoming_vote_target_num), reorg) in cases {
            let current = header_with_attestation(current_number, current_vote_source_num, current_vote_target_num);
            let incoming = header_with_attestation(incoming_number, incoming_vote_source_num, incoming_vote_target_num);

            sp.insert(Snapshot { block_hash: current.hash_slow(), block_number: current.number, vote_data: VoteData { source_number: current_vote_source_num, target_number: current_vote_target_num, ..Default::default() }, epoch_num: 200, ..Default::default() });
            sp.insert(Snapshot { block_hash: incoming.hash_slow(), block_number: incoming.number, vote_data: VoteData { source_number: incoming_vote_source_num, target_number: incoming_vote_target_num, ..Default::default() }, epoch_num: 200, ..Default::default() });

            let mut provider = MockProvider::new();
            provider.insert(current.clone(), U256::from(current_td));
            provider.insert(incoming.clone(), U256::from(incoming_td));
            let consensus = ParliaConsensus::new(
                provider,
                chain_spec.clone(),
            );

            let canonical_head = consensus.canonical_head((&incoming, Some(U256::from(incoming_td))), (&current, Some(U256::from(current_td)))).unwrap();
            if reorg {
                assert_eq!(canonical_head, &incoming);
            } else {
                assert_eq!(canonical_head, &current);
            }
        }
    }
}
