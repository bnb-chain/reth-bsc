use crate::{hardforks::BscHardforks, node::BscNode, BscBlock, BscBlockBody, BscPrimitives};
use alloy_consensus::{Header, TxReceipt};
use alloy_primitives::{B256, Bytes};
use alloy_eips::Encodable2718;
use reth::{
    api::FullNodeTypes,
    beacon_consensus::EthBeaconConsensus,
    builder::{components::ConsensusBuilder, BuilderContext},
    consensus::{Consensus, ConsensusError, FullConsensus, HeaderValidator},
    consensus_common::validation::{
        validate_against_parent_4844, validate_against_parent_hash_number,
    },
};

use reth_chainspec::EthChainSpec;
use reth_primitives::{Receipt, RecoveredBlock, SealedBlock, SealedHeader, gas_spent_by_transactions, GotExpected};
use reth_provider::BlockExecutionResult;
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

    async fn build_consensus(self, ctx: &BuilderContext<Node>) -> eyre::Result<Self::Consensus> {
        Ok(Arc::new(BscConsensus::new(ctx.chain_spec())))
    }
}

/// BSC consensus implementation.
///
/// Provides basic checks as outlined in the execution specs.
#[derive(Debug, Clone)]
pub struct BscConsensus<ChainSpec> {
    inner: EthBeaconConsensus<ChainSpec>,
    chain_spec: Arc<ChainSpec>,
}

impl<ChainSpec: EthChainSpec + BscHardforks> BscConsensus<ChainSpec> {
    /// Create a new instance of [`BscConsensus`]
    pub fn new(chain_spec: Arc<ChainSpec>) -> Self {
        Self { inner: EthBeaconConsensus::new(chain_spec.clone()), chain_spec }
    }
}

impl<ChainSpec: EthChainSpec + BscHardforks> HeaderValidator for BscConsensus<ChainSpec> {
    fn validate_header(&self, _header: &SealedHeader) -> Result<(), ConsensusError> {
        // TODO: doesn't work because of extradata check
        // self.inner.validate_header(header)

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
        if self.chain_spec.is_london_active_at_block(header.header().number) && self.chain_spec.is_prague_active_at_timestamp(header.header().timestamp) {
            if let Some(blob_params) = self.chain_spec.blob_params_at_timestamp(header.timestamp) {
                validate_against_parent_4844(header.header(), parent.header(), blob_params)?;
            }
        }
       

        Ok(())
    }
}

impl<ChainSpec: EthChainSpec<Header = Header> + BscHardforks> Consensus<BscBlock>
    for BscConsensus<ChainSpec>
{
    type Error = ConsensusError;

    fn validate_body_against_header(
        &self,
        body: &BscBlockBody,
        header: &SealedHeader,
    ) -> Result<(), ConsensusError> {
        Consensus::<BscBlock>::validate_body_against_header(&self.inner, body, header)
    }

    fn validate_block_pre_execution(
        &self,
        _block: &SealedBlock<BscBlock>,
    ) -> Result<(), ConsensusError> {
        // Check ommers hash
        // let ommers_hash = block.body().calculate_ommers_root();
        // if Some(block.ommers_hash()) != ommers_hash {
        //     return Err(ConsensusError::BodyOmmersHashDiff(
        //         GotExpected {
        //             got: ommers_hash.unwrap_or(EMPTY_OMMER_ROOT_HASH),
        //             expected: block.ommers_hash(),
        //         }
        //         .into(),
        //     ))
        // }

        // // Check transaction root
        // if let Err(error) = block.ensure_transaction_root_valid() {
        //     return Err(ConsensusError::BodyTransactionRootDiff(error.into()))
        // }

        // if self.chain_spec.is_cancun_active_at_timestamp(block.timestamp()) {
        //     validate_cancun_gas(block)?;
        // } else {
        //     return Ok(())
        // }

        Ok(())
    }
}

impl<ChainSpec: EthChainSpec<Header = Header> + BscHardforks> FullConsensus<BscPrimitives>
    for BscConsensus<ChainSpec>
{
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
