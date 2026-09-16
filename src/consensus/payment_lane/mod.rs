//! BEP-703 payment lane: a block reserves gas only payment transactions may consume.
//!
//! Pure accounting — ordering and pricing are untouched and nothing reaches the header, so every
//! node derives the quota from the parent's ratio and this block's gas limit.
//!
//! Layered like go-bsc: [`rules`] the arithmetic and gates (`core/paymentlane`), [`meta`] the
//! `0x2007` reads and their cache (`core/paymentlanemeta`), [`state`] the per-block façade
//! (`core/payment_lane.go`).

pub mod meta;
pub mod rules;
pub mod state;

pub use crate::system_contracts::PAYMENT_LANE_CONTRACT;
use alloy_primitives::{Address, Bytes};

/// The parent post-state, reached through `0x2007`'s getters.
///
/// Every read must precede any mutation by the block being built or imported; the implementor
/// enforces that with [`LaneError::StateUnavailable`].
pub trait LaneParentState {
    /// Runs one read-only PaymentLane getter against the parent post-state.
    fn call_lane_getter(&mut self, to: Address, data: Bytes) -> Result<Bytes, LaneError>;
}

/// The live state as execution advances — a different view from [`LaneParentState`].
///
/// The code gate is settled when the transaction runs, so this must never be memoized per
/// address nor answered out of the parent post-state.
pub trait LaneLiveState {
    /// Whether `addr` has no code *right now*. An absent account counts as empty.
    fn lane_code_is_empty(&mut self, addr: Address) -> Result<bool, LaneError>;
}

/// A stored ratio of `N` reserves `N / RATIO_DENOM` of the gas limit.
pub(crate) const RATIO_DENOM: u64 = 10_000;

/// The lane may never reserve more than 10% of the gas limit, and that bound is not governable.
pub(crate) const MAX_LANE_RATIO: u64 = 1_000;

/// Entries per contract-list page. Matches go-bsc, so both clients spend the same gas walking it.
pub(crate) const PAGE_SIZE: u64 = 128;

/// The contract's own ceiling, enforced on governance writes; checked here only to bound the walk.
pub(crate) const MAX_LISTED_CONTRACTS: u64 = 100_000;

/// Gas for one getter call. Fixed, not the block's limit: running out mid-walk is a verdict both
/// clients must reach at the same point.
pub(crate) const GETTER_GAS_LIMIT: u64 = 50_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaneType {
    GeneralLane,
    PaymentLane,
}

/// The quota derived before execution, plus the payment gas booked as the block runs. Not `Copy`:
/// a budget passed by value would drop the accumulation.
#[derive(Clone, Debug, Default)]
pub struct Budget {
    pub payment_lane_quota: u64,
    pub payment_lane_used: u64,
}

/// `StateUnavailable` is a local fault and must never reject a block — collapsing it into the
/// verdicts would make a pruned node reject the whole network. The verdicts are worded exactly as
/// go-bsc words them, down to the shared `payment lane inequality violated` prefix.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LaneError {
    /// The block rule — [`rules::check_inequality`].
    #[error(
        "payment lane inequality violated: gas used {gas_used} payment {payment_gas_used} \
         quota {quota} limit {gas_limit}"
    )]
    Violated { gas_limit: u64, gas_used: u64, quota: u64, payment_gas_used: u64 },

    /// A bid left less gas than the reservation needs — the producer-side form of
    /// [`Self::Violated`].
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
