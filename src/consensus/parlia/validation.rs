use reth::consensus::{Consensus, ConsensusError, HeaderValidator};
use reth::primitives::SealedHeader;
use reth_chainspec::{EthChainSpec, EthereumHardforks, EthereumHardfork};
use crate::consensus::parlia::util::calculate_millisecond_timestamp;
use crate::hardforks::BscHardforks;
use crate::consensus::eip4844::is_blob_eligible_block;
use super::{Parlia, EMPTY_WITHDRAWALS_HASH};
use alloy_consensus::{Header, Transaction, EMPTY_OMMER_ROOT_HASH};
use alloy_primitives::B256;
use reth_primitives_traits::GotExpected;
use alloy_eips::eip4844::{DATA_GAS_PER_BLOB, MAX_DATA_GAS_PER_BLOCK_DENCUN};
use crate::BscBlock;
use reth_primitives_traits::Block;
use std::{sync::Arc, time::SystemTime};

const MAX_RLP_BLOCK_SIZE_OSAKA: usize = 8 * 1024 * 1024;


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
pub fn validate_4844_header_of_bsc<ChainSpec: BscHardforks>(
    header: &SealedHeader,
    chain_spec: &ChainSpec,
) -> Result<(), ConsensusError> {
    let blob_gas_used = header.blob_gas_used.ok_or(ConsensusError::BlobGasUsedMissing)?;
    let excess_blob_gas = header.excess_blob_gas.ok_or(ConsensusError::ExcessBlobGasMissing)?;

    // BEP-657: After Mendel, non-eligible blocks must have blob_gas_used == 0
    if !is_blob_eligible_block(chain_spec, header.number, header.timestamp) && blob_gas_used != 0 {
        return Err(ConsensusError::Other(Arc::new(std::io::Error::other(format!(
            "blob transactions not allowed in block {} (N % {} != 0)",
            header.number,
            crate::consensus::eip4844::BLOB_ELIGIBLE_BLOCK_INTERVAL
        )))));
    }

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

#[inline]
fn present_unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("system time before unix epoch")
        .as_secs()
}

#[inline]
fn validate_header_not_from_future(
    header: &SealedHeader,
    present_timestamp_secs: u64,
) -> Result<(), ConsensusError> {
    // Keep parity with go-bsc: only second-level timestamp is checked here.
    if header.timestamp > present_timestamp_secs {
        return Err(ConsensusError::TimestampIsInFuture {
            timestamp: header.timestamp,
            present_timestamp: present_timestamp_secs,
        });
    }
    Ok(())
}

#[inline]
fn validate_mix_digest_for_parlia(
    header: &SealedHeader,
    lorentz_active: bool,
) -> Result<(), ConsensusError> {
    if !lorentz_active {
        if header.mix_hash != B256::ZERO {
            return Err(ConsensusError::Other(Arc::new(std::io::Error::other("non-zero mix digest"))));
        }
        return Ok(());
    }

    // In Lorentz+, mix digest carries the millisecond remainder. It must not overflow seconds.
    if calculate_millisecond_timestamp(header) / 1000 != header.timestamp {
        return Err(ConsensusError::Other(Arc::new(std::io::Error::other(
            "invalid mix digest milliseconds component",
        ))));
    }
    Ok(())
}

#[inline]
fn validate_withdrawals_root_for_bsc(
    header: &SealedHeader,
    cancun_active: bool,
) -> Result<(), ConsensusError> {
    if !cancun_active {
        if header.withdrawals_root.is_some() {
            return Err(ConsensusError::WithdrawalsRootUnexpected);
        }
        return Ok(());
    }

    let got = header
        .withdrawals_root
        .ok_or(ConsensusError::WithdrawalsRootMissing)?;
    if got != EMPTY_WITHDRAWALS_HASH {
        return Err(ConsensusError::BodyWithdrawalsRootDiff(
            GotExpected { got, expected: EMPTY_WITHDRAWALS_HASH }.into(),
        ));
    }
    Ok(())
}

#[inline]
fn validate_requests_hash_for_bsc(
    header: &SealedHeader,
    prague_active: bool,
) -> Result<(), ConsensusError> {
    if prague_active {
        if header.requests_hash.is_none() {
            return Err(ConsensusError::RequestsHashMissing);
        }
    } else if header.requests_hash.is_some() {
        return Err(ConsensusError::RequestsHashUnexpected);
    }
    Ok(())
}

/// go-bsc `parlia.VerifyUnsealedHeader`: reject the two trailing `rlp:"optional"` header fields
/// `BlockAccessListHash` (EIP-7928) and `SlotNumber` (EIP-7843).
///
/// Both are covered by the block hash but NOT by the Parlia seal preimage —
/// [`super::util::encode_header_with_chain_id`] stops after `requests_hash`, mirroring go-bsc's
/// `EncodeSigHeader`. Honest blocks never set them, so without this check an unprivileged peer
/// could append a crafted value to a validator-signed header, changing its block hash while
/// leaving the seal (and therefore the recovered signer) valid.
///
/// go-bsc gates the rule on Amsterdam — absent before, required from — but BSC has not
/// scheduled Amsterdam and reth-bsc has no such fork, so only the "must be absent" half is
/// expressible here. The presence half lands with Amsterdam itself, at which point the fields'
/// *content* is bound by state validation (`BlockAccessListHash` is recomputed from execution)
/// and the slot rules rather than by the seal.
#[inline]
fn validate_optional_trailing_fields_for_bsc(header: &SealedHeader) -> Result<(), ConsensusError> {
    if let Some(block_access_list_hash) = header.block_access_list_hash {
        return Err(ConsensusError::msg(format!(
            "invalid BlockAccessListHash, have {block_access_list_hash:#x}, expected nil"
        )));
    }
    if let Some(slot_number) = header.slot_number {
        return Err(ConsensusError::msg(format!(
            "invalid SlotNumber, have {slot_number}, expected nil"
        )));
    }
    Ok(())
}

impl<ChainSpec: EthChainSpec + BscHardforks + std::fmt::Debug + Send + Sync + 'static> Parlia<ChainSpec> {
    /// The standalone header-field checks shared by the sync path and the BidBlock path —
    /// go-bsc's `VerifyUnsealedHeader` scope. Deliberately EXCLUDES the wall-clock future
    /// bound: go-bsc applies that only in `verifyHeader` (the sync path), never to
    /// builder-submitted BidBlock headers, whose next-slot timestamp is legitimately a few
    /// hundred milliseconds ahead of the validator's clock at admission time.
    pub fn validate_unsealed_header_fields(
        &self,
        header: &SealedHeader,
    ) -> Result<(), ConsensusError> {
        // Check extra data
        self.check_header_extra(header).map_err(|e| ConsensusError::Other(Arc::new(std::io::Error::other(format!("Invalid header extra: {e}")))))?;

        // Ensure that the block with no uncles
        if header.ommers_hash != EMPTY_OMMER_ROOT_HASH {
            return Err(ConsensusError::BodyOmmersHashDiff(
                GotExpected { got: header.ommers_hash, expected: EMPTY_OMMER_ROOT_HASH }.into(),
            ));
        }

        validate_header_gas(header)?;
        validate_header_base_fee(header, &self.spec)?;

        let cancun_active =
            BscHardforks::is_cancun_active_at_timestamp(&*self.spec, header.number, header.timestamp);
        validate_withdrawals_root_for_bsc(header, cancun_active)?;

        // Ensures that EIP-4844 fields are valid once cancun is active.
        if cancun_active {
            validate_4844_header_of_bsc(header, &*self.spec)?;
        } else if header.blob_gas_used.is_some() {
            return Err(ConsensusError::BlobGasUsedUnexpected)
        } else if header.excess_blob_gas.is_some() {
            return Err(ConsensusError::ExcessBlobGasUnexpected)
        }

        let lorentz_active =
            self.spec.is_lorentz_active_at_timestamp(header.number, header.timestamp);
        validate_mix_digest_for_parlia(header, lorentz_active)?;

        if self.spec.is_bohr_active_at_timestamp(header.number, header.timestamp) {
            if header.parent_beacon_block_root.is_none() ||
               header.parent_beacon_block_root.unwrap() != B256::default()
            {
                return Err(ConsensusError::ParentBeaconBlockRootUnexpected)
            }
        } else if header.parent_beacon_block_root.is_some() {
           return Err(ConsensusError::ParentBeaconBlockRootUnexpected)
        }

        let prague_active =
            self.spec.is_prague_active_at_block_and_timestamp(header.number, header.timestamp);
        validate_requests_hash_for_bsc(header, prague_active)?;

        validate_optional_trailing_fields_for_bsc(header)?;

       Ok(())
    }
}

impl<ChainSpec: EthChainSpec + BscHardforks + std::fmt::Debug + Send + Sync + 'static> HeaderValidator for Parlia<ChainSpec> {
    fn validate_header(&self, header: &SealedHeader) -> Result<(), ConsensusError> {
        // Don't waste time checking blocks from the future (sync path only — go-bsc's
        // `verifyHeader`; the BidBlock path uses `validate_unsealed_header_fields` directly).
        validate_header_not_from_future(header, present_unix_seconds())?;

        self.validate_unsealed_header_fields(header)
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

        if BscHardforks::is_osaka_active_at_timestamp(&*self.spec, block.number, block.timestamp) {
            let rlp_length = BscBlock::rlp_length(block.header(), block.body());
            if rlp_length > MAX_RLP_BLOCK_SIZE_OSAKA {
                return Err(ConsensusError::BlockTooLarge {
                    rlp_length,
                    max_rlp_length: MAX_RLP_BLOCK_SIZE_OSAKA,
                });
            }
            // Note: Individual transaction gas limit validation (EIP-7825) is intentionally
            // NOT performed here because system transactions use i64::MAX gas limit.
            // The validation happens during EVM execution via cfg_env.tx_gas_limit_cap,
            // where system transactions can be properly identified and exempted.
            // This is consistent with go-bsc's block_validator.go behavior.
        }

        // EIP-4844: Shard Blob Transactions
        if BscHardforks::is_cancun_active_at_timestamp(&*self.spec, block.number, block.timestamp) {
            if !is_blob_eligible_block(&*self.spec, block.number, block.timestamp)
                && block.body().transactions().any(|tx| tx.is_eip4844())
            {
                return Err(ConsensusError::Other(Arc::new(std::io::Error::other(
                    "blob transactions not allowed in this block",
                ))));
            }
            // Check that the blob gas used in the header matches the sum of the blob gas used by
            // each blob tx
            let header_blob_gas_used =
                block.blob_gas_used.ok_or(ConsensusError::BlobGasUsedMissing)?;
            let total_blob_gas: u64 = block
                .body()
                .transactions()
                .map(|tx| tx.blob_gas_used().unwrap_or(0))
                .sum();
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

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::B256;
    use reth_primitives_traits::SealedHeader as RethSealedHeader;

    fn sealed(header: Header) -> SealedHeader {
        RethSealedHeader::new(header, B256::ZERO)
    }

    #[test]
    fn future_timestamp_check_uses_seconds_parity() {
        let header = sealed(Header { timestamp: 101, ..Default::default() });
        assert!(validate_header_not_from_future(&header, 100).is_err());

        let header = sealed(Header { timestamp: 100, ..Default::default() });
        assert!(validate_header_not_from_future(&header, 100).is_ok());
    }

    #[test]
    fn pre_lorentz_requires_zero_mix_digest() {
        let header = sealed(Header { mix_hash: B256::from([1u8; 32]), ..Default::default() });
        assert!(validate_mix_digest_for_parlia(&header, false).is_err());

        let header = sealed(Header { mix_hash: B256::ZERO, ..Default::default() });
        assert!(validate_mix_digest_for_parlia(&header, false).is_ok());
    }

    #[test]
    fn lorentz_mix_digest_milliseconds_must_not_overflow_seconds() {
        // 999ms remainder => valid
        let mut valid_mix = [0u8; 32];
        valid_mix[24..].copy_from_slice(&999u64.to_be_bytes());
        let header = sealed(Header {
            timestamp: 10,
            mix_hash: B256::from(valid_mix),
            ..Default::default()
        });
        assert!(validate_mix_digest_for_parlia(&header, true).is_ok());

        // 1000ms remainder => invalid (would spill into next second)
        let mut invalid_mix = [0u8; 32];
        invalid_mix[24..].copy_from_slice(&1000u64.to_be_bytes());
        let header = sealed(Header {
            timestamp: 10,
            mix_hash: B256::from(invalid_mix),
            ..Default::default()
        });
        assert!(validate_mix_digest_for_parlia(&header, true).is_err());
    }

    #[test]
    fn cancun_requires_empty_withdrawals_root() {
        let header = sealed(Header::default());
        assert!(matches!(
            validate_withdrawals_root_for_bsc(&header, true),
            Err(ConsensusError::WithdrawalsRootMissing)
        ));

        let header = sealed(Header { withdrawals_root: Some(B256::from([1u8; 32])), ..Default::default() });
        assert!(validate_withdrawals_root_for_bsc(&header, true).is_err());

        let header =
            sealed(Header { withdrawals_root: Some(EMPTY_WITHDRAWALS_HASH), ..Default::default() });
        assert!(validate_withdrawals_root_for_bsc(&header, true).is_ok());
    }

    #[test]
    fn pre_cancun_rejects_withdrawals_root() {
        let header =
            sealed(Header { withdrawals_root: Some(EMPTY_WITHDRAWALS_HASH), ..Default::default() });
        assert!(matches!(
            validate_withdrawals_root_for_bsc(&header, false),
            Err(ConsensusError::WithdrawalsRootUnexpected)
        ));
    }

    #[test]
    fn prague_requests_hash_presence_rules() {
        let header = sealed(Header::default());

        assert!(matches!(
            validate_requests_hash_for_bsc(&header, true),
            Err(ConsensusError::RequestsHashMissing)
        ));

        let header = sealed(Header { requests_hash: Some(B256::from([2u8; 32])), ..Default::default() });
        assert!(validate_requests_hash_for_bsc(&header, true).is_ok());
        assert!(matches!(
            validate_requests_hash_for_bsc(&header, false),
            Err(ConsensusError::RequestsHashUnexpected)
        ));
    }

    #[test]
    fn rejects_block_access_list_hash_and_slot_number() {
        // Both absent: the only shape honest BSC blocks have today.
        assert!(validate_optional_trailing_fields_for_bsc(&sealed(Header::default())).is_ok());

        let header = sealed(Header {
            block_access_list_hash: Some(B256::from([3u8; 32])),
            ..Default::default()
        });
        let err = validate_optional_trailing_fields_for_bsc(&header).unwrap_err().to_string();
        assert!(err.contains("invalid BlockAccessListHash"), "unexpected error: {err}");

        let header = sealed(Header { slot_number: Some(7), ..Default::default() });
        let err = validate_optional_trailing_fields_for_bsc(&header).unwrap_err().to_string();
        assert!(err.contains("invalid SlotNumber"), "unexpected error: {err}");
    }

    /// Why the check has to exist: appending either field leaves the Parlia seal preimage — and
    /// so the recovered signer — untouched, while changing the block hash. That is exactly the
    /// malleability go-bsc closes; without the check, a peer can mint a second valid-looking
    /// hash for a block it did not sign.
    #[test]
    fn trailing_fields_change_the_block_hash_but_not_the_seal_hash() {
        use crate::consensus::parlia::util::hash_with_chain_id;

        let base = Header { number: 1, extra_data: vec![0u8; 97].into(), ..Default::default() };

        let mut malleated = base.clone();
        malleated.block_access_list_hash = Some(B256::from([9u8; 32]));

        // The seal covers neither field, so the signature over the original header still
        // verifies against the malleated one.
        assert_eq!(hash_with_chain_id(&base, 56), hash_with_chain_id(&malleated, 56));
        // But the block hash — what peers index and fork-choice keys on — differs.
        assert_ne!(base.hash_slow(), malleated.hash_slow());

        let mut malleated = base.clone();
        malleated.slot_number = Some(1);
        assert_eq!(hash_with_chain_id(&base, 56), hash_with_chain_id(&malleated, 56));
        assert_ne!(base.hash_slow(), malleated.hash_slow());
    }

    /// The check is reached from `validate_unsealed_header_fields` (go-bsc's
    /// `VerifyUnsealedHeader` scope), not just callable in isolation: an otherwise-valid
    /// pre-Cancun header passes, and the same header with a `BlockAccessListHash` does not.
    #[test]
    fn unsealed_header_fields_rejects_trailing_optional_fields() {
        use crate::chainspec::BscChainSpec;
        use reth_chainspec::ChainSpecBuilder;
        let parlia = Parlia::new(Arc::new(BscChainSpec::from(ChainSpecBuilder::mainnet().build())), 200);

        // Non-epoch height, vanity+seal extra data, pre-Cancun timestamp: every other optional
        // field is legitimately absent, so this header's only variable is the one under test.
        let valid = Header {
            number: 1,
            timestamp: 1_600_000_000,
            extra_data: vec![0u8; 97].into(),
            ommers_hash: EMPTY_OMMER_ROOT_HASH,
            base_fee_per_gas: Some(0),
            ..Default::default()
        };
        parlia
            .validate_unsealed_header_fields(&sealed(valid.clone()))
            .expect("baseline header must pass");

        let with_bal = Header {
            block_access_list_hash: Some(B256::from([4u8; 32])),
            ..valid.clone()
        };
        let err = parlia
            .validate_unsealed_header_fields(&sealed(with_bal))
            .expect_err("BlockAccessListHash must be rejected")
            .to_string();
        assert!(err.contains("invalid BlockAccessListHash"), "unexpected error: {err}");

        let with_slot = Header { slot_number: Some(1), ..valid };
        let err = parlia
            .validate_unsealed_header_fields(&sealed(with_slot))
            .expect_err("SlotNumber must be rejected")
            .to_string();
        assert!(err.contains("invalid SlotNumber"), "unexpected error: {err}");
    }

    /// Regression guard for the BidBlock future-timestamp fix: the sync path (`validate_header`)
    /// applies the wall-clock future bound (go-bsc `verifyHeader`), but the BidBlock admission path
    /// (`validate_unsealed_header_fields`, go-bsc `VerifyUnsealedHeader`) must NOT — a bid's
    /// next-slot timestamp is legitimately in the future when it arrives. If someone re-adds the
    /// future check to the unsealed path, this fails.
    #[test]
    fn unsealed_header_fields_skips_wall_clock_future_check() {
        use crate::chainspec::BscChainSpec;
        use reth_chainspec::ChainSpecBuilder;
        let parlia = Parlia::new(Arc::new(BscChainSpec::from(ChainSpecBuilder::mainnet().build())), 200);

        // Far-future timestamp AND empty extra (so check_header_extra would also reject). The
        // discriminating signal is *which* error each path returns first.
        let header = sealed(Header { number: 1, timestamp: u64::MAX / 2, ..Default::default() });

        // Sync path: the future bound is checked first → TimestampIsInFuture.
        assert!(
            matches!(parlia.validate_header(&header), Err(ConsensusError::TimestampIsInFuture { .. })),
            "sync validate_header must reject a future timestamp"
        );

        // Unsealed/BidBlock path: no future bound → it must NOT be TimestampIsInFuture
        // (it fails later on the empty extra instead).
        if let Err(ConsensusError::TimestampIsInFuture { .. }) =
            parlia.validate_unsealed_header_fields(&header)
        {
            panic!("validate_unsealed_header_fields must NOT apply the wall-clock future bound")
        }
    }
}
