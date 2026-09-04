use reth::consensus::{Consensus, ConsensusError, HeaderValidator};
use reth::primitives::SealedHeader;
use reth_chainspec::{EthChainSpec, EthereumHardforks, EthereumHardfork};
use crate::consensus::parlia::util::calculate_millisecond_timestamp;
use crate::consensus::payment_lane::Commitment;
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

        // No uncles, ever. Before Jenner this must be the empty ommers root; from Jenner on this
        // path only shape-checks the commitment. The activation block still relies on the
        // pre-Jenner `else` arm because this path has no parent.
        let ommers_hash_ok = if BscHardforks::is_jenner_active_at_timestamp(
            &*self.spec,
            header.number,
            header.timestamp,
        ) {
            Commitment::commits_no_uncles(header.ommers_hash)
        } else {
            header.ommers_hash == EMPTY_OMMER_ROOT_HASH
        };
        if !ommers_hash_ok {
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
        // The body must still carry no ommers. Since Jenner `ommers_hash` is a commitment, so
        // the backfill path has to enforce emptiness here.
        if let Some(ommers) =
            reth_primitives_traits::BlockBody::ommers(block.body()).filter(|o| !o.is_empty())
        {
            return Err(ConsensusError::BodyOmmersHashDiff(
                GotExpected {
                    got: alloy_consensus::proofs::calculate_ommers_root(ommers),
                    expected: EMPTY_OMMER_ROOT_HASH,
                }
                .into(),
            ));
        }

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

    /// The backfill path reaches ommers validation only here, so the body must still reject
    /// real ommers after Jenner.
    #[test]
    fn block_pre_execution_requires_an_empty_ommers_list() {
        use crate::chainspec::BscChainSpec;
        use crate::consensus::payment_lane::Commitment;
        use crate::node::primitives::BscBlockBody;
        use reth_chainspec::ChainSpecBuilder;
        use reth_primitives_traits::SealedBlock;

        let parlia = Parlia::new(
            Arc::new(BscChainSpec::from(ChainSpecBuilder::mainnet().build())),
            200,
        );
        let block = |ommers: Vec<Header>| {
            let body = BscBlockBody {
                inner: alloy_consensus::BlockBody { ommers, ..Default::default() },
                sidecars: None,
            };
            let header = Header {
                number: 1,
                // Use a commitment so the test proves this path inspects the body, not the field.
                ommers_hash: Commitment { quota: 1, payment_gas_used: 0 }.encode(),
                transactions_root: alloy_consensus::proofs::calculate_transaction_root::<
                    reth_ethereum_primitives::TransactionSigned,
                >(&[]),
                ..Default::default()
            };
            SealedBlock::<BscBlock>::seal_slow(crate::BscBlock { header, body })
        };

        assert!(parlia.validate_block_pre_execution(&block(vec![])).is_ok());
        assert!(matches!(
            parlia.validate_block_pre_execution(&block(vec![Header {
                number: 1,
                ..Default::default()
            }])),
            Err(ConsensusError::BodyOmmersHashDiff(_))
        ));
    }

    /// Since Jenner, this path shape-checks the commitment in `ommers_hash`.
    /// It has no parent, so it gates on the header timestamp.
    #[test]
    fn jenner_ommers_hash_is_shape_checked_not_compared_to_the_empty_root() {
        use crate::chainspec::BscChainSpec;
        use crate::consensus::payment_lane::Commitment;
        use crate::hardforks::bsc::BscHardfork;
        use reth_chainspec::{ChainSpecBuilder, ForkCondition};

        const JENNER: u64 = 1_800_000_000;
        let parlia = Parlia::new(
            Arc::new(BscChainSpec::from(
                ChainSpecBuilder::mainnet()
                    .with_fork(BscHardfork::Jenner, ForkCondition::Timestamp(JENNER))
                    .build(),
            )),
            200,
        );
        // Past London and off-epoch, with just enough extra data to clear `check_header_extra`.
        use crate::consensus::parlia::constants::{EXTRA_SEAL_LEN, EXTRA_VANITY_LEN};
        const BLOCK: u64 = 12_965_001;
        let extra = alloy_primitives::Bytes::from(vec![0u8; EXTRA_VANITY_LEN + EXTRA_SEAL_LEN]);
        let header = |timestamp, ommers_hash| {
            sealed(Header {
                number: BLOCK,
                timestamp,
                ommers_hash,
                extra_data: extra.clone(),
                mix_hash: B256::ZERO,
                ..Default::default()
            })
        };
        let ommers_rejected = |h: &SealedHeader| {
            matches!(
                parlia.validate_unsealed_header_fields(h),
                Err(ConsensusError::BodyOmmersHashDiff(_))
            )
        };

        let commitment = Commitment { quota: 3_000_000, payment_gas_used: 1_000 }.encode();
        let reserved_set = {
            let mut b = commitment.0;
            b[31] = 1;
            B256::from(b)
        };

        // Before Jenner only the empty root passes, commitment shape included.
        assert!(!ommers_rejected(&header(JENNER - 1, EMPTY_OMMER_ROOT_HASH)));
        assert!(ommers_rejected(&header(JENNER - 1, commitment)));

        // From Jenner on, both a well-shaped commitment and EMPTY pass on this path.
        assert!(!ommers_rejected(&header(JENNER, commitment)));
        assert!(!ommers_rejected(&header(JENNER, EMPTY_OMMER_ROOT_HASH)));
        // Reserved bytes set still fails shape validation.
        assert!(ommers_rejected(&header(JENNER, reserved_set)));
    }

    /// Regression guard: sync checks the wall clock, unsealed-header validation does not.
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
