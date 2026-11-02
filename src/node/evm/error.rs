//! Error types for the Bsc EVM module.

use crate::consensus::{parlia::error::ParliaConsensusError, payment_lane::LaneError};
use alloy_primitives::{Address, BlockHash, BlockNumber, B256, U256};
use reth_evm::execute::{BlockExecutionError, BlockValidationError};
use reth_primitives_traits::{GotExpected, GotExpectedBoxed};
use reth_provider::ProviderError;

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

/// Routes a payment lane failure to the local-fault side or the block-verdict side, counting
/// and logging it on the way.
pub fn lane_reject(err: LaneError) -> BlockExecutionError {
    let metrics = &crate::metrics::LANE_METRICS;
    let (counter, category) = match err {
        LaneError::StateUnavailable(_) => (&metrics.state_unavailable, "stateUnavailable"),
        _ => (&metrics.rejected, "verdict"),
    };
    counter.increment(1);
    tracing::error!(target: "bsc::payment_lane", category, error = %err, "payment lane check failed");

    match err {
        LaneError::StateUnavailable(reason) => {
            BscBlockExecutionError::PaymentLaneStateUnavailable(reason)
        }
        verdict => BscBlockValidationError::PaymentLane(verdict.to_string()).into(),
    }
    .into()
}

impl From<BscBlockExecutionError> for BlockExecutionError {
    fn from(err: BscBlockExecutionError) -> Self {
        // Update execution errors metric for all types of errors
        use crate::metrics::{BscConsensusMetrics, BscExecutorMetrics};
        use once_cell::sync::Lazy;
        static CONSENSUS_METRICS: Lazy<BscConsensusMetrics> =
            Lazy::new(BscConsensusMetrics::default);
        static EXECUTOR_METRICS: Lazy<BscExecutorMetrics> = Lazy::new(BscExecutorMetrics::default);
        EXECUTOR_METRICS.execution_errors_total.increment(1);

        match err {
            // Transient validation error: the block proposer is in the backoff period
            // and the block will become valid after a short delay. Must NOT be cached
            // in engine-tree's invalid_headers to avoid poisoning the chain.
            BscBlockExecutionError::Validation(BscBlockValidationError::FutureBlock { .. }) => {
                Self::other(err)
            }

            BscBlockExecutionError::Validation(validation_err) => {
                // Permanent validation errors: the block genuinely violates consensus
                // rules and will never become valid. Safe to cache in invalid_headers.
                CONSENSUS_METRICS.bad_blocks_total.increment(1);

                // TODO: now use DepositRequestDecode as the validation error carrier,
                // but we should refine it by rewrite some validation error types in reth
                // engine-tree. Note: Validation errors will be identified in the
                // engine-tree and treated as invalid blocks.
                Self::Validation(BlockValidationError::DepositRequestDecode(format!(
                    "BSC validation error: {}",
                    validation_err
                )))
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

    #[test]
    fn payment_lane_errors_split_local_faults_from_verdicts() {
        let local = lane_reject(LaneError::StateUnavailable("missing trie node".into()));
        assert!(
            matches!(local, BlockExecutionError::Internal(_)),
            "a failed state read must not be a validation error: {local:?}"
        );

        for verdict in [
            LaneError::CorruptConfig("total changed mid-walk".into()),
            LaneError::Violated { gas_limit: 1, gas_used: 2, quota: 3, payment_gas_used: 4 },
        ] {
            let message = verdict.to_string();
            match lane_reject(verdict) {
                BlockExecutionError::Validation(ref inner) => assert!(
                    inner.to_string().contains(&message),
                    "the LaneError message must survive: {inner}"
                ),
                other => panic!("expected a validation error, got {other:?}"),
            }
        }
    }
}

use evm2::registry::HandlerError;
use reth_evm::InvalidTxError;
/// Retains native validation categories used by the miner's pool traversal.
#[derive(Debug, thiserror::Error)]
#[error(transparent)]
pub(super) struct InvalidNativeTransaction(pub HandlerError);

impl InvalidTxError for InvalidNativeTransaction {
    fn is_nonce_too_low(&self) -> bool {
        matches!(self.0, HandlerError::InvalidNonce { expected, got } if got < expected)
    }
    fn is_gas_limit_too_high(&self) -> bool {
        matches!(
            self.0,
            HandlerError::GasLimitMoreThanBlock { .. } |
                HandlerError::TxGasLimitGreaterThanCap { .. }
        )
    }
    fn is_gas_limit_too_low(&self) -> bool {
        matches!(self.0, HandlerError::IntrinsicGasTooLow { .. })
    }
    fn as_any(&self) -> &(dyn std::any::Any + 'static) {
        self
    }
}

pub(super) fn with_transaction_hash(error: BlockExecutionError, hash: B256) -> BlockExecutionError {
    match error {
        BlockExecutionError::Validation(BlockValidationError::Other(error)) => {
            match error.downcast::<HandlerError>() {
                Ok(error) => BlockValidationError::InvalidTx {
                    hash,
                    error: Box::new(InvalidNativeTransaction(*error)),
                }
                .into(),
                Err(error) => BlockValidationError::Other(error).into(),
            }
        }
        error => error,
    }
}

#[cfg(test)]
mod native_transaction_error_tests {
    use super::*;

    #[test]
    fn miner_validation_categories_survive_native_error_conversion() {
        let hash = B256::repeat_byte(0x11);
        let cases = [
            (HandlerError::InvalidNonce { expected: 3, got: 2 }, true, false, false),
            (HandlerError::InvalidNonce { expected: 3, got: 4 }, false, false, false),
            (
                HandlerError::GasLimitMoreThanBlock {
                    gas_limit: 100,
                    block_gas_limit: U256::from(90),
                },
                false,
                true,
                false,
            ),
            (
                HandlerError::TxGasLimitGreaterThanCap { gas_limit: 100, cap: 90 },
                false,
                true,
                false,
            ),
            (
                HandlerError::IntrinsicGasTooLow { required: 21_000, got: 20_000 },
                false,
                false,
                true,
            ),
        ];
        for (error, nonce_low, gas_high, gas_low) in cases {
            let error =
                with_transaction_hash(BlockValidationError::Other(Box::new(error)).into(), hash);
            let BlockExecutionError::Validation(BlockValidationError::InvalidTx {
                hash: actual,
                error,
            }) = error
            else {
                panic!("native transaction category lost");
            };
            assert_eq!(actual, hash);
            assert_eq!(error.is_nonce_too_low(), nonce_low);
            assert_eq!(error.is_gas_limit_too_high(), gas_high);
            assert_eq!(error.is_gas_limit_too_low(), gas_low);
        }
    }

    #[test]
    fn provider_failure_is_not_misclassified_as_invalid_transaction() {
        let error = with_transaction_hash(
            BlockExecutionError::msg("state provider unavailable"),
            B256::ZERO,
        );
        assert!(error.as_internal().is_some());
        assert!(error.to_string().contains("state provider unavailable"));
        let other =
            BlockValidationError::Other(Box::new(std::io::Error::other("non-native error")));
        let error = with_transaction_hash(other.into(), B256::ZERO);
        assert!(matches!(error, BlockExecutionError::Validation(BlockValidationError::Other(_))));
    }
}
