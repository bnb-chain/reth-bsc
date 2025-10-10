use alloy_consensus::constants::ETH_TO_WEI;
use alloy_primitives::{address, Address, BlockNumber, B256};
use std::sync::Arc;
use tracing::{info, warn};

use crate::{chainspec::BscChainSpec, hardforks::BscHardforks, shared};
use reth_provider::{BlockNumReader, ProviderError};
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
}

/// Parlia consensus implementation
pub struct ParliaConsensus<P> {
    /// The provider for reading block information
    pub provider: P,
    /// Chain spec is required to determine hardfork activation and parse attestations.
    pub chain_spec: Arc<BscChainSpec>,
}

impl<P> ParliaConsensus<P>
where
    P: BlockNumReader + Clone,
{
    /// Determines the head block hash according to Parlia consensus rules:
    /// 1. Follow the highest block number
    /// 2. For same height blocks, pick the one with lower hash
    pub(crate) fn canonical_head(&self, hash: B256, number: BlockNumber) -> Result<(B256, B256), ParliaConsensusErr> {
        let current_head = self.provider.best_block_number()?;
        let current_hash = self.provider.block_hash(current_head)?.ok_or(ParliaConsensusErr::HeadHashNotFound)?;

        // Try fast-finality fork choice first (Plato and later)
        if let Some((ff_head, ff_current)) = self.fast_finality_head_choice(hash, number, current_hash, current_head) {
            return Ok((ff_head, ff_current));
        }

        // Fallback to height and hash tie-breaker
        match number.cmp(&current_head) {
            Ordering::Greater => Ok((hash, current_hash)),
            Ordering::Equal => Ok((hash.min(current_hash), current_hash)),
            Ordering::Less => Ok((current_hash, current_hash)),
        }
    }

    /// Implements BSC fast finality fork choice similar to geth's
    /// `ReorgNeededWithFastFinality`.
    ///
    /// Returns `Some((head, current))` if a decision could be made using fast finality. If fast
    /// finality is not applicable (pre-Plato or missing headers), returns `None` so that caller can
    /// fallback to default selection.
    fn fast_finality_head_choice(
        &self,
        incoming_hash: B256,
        incoming_number: BlockNumber,
        current_hash: B256,
        current_number: BlockNumber,
    ) -> Option<(B256, B256)> {
        // Only applicable when Plato is active on at least one of the compared blocks
        let plato_incoming = self.chain_spec.is_plato_active_at_block(incoming_number);
        let plato_current = self.chain_spec.is_plato_active_at_block(current_number);
        if !(plato_incoming || plato_current) {
            return None;
        }

        // Fetch headers via global provider; fallback to in-process header cache if unavailable
        let incoming_header = shared::get_header_by_hash_from_provider(&incoming_hash)
            .or_else(|| {
                crate::node::evm::util::HEADER_CACHE_READER
                    .lock()
                    .ok()
                    .and_then(|mut c| c.get_header_by_hash(&incoming_hash))
            });
        let current_header = shared::get_header_by_hash_from_provider(&current_hash).or_else(|| {
            crate::node::evm::util::HEADER_CACHE_READER
                .lock()
                .ok()
                .and_then(|mut c| c.get_header_by_hash(&current_hash))
        });

        // If we can't access headers, we can't apply fast finality
        let (Some(incoming_header), Some(current_header)) = (incoming_header, current_header) else {
            return None;
        };

        // Decode vote attestations from headers (post-Luban) to extract justified numbers.
        // Use Parlia helper that understands header layout across hardforks.
        // Fast finality must use snapshot provider; if unavailable, skip FF.
        let sp = match shared::get_snapshot_provider() {
            Some(sp) => sp,
            None => {
                warn!(target: "forkchoice", "Snapshot provider not set; skipping fast-finality");
                return None;
            }
        };
        // Get justified numbers from snapshots at parent blocks
        let incoming_justified = if plato_incoming {
            match sp.snapshot(incoming_header.number) {
                Some(snap) => snap.vote_data.target_number,
                None => {
                    warn!(target: "forkchoice", "Missing snapshot for incoming parent; skipping fast-finality");
                    return None;
                }
            }
        } else {
            0
        };

        let current_justified = if plato_current {
            match sp.snapshot(current_header.number) {
                Some(snap) => snap.vote_data.target_number,
                None => {
                    warn!(target: "forkchoice", "Missing snapshot for current parent; skipping fast-finality");
                    return None;
                }
            }
        } else {
            0
        };

        // If equal, fast finality can't decide: let caller fallback
        if incoming_justified == current_justified {
            return None;
        }

        // If the incoming branch has a higher justified number, prefer it.
        if incoming_justified > current_justified {
            // Log if switching to a lower/equal height for visibility (parity with geth's log)
            if incoming_number <= current_number {
                info!(
                    target: "forkchoice",
                    fromHeight = current_number,
                    fromHash = ?current_hash,
                    toHeight = incoming_number,
                    toHash = ?incoming_hash,
                    fromJustified = current_justified,
                    toJustified = incoming_justified,
                    "Chain find higher justifiedNumber"
                );
            }
            Some((incoming_hash, current_hash))
        } else {
            Some((current_hash, current_hash))
        }
    }
}

pub mod parlia;

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::hex;
    use reth_chainspec::ChainInfo;
    use reth_provider::BlockHashReader;
    use std::collections::HashMap;
    use crate::chainspec::bsc_rialto::bsc_qanet;
    use crate::consensus::parlia::vote::{VoteAttestation, VoteData};
    use crate::consensus::parlia::constants::{EXTRA_SEAL_LEN, EXTRA_VANITY_LEN};
    use crate::consensus::parlia::Snapshot;
    use crate::consensus::parlia::provider::SnapshotProvider as ParliaSnapshotProvider;
    use std::sync::Arc;

    use once_cell::sync::Lazy;
    use std::collections::HashMap as StdHashMap;
    use std::sync::RwLock;

    static DUMMY_SNAP_STORE: Lazy<RwLock<StdHashMap<u64, Snapshot>>> =
        Lazy::new(|| RwLock::new(StdHashMap::new()));

    #[derive(Debug)]
    struct DummySnapProvider;
    impl ParliaSnapshotProvider for DummySnapProvider {
        fn snapshot(&self, block_number: u64) -> Option<Snapshot> {
            DUMMY_SNAP_STORE.read().ok().and_then(|m| m.get(&block_number).cloned())
        }
        fn insert(&self, snapshot: Snapshot) {
            if let Ok(mut m) = DUMMY_SNAP_STORE.write() {
                m.insert(snapshot.block_number, snapshot);
            }
        }
        fn get_header(&self, _block_number: u64) -> Option<alloy_consensus::Header> { None }
    }

    fn ensure_snapshot_provider() {
        if crate::shared::get_snapshot_provider().is_none() {
            let _ = crate::shared::set_snapshot_provider(Arc::new(DummySnapProvider));
        }
    }

    #[derive(Clone)]
    struct MockProvider {
        blocks: HashMap<BlockNumber, B256>,
        head_number: BlockNumber,
        head_hash: B256,
    }

    impl MockProvider {
        fn new(head_number: BlockNumber, head_hash: B256) -> Self {
            let mut blocks = HashMap::new();
            blocks.insert(head_number, head_hash);
            Self { blocks, head_number, head_hash }
        }
    }

    impl BlockHashReader for MockProvider {
        fn block_hash(&self, number: BlockNumber) -> Result<Option<B256>, ProviderError> {
            Ok(self.blocks.get(&number).copied())
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
            Ok(self.blocks.iter().find_map(|(num, h)| (*h == hash).then_some(*num)))
        }
    }

    #[test]
    fn test_canonical_head() {
        let hash1 = B256::from_slice(&hex!("1111111111111111111111111111111111111111111111111111111111111111"));
        let hash2 = B256::from_slice(&hex!("2222222222222222222222222222222222222222222222222222222222222222"));

        let test_cases = [
            ((hash1, 2, 1, hash2), hash1), // Higher block wins
            ((hash1, 1, 2, hash2), hash2), // Lower block stays
            ((hash1, 1, 1, hash2), hash1), // Same height, lower hash wins
            ((hash2, 1, 1, hash1), hash1), // Same height, lower hash stays
        ];

        for ((curr_hash, curr_num, head_num, head_hash), expected) in test_cases {
            let provider = MockProvider::new(head_num, head_hash);
            let consensus = ParliaConsensus {
                provider,
                chain_spec: Arc::new(crate::chainspec::BscChainSpec::from(crate::chainspec::bsc::bsc_mainnet())),
            };
            let (head_block_hash, current_hash) = consensus.canonical_head(curr_hash, curr_num).unwrap();
            assert_eq!(head_block_hash, expected);
            assert_eq!(current_hash, head_hash);
        }
    }

    /// Helper: create a header with an embedded VoteAttestation in extra_data
    fn header_with_attestation(number: u64, justified_number: u64) -> alloy_consensus::Header {
        use alloy_consensus::Header as H;
        // craft a minimal attestation
        let att = VoteAttestation {
            vote_address_set: 0,
            agg_signature: Default::default(),
            data: VoteData {
                source_number: justified_number,
                source_hash: B256::ZERO,
                target_number: number.saturating_sub(1),
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
    fn test_fast_finality_prefers_higher_justified_even_if_lower_height() {
        ensure_snapshot_provider();
        // Arrange chain spec with Plato/Luban early activations (qanet)
        let chain_spec = Arc::new(crate::chainspec::BscChainSpec::from(bsc_qanet()));

        // Current canonical head: number 10, justified=50
        let current = header_with_attestation(10, 50);
        let current_hash = current.hash_slow();

        // Incoming competing header: number 9, justified=60 (higher)
        let incoming = header_with_attestation(9, 60);
        let incoming_hash = incoming.hash_slow();

        // Insert into local header cache so fast-finality can read them
        {
            let mut cache = crate::node::evm::util::HEADER_CACHE_READER.lock().unwrap();
            cache.insert_header_to_cache(current.clone());
            cache.insert_header_to_cache(incoming.clone());
        }

        // Provide snapshots with justified numbers at head blocks
        let sp = crate::shared::get_snapshot_provider().unwrap().clone();
        sp.insert(Snapshot { block_number: current.number, vote_data: VoteData { target_number: 50, ..Default::default() }, epoch_num: 200, ..Default::default() });
        sp.insert(Snapshot { block_number: incoming.number, vote_data: VoteData { target_number: 60, ..Default::default() }, epoch_num: 200, ..Default::default() });

        // Mock provider returns current head number/hash
        let provider = MockProvider::new(10, current_hash);
        let consensus = ParliaConsensus { provider, chain_spec };

        // Act: ask fork choice with incoming header
        let (head, cur) = consensus.canonical_head(incoming_hash, 9).unwrap();

        // Assert: reorg to incoming (higher justified), current stays as current hash
        assert_eq!(head, incoming_hash);
        assert_eq!(cur, current_hash);
    }

    #[test]
    fn test_fast_finality_falls_back_when_equal_justified() {
        ensure_snapshot_provider();
        let chain_spec = Arc::new(crate::chainspec::BscChainSpec::from(bsc_qanet()));

        // Current canonical head: number 10, justified=50
        let current = header_with_attestation(10, 50);
        let current_hash = current.hash_slow();

        // Incoming competing header: number 9, justified=50 (equal)
        let incoming = header_with_attestation(9, 50);
        let incoming_hash = incoming.hash_slow();

        // Insert into local header cache
        {
            let mut cache = crate::node::evm::util::HEADER_CACHE_READER.lock().unwrap();
            cache.insert_header_to_cache(current.clone());
            cache.insert_header_to_cache(incoming.clone());
        }

        // Provide snapshots with equal justified numbers at head blocks
        let sp = crate::shared::get_snapshot_provider().unwrap().clone();
        sp.insert(Snapshot { block_number: current.number, vote_data: VoteData { target_number: 50, ..Default::default() }, epoch_num: 200, ..Default::default() });
        sp.insert(Snapshot { block_number: incoming.number, vote_data: VoteData { target_number: 50, ..Default::default() }, epoch_num: 200, ..Default::default() });

        let provider = MockProvider::new(10, current_hash);
        let consensus = ParliaConsensus { 
            provider, 
            chain_spec,
        };

        let (head, cur) = consensus.canonical_head(incoming_hash, 9).unwrap();

        // Fallback: since incoming height < current and justified equal, keep current
        assert_eq!(head, current_hash);
        assert_eq!(cur, current_hash);
    }

    #[test]
    fn test_fast_finality_rejects_higher_height_if_lower_justified() {
        ensure_snapshot_provider();
        let chain_spec = Arc::new(crate::chainspec::BscChainSpec::from(bsc_qanet()));

        // Current canonical head: number 10, justified=50
        let current = header_with_attestation(10, 50);
        let current_hash = current.hash_slow();

        // Incoming competing header: number 11 (higher), justified=40 (lower)
        let incoming = header_with_attestation(11, 40);
        let incoming_hash = incoming.hash_slow();

        // Insert into local header cache
        {
            let mut cache = crate::node::evm::util::HEADER_CACHE_READER.lock().unwrap();
            cache.insert_header_to_cache(current.clone());
            cache.insert_header_to_cache(incoming.clone());
        }

        // Provide snapshots with lower justified number for incoming head
        let sp = crate::shared::get_snapshot_provider().unwrap().clone();
        sp.insert(Snapshot { block_number: current.number, vote_data: VoteData { target_number: 50, ..Default::default() }, epoch_num: 200, ..Default::default() });
        sp.insert(Snapshot { block_number: incoming.number, vote_data: VoteData { target_number: 40, ..Default::default() }, epoch_num: 200, ..Default::default() });

        let provider = MockProvider::new(10, current_hash);
        let consensus = ParliaConsensus { 
            provider, 
            chain_spec,
        };

        let (head, cur) = consensus.canonical_head(incoming_hash, 11).unwrap();

        // Even though incoming height is higher, lower justified keeps current head
        assert_eq!(head, current_hash);
        assert_eq!(cur, current_hash);
    }
}
