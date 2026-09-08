//! BEP-703 payment lane.
//!
//! A block reserves `paymentLaneQuota` gas that only payment transactions may consume. It is a
//! gas accounting rule, not a region of the block: ordering and pricing are untouched, and
//! nothing reaches the header — the quota is a pure function of the ratio in the parent's
//! post-state and this block's gas limit, so every node derives it independently.
//!
//! Pure rules only; reading `0x2007` and caching the result live in
//! `src/node/evm/pre_execution.rs`.

pub mod meta;
pub mod rules;

pub use crate::system_contracts::PAYMENT_LANE_CONTRACT;

/// Denominator of the ratio stored in the PaymentLane contract: a stored `N` reserves
/// `N / RATIO_DENOM` of the gas limit. BEP-703 §3.6.1.
pub(crate) const RATIO_DENOM: u64 = 10_000;

/// BEP-703 §3.6.1's upper bound on the ratio — at most 10% of the gas limit.
///
/// A protocol constant, not a governable one: a ceiling governance can raise is not a ceiling.
pub(crate) const MAX_LANE_RATIO: u64 = 1_000;

/// Entries requested per `getPaymentContracts` page.
///
/// Matches go-bsc so both clients spend the same gas walking the list.
pub(crate) const PAGE_SIZE: u64 = 128;

/// Contract-enforced ceiling on the payment contract list (BEP-703 §3.6.1).
///
/// The contract enforces it on governance writes, so no node treats the count as a block
/// validity condition; it is checked here only to bound the walk.
pub(crate) const MAX_LISTED_CONTRACTS: u64 = 100_000;

/// Gas budget for one read-only call into the PaymentLane contract.
///
/// Fixed, never the block's gas limit: a page walk that runs out of gas is a consensus verdict,
/// so both clients must run out at the same point.
pub(crate) const GETTER_GAS_LIMIT: u64 = 50_000_000;

/// Which lane a transaction's gas is booked against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lane {
    General,
    Payment,
}

/// One block's lane: the quota derived before execution, plus the payment gas booked as the
/// block runs.
///
/// Deliberately not `Copy`: a budget passed by value would discard the accumulation.
#[derive(Clone, Debug, Default)]
pub struct Budget {
    pub quota: u64,
    pub used: u64,
}

/// `StateUnavailable` is a local fault and must never reject a block; every other variant is a
/// consensus verdict. Collapsing the two makes a pruned node reject the whole network — the same
/// split go-bsc draws with `ErrStateUnavailable` and its `reportBadBlock` bypass.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LaneError {
    #[error(
        "payment lane accounting violated: gas_limit={gas_limit} gas_used={gas_used} \
         quota={quota} payment_gas_used={payment_gas_used}"
    )]
    Violated { gas_limit: u64, gas_used: u64, quota: u64, payment_gas_used: u64 },

    #[error("corrupt payment lane config: {0}")]
    CorruptConfig(String),

    #[error("payment lane state unavailable: {0}")]
    StateUnavailable(String),
}
