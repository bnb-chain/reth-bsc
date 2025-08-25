use crate::{
    node::BscNode,
    BscPrimitives, BscBlock, BscBlockBody,
    consensus::parlia::{provider::EnhancedDbSnapshotProvider, Parlia},
    hardforks::BscHardforks,
};
use reth::{
    api::FullNodeTypes,
    builder::{components::ConsensusBuilder, BuilderContext},
    consensus::{ConsensusError, FullConsensus, Consensus, HeaderValidator},
    beacon_consensus::EthBeaconConsensus,
    consensus_common::validation::{validate_against_parent_hash_number, validate_against_parent_4844},
    primitives::{SealedHeader, SealedBlock, RecoveredBlock},
    providers::BlockExecutionResult,
};
use alloy_consensus::Header;
use reth_ethereum_primitives::Receipt;
use crate::consensus::parlia::util::calculate_millisecond_timestamp;
use reth_chainspec::EthChainSpec;

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
                panic!("Failed to initialize snapshot provider, due to {}", e);
            });
        
        crate::shared::set_snapshot_provider(
            snapshot_provider as Arc<dyn crate::consensus::parlia::SnapshotProvider + Send + Sync>,
        ).unwrap_or_else(|_| panic!("Failed to set global snapshot provider"));

        crate::shared::set_header_provider(Arc::new(ctx.provider().clone()))
            .unwrap_or_else(|e| panic!("Failed to set global header provider: {}", e));

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
        // tracing::info!("Validating header, block_number: {:?}", header.number);
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
        // tracing::info!("Validating header against parent, block_number: {:?}", header.number);
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
        if let Some(blob_params) = self.chain_spec.blob_params_at_timestamp(header.timestamp) {
            if let Err(err) = validate_against_parent_4844(header.header(), parent.header(), blob_params) {
                tracing::warn!("Failed to validate_against_parent_4844, block_number: {}, err: {:?}", header.number, err);
                return Err(err)
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
        // tracing::info!("Validating body against header, block_number: {:?}", header.number);
        Consensus::<BscBlock>::validate_body_against_header(&self.base, body, header)
    }

    /// body stage validation.
    fn validate_block_pre_execution(
        &self,
        block: &SealedBlock<BscBlock>,
    ) -> Result<(), ConsensusError> {
        // tracing::info!("Validating block pre-execution, block_number: {:?}", block.header().number);
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
        // tracing::info!("Validating block post-execution, block_number: {:?}", block.header().number);
        FullConsensus::<BscPrimitives>::validate_block_post_execution(&self.base, block, result)
    }
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