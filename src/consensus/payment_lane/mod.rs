//! BEP-703 payment lane.
//!
//! A block reserves gas that only payment transactions may consume. It is an accounting rule,
//! not a region of the block: ordering and pricing are untouched, and nothing reaches the header
//! — the quota is a function of the parent's ratio and this block's gas limit, so every node
//! derives it independently.
//!
//! Rules only; reading `0x2007` and caching the result live in `node/evm/pre_execution.rs`.

pub mod meta;
pub mod rules;

pub use crate::system_contracts::PAYMENT_LANE_CONTRACT;

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

    #[error("corrupt payment lane config: {0}")]
    CorruptConfig(String),

    #[error("payment lane state unavailable: {0}")]
    StateUnavailable(String),
}
