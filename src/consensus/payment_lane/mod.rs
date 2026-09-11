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
/// Both verdicts are worded exactly as go-bsc words them, down to the shared
/// `payment lane inequality violated` prefix it gets from wrapping one `ErrViolated`. The rule
/// is one two clients have to agree on, and the only way anyone notices they did not is by
/// comparing the two sides' rejections — so the text is an interop surface, not a place to be
/// original. Structured detail belongs in the tracing fields, which carry more than these
/// strings ever could.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LaneError {
    /// The block rule — go-bsc `paymentlane.CheckInequality`.
    #[error(
        "payment lane inequality violated: gas used {gas_used} payment {payment_gas_used} \
         quota {quota} limit {gas_limit}"
    )]
    Violated { gas_limit: u64, gas_used: u64, quota: u64, payment_gas_used: u64 },

    /// A bid left less gas than the reservation needs — the producer-side form of
    /// [`Self::Violated`], go-bsc `LaneState.VerifyPackedBid`.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The two verdicts are an interop surface: when reth and go-bsc disagree about the same
    /// block or the same bid, identical text is what tells an operator it is the same rule and
    /// only the answer differs. Pinned against go-bsc `core/paymentlane/paymentlane.go`
    /// (`CheckInequality`) and `core/payment_lane.go` (`VerifyPackedBid`).
    #[test]
    fn verdicts_are_worded_as_go_bsc_words_them() {
        assert_eq!(
            LaneError::Violated {
                gas_limit: 30_000_000,
                gas_used: 29_000_000,
                quota: 1_500_000,
                payment_gas_used: 21_000,
            }
            .to_string(),
            "payment lane inequality violated: gas used 29000000 payment 21000 \
             quota 1500000 limit 30000000"
        );
        assert_eq!(
            LaneError::BidEatsReservation { idle: 1_479_000, shared: 979_000 }.to_string(),
            "payment lane inequality violated: idle lane 1479000 exceeds the 979000 \
             gas left in the pool"
        );
    }
}
