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
use alloy_primitives::B256;
use alloy_consensus::Header;
use reth_ethereum_primitives::Receipt;

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
        // TODO: refine this part.
        let snapshot_provider = try_create_ondemand_snapshots(ctx)
            .unwrap_or_else(|e| {
                panic!("Failed to initialize on-demand MDBX snapshots: {}", e);
            });
        // Store the snapshot provider globally so RPC can access it
        let _ = crate::shared::set_snapshot_provider(
            snapshot_provider as Arc<dyn crate::consensus::parlia::SnapshotProvider + Send + Sync>,
        );
        // Store the header provider globally for shared access
        if let Err(_) = crate::shared::set_header_provider(Arc::new(ctx.provider().clone())) {
            tracing::warn!("Failed to set global header provider");
        } else {
            tracing::info!("Succeed to set global header provider");
        }

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

impl<ChainSpec: EthChainSpec + BscHardforks + 'static> HeaderValidator<Header> 
    for BscConsensus<ChainSpec> {
    fn validate_header(&self, header: &SealedHeader) -> Result<(), ConsensusError> {
        self.parlia.validate_header(header)?;
        Ok(())
    }

    fn validate_header_against_parent(
        &self,
        header: &SealedHeader,
        parent: &SealedHeader,
    ) -> Result<(), ConsensusError> {
        validate_against_parent_hash_number(header.header(), parent)?;

        let header_ts = calculate_millisecond_timestamp(header.header());
        let parent_ts = calculate_millisecond_timestamp(parent.header());
        if header_ts <= parent_ts {
            return Err(ConsensusError::TimestampIsInPast {
                parent_timestamp: parent_ts,
                timestamp: header_ts,
            })
        }

        // ensure that the blob gas fields for this block
        if let Some(blob_params) = self.chain_spec.blob_params_at_timestamp(header.timestamp) {
            validate_against_parent_4844(header.header(), parent.header(), blob_params)?;
        }

        Ok(())
    }
}

impl<ChainSpec: EthChainSpec<Header = Header> + BscHardforks + 'static> Consensus<BscBlock>
    for BscConsensus<ChainSpec>
{
    type Error = ConsensusError;

    fn validate_body_against_header(
        &self,
        body: &BscBlockBody,
        header: &SealedHeader,
    ) -> Result<(), ConsensusError> {
        Consensus::<BscBlock>::validate_body_against_header(&self.base, body, header)
    }

    fn validate_block_pre_execution(
        &self,
        block: &SealedBlock<BscBlock>,
    ) -> Result<(), ConsensusError> {
        self.parlia.validate_block_pre_execution(block)?;
        Ok(())
    }
}

impl<ChainSpec: EthChainSpec<Header = Header> + BscHardforks + 'static> FullConsensus<BscPrimitives>
    for BscConsensus<ChainSpec>
{
    fn validate_block_post_execution(
        &self,
        block: &RecoveredBlock<BscBlock>,
        result: &BlockExecutionResult<Receipt>,
    ) -> Result<(), ConsensusError> {
        FullConsensus::<BscPrimitives>::validate_block_post_execution(&self.base, block, result)
    }
}


/// Calculate the millisecond timestamp of a block header.
/// Refer to https://github.com/bnb-chain/BEPs/blob/master/BEPs/BEP-520.md.
pub fn calculate_millisecond_timestamp<H: alloy_consensus::BlockHeader>(header: &H) -> u64 {
    let seconds = header.timestamp();
    let mix_digest = header.mix_hash().unwrap_or(B256::ZERO);

    let milliseconds = if mix_digest != B256::ZERO {
        let bytes = mix_digest.as_slice();
        // Convert last 8 bytes to u64 (big-endian), equivalent to Go's
        // uint256.SetBytes32().Uint64()
        let mut result = 0u64;
        for &byte in bytes.iter().skip(24).take(8) {
            result = (result << 8) | u64::from(byte);
        }
        result
    } else {
        0
    };

    seconds * 1000 + milliseconds
}


// TODO: refine this part.
fn try_create_ondemand_snapshots<Node>(
    ctx: &BuilderContext<Node>,
) -> eyre::Result<Arc<EnhancedDbSnapshotProvider<Arc<reth_db::DatabaseEnv>>>>
where
    Node: FullNodeTypes<Types = BscNode>,
{
    // Create a separate database instance for snapshot storage in its own directory
    // This avoids conflicts with the main database
    let datadir = ctx.config().datadir.clone();
    let main_dir = datadir.resolve_datadir(ctx.chain_spec().chain());
    let db_path = main_dir.data_dir().join("parlia_snapshots");

    // Initialize our own database instance for snapshot storage
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


#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::Header;
    use alloy_primitives::B256;

    #[test]
    fn test_calculate_millisecond_timestamp_without_mix_hash() {
        // Create a header with current timestamp and zero mix_hash
        let timestamp =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();

        let header = Header { timestamp, mix_hash: B256::ZERO, ..Default::default() };

        let result = calculate_millisecond_timestamp(&header);
        assert_eq!(result, timestamp * 1000);
    }

    #[test]
    fn test_calculate_millisecond_timestamp_with_milliseconds() {
        // Create a header with current timestamp and mix_hash containing milliseconds
        let timestamp =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();

        let milliseconds = 750u64;
        let mut mix_hash_bytes = [0u8; 32];
        mix_hash_bytes[24..32].copy_from_slice(&milliseconds.to_be_bytes());
        let mix_hash = B256::new(mix_hash_bytes);

        let header = Header { timestamp, mix_hash, ..Default::default() };

        let result = calculate_millisecond_timestamp(&header);
        assert_eq!(result, timestamp * 1000 + milliseconds);
    }
}