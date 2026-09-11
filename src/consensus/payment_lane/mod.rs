//! BEP-703 payment lane.
//!
//! A block reserves gas that only payment transactions may consume. It is an accounting rule,
//! not a region of the block: ordering and pricing are untouched, and nothing reaches the header
//! — the quota is a function of the parent's ratio and this block's gas limit, so every node
//! derives it independently.
//!
//! Layered like go-bsc: [`rules`] is the arithmetic and the gates (`core/paymentlane`),
//! [`meta`] reads and caches `0x2007` (`core/paymentlanemeta`), and [`state`] is the per-block
//! façade every call site talks to (`core/payment_lane.go`).

pub mod meta;
pub mod rules;
pub mod state;

pub use crate::system_contracts::PAYMENT_LANE_CONTRACT;
use alloy_primitives::{Address, Bytes};

/// The **parent post-state**, reached through `0x2007`'s getters.
///
/// §3.6.4 pins the lane config to the parent's post-state, so every read here must happen before
/// the block being built or imported has mutated anything. The implementor owns that guarantee
/// and answers [`LaneError::StateUnavailable`] once it can no longer honour it — nothing in this
/// module may relax it, and nothing here may reach for state by any other route.
///
/// Deliberately *not* the same trait as [`LaneLiveState`]: the two see different states, and the
/// type is what stops a call site from picking the wrong one.
pub trait LaneParentState {
    /// Runs one read-only PaymentLane getter against the parent post-state.
    fn call_lane_getter(&mut self, to: Address, data: Bytes) -> Result<Bytes, LaneError>;
}

/// The **live state**, as execution advances — a different view from [`LaneParentState`].
///
/// §3.2's code gate is settled at the moment the transaction runs: a destination that gained code
/// earlier in the same block has code by the time a transfer to it is classified. So this must
/// never be memoized per address, and must never be answered out of the parent post-state.
pub trait LaneLiveState {
    /// Whether `addr` has no code *right now*. An absent account counts as empty.
    fn lane_code_is_empty(&mut self, addr: Address) -> Result<bool, LaneError>;
}

/// A stored ratio of `N` reserves `N / RATIO_DENOM` of the gas limit.
pub(crate) const RATIO_DENOM: u64 = 10_000;

/// The lane may never reserve more than 10% of the gas limit, and that bound is not governable.
pub(crate) const MAX_LANE_RATIO: u64 = 1_000;

/// Entries per contract-list page. Matches go-bsc, so both clients spend the same gas walking
/// the list.
pub(crate) const PAGE_SIZE: u64 = 128;

/// The contract's own ceiling on the list. It enforces this on governance writes, so no node
/// treats the count as a validity condition; checked here only to bound the walk.
pub(crate) const MAX_LISTED_CONTRACTS: u64 = 100_000;

/// Gas for one read-only PaymentLane call. Fixed, never the block's gas limit: running out
/// mid-walk is a consensus verdict, so both clients must run out at the same point.
pub(crate) const GETTER_GAS_LIMIT: u64 = 50_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lane {
    General,
    Payment,
}

/// One block's lane: the quota derived before execution, plus the payment gas booked as the
/// block runs. Deliberately not `Copy` — a budget passed by value would drop the accumulation.
#[derive(Clone, Debug, Default)]
pub struct Budget {
    pub quota: u64,
    pub used: u64,
}

/// `StateUnavailable` is a local fault and must never reject a block; every other variant is a
/// verdict on the block. Collapsing the two would make a pruned node reject the whole network.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LaneError {
    #[error(
        "payment lane accounting violated: gas_limit={gas_limit} gas_used={gas_used} \
         quota={quota} payment_gas_used={payment_gas_used}"
    )]
    Violated { gas_limit: u64, gas_used: u64, quota: u64, payment_gas_used: u64 },

    /// The producer-side form of [`Self::Violated`], for a transaction set this node did not pack
    /// itself. Worded exactly as go-bsc's `LaneState.VerifyPackedBid` so one grep covers both
    /// clients' logs.
    #[error(
        "payment lane inequality violated: idle lane {idle} exceeds the {shared} gas left in \
         the pool"
    )]
    PackedBidOverrun { idle: u64, shared: u64 },

    #[error("corrupt payment lane config: {0}")]
    CorruptConfig(String),

    #[error("payment lane state unavailable: {0}")]
    StateUnavailable(String),
}
