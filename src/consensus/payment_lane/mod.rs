//! BEP-703 payment lane.
//!
//! A block reserves `paymentLaneQuota` gas that only payment transactions may consume.
//! The reservation is a gas accounting rule, not a region of the block: ordering and pricing
//! are untouched. The quota is an accumulator committed into `header.ommers_hash`, so deriving
//! it needs nothing older than the parent header.
//!
//! The rule layer: arithmetic, classification and the commitment codec. No provider, no state,
//! no EVM — the reading and caching live in `src/node/evm/pre_execution.rs`.

pub mod meta;
pub mod rules;

pub use crate::{
    consensus::parlia::constants::SYSTEM_TXS_GAS_HARD_LIMIT,
    system_contracts::PAYMENT_LANE_CONTRACT,
};
use alloy_primitives::B256;

/// Denominator of every ratio parameter read from the PaymentLane contract.
pub(crate) const RATIO_DENOM: u64 = 10_000;

/// Entries requested per `getPaymentContracts` page.
///
/// Matches go-bsc so both clients spend the same gas walking the list.
pub(crate) const PAGE_SIZE: u64 = 128;

/// Contract-enforced ceiling on the payment contract list.
pub(crate) const MAX_LISTED_CONTRACTS: u64 = 100_000;

/// Gas budget for one read-only call into the PaymentLane contract.
///
/// Fixed, never the block's gas limit: a page walk that runs out of gas is a consensus
/// verdict, so both clients must run out at the same point.
pub const GETTER_GAS_LIMIT: u64 = 50_000_000;

/// The eight governable values, in the contract's slot order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GovernanceParams {
    pub min_ratio: u64,
    pub max_ratio: u64,
    pub expand_trigger: u64,
    pub shrink_trigger: u64,
    pub expand_step: u64,
    pub shrink_step: u64,
    pub min_gas: u64,
    pub max_gas: u64,
}

/// Which lane a transaction's gas is booked against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lane {
    General,
    Payment,
}

/// The two values a block commits into `header.ommers_hash`.
///
/// `generalGasUsed` needs no field: it is the residual `header.gas_used - payment_gas_used`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Commitment {
    pub quota: u64,
    pub payment_gas_used: u64,
}

/// How congested the parent was — all the quota recurrence needs.
///
/// `None` means the parent's `ommers_hash` is still `EMPTY_OMMER_ROOT_HASH`. A parent with a
/// commitment but `gas_limit == 0` is still `Some` — see `ParentSignal::stepped`.
#[derive(Clone, Copy, Debug)]
pub struct Signal(Option<ParentSignal>);

/// Kept private: a forged `None` seed silently resets the accumulator to the floor and never
/// reconverges. `rules` is a child module and sees these fields without a modifier.
#[derive(Clone, Copy, Debug)]
struct ParentSignal {
    lane_quota: u64,
    signal_gas_used: u64,
    gas_limit: u64,
}

/// Deliberately not `Copy`: a budget passed by value would discard the accumulation.
#[derive(Clone, Debug, Default)]
pub struct Budget {
    pub quota: u64,
    pub used: u64,
}

/// The three clamps `next_lane_quota` applies.
#[derive(Clone, Copy, Debug)]
pub struct Bounds {
    pub floor: u64,
    pub ceiling: u64,
    pub reserve_cap: u64,
}

/// The two ways `header.ommers_hash` can fail once the parent is known.
///
/// Kept apart because a commitment before the fork must surface as Parlia's own uncle-hash
/// error, not as a payment lane error — go-bsc's sentinel for that case is `errInvalidUncleHash`.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum OmmersHashError {
    #[error("ommers_hash {0} is not empty before the payment lane activates")]
    CommitmentBeforeFork(B256),

    #[error(transparent)]
    Lane(#[from] LaneError),
}

/// `StateUnavailable` is a local fault and must never reject a block; every other variant is a
/// consensus verdict. Collapsing the two makes a pruned node reject the whole network.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LaneError {
    #[error(
        "payment lane accounting violated: gas_limit={gas_limit} gas_used={gas_used} \
         quota={quota} payment_gas_used={payment_gas_used}"
    )]
    Violated { gas_limit: u64, gas_used: u64, quota: u64, payment_gas_used: u64 },

    #[error("malformed payment lane commitment: reserved bytes are non-zero ({0})")]
    BadCommitment(B256),

    #[error("untruthful payment lane commitment: committed={committed} actual={actual}")]
    Untruthy { committed: u64, actual: u64 },

    #[error("payment lane quota mismatch: committed={committed} derived={derived}")]
    QuotaMismatch { committed: u64, derived: u64 },

    #[error("corrupt payment lane config: {0}")]
    CorruptConfig(String),

    #[error("payment lane state unavailable: {0}")]
    StateUnavailable(String),
}

/// BEP-703 §3.6.1's values for a parameter governance never wrote. For tests and cross-client
/// fixtures only: production reads the contract's getter, which applies them itself.
pub const DEFAULT_PARAMS: GovernanceParams = GovernanceParams {
    min_ratio: 200,
    max_ratio: 800,
    expand_trigger: 8_000,
    shrink_trigger: 7_000,
    expand_step: 200,
    shrink_step: 50,
    min_gas: 2_000_000,
    max_gas: 8_000_000,
};
