//! Error types for the Bsc EVM module.

use alloy_primitives::{Address, BlockHash, BlockNumber, B256, U256};
use crate::consensus::{parlia::error::ParliaConsensusError, payment_lane::LaneError};
use reth_evm::execute::{BlockExecutionError, BlockValidationError};
use reth_provider::ProviderError;
use reth_primitives_traits::{GotExpected, GotExpectedBoxed};

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
    
    /// A payment lane rule the block breaks.
    #[error("{0}")]
    PaymentLane(String),

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

    /// A local payment lane state read failed on this node.
    #[error("payment lane state unavailable: {0}")]
    PaymentLaneStateUnavailable(String),

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

/// Routes payment lane failures to the local-fault or block-verdict side.
pub fn lane_reject(err: LaneError) -> BscBlockExecutionError {
    use crate::metrics::LANE_METRICS as METRICS;

    let local_fault = matches!(err, LaneError::StateUnavailable(_));
    if local_fault {
        METRICS.state_unavailable.increment(1);
    } else {
        METRICS.rejected.increment(1);
    }
    let category = if local_fault { "stateUnavailable" } else { "verdict" };
    tracing::error!(target: "bsc::payment_lane", category, error = %err, "payment lane check failed");
    BscBlockExecutionError::from(err)
}

impl From<LaneError> for BscBlockExecutionError {
    fn from(err: LaneError) -> Self {
        match err {
            LaneError::StateUnavailable(reason) => Self::PaymentLaneStateUnavailable(reason),
            verdict => BscBlockValidationError::PaymentLane(verdict.to_string()).into(),
        }
    }
}

/// Lets `?` lift `LaneError` into `BlockExecutionError` directly.
impl From<LaneError> for BlockExecutionError {
    fn from(err: LaneError) -> Self {
        BscBlockExecutionError::from(err).into()
    }
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

            BscBlockExecutionError::PaymentLaneStateUnavailable(_) |
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
#[cfg(test)]
mod tests {
    use super::*;
    use crate::consensus::payment_lane::LaneError;

    #[test]
    fn payment_lane_errors_split_local_faults_from_verdicts() {
        let local: BlockExecutionError =
            LaneError::StateUnavailable("missing trie node".into()).into();
        assert!(
            matches!(local, BlockExecutionError::Internal(_)),
            "a failed state read must not be a validation error: {local:?}"
        );

        for verdict in [
            LaneError::CorruptConfig("total changed mid-walk".into()),
            LaneError::QuotaMismatch { committed: 1, derived: 2 },
            LaneError::Untruthy { committed: 1, actual: 2 },
            LaneError::BadCommitment(B256::ZERO),
            LaneError::Violated { gas_limit: 1, gas_used: 2, quota: 3, payment_gas_used: 4 },
        ] {
            let message = verdict.to_string();
            let mapped: BlockExecutionError = verdict.into();
            match mapped {
                BlockExecutionError::Validation(ref inner) => assert!(
                    inner.to_string().contains(&message),
                    "the LaneError message must survive: {inner}"
                ),
                other => panic!("expected a validation error, got {other:?}"),
            }
        }
    }
}
