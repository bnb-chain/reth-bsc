use reth::consensus::{HeaderValidator, ConsensusError, Consensus};
use reth::primitives::SealedHeader;
use reth_chainspec::{EthChainSpec, EthereumHardforks, EthereumHardfork};
use crate::hardforks::BscHardforks;
use super::Parlia;
use alloy_consensus::{Header, EMPTY_OMMER_ROOT_HASH};
use alloy_primitives::B256;
use reth_primitives::GotExpected;
use alloy_eips::eip4844::{DATA_GAS_PER_BLOB, MAX_DATA_GAS_PER_BLOCK_DENCUN};
use crate::BscBlock;
use reth_primitives_traits::Block;


pub const fn validate_header_gas(header: &Header) -> Result<(), ConsensusError> {
    if header.gas_used > header.gas_limit {
        return Err(ConsensusError::HeaderGasUsedExceedsGasLimit {
            gas_used: header.gas_used,
            gas_limit: header.gas_limit,
        })
    }
    Ok(())
}

/// Ensure the EIP-1559 base fee is set if the London hardfork is active.
#[inline]
pub fn validate_header_base_fee<ChainSpec: EthereumHardforks>(
    header: &Header,
    chain_spec: &ChainSpec,
) -> Result<(), ConsensusError> {
    if chain_spec.is_ethereum_fork_active_at_block(EthereumHardfork::London, header.number) &&
        header.base_fee_per_gas.is_none()
    {
        return Err(ConsensusError::BaseFeeMissing)
    }
    Ok(())
}

/// Validate the 4844 header of BSC block.
/// Compared to Ethereum, BSC block doesn't have `parent_beacon_block_root`.
pub fn validate_4844_header_of_bsc(header: &SealedHeader) -> Result<(), ConsensusError> {
    let blob_gas_used = header.blob_gas_used.ok_or(ConsensusError::BlobGasUsedMissing)?;
    let excess_blob_gas = header.excess_blob_gas.ok_or(ConsensusError::ExcessBlobGasMissing)?;

    if blob_gas_used > MAX_DATA_GAS_PER_BLOCK_DENCUN {
        return Err(ConsensusError::BlobGasUsedExceedsMaxBlobGasPerBlock {
            blob_gas_used,
            max_blob_gas_per_block: MAX_DATA_GAS_PER_BLOCK_DENCUN,
        })
    }

    if blob_gas_used % DATA_GAS_PER_BLOB != 0 {
        return Err(ConsensusError::BlobGasUsedNotMultipleOfBlobGasPerBlob {
            blob_gas_used,
            blob_gas_per_blob: DATA_GAS_PER_BLOB,
        })
    }

    // `excess_blob_gas` must also be a multiple of `DATA_GAS_PER_BLOB`. This will be checked later
    // (via `calculate_excess_blob_gas`), but it doesn't hurt to catch the problem sooner.
    if excess_blob_gas % DATA_GAS_PER_BLOB != 0 {
        return Err(ConsensusError::BlobGasUsedNotMultipleOfBlobGasPerBlob {
            blob_gas_used: excess_blob_gas,
            blob_gas_per_blob: DATA_GAS_PER_BLOB,
        })
    }

    Ok(())
}

impl<ChainSpec: EthChainSpec + BscHardforks + std::fmt::Debug + Send + Sync + 'static> HeaderValidator for Parlia<ChainSpec> {
    fn validate_header(&self, header: &SealedHeader) -> Result<(), ConsensusError> {
        // Don't waste time checking blocks from the future
        let present_timestamp = self.present_timestamp();
        if header.timestamp > present_timestamp {
            return Err(ConsensusError::TimestampIsInFuture {
               timestamp: header.timestamp,
               present_timestamp,
            });
        }

        // Check extra data
        self.check_header_extra(header).map_err(|e| ConsensusError::Other(format!("Invalid header extra: {}", e)))?;

        // Ensure that the mix digest is zero as we don't have fork protection currently
        // mix_hash is millisecond timestamp after Lorentz/Maxwell.
        // if header.mix_hash != EMPTY_MIX_HASH {
        // return Err(ConsensusError::InvalidMixHash);
        // }

        // Ensure that the block with no uncles
        if header.ommers_hash != EMPTY_OMMER_ROOT_HASH {
            return Err(ConsensusError::BodyOmmersHashDiff(
                GotExpected { got: header.ommers_hash, expected: EMPTY_OMMER_ROOT_HASH }.into(),
            ));
        }

        validate_header_gas(header)?;
        validate_header_base_fee(header, &self.spec)?;

        // Ensures that EIP-4844 fields are valid once cancun is active.
        if self.spec.is_cancun_active_at_timestamp(header.timestamp) {
            validate_4844_header_of_bsc(header)?;
        } else if header.blob_gas_used.is_some() {
            return Err(ConsensusError::BlobGasUsedUnexpected)
        } else if header.excess_blob_gas.is_some() {
            return Err(ConsensusError::ExcessBlobGasUnexpected)
        }

        if self.spec.is_bohr_active_at_timestamp(header.timestamp) {
           if header.parent_beacon_block_root.is_none() ||
               header.parent_beacon_block_root.unwrap() != B256::default()
           {
               return Err(ConsensusError::ParentBeaconBlockRootUnexpected)
           }
        } else if header.parent_beacon_block_root.is_some() {
           return Err(ConsensusError::ParentBeaconBlockRootUnexpected)
        }

       Ok(())
    }

    fn validate_header_against_parent(
        &self,
        _header: &SealedHeader,
        _parent: &SealedHeader,
    ) -> Result<(), ConsensusError> {
        // is unused.
        unimplemented!()
    }
}


impl<ChainSpec: EthChainSpec + BscHardforks + std::fmt::Debug + Send + Sync + 'static> Consensus<BscBlock> for Parlia<ChainSpec> {
    type Error = ConsensusError;

    fn validate_body_against_header(
        &self,
        _body: &<BscBlock as Block>::Body,
        _header: &SealedHeader,
    ) -> Result<(), ConsensusError> {
        // is unused.
        unimplemented!()
    }

    fn validate_block_pre_execution(
        &self,
        block: &reth_primitives_traits::SealedBlock<BscBlock>,
    ) -> Result<(), ConsensusError> {
        // Check transaction root
        if let Err(error) = block.ensure_transaction_root_valid() {
            return Err(ConsensusError::BodyTransactionRootDiff(error.into()));
        }

        // EIP-4844: Shard Blob Transactions
        if self.spec.is_cancun_active_at_timestamp(block.timestamp) {
            // Check that the blob gas used in the header matches the sum of the blob gas used by
            // each blob tx
            let header_blob_gas_used =
                block.blob_gas_used.ok_or(ConsensusError::BlobGasUsedMissing)?;
            let total_blob_gas = block.blob_gas_used.ok_or(ConsensusError::BlobGasUsedMissing)?;
            if total_blob_gas != header_blob_gas_used {
                return Err(ConsensusError::BlobGasUsedDiff(GotExpected {
                    got: header_blob_gas_used,
                    expected: total_blob_gas,
                }));
            }
        }

        Ok(())
    }
}