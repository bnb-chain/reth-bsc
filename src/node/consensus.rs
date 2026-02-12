use crate::{
    chainspec::BscChainSpec,
    consensus::{
        eip4844::should_recalculate_excess_blob_gas,
        parlia::{
            provider::EnhancedDbSnapshotProvider, util::calculate_millisecond_timestamp,
            vote_pool,
            BscForkChoiceRule, HeaderForForkchoice, Parlia,
        },
        ParliaConsensusErr,
    },
    hardforks::BscHardforks,
    metrics::{BscBlockchainMetrics, BscFinalityMetrics},
    node::{engine_api::payload::BscPayloadTypes, BscNode},
    shared, BscBlock, BscBlockBody, BscPrimitives,
};
use alloy_consensus::{Header, TxReceipt};
use alloy_eips::Encodable2718;
use alloy_primitives::{Bytes, B256};
use alloy_rpc_types::engine::{ForkchoiceState, PayloadStatusEnum};
use reth::{
    api::FullNodeTypes,
    beacon_consensus::EthBeaconConsensus,
    builder::{components::ConsensusBuilder, BuilderContext},
    consensus::{Consensus, ConsensusError, FullConsensus, HeaderValidator, ReceiptRootBloom},
    consensus_common::validation::{
        validate_against_parent_4844, validate_against_parent_hash_number,
    },
    primitives::{RecoveredBlock, SealedBlock, SealedHeader},
    providers::BlockExecutionResult,
};
use reth_chainspec::EthChainSpec;
use reth_engine_primitives::ConsensusEngineHandle;
use reth_ethereum_primitives::Receipt;
use reth_payload_primitives::EngineApiMessageVersion;
use reth_primitives::{gas_spent_by_transactions, GotExpected};
use reth_primitives_traits::constants::{GAS_LIMIT_BOUND_DIVISOR, MINIMUM_GAS_LIMIT};
use reth_provider::{BlockNumReader, HeaderProvider};
use std::sync::Arc;

/// A basic Bsc consensus builder.
#[derive(Debug, Default, Clone, Copy)]
#[non_exhaustive]
pub struct BscConsensusBuilder;

impl<Node> ConsensusBuilder<Node> for BscConsensusBuilder
where
    Node: FullNodeTypes<Types = BscNode>,
{
    type Consensus = Arc<dyn FullConsensus<BscPrimitives>>;

    /// return a parlia consensus instance, automatically called by the ComponentsBuilder framework.
    async fn build_consensus(self, ctx: &BuilderContext<Node>) -> eyre::Result<Self::Consensus> {
        let snapshot_provider = create_snapshot_provider(ctx).unwrap_or_else(|e| {
            panic!("Failed to initialize snapshot provider, due to {e}");
        });

        crate::shared::set_snapshot_provider(
            snapshot_provider as Arc<dyn crate::consensus::parlia::SnapshotProvider + Send + Sync>,
        )
        .unwrap_or_else(|_| panic!("Failed to set global snapshot provider"));

        crate::shared::set_header_provider(Arc::new(ctx.provider().clone()))
            .unwrap_or_else(|e| panic!("Failed to set global header provider: {e}"));

        Ok(Arc::new(BscConsensus::new(ctx.chain_spec())))
    }
}

/// BSC consensus implementation.
///
/// Provides basic checks as outlined in the execution specs.
#[derive(Debug, Clone)]
pub struct BscConsensus<ChainSpec> {
    base: EthBeaconConsensus<ChainSpec>,
    parlia: Arc<Parlia<ChainSpec>>,
    chain_spec: Arc<ChainSpec>,
}

const GAS_LIMIT_CAPACITY: u64 = 0x7fff_ffff_ffff_ffff;
const GAS_LIMIT_BOUND_DIVISOR_BEFORE_LORENTZ: u64 = 256;

fn validate_bsc_gas_limit_against_parent<ChainSpec: BscHardforks>(
    header: &Header,
    parent: &Header,
    chain_spec: &ChainSpec,
) -> Result<(), ConsensusError> {
    // Keep parity with go-bsc's Parlia checks.
    if header.gas_limit > GAS_LIMIT_CAPACITY {
        return Err(ConsensusError::Other(format!(
            "invalid gasLimit: have {}, max {}",
            header.gas_limit, GAS_LIMIT_CAPACITY
        )));
    }

    if header.gas_used > header.gas_limit {
        return Err(ConsensusError::HeaderGasUsedExceedsGasLimit {
            gas_used: header.gas_used,
            gas_limit: header.gas_limit,
        });
    }

    let diff = parent.gas_limit.abs_diff(header.gas_limit);
    let bound_divisor = if chain_spec.is_lorentz_active_at_timestamp(header.number, header.timestamp)
    {
        GAS_LIMIT_BOUND_DIVISOR
    } else {
        GAS_LIMIT_BOUND_DIVISOR_BEFORE_LORENTZ
    };
    let limit = parent.gas_limit / bound_divisor;

    if diff >= limit || header.gas_limit < MINIMUM_GAS_LIMIT {
        return Err(ConsensusError::Other(format!(
            "invalid gas limit: have {}, want {} += {}",
            header.gas_limit,
            parent.gas_limit,
            limit.saturating_sub(1)
        )));
    }

    Ok(())
}

impl<ChainSpec: EthChainSpec + BscHardforks + 'static> BscConsensus<ChainSpec> {
    pub fn new(chain_spec: Arc<ChainSpec>) -> Self {
        Self {
            base: EthBeaconConsensus::new(chain_spec.clone()),
            parlia: Arc::new(Parlia::new(chain_spec.clone(), 200)),
            chain_spec,
        }
    }
}

/// header stage validation.
impl<ChainSpec: EthChainSpec + BscHardforks + 'static> HeaderValidator<Header>
    for BscConsensus<ChainSpec>
{
    fn validate_header(&self, header: &SealedHeader) -> Result<(), ConsensusError> {
        // tracing::debug!("Validating header, block_number: {:?}", header.number);
        if let Err(err) = self.parlia.validate_header(header) {
            tracing::warn!(
                "Failed to validate_header, block_number: {}, err: {:?}",
                header.number,
                err
            );
            return Err(err);
        }
        Ok(())
    }

    fn validate_header_against_parent(
        &self,
        header: &SealedHeader,
        parent: &SealedHeader,
    ) -> Result<(), ConsensusError> {
        // tracing::debug!("Validating header against parent, block_number: {:?}", header.number);
        if let Err(err) = validate_against_parent_hash_number(header.header(), parent) {
            tracing::warn!(
                "Failed to validate_against_parent_hash_number, block_number: {}, err: {:?}",
                header.number,
                err
            );
            return Err(err);
        }

        let header_ts = calculate_millisecond_timestamp(header.header());
        let parent_ts = calculate_millisecond_timestamp(parent.header());
        if header_ts <= parent_ts {
            tracing::warn!("Failed to check timestamp, block_number: {}", header.number);
            return Err(ConsensusError::TimestampIsInPast {
                parent_timestamp: parent_ts,
                timestamp: header_ts,
            });
        }

        validate_bsc_gas_limit_against_parent(header.header(), parent.header(), &*self.chain_spec)?;

        // ensure that the blob gas fields for this block
        if BscHardforks::is_cancun_active_at_timestamp(
            &*self.chain_spec,
            header.header().number,
            header.header().timestamp,
        ) {
            if let Some(blob_params) = self.chain_spec.blob_params_at_timestamp(header.timestamp) {
                if self.chain_spec.is_mendel_active_at_timestamp(header.number, header.timestamp)
                    && !should_recalculate_excess_blob_gas(
                        &*self.chain_spec,
                        header.number,
                        header.timestamp,
                    )
                {
                    let expected = parent.header().excess_blob_gas.unwrap_or(0);
                    let got = header
                        .header()
                        .excess_blob_gas
                        .ok_or(ConsensusError::ExcessBlobGasMissing)?;
                    if got != expected {
                        return Err(ConsensusError::ExcessBlobGasDiff {
                            diff: GotExpected { got, expected },
                            parent_excess_blob_gas: expected,
                            parent_blob_gas_used: parent.header().blob_gas_used.unwrap_or(0),
                        });
                    }
                } else if let Err(err) =
                    validate_against_parent_4844(header.header(), parent.header(), blob_params)
                {
                    tracing::warn!(
                        "Failed to validate_against_parent_4844, block_number: {}, err: {:?}",
                        header.number,
                        err
                    );
                    return Err(err);
                }
            }
        }

        Ok(())
    }
}

impl<ChainSpec: EthChainSpec<Header = Header> + BscHardforks + 'static> Consensus<BscBlock>
    for BscConsensus<ChainSpec>
{
    /// live-sync validation.
    fn validate_body_against_header(
        &self,
        body: &BscBlockBody,
        header: &SealedHeader,
    ) -> Result<(), ConsensusError> {
        // tracing::debug!("Validating body against header, block_number: {:?}", header.number);
        Consensus::<BscBlock>::validate_body_against_header(&self.base, body, header)
    }

    /// body stage validation.
    fn validate_block_pre_execution(
        &self,
        block: &SealedBlock<BscBlock>,
    ) -> Result<(), ConsensusError> {
        // tracing::debug!("Validating block pre-execution, block_number: {:?}", block.header().number);
        self.parlia.validate_block_pre_execution(block)?;
        Ok(())
    }
}

impl<ChainSpec: EthChainSpec<Header = Header> + BscHardforks + 'static> FullConsensus<BscPrimitives>
    for BscConsensus<ChainSpec>
{
    /// execution stage validation.
    fn validate_block_post_execution(
        &self,
        block: &RecoveredBlock<BscBlock>,
        result: &BlockExecutionResult<Receipt>,
        _receipt_root_bloom: Option<ReceiptRootBloom>,
    ) -> Result<(), ConsensusError> {
        let receipts = &result.receipts;
        let requests = &result.requests;
        let chain_spec = &self.chain_spec;

        // Check if gas used matches the value set in header.
        let cumulative_gas_used =
            receipts.last().map(|receipt| receipt.cumulative_gas_used).unwrap_or(0);
        if block.header().gas_used != cumulative_gas_used {
            return Err(ConsensusError::BlockGasUsed {
                gas: GotExpected { got: cumulative_gas_used, expected: block.header().gas_used },
                gas_spent_by_tx: gas_spent_by_transactions(receipts),
            });
        }

        // Before Byzantium, receipts contained state root that would mean that expensive
        // operation as hashing that is required for state root got calculated in every
        // transaction This was replaced with is_success flag.
        // See more about EIP here: https://eips.ethereum.org/EIPS/eip-658
        if chain_spec.is_byzantium_active_at_block(block.header().number) {
            if let Err(error) =
                verify_receipts(block.header().receipts_root, block.header().logs_bloom, receipts)
            {
                let receipts = receipts
                    .iter()
                    .map(|r| Bytes::from(r.with_bloom_ref().encoded_2718()))
                    .collect::<Vec<_>>();
                tracing::debug!(%error, ?receipts, "receipts verification failed");
                return Err(error);
            }
        }

        // Validate that the header requests hash matches the calculated requests hash
        if chain_spec.is_prague_active_at_block_and_timestamp(
            block.header().number,
            block.header().timestamp,
        ) {
            let Some(header_requests_hash) = block.header().requests_hash else {
                return Err(ConsensusError::RequestsHashMissing);
            };
            let requests_hash = requests.requests_hash();
            if requests_hash != header_requests_hash {
                return Err(ConsensusError::BodyRequestsHashDiff(
                    GotExpected::new(requests_hash, header_requests_hash).into(),
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::consensus::parlia::{
        provider::SnapshotProvider, vote_pool, Snapshot, VoteAddress, VoteData, VoteEnvelope,
        VoteSignature,
    };
    use crate::hardforks::bsc::BscHardfork;
    use alloy_consensus::Header;
    use alloy_primitives::{Address, B256};
    use reth_chainspec::{ChainInfo, ChainSpecBuilder, ForkCondition};
    use reth_engine_primitives::BeaconEngineMessage;
    use reth_provider::{BlockHashReader, BlockNumReader, HeaderProvider};
    use std::collections::HashMap;
    use std::sync::{Arc, RwLock};
    use tokio::sync::mpsc;

    #[derive(Clone, Default)]
    struct TestProvider;

    impl BlockHashReader for TestProvider {
        fn block_hash(&self, _number: u64) -> reth_provider::ProviderResult<Option<B256>> {
            Ok(None)
        }

        fn canonical_hashes_range(
            &self,
            _start: u64,
            _end: u64,
        ) -> reth_provider::ProviderResult<Vec<B256>> {
            Ok(Vec::new())
        }
    }

    impl BlockNumReader for TestProvider {
        fn chain_info(&self) -> reth_provider::ProviderResult<ChainInfo> {
            Ok(ChainInfo::default())
        }

        fn best_block_number(&self) -> reth_provider::ProviderResult<u64> {
            Ok(0)
        }

        fn last_block_number(&self) -> reth_provider::ProviderResult<u64> {
            Ok(0)
        }

        fn block_number(&self, _hash: B256) -> reth_provider::ProviderResult<Option<u64>> {
            Ok(None)
        }
    }

    impl HeaderProvider for TestProvider {
        type Header = Header;

        fn header(&self, _block_hash: B256) -> reth_provider::ProviderResult<Option<Self::Header>> {
            Ok(None)
        }

        fn header_by_number(&self, _num: u64) -> reth_provider::ProviderResult<Option<Self::Header>> {
            Ok(None)
        }

        fn headers_range(
            &self,
            _range: impl core::ops::RangeBounds<u64>,
        ) -> reth_provider::ProviderResult<Vec<Self::Header>> {
            Ok(Vec::new())
        }

        fn sealed_header(
            &self,
            _number: u64,
        ) -> reth_provider::ProviderResult<Option<reth_primitives::SealedHeader<Self::Header>>> {
            Ok(None)
        }

        fn sealed_headers_while(
            &self,
            _range: impl core::ops::RangeBounds<u64>,
            _predicate: impl FnMut(&reth_primitives::SealedHeader<Self::Header>) -> bool,
        ) -> reth_provider::ProviderResult<Vec<reth_primitives::SealedHeader<Self::Header>>> {
            Ok(Vec::new())
        }
    }

    #[derive(Default)]
    struct TestSnapshotProvider {
        snaps: RwLock<HashMap<B256, Snapshot>>,
    }

    impl SnapshotProvider for TestSnapshotProvider {
        fn snapshot_by_hash(&self, block_hash: &B256) -> Option<Snapshot> {
            self.snaps.read().ok().and_then(|m| m.get(block_hash).cloned())
        }

        fn insert(&self, snapshot: Snapshot) {
            if let Ok(mut m) = self.snaps.write() {
                m.insert(snapshot.block_hash, snapshot);
            }
        }
    }

    fn test_chain_spec_with_lorentz(fork_condition: ForkCondition) -> BscChainSpec {
        BscChainSpec::from(
            ChainSpecBuilder::mainnet()
                .with_fork(BscHardfork::Lorentz, fork_condition)
                .build(),
        )
    }

    const LONDON_ACTIVE_BLOCK: u64 = 20_000_000;

    #[test]
    fn gas_limit_validation_enforces_capacity_and_gas_used_bounds() {
        let chain_spec = test_chain_spec_with_lorentz(ForkCondition::Timestamp(u64::MAX));
        let parent = Header {
            number: LONDON_ACTIVE_BLOCK,
            timestamp: 1,
            gas_limit: 30_000_000,
            ..Default::default()
        };

        let over_capacity = Header {
            number: LONDON_ACTIVE_BLOCK + 1,
            timestamp: 2,
            gas_limit: GAS_LIMIT_CAPACITY + 1,
            gas_used: 0,
            ..Default::default()
        };
        assert!(validate_bsc_gas_limit_against_parent(&over_capacity, &parent, &chain_spec).is_err());

        let gas_used_over_limit = Header {
            number: LONDON_ACTIVE_BLOCK + 1,
            timestamp: 2,
            gas_limit: 30_000_000,
            gas_used: 30_000_001,
            ..Default::default()
        };
        assert!(matches!(
            validate_bsc_gas_limit_against_parent(&gas_used_over_limit, &parent, &chain_spec),
            Err(ConsensusError::HeaderGasUsedExceedsGasLimit { .. })
        ));
    }

    #[test]
    fn gas_limit_validation_uses_pre_lorentz_divisor_256() {
        let chain_spec = test_chain_spec_with_lorentz(ForkCondition::Timestamp(u64::MAX));
        let parent = Header {
            number: LONDON_ACTIVE_BLOCK,
            timestamp: 1,
            gas_limit: 30_000_000,
            ..Default::default()
        };
        let pre_lorentz_limit = parent.gas_limit / 256;

        let invalid = Header {
            number: LONDON_ACTIVE_BLOCK + 1,
            timestamp: 2,
            gas_limit: parent.gas_limit + pre_lorentz_limit,
            gas_used: 0,
            ..Default::default()
        };
        assert!(validate_bsc_gas_limit_against_parent(&invalid, &parent, &chain_spec).is_err());

        let valid = Header {
            number: LONDON_ACTIVE_BLOCK + 1,
            timestamp: 2,
            gas_limit: parent.gas_limit + pre_lorentz_limit - 1,
            gas_used: 0,
            ..Default::default()
        };
        assert!(validate_bsc_gas_limit_against_parent(&valid, &parent, &chain_spec).is_ok());
    }

    #[test]
    fn gas_limit_validation_uses_lorentz_divisor_1024() {
        let chain_spec = test_chain_spec_with_lorentz(ForkCondition::Timestamp(0));
        let parent = Header {
            number: LONDON_ACTIVE_BLOCK,
            timestamp: 1,
            gas_limit: 30_000_000,
            ..Default::default()
        };
        let lorentz_limit = parent.gas_limit / GAS_LIMIT_BOUND_DIVISOR;

        let invalid = Header {
            number: LONDON_ACTIVE_BLOCK + 1,
            timestamp: 2,
            gas_limit: parent.gas_limit + lorentz_limit,
            gas_used: 0,
            ..Default::default()
        };
        assert!(validate_bsc_gas_limit_against_parent(&invalid, &parent, &chain_spec).is_err());

        let valid = Header {
            number: LONDON_ACTIVE_BLOCK + 1,
            timestamp: 2,
            gas_limit: parent.gas_limit + lorentz_limit - 1,
            gas_used: 0,
            ..Default::default()
        };
        assert!(validate_bsc_gas_limit_against_parent(&valid, &parent, &chain_spec).is_ok());
    }

    #[test]
    fn gas_limit_validation_enforces_minimum_limit() {
        let chain_spec = test_chain_spec_with_lorentz(ForkCondition::Timestamp(0));
        let parent = Header {
            number: LONDON_ACTIVE_BLOCK,
            timestamp: 1,
            gas_limit: 30_000_000,
            ..Default::default()
        };
        let invalid = Header {
            number: LONDON_ACTIVE_BLOCK + 1,
            timestamp: 2,
            gas_limit: MINIMUM_GAS_LIMIT - 1,
            gas_used: 0,
            ..Default::default()
        };
        assert!(validate_bsc_gas_limit_against_parent(&invalid, &parent, &chain_spec).is_err());
    }

    #[test]
    fn finalized_uses_vote_pool_quorum_for_head_child() {
        vote_pool::drain();

        let chain_spec = Arc::new(BscChainSpec::from(
            ChainSpecBuilder::mainnet()
                .with_fork(BscHardfork::Plato, ForkCondition::Block(0))
                .build(),
        ));

        let (tx, _rx) =
            mpsc::unbounded_channel::<BeaconEngineMessage<BscPayloadTypes>>();
        let engine = BscForkChoiceEngine::new(
            TestProvider,
            ConsensusEngineHandle::new(tx),
            chain_spec,
        );

        let parent = Header { number: 9, timestamp: 1, ..Default::default() };
        let parent_hash = parent.hash_slow();

        let head = Header {
            number: 10,
            timestamp: 2,
            parent_hash,
            ..Default::default()
        };
        let head_hash = head.hash_slow();

        let fallback_hash = B256::from([7u8; 32]);
        let validators = vec![
            Address::from([1u8; 20]),
            Address::from([2u8; 20]),
            Address::from([3u8; 20]),
        ];
        let mut snap = Snapshot::new(validators, head.number, head_hash, 200, None);
        snap.vote_data = VoteData {
            source_number: 7,
            source_hash: fallback_hash,
            target_number: 9,
            target_hash: parent_hash,
        };

        let provider: Arc<dyn SnapshotProvider + Send + Sync> =
            if let Some(existing) = crate::shared::get_snapshot_provider() {
                existing.clone()
            } else {
                let p: Arc<dyn SnapshotProvider + Send + Sync> =
                    Arc::new(TestSnapshotProvider::default());
                let _ = crate::shared::set_snapshot_provider(p.clone());
                p
            };
        provider.insert(snap);

        let baseline = engine.get_finalized_number_and_hash(&head).unwrap();
        assert_eq!(baseline, (7, fallback_hash));

        let vote_data = VoteData {
            source_number: 9,
            source_hash: parent_hash,
            target_number: 10,
            target_hash: head_hash,
        };
        let vote1 = VoteEnvelope {
            vote_address: VoteAddress::from([1u8; 48]),
            signature: VoteSignature::default(),
            data: vote_data,
        };
        let vote2 = VoteEnvelope {
            vote_address: VoteAddress::from([2u8; 48]),
            signature: VoteSignature::default(),
            data: vote_data,
        };

        vote_pool::put_vote(vote1);
        vote_pool::put_vote(vote2);

        let finalized = engine.get_finalized_number_and_hash(&head).unwrap();
        assert_eq!(finalized, (9, parent_hash));

        vote_pool::drain();
    }

    #[test]
    fn finalized_requires_quorum_not_reached_with_one_vote() {
        // With 3 validators, quorum is ceil(3 * 2 / 3) = 2
        // Only 1 vote should NOT advance finality
        vote_pool::drain();

        let chain_spec = Arc::new(BscChainSpec::from(
            ChainSpecBuilder::mainnet()
                .with_fork(BscHardfork::Plato, ForkCondition::Block(0))
                .build(),
        ));

        let (tx, _rx) = mpsc::unbounded_channel::<BeaconEngineMessage<BscPayloadTypes>>();
        let engine = BscForkChoiceEngine::new(
            TestProvider,
            ConsensusEngineHandle::new(tx),
            chain_spec,
        );

        let parent = Header { number: 9, timestamp: 1, ..Default::default() };
        let parent_hash = parent.hash_slow();

        let head = Header {
            number: 10,
            timestamp: 2,
            parent_hash,
            ..Default::default()
        };
        let head_hash = head.hash_slow();

        let fallback_hash = B256::from([7u8; 32]);
        let validators = vec![
            Address::from([1u8; 20]),
            Address::from([2u8; 20]),
            Address::from([3u8; 20]),
        ];
        let mut snap = Snapshot::new(validators, head.number, head_hash, 200, None);
        snap.vote_data = VoteData {
            source_number: 7,
            source_hash: fallback_hash,
            target_number: 9,
            target_hash: parent_hash,
        };

        let provider: Arc<dyn SnapshotProvider + Send + Sync> =
            if let Some(existing) = crate::shared::get_snapshot_provider() {
                existing.clone()
            } else {
                let p: Arc<dyn SnapshotProvider + Send + Sync> =
                    Arc::new(TestSnapshotProvider::default());
                let _ = crate::shared::set_snapshot_provider(p.clone());
                p
            };
        provider.insert(snap);

        // Add only ONE vote (quorum requires 2)
        let vote_data = VoteData {
            source_number: 9,
            source_hash: parent_hash,
            target_number: 10,
            target_hash: head_hash,
        };
        let vote1 = VoteEnvelope {
            vote_address: VoteAddress::from([1u8; 48]),
            signature: VoteSignature::default(),
            data: vote_data,
        };
        vote_pool::put_vote(vote1);

        // Finality should NOT advance - still returns fallback
        let finalized = engine.get_finalized_number_and_hash(&head).unwrap();
        assert_eq!(finalized, (7, fallback_hash), "1 vote should not reach quorum of 2");

        vote_pool::drain();
    }

    #[test]
    fn finalized_ignores_votes_with_wrong_source_number() {
        vote_pool::drain();

        let chain_spec = Arc::new(BscChainSpec::from(
            ChainSpecBuilder::mainnet()
                .with_fork(BscHardfork::Plato, ForkCondition::Block(0))
                .build(),
        ));

        let (tx, _rx) = mpsc::unbounded_channel::<BeaconEngineMessage<BscPayloadTypes>>();
        let engine = BscForkChoiceEngine::new(
            TestProvider,
            ConsensusEngineHandle::new(tx),
            chain_spec,
        );

        let parent = Header { number: 9, timestamp: 1, ..Default::default() };
        let parent_hash = parent.hash_slow();

        let head = Header {
            number: 10,
            timestamp: 2,
            parent_hash,
            ..Default::default()
        };
        let head_hash = head.hash_slow();

        let fallback_hash = B256::from([7u8; 32]);
        let validators = vec![
            Address::from([1u8; 20]),
            Address::from([2u8; 20]),
            Address::from([3u8; 20]),
        ];
        let mut snap = Snapshot::new(validators, head.number, head_hash, 200, None);
        snap.vote_data = VoteData {
            source_number: 7,
            source_hash: fallback_hash,
            target_number: 9,
            target_hash: parent_hash,
        };

        let provider: Arc<dyn SnapshotProvider + Send + Sync> =
            if let Some(existing) = crate::shared::get_snapshot_provider() {
                existing.clone()
            } else {
                let p: Arc<dyn SnapshotProvider + Send + Sync> =
                    Arc::new(TestSnapshotProvider::default());
                let _ = crate::shared::set_snapshot_provider(p.clone());
                p
            };
        provider.insert(snap);

        // Add votes with WRONG source_number (8 instead of 9)
        let wrong_vote_data = VoteData {
            source_number: 8, // Wrong! Should be 9 (current_justified_number)
            source_hash: parent_hash,
            target_number: 10,
            target_hash: head_hash,
        };
        let vote1 = VoteEnvelope {
            vote_address: VoteAddress::from([1u8; 48]),
            signature: VoteSignature::default(),
            data: wrong_vote_data,
        };
        let vote2 = VoteEnvelope {
            vote_address: VoteAddress::from([2u8; 48]),
            signature: VoteSignature::default(),
            data: wrong_vote_data,
        };
        vote_pool::put_vote(vote1);
        vote_pool::put_vote(vote2);

        // Finality should NOT advance - votes have wrong source
        let finalized = engine.get_finalized_number_and_hash(&head).unwrap();
        assert_eq!(finalized, (7, fallback_hash), "votes with wrong source should be ignored");

        vote_pool::drain();
    }

    #[test]
    fn finalized_ignores_votes_with_wrong_target_number() {
        vote_pool::drain();

        let chain_spec = Arc::new(BscChainSpec::from(
            ChainSpecBuilder::mainnet()
                .with_fork(BscHardfork::Plato, ForkCondition::Block(0))
                .build(),
        ));

        let (tx, _rx) = mpsc::unbounded_channel::<BeaconEngineMessage<BscPayloadTypes>>();
        let engine = BscForkChoiceEngine::new(
            TestProvider,
            ConsensusEngineHandle::new(tx),
            chain_spec,
        );

        let parent = Header { number: 9, timestamp: 1, ..Default::default() };
        let parent_hash = parent.hash_slow();

        let head = Header {
            number: 10,
            timestamp: 2,
            parent_hash,
            ..Default::default()
        };
        let head_hash = head.hash_slow();

        let fallback_hash = B256::from([7u8; 32]);
        let validators = vec![
            Address::from([1u8; 20]),
            Address::from([2u8; 20]),
            Address::from([3u8; 20]),
        ];
        let mut snap = Snapshot::new(validators, head.number, head_hash, 200, None);
        snap.vote_data = VoteData {
            source_number: 7,
            source_hash: fallback_hash,
            target_number: 9,
            target_hash: parent_hash,
        };

        let provider: Arc<dyn SnapshotProvider + Send + Sync> =
            if let Some(existing) = crate::shared::get_snapshot_provider() {
                existing.clone()
            } else {
                let p: Arc<dyn SnapshotProvider + Send + Sync> =
                    Arc::new(TestSnapshotProvider::default());
                let _ = crate::shared::set_snapshot_provider(p.clone());
                p
            };
        provider.insert(snap);

        // Add votes with WRONG target_number (11 instead of 10)
        let wrong_vote_data = VoteData {
            source_number: 9,
            source_hash: parent_hash,
            target_number: 11, // Wrong! Should be 10 (head.number)
            target_hash: head_hash,
        };
        let vote1 = VoteEnvelope {
            vote_address: VoteAddress::from([1u8; 48]),
            signature: VoteSignature::default(),
            data: wrong_vote_data,
        };
        let vote2 = VoteEnvelope {
            vote_address: VoteAddress::from([2u8; 48]),
            signature: VoteSignature::default(),
            data: wrong_vote_data,
        };
        vote_pool::put_vote(vote1);
        vote_pool::put_vote(vote2);

        // Finality should NOT advance - votes have wrong target
        let finalized = engine.get_finalized_number_and_hash(&head).unwrap();
        assert_eq!(finalized, (7, fallback_hash), "votes with wrong target should be ignored");

        vote_pool::drain();
    }

    #[test]
    fn finalized_no_advance_when_head_not_direct_child_of_justified() {
        // BEP-648 requires head.number - 1 == current_justified_number
        // If there's a gap, finality should not advance via vote pool
        vote_pool::drain();

        let chain_spec = Arc::new(BscChainSpec::from(
            ChainSpecBuilder::mainnet()
                .with_fork(BscHardfork::Plato, ForkCondition::Block(0))
                .build(),
        ));

        let (tx, _rx) = mpsc::unbounded_channel::<BeaconEngineMessage<BscPayloadTypes>>();
        let engine = BscForkChoiceEngine::new(
            TestProvider,
            ConsensusEngineHandle::new(tx),
            chain_spec,
        );

        // Head is at 12
        let head = Header { number: 12, timestamp: 2, ..Default::default() };
        let head_hash = head.hash_slow();

        let fallback_hash = B256::from([7u8; 32]);
        let validators = vec![
            Address::from([1u8; 20]),
            Address::from([2u8; 20]),
            Address::from([3u8; 20]),
        ];
        let mut snap = Snapshot::new(validators, head.number, head_hash, 200, None);
        snap.vote_data = VoteData {
            source_number: 7,
            source_hash: fallback_hash,
            target_number: 9, // Justified is at 9, but head is at 12 (gap of 2)
            target_hash: B256::from([9u8; 32]),
        };

        let provider: Arc<dyn SnapshotProvider + Send + Sync> =
            if let Some(existing) = crate::shared::get_snapshot_provider() {
                existing.clone()
            } else {
                let p: Arc<dyn SnapshotProvider + Send + Sync> =
                    Arc::new(TestSnapshotProvider::default());
                let _ = crate::shared::set_snapshot_provider(p.clone());
                p
            };
        provider.insert(snap);

        // Add valid votes for head
        let vote_data = VoteData {
            source_number: 9,
            source_hash: B256::from([9u8; 32]),
            target_number: 12,
            target_hash: head_hash,
        };
        let vote1 = VoteEnvelope {
            vote_address: VoteAddress::from([1u8; 48]),
            signature: VoteSignature::default(),
            data: vote_data,
        };
        let vote2 = VoteEnvelope {
            vote_address: VoteAddress::from([2u8; 48]),
            signature: VoteSignature::default(),
            data: vote_data,
        };
        vote_pool::put_vote(vote1);
        vote_pool::put_vote(vote2);

        // Finality should NOT advance because head.number - 1 (11) != current_justified_number (9)
        let finalized = engine.get_finalized_number_and_hash(&head).unwrap();
        assert_eq!(finalized, (7, fallback_hash), "should not advance when head is not direct child");

        vote_pool::drain();
    }

    #[test]
    fn finalized_with_larger_validator_set() {
        // Test with 21 validators (typical BSC validator set)
        // Quorum = ceil(21 * 2 / 3) = 14
        vote_pool::drain();

        let chain_spec = Arc::new(BscChainSpec::from(
            ChainSpecBuilder::mainnet()
                .with_fork(BscHardfork::Plato, ForkCondition::Block(0))
                .build(),
        ));

        let (tx, _rx) = mpsc::unbounded_channel::<BeaconEngineMessage<BscPayloadTypes>>();
        let engine = BscForkChoiceEngine::new(
            TestProvider,
            ConsensusEngineHandle::new(tx),
            chain_spec,
        );

        let parent = Header { number: 99, timestamp: 1, ..Default::default() };
        let parent_hash = parent.hash_slow();

        let head = Header {
            number: 100,
            timestamp: 2,
            parent_hash,
            ..Default::default()
        };
        let head_hash = head.hash_slow();

        let fallback_hash = B256::from([7u8; 32]);

        // 21 validators
        let validators: Vec<Address> = (1..=21)
            .map(|i| Address::from([i as u8; 20]))
            .collect();

        let mut snap = Snapshot::new(validators, head.number, head_hash, 200, None);
        snap.vote_data = VoteData {
            source_number: 97,
            source_hash: fallback_hash,
            target_number: 99,
            target_hash: parent_hash,
        };

        let provider: Arc<dyn SnapshotProvider + Send + Sync> =
            if let Some(existing) = crate::shared::get_snapshot_provider() {
                existing.clone()
            } else {
                let p: Arc<dyn SnapshotProvider + Send + Sync> =
                    Arc::new(TestSnapshotProvider::default());
                let _ = crate::shared::set_snapshot_provider(p.clone());
                p
            };
        provider.insert(snap);

        let vote_data = VoteData {
            source_number: 99,
            source_hash: parent_hash,
            target_number: 100,
            target_hash: head_hash,
        };

        // Add 13 votes (not enough, quorum is 14)
        for i in 1..=13 {
            let vote = VoteEnvelope {
                vote_address: VoteAddress::from([i as u8; 48]),
                signature: VoteSignature::default(),
                data: vote_data,
            };
            vote_pool::put_vote(vote);
        }

        let finalized = engine.get_finalized_number_and_hash(&head).unwrap();
        assert_eq!(finalized, (97, fallback_hash), "13 votes should not reach quorum of 14");

        // Add 14th vote to reach quorum
        let vote14 = VoteEnvelope {
            vote_address: VoteAddress::from([14u8; 48]),
            signature: VoteSignature::default(),
            data: vote_data,
        };
        vote_pool::put_vote(vote14);

        let finalized = engine.get_finalized_number_and_hash(&head).unwrap();
        assert_eq!(finalized, (99, parent_hash), "14 votes should reach quorum");

        vote_pool::drain();
    }
}

/// Calculate the receipts root, and compare it against the expected receipts root and logs bloom.
/// This is a direct copy of reth's implementation from:
/// https://github.com/paradigmxyz/reth/blob/616e492c79bb4143071ac6bf0831a249a504359f/crates/ethereum/consensus/src/validation.rs#L71
fn verify_receipts<R: reth_primitives_traits::Receipt>(
    expected_receipts_root: B256,
    expected_logs_bloom: alloy_primitives::Bloom,
    receipts: &[R],
) -> Result<(), reth::consensus::ConsensusError> {
    // Calculate receipts root.
    let receipts_with_bloom = receipts.iter().map(TxReceipt::with_bloom_ref).collect::<Vec<_>>();
    let receipts_root = alloy_consensus::proofs::calculate_receipt_root(&receipts_with_bloom);

    // Calculate header logs bloom.
    let logs_bloom = receipts_with_bloom
        .iter()
        .fold(alloy_primitives::Bloom::ZERO, |bloom, r| bloom | r.bloom_ref());

    compare_receipts_root_and_logs_bloom(
        receipts_root,
        logs_bloom,
        expected_receipts_root,
        expected_logs_bloom,
    )?;

    Ok(())
}

/// Compare the calculated receipts root with the expected receipts root, also compare
/// the calculated logs bloom with the expected logs bloom.
/// This is a direct copy of reth's implementation.
fn compare_receipts_root_and_logs_bloom(
    calculated_receipts_root: B256,
    calculated_logs_bloom: alloy_primitives::Bloom,
    expected_receipts_root: B256,
    expected_logs_bloom: alloy_primitives::Bloom,
) -> Result<(), reth::consensus::ConsensusError> {
    if calculated_receipts_root != expected_receipts_root {
        return Err(reth::consensus::ConsensusError::BodyReceiptRootDiff(
            GotExpected { got: calculated_receipts_root, expected: expected_receipts_root }.into(),
        ));
    }

    if calculated_logs_bloom != expected_logs_bloom {
        return Err(reth::consensus::ConsensusError::BodyBloomLogDiff(
            GotExpected { got: calculated_logs_bloom, expected: expected_logs_bloom }.into(),
        ));
    }

    Ok(())
}

fn create_snapshot_provider<Node>(
    ctx: &BuilderContext<Node>,
) -> eyre::Result<Arc<EnhancedDbSnapshotProvider<Arc<reth_db::DatabaseEnv>>>>
where
    Node: FullNodeTypes<Types = BscNode>,
{
    let datadir = ctx.config().datadir.clone();
    let main_dir = datadir.resolve_datadir(ctx.chain_spec().chain());
    let db_path = main_dir.data_dir().join("parlia_snapshots");
    use reth_db::{init_db, mdbx::DatabaseArguments};
    let snapshot_db = Arc::new(
        init_db(&db_path, DatabaseArguments::new(Default::default()))
            .map_err(|e| eyre::eyre!("Failed to initialize snapshot database: {}", e))?,
    );
    tracing::info!("Succeed to create a separate database instance for persistent snapshots");

    let snapshot_provider = Arc::new(EnhancedDbSnapshotProvider::new(
        snapshot_db,
        2048, // Production LRU cache size
        ctx.chain_spec().clone(),
    ));
    tracing::info!("Succeed to create EnhancedDbSnapshotProvider with backward walking capability");

    Ok(snapshot_provider)
}

/// BSC Fork Choice Engine
///
/// Manages fork choice decisions for BSC/Parlia consensus, including:
/// - Evaluating incoming blocks against current canonical head
/// - Applying fast finality rules (justified/finalized blocks)
/// - Communicating fork choice updates to the consensus engine
#[derive(Debug, Clone)]
pub struct BscForkChoiceEngine<P> {
    /// The provider for reading block information
    pub(crate) provider: P,
    /// The engine handle for communicating with the consensus engine
    pub(crate) engine_handle: ConsensusEngineHandle<BscPayloadTypes>,
    /// Chain specification
    chain_spec: Arc<BscChainSpec>,
    /// The fork choice rule
    forkchoice_rule: Arc<BscForkChoiceRule>,
    /// Cache for header total difficulties
    header_td_cache: Arc<
        parking_lot::RwLock<
            schnellru::LruMap<B256, Option<alloy_primitives::U256>, schnellru::ByLength>,
        >,
    >,
    /// Finality metrics
    finality_metrics: BscFinalityMetrics,
    /// Blockchain metrics (including reorg metrics)
    blockchain_metrics: BscBlockchainMetrics,
}

impl<P> BscForkChoiceEngine<P>
where
    P: BlockNumReader + HeaderProvider<Header = Header> + Clone + Send + Sync,
{
    /// Creates a new `BscForkChoiceEngine` instance.
    pub fn new(
        provider: P,
        engine_handle: ConsensusEngineHandle<BscPayloadTypes>,
        chain_spec: Arc<BscChainSpec>,
    ) -> Self {
        Self {
            provider,
            engine_handle,
            chain_spec: chain_spec.clone(),
            forkchoice_rule: Arc::new(BscForkChoiceRule::new(chain_spec)),
            header_td_cache: Arc::new(parking_lot::RwLock::new(schnellru::LruMap::new(
                schnellru::ByLength::new(128),
            ))),
            finality_metrics: BscFinalityMetrics::default(),
            blockchain_metrics: BscBlockchainMetrics::default(),
        }
    }

    /// Returns a reference to the chain specification.
    pub fn chain_spec(&self) -> &Arc<BscChainSpec> {
        &self.chain_spec
    }

    /// Updates the fork choice based on the incoming header.
    ///
    /// This function evaluates whether the incoming header should become the new canonical head
    /// according to BSC's fork choice rules (Parlia consensus with fast finality).
    ///
    /// # Arguments
    ///
    /// * `incoming_header` - The incoming header to evaluate for fork choice
    ///
    /// # Returns
    ///
    /// Returns `Ok(())` if the fork choice was successfully updated, or an error if the update failed.
    pub async fn update_forkchoice(
        &self,
        incoming_header: &Header,
    ) -> Result<(), ParliaConsensusErr> {
        tracing::debug!(
            target: "bsc::forkchoice",
            block_number = incoming_header.number,
            block_hash = ?incoming_header.hash_slow(),
            "Updating fork choice with incoming header"
        );

        let current_number = self.provider.chain_info()?.best_number;
        tracing::trace!(target: "bsc::forkchoice", "Best canonical number: {:?}, new_header = {:?}", current_number, incoming_header);

        let current_head = self
            .provider
            .header_by_number(current_number)?
            .ok_or(ParliaConsensusErr::HeadHashNotFound)?;

        // Determine if we need to reorg using fork choice rules
        let need_reorg = self.is_need_reorg(incoming_header, &current_head).await?;

        // Only count as reorg if:
        // 1. Fork choice says we need to reorg AND
        // 2. The incoming block number is <= current head (actual chain reorganization)
        //    OR the incoming block's parent is not the current head (side chain switch)
        let is_actual_reorg = need_reorg
            && (incoming_header.number <= current_head.number
                || incoming_header.parent_hash != current_head.hash_slow());

        if is_actual_reorg {
            // Calculate reorg depth: the difference between incoming and current head numbers
            // Note: This is a simplified calculation.
            let reorg_depth = incoming_header.number.abs_diff(current_head.number);

            self.blockchain_metrics.reorg_executions_total.increment(1);
            self.blockchain_metrics.latest_reorg_depth.set(reorg_depth as f64);

            tracing::info!(
                target: "bsc::forkchoice",
                incoming_number = incoming_header.number,
                incoming_hash = ?incoming_header.hash_slow(),
                current_number = current_head.number,
                current_hash = ?current_head.hash_slow(),
                reorg_depth,
                "Reorg detected and metrics recorded"
            );
        }

        let new_canonical_head = if need_reorg { incoming_header } else { &current_head };

        // Get safe block and finalized block with new canonical head
        // ref: https://github.com/bnb-chain/bsc/blob/f70aaa8399ccee429804eecf3fc4c6fd8d9e6cab/eth/api_backend.go#L72
        let (safe_block_number, safe_block_hash) =
            self.get_justified_number_and_hash(new_canonical_head).unwrap_or((0, B256::ZERO));
        let (finalized_block_number, finalized_block_hash) =
            self.get_finalized_number_and_hash(new_canonical_head).unwrap_or((0, B256::ZERO));

        // Update finality metrics
        self.finality_metrics.justified_block_height.set(safe_block_number as f64);
        self.finality_metrics.safe_block_height.set(safe_block_number as f64);
        self.finality_metrics.finalized_block_height.set(finalized_block_number as f64);

        let state = ForkchoiceState {
            head_block_hash: new_canonical_head.hash_slow(),
            safe_block_hash,
            finalized_block_hash,
        };

        tracing::debug!(
            target: "bsc::forkchoice",
            ?state,
            new_canonical_head_number = new_canonical_head.number,
            new_canonical_head_hash = ?new_canonical_head.hash_slow(),
            incoming_header_number = incoming_header.number,
            incoming_header_hash = ?incoming_header.hash_slow(),
            safe_block_number,
            finalized_block_number,
            "Fork choice updated"
        );

        match self
            .engine_handle
            .fork_choice_updated(state, None, EngineApiMessageVersion::default())
            .await
        {
            Ok(response) => match response.payload_status.status {
                PayloadStatusEnum::Invalid { validation_error } => {
                    Err(ParliaConsensusErr::ForkChoiceUpdateError(validation_error))
                }
                _ => Ok(()),
            },
            Err(err) => Err(ParliaConsensusErr::ForkChoiceUpdateError(err.to_string())),
        }
    }

    /// Determines if a chain reorganization is needed based on fork choice rules.
    ///
    /// This function compares the incoming header with the current canonical header
    /// and decides whether the incoming chain should replace the current canonical chain.
    /// The decision is based on BSC's Parlia consensus rules with fast finality support.
    ///
    /// This function handles TD (Total Difficulty) fetching internally.
    ///
    /// # Arguments
    ///
    /// * `incoming_header` - The incoming header from a potentially better chain
    /// * `current_header` - The current canonical head header
    ///
    /// # Returns
    ///
    /// Returns `Ok(true)` if a reorg is needed (incoming should become canonical),
    /// `Ok(false)` if no reorg is needed (current remains canonical),
    /// or an error if the comparison failed.
    pub async fn is_need_reorg(
        &self,
        incoming_header: &Header,
        current_header: &Header,
    ) -> Result<bool, ParliaConsensusErr> {
        let (incoming_td, current_td) =
            self.header_td_fcu(&self.engine_handle, incoming_header, current_header).await?;

        // Ensure monotonic TD when incoming is a direct child of current.
        let incoming_td = if incoming_header.parent_hash == current_header.hash_slow() {
            if let (Some(in_td), Some(cur_td)) = (incoming_td, current_td) {
                if in_td <= cur_td {
                    Some(cur_td + incoming_header.difficulty)
                } else {
                    Some(in_td)
                }
            } else {
                incoming_td
            }
        } else {
            incoming_td
        };
        let incoming_justified_num =
            self.get_justified_number_and_hash(incoming_header).map(|(num, _)| num).unwrap_or(0);
        let current_justified_num =
            self.get_justified_number_and_hash(current_header).map(|(num, _)| num).unwrap_or(0);

        let incoming_for_fc =
            HeaderForForkchoice::new(incoming_header, incoming_td, incoming_justified_num);
        let current_for_fc =
            HeaderForForkchoice::new(current_header, current_td, current_justified_num);

        self.forkchoice_rule.is_need_reorg(&incoming_for_fc, &current_for_fc)
    }

    /// Gets the justified number and hash from the header's snapshot.
    fn get_justified_number_and_hash(&self, header: &Header) -> Option<(u64, B256)> {
        if !self.chain_spec.is_luban_active_at_block(header.number) {
            return None;
        }

        let sp = shared::get_snapshot_provider()?;

        match sp.snapshot_by_hash(&header.hash_slow()) {
            Some(snap) => Some((snap.vote_data.target_number, snap.vote_data.target_hash)),
            None => {
                tracing::warn!(
                    target: "bsc::forkchoice",
                    header_hash = ?header.hash_slow(),
                    "Missing snapshot for header when get justified number and hash"
                );
                None
            }
        }
    }

    /// Gets the finalized number and hash from the header's snapshot.
    fn get_finalized_number_and_hash(&self, header: &Header) -> Option<(u64, B256)> {
        if !self.chain_spec.is_plato_active_at_block(header.number) {
            return None;
        }

        let sp = shared::get_snapshot_provider()?;

        match sp.snapshot_by_hash(&header.hash_slow()) {
            Some(snap) => {
                let current_justified_number = snap.vote_data.target_number;
                let current_justified_hash = snap.vote_data.target_hash;

                if header.number > 0 && header.number - 1 == current_justified_number
                    && !snap.validators.is_empty()
                {
                    let quorum = usize::div_ceil(snap.validators.len() * 2, 3);
                    let eligible_votes = vote_pool::fetch_vote_by_block_hash(header.hash_slow())
                        .into_iter()
                        .filter(|vote| {
                            vote.data.source_number == current_justified_number
                                && vote.data.target_number == header.number
                        })
                        .count();
                    if eligible_votes >= quorum {
                        return Some((current_justified_number, current_justified_hash));
                    }
                }

                Some((snap.vote_data.source_number, snap.vote_data.source_hash))
            }
            None => {
                tracing::warn!(
                    target: "bsc::forkchoice",
                    header_hash = ?header.hash_slow(),
                    "Missing snapshot for header when get finalized number and hash"
                );
                None
            }
        }
    }

    /// Gets the total difficulty for both incoming and current headers.
    ///
    /// This private method queries the total difficulty (TD) from the engine for both headers,
    /// with fallback logic for the incoming header if not found directly.
    async fn header_td_fcu(
        &self,
        engine: &ConsensusEngineHandle<BscPayloadTypes>,
        incoming: &Header,
        current: &Header,
    ) -> Result<(Option<alloy_primitives::U256>, Option<alloy_primitives::U256>), ParliaConsensusErr>
    {
        let current_td = self.header_td(engine, current.number, current.hash_slow()).await?;
        let incoming_td = match self.header_td(engine, incoming.number, incoming.hash_slow()).await
        {
            Ok(td) => td,
            Err(e) => {
                tracing::debug!(target: "bsc::forkchoice", "Failed to get incoming header TD: {:?}, try to query parent block TD", e);
                match self.header_td(engine, incoming.number - 1, incoming.parent_hash).await? {
                    Some(td) => Some(td + incoming.difficulty),
                    None => {
                        tracing::debug!(target: "bsc::forkchoice", "Failed to get parent header TD, return None");
                        None
                    }
                }
            }
        };
        Ok((incoming_td, current_td))
    }

    /// Gets the total difficulty for a specific header.
    ///
    /// This private method queries the TD from the engine and caches it for future use.
    async fn header_td(
        &self,
        engine: &ConsensusEngineHandle<BscPayloadTypes>,
        number: u64,
        hash: B256,
    ) -> Result<Option<alloy_primitives::U256>, ParliaConsensusErr> {
        if let Some(td) = self.header_td_cache.write().get(&hash) {
            return Ok(*td);
        }
        let td = engine.query_td(number, hash).await.map_err(ParliaConsensusErr::internal)?;
        self.header_td_cache.write().insert(hash, td);
        Ok(td)
    }
}
