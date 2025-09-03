use crate::{
    hardforks::BscHardforks, 
    node::BscNode, 
    BscBlock, BscBlockBody, BscPrimitives,
    consensus::parlia::{provider::EnhancedDbSnapshotProvider, Parlia, util::calculate_millisecond_timestamp},
};
use alloy_consensus::{Header, TxReceipt};
use alloy_primitives::{B256, Bytes};
use alloy_eips::Encodable2718;
use reth::{
    api::FullNodeTypes,
    builder::{components::ConsensusBuilder, BuilderContext},
    consensus::{ConsensusError, FullConsensus, Consensus, HeaderValidator},
    beacon_consensus::EthBeaconConsensus,
    consensus_common::validation::{validate_against_parent_hash_number, validate_against_parent_4844},
    primitives::{SealedHeader, SealedBlock, RecoveredBlock},
    providers::BlockExecutionResult,
};
use reth_chainspec::EthChainSpec;
use reth_primitives::{gas_spent_by_transactions, GotExpected};
use reth_ethereum_primitives::Receipt;
use std::sync::Arc;

/// A basic Bsc consensus builder.
#[derive(Debug, Default, Clone, Copy)]
#[non_exhaustive]
pub struct BscConsensusBuilder;

impl<Node> ConsensusBuilder<Node> for BscConsensusBuilder
where
    Node: FullNodeTypes<Types = BscNode>,
{
    type Consensus = Arc<dyn FullConsensus<BscPrimitives, Error = ConsensusError>>;

    /// return a parlia consensus instance, automatically called by the ComponentsBuilder framework.
    async fn build_consensus(self, ctx: &BuilderContext<Node>) -> eyre::Result<Self::Consensus> {
        let snapshot_provider = create_snapshot_provider(ctx)
            .unwrap_or_else(|e| {
                panic!("Failed to initialize snapshot provider, due to {e}");
            });
        
        crate::shared::set_snapshot_provider(
            snapshot_provider as Arc<dyn crate::consensus::parlia::SnapshotProvider + Send + Sync>,
        ).unwrap_or_else(|_| panic!("Failed to set global snapshot provider"));

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

impl<ChainSpec: EthChainSpec + BscHardforks + 'static> BscConsensus<ChainSpec> {
    pub fn new(chain_spec: Arc<ChainSpec>) -> Self {
        Self { base: EthBeaconConsensus::new(chain_spec.clone()), parlia: Arc::new(Parlia::new(chain_spec.clone(), 200)), chain_spec }
    }
}

/// header stage validation.
impl<ChainSpec: EthChainSpec + BscHardforks + 'static> HeaderValidator<Header> 
    for BscConsensus<ChainSpec> {
    fn validate_header(&self, header: &SealedHeader) -> Result<(), ConsensusError> {
        // tracing::debug!("Validating header, block_number: {:?}", header.number);
        if let Err(err) = self.parlia.validate_header(header) {
            tracing::warn!("Failed to validate_header, block_number: {}, err: {:?}", header.number, err);
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
            tracing::warn!("Failed to validate_against_parent_hash_number, block_number: {}, err: {:?}", header.number, err);
            return Err(err)
        }

        let header_ts = calculate_millisecond_timestamp(header.header());
        let parent_ts = calculate_millisecond_timestamp(parent.header());
        if header_ts <= parent_ts {
            tracing::warn!("Failed to check timestamp, block_number: {}", header.number);
            return Err(ConsensusError::TimestampIsInPast {
                parent_timestamp: parent_ts,
                timestamp: header_ts,
            })
        }

        // ensure that the blob gas fields for this block
        if self.chain_spec.is_london_active_at_block(header.header().number) && BscHardforks::is_cancun_active_at_timestamp(&*self.chain_spec, header.header().number, header.header().timestamp) {
            if let Some(blob_params) = self.chain_spec.blob_params_at_timestamp(header.timestamp) {
                if let Err(err) = validate_against_parent_4844(header.header(), parent.header(), blob_params) {
                    tracing::warn!("Failed to validate_against_parent_4844, block_number: {}, err: {:?}", header.number, err);
                    return Err(err)
                }
            }
        }

        Ok(())
    }
}

impl<ChainSpec: EthChainSpec<Header = Header> + BscHardforks + 'static> Consensus<BscBlock>
    for BscConsensus<ChainSpec>
{
    type Error = ConsensusError;

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
            })
        }

        // Before Byzantium, receipts contained state root that would mean that expensive
        // operation as hashing that is required for state root got calculated in every
        // transaction This was replaced with is_success flag.
        // See more about EIP here: https://eips.ethereum.org/EIPS/eip-658
        if chain_spec.is_byzantium_active_at_block(block.header().number) {
            if let Err(error) = verify_receipts(block.header().receipts_root, block.header().logs_bloom, receipts)
            {
                let receipts = receipts
                    .iter()
                    .map(|r| Bytes::from(r.with_bloom_ref().encoded_2718()))
                    .collect::<Vec<_>>();
                tracing::debug!(%error, ?receipts, "receipts verification failed");
                return Err(error)
            }
        }

        // Validate that the header requests hash matches the calculated requests hash
        if chain_spec.is_london_active_at_block(block.header().number) && chain_spec.is_prague_active_at_timestamp(block.header().timestamp) {
            let Some(header_requests_hash) = block.header().requests_hash else {
                return Err(ConsensusError::RequestsHashMissing)
            };
            let requests_hash = requests.requests_hash();
            if requests_hash != header_requests_hash {
                return Err(ConsensusError::BodyRequestsHashDiff(
                    GotExpected::new(requests_hash, header_requests_hash).into(),
                ))
            }
        }

        Ok(())
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
    let logs_bloom = receipts_with_bloom.iter().fold(alloy_primitives::Bloom::ZERO, |bloom, r| bloom | r.bloom_ref());

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
        ))
    }

    if calculated_logs_bloom != expected_logs_bloom {
        return Err(reth::consensus::ConsensusError::BodyBloomLogDiff(
            GotExpected { got: calculated_logs_bloom, expected: expected_logs_bloom }.into(),
        ))
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
    let snapshot_db = Arc::new(init_db(
        &db_path,
        DatabaseArguments::new(Default::default())
    ).map_err(|e| eyre::eyre!("Failed to initialize snapshot database: {}", e))?);
    tracing::info!("Succeed to create a separate database instance for persistent snapshots");

    let snapshot_provider = Arc::new(EnhancedDbSnapshotProvider::new(
        snapshot_db,
        2048, // Production LRU cache size
        ctx.chain_spec().clone(),
    ));
    tracing::info!("Succeed to create EnhancedDbSnapshotProvider with backward walking capability");

    Ok(snapshot_provider)
}