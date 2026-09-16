//! BEP-703 reserves gas for payment transactions without changing ordering, pricing or headers.
//! Quota uses the parent's ratio and this block's gas limit.
//!
//! Like go-bsc: [`rules`] handles classification/accounting (`core/paymentlane`), [`meta`] loads
//! and caches getters (`core/paymentlanemeta`), [`state`] tracks a block (`core/payment_lane.go`).

pub mod meta;
pub mod rules;
pub mod state;

pub use crate::system_contracts::PAYMENT_LANE_CONTRACT;
use alloy_primitives::{Address, Bytes};

/// Parent post-state. Implementations must refuse reads after the block mutates that state.
pub trait LaneParentState {
    /// Runs one read-only PaymentLane getter against the parent post-state.
    fn call_lane_getter(&mut self, to: Address, data: Bytes) -> Result<Bytes, LaneError>;
}

/// State at transaction execution, not the parent view. Code checks must not be cached per block.
pub trait LaneLiveState {
    /// Whether `addr` has no code *right now*. An absent account counts as empty.
    fn lane_code_is_empty(&mut self, addr: Address) -> Result<bool, LaneError>;
}

/// A stored ratio of `N` reserves `N / RATIO_DENOM` of the gas limit.
pub(crate) const RATIO_DENOM: u64 = 10_000;

/// The lane may never reserve more than 10% of the gas limit, and that bound is not governable.
pub(crate) const MAX_LANE_RATIO: u64 = 1_000;

/// Entries per getter call, matching go-bsc.
pub(crate) const PAGE_SIZE: u64 = 128;

/// The contract's own ceiling, enforced on governance writes; checked here only to bound the walk.
pub(crate) const MAX_LISTED_CONTRACTS: u64 = 100_000;

/// Fixed getter gas limit, matching go-bsc and independent of block gas limits.
pub(crate) const GETTER_GAS_LIMIT: u64 = 50_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaneType {
    GeneralLane,
    PaymentLane,
}

/// This block's quota and accumulated payment gas.
#[derive(Clone, Debug, Default)]
pub struct Budget {
    pub payment_lane_quota: u64,
    pub payment_lane_used: u64,
}

/// Lane verdicts and local failures; a local failure is not evidence that a block is invalid.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LaneError {
    /// The block rule in [`rules::check_inequality`].
    #[error(
        "payment lane inequality violated: gas used {gas_used} payment {payment_gas_used} \
         quota {quota} limit {gas_limit}"
    )]
    Violated { gas_limit: u64, gas_used: u64, quota: u64, payment_gas_used: u64 },

    /// A bid left less gas than the idle reservation requires.
    #[error(
        "payment lane inequality violated: idle lane {idle} exceeds the {shared} gas left in \
         the pool"
    )]
    BidEatsReservation { idle: u64, shared: u64 },

    #[error("corrupt payment lane config: {0}")]
    CorruptConfig(String),

    #[error("payment lane state unavailable: {0}")]
    StateUnavailable(String),
}
