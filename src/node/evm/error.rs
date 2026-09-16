//! Error types for the Bsc EVM module.

use alloy_primitives::{Address, BlockHash, BlockNumber, B256, U256};
use crate::consensus::parlia::error::ParliaConsensusError;
use reth_evm::execute::{BlockExecutionError, BlockValidationError};
use reth_provider::ProviderError;
use reth_primitives_traits::{GotExpected, GotExpectedBoxed};

/// A validation failure before Parlia has authenticated the imported block.
/// Keep this typed across the engine boundary: it invalidates the block, but cannot be used
/// as evidence against the builder named in its untrusted header.
#[derive(Debug, thiserror::Error)]
#[error(transparent)]
pub(crate) struct PreExecutionValidationError(BlockValidationError);

pub(crate) fn mark_pre_execution_error(error: BlockExecutionError) -> BlockExecutionError {
    match error {
        BlockExecutionError::Validation(error) => BlockExecutionError::Validation(
            BlockValidationError::Other(Box::new(PreExecutionValidationError(error))),
        ),
        internal => internal,
    }
}

pub(crate) fn is_execution_evidence(error: &BlockExecutionError) -> bool {
    match error {
        BlockExecutionError::Validation(BlockValidationError::Other(error)) => {
            !error.is::<PreExecutionValidationError>()
        }
        BlockExecutionError::Validation(_) => true,
        BlockExecutionError::Internal(_) => false,
    }
}

#[cfg(test)]
mod bid_block_evidence_tests {
    use super::*;

    #[test]
    fn unauthenticated_seal_errors_cannot_be_evidence() {
        let failures = [
            BscBlockValidationError::WrongHeaderSigner {
                block_number: 1,
                signer: GotExpected {
                    got: Address::with_last_byte(1),
                    expected: Address::with_last_byte(2),
                }
                .into(),
            },
            BscBlockValidationError::SignerUnauthorized {
                block_number: 1,
                proposer: Address::with_last_byte(1),
            },
            BscBlockValidationError::InvalidAttestationSignature,
        ];
        for failure in failures {
            let error: BlockExecutionError = BscBlockExecutionError::Validation(failure).into();
            let marked = mark_pre_execution_error(error);
            assert!(marked.as_validation().is_some(), "must still invalidate the block");
            assert!(!is_execution_evidence(&marked), "must never count unauthenticated evidence");
        }
    }

    #[test]
    fn execution_failures_are_evidence_but_internal_errors_are_not() {
        let error = BlockExecutionError::Validation(
            BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas {
                transaction_gas_limit: 100,
                block_available_gas: 10,
            },
        );
        assert!(is_execution_evidence(&error));
        let error = mark_pre_execution_error(BlockExecutionError::msg("parent state unavailable"));
        assert!(error.as_internal().is_some());
        assert!(!is_execution_evidence(&error));
    }
}

/// BSC specific block validation error
#[derive(thiserror::Error, Debug, Clone)]
pub enum BscBlockValidationError {
    /// Error when the block proposer is in the backoff period
    #[error("block [number={block_number}, hash={hash}] proposer is in the backoff period")]
    FutureBlock {
        /// The block number
        block_number: BlockNumber,
        /// The block hash
        hash: B256,
    },
    
    /// Error when the system txs are more than expected
    #[error("unexpected system tx")]
    UnexpectedSystemTx,

    /// Error when there are normal tx after system tx
    #[error("unexpected normal tx after system tx")]
    UnexpectedNormalTx,

    /// Error when the validators in header are invalid
    #[error("invalid validators in header")]
    InvalidValidators,

    /// Error when the attestation's extra length is too large
    #[error("attestation extra length {extra_len} is too large")]
    TooLargeAttestationExtraLen {
        /// The extra length
        extra_len: usize,
    },

    /// Error when the attestation's target is invalid
    #[error("invalid attestation target: number {block_number}, hash {block_hash}")]
    InvalidAttestationTarget {
        /// The expected and got block number
        block_number: GotExpected<u64>,
        /// The expected and got block hash
        block_hash: GotExpectedBoxed<B256>,
    },

    /// Error when the attestation's source is invalid
    #[error("invalid attestation source: number {block_number}, hash {block_hash}")]
    InvalidAttestationSource {
        /// The expected and got block number
        block_number: GotExpected<u64>,
        /// The expected and got block hash
        block_hash: GotExpectedBoxed<B256>,
    },

    /// Error when the attestation's vote count is invalid
    #[error("invalid attestation vote count: {0}")]
    InvalidAttestationVoteCount(GotExpected<u64>),

    /// Error when the attestation's aggregate signature is invalid
    #[error("invalid attestation signature")]
    InvalidAttestationSignature,

    /// Error when the block's header signer is invalid
    #[error("wrong header signer: block number {block_number}, signer {signer}")]
    WrongHeaderSigner {
        /// The block number
        block_number: BlockNumber,
        /// The expected and got signer address
        signer: GotExpectedBoxed<Address>,
    },

    /// Error when the block signer is not authorized
    #[error("proposer {proposer} at height {block_number} is not authorized")]
    SignerUnauthorized {
        /// The block number
        block_number: BlockNumber,
        /// The proposer address
        proposer: Address,
    },

    /// Error when the block signer is over limit
    #[error("proposer {proposer} is over limit")]
    SignerOverLimit {
        /// The proposer address
        proposer: Address,
    },

    /// Error for invalid block difficulty
    #[error("invalid block difficulty: {difficulty}")]
    InvalidDifficulty {
        /// The block difficulty
        difficulty: U256,
    },

    /// Error for invalid current validators data
    #[error("invalid current validators data")]
    InvalidCurrentValidatorsData,

    /// Error for invalid validators election info data
    #[error("invalid validators election info data")]
    InvalidValidatorsElectionInfoData,

    /// Error when the turn length is different from the calculated turn length
    #[error("mismatching turn length on epoch block")]
    MismatchingEpochTurnLengthError,

    /// Error when encountering a parlia consensus error
    #[error("parlia consensus error: {error}")]
    ParliaConsensusError {
        /// The parlia error.
        #[source]
        error: Box<ParliaConsensusError>,
    },
}

/// Bsc Block Executor Errors
#[derive(thiserror::Error, Debug, Clone)]
pub enum BscBlockExecutionError {
    /// BSC validation error
    #[error(transparent)]
    Validation(#[from] BscBlockValidationError),

    /// Error when there is no snapshot found
    #[error("no snapshot found")]
    SnapshotNotFound,

    /// Error when eth call failed
    #[error("eth call failed")]
    EthCallFailed,

    /// Error when get top validators failed
    #[error("get top validators failed")]
    GetTopValidatorsFailed,

    /// Error when the parent hash of a block is not known.
    #[error("block parent [hash={hash}] is not known")]
    ParentUnknown {
        /// The hash of the unknown parent block.
        hash: BlockHash,
    },

    /// Error when apply snapshot failed
    #[error("apply snapshot failed")]
    ApplySnapshotFailed,

    /// Error when the header is unknown
    #[error("unknown header [hash={block_hash}]")]
    UnknownHeader {
        /// The block hash
        block_hash: B256,
    },

    /// Error when the vote address is not found
    #[error("vote address not found: {address}")]
    VoteAddrNotFoundInSnap {
        /// The vote address
        address: Address,
    },

    /// Error when encountering a blst inner error
    #[error("blst inner error")]
    BLSTInnerError,

    /// Error when encountering a provider inner error
    #[error("provider inner error: {error}")]
    ProviderInnerError {
        /// The provider error.
        #[source]
        error: Box<ProviderError>,
    },

    /// Error when failed to execute system contract upgrade
    #[error("system contract upgrade error")]
    SystemContractUpgradeError,

    /// Error when failed to sign system transaction
    #[error("failed to sign system transaction: {error}")]
    FailedToSignSystemTransaction {
        /// The underlying error message
        error: String,
    },

    /// Error when global signer is not initialized for mining mode
    #[error("global signer not initialized for mining mode")]
    GlobalSignerNotInitializedForMiningMode,
}

impl From<BscBlockExecutionError> for BlockExecutionError {
    fn from(err: BscBlockExecutionError) -> Self {
        // Update execution errors metric for all types of errors
        use once_cell::sync::Lazy;
        use crate::metrics::{BscConsensusMetrics, BscExecutorMetrics};
        static CONSENSUS_METRICS: Lazy<BscConsensusMetrics> = Lazy::new(BscConsensusMetrics::default);
        static EXECUTOR_METRICS: Lazy<BscExecutorMetrics> = Lazy::new(BscExecutorMetrics::default);
        EXECUTOR_METRICS.execution_errors_total.increment(1);

        match err {
            // Transient validation error: the block proposer is in the backoff period
            // and the block will become valid after a short delay. Must NOT be cached
            // in engine-tree's invalid_headers to avoid poisoning the chain.
            BscBlockExecutionError::Validation(
                BscBlockValidationError::FutureBlock { .. }
            ) => Self::other(err),

            BscBlockExecutionError::Validation(validation_err) => {
                // Permanent validation errors: the block genuinely violates consensus
                // rules and will never become valid. Safe to cache in invalid_headers.
                CONSENSUS_METRICS.bad_blocks_total.increment(1);

                // TODO: now use DepositRequestDecode as the validation error carrier,
                // but we should refine it by rewrite some validation error types in reth engine-tree.
                // Note: Validation errors will be identified in the engine-tree and treated as invalid blocks.
                Self::Validation(BlockValidationError::DepositRequestDecode(
                    format!("BSC validation error: {}", validation_err)
                ))
            }

            BscBlockExecutionError::SnapshotNotFound |
            BscBlockExecutionError::EthCallFailed |
            BscBlockExecutionError::GetTopValidatorsFailed |
            BscBlockExecutionError::ParentUnknown { .. } |
            BscBlockExecutionError::ApplySnapshotFailed |
            BscBlockExecutionError::UnknownHeader { .. } |
            BscBlockExecutionError::VoteAddrNotFoundInSnap { .. } |
            BscBlockExecutionError::BLSTInnerError |
            BscBlockExecutionError::ProviderInnerError { .. } |
            BscBlockExecutionError::SystemContractUpgradeError |
            BscBlockExecutionError::FailedToSignSystemTransaction { .. } |
            BscBlockExecutionError::GlobalSignerNotInitializedForMiningMode => {
                // Internal errors: mapped to BlockExecutionError::Internal, which
                // engine-tree handles without caching in invalid_headers.
                Self::other(err)
            }
        }
    }
}
