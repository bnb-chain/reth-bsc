use alloy_primitives::{BlockHash, BlockNumber};

use crate::consensus::parlia::VoteAddress;

/// Parlia consensus error.
#[derive(thiserror::Error, Debug, PartialEq, Eq, Clone)]
pub enum ParliaConsensusError {
    /// Error when header extra vanity is missing
    #[error("missing header extra vanity")]
    ExtraVanityMissing,

    /// Error when header extra signature is missing
    #[error("missing header extra signature")]
    ExtraSignatureMissing,

    /// Error when header extra length is invalid
    #[error("header extra length {header_extra_len} is invalid")]
    InvalidHeaderExtraLen {
        /// The validator bytes length
        header_extra_len: u64,
    },

    /// Error when header extra validator bytes length is invalid
    #[error("header extra validator bytes length {validator_bytes_len} is invalid")]
    InvalidHeaderExtraValidatorBytesLen {
        /// Is epoch
        is_epoch: bool,
        /// The validator bytes length
        validator_bytes_len: usize,
    },

    /// Error for invalid header extra
    #[error("invalid header extra")]
    InvalidHeaderExtra,

    /// Error when the header is not in epoch
    #[error("{block_number} is not in epoch")]
    NotInEpoch {
        /// The block number
        block_number: BlockNumber,
    },

    /// Error when encountering a recover ecdsa inner error
    #[error("recover ecdsa inner error")]
    RecoverECDSAInnerError,

    /// Error when header extra turn is invalid
    #[error("invalid turnLength")]
    ExtraInvalidTurnLength,

    /// Error when header extra attestation is invalid
    #[error("invalid attestation")]
    ExtraInvalidAttestation,

    /// Error when header extra attestation is invalid
    #[error("fetch vote error")]
    FetchVoteError {
        address: VoteAddress,
    },

    /// Error when aggregate signature failed
    #[error("aggregate signature failed")]
    AggregateSignatureError,

    /// Error when invalid attestation vote count
    #[error("invalid attestation vote count")]
    InvalidAttestationVoteCount {
        got: u32,
        expected: u32,
    },

    /// Error when turn length is not found
    #[error("turn length not found")]
    TurnLengthNotFound {
        block_hash: BlockHash,
    },
}