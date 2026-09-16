//! One block's lane, and the only surface the rest of the node talks to — go-bsc's
//! `core/payment_lane.go`. [`LaneState::resolve`] once per block, [`LaneState::classify`] and
//! [`LaneState::record_used`] per transaction, [`LaneState::verify`] on the finished block; a
//! producer also gates on [`LaneState::admits`] or [`LaneState::verify_packed_bid`].

use super::{
    meta::{self, LaneMeta},
    rules, Budget, LaneError, LaneLiveState, LaneParentState, LaneType,
};
use alloy_consensus::Transaction;
use alloy_primitives::BlockHash;

/// `None` is "the lane does not bind here" — before Jenner, and on the activation block, whose
/// parent is still pre-fork. Every verb then answers as if switched off, so no call site branches
/// on it; go-bsc gets the same from a nil-safe `*LaneState` plus `On()`.
#[derive(Clone, Debug, Default)]
pub struct LaneState(Option<Active>);

#[derive(Clone, Debug)]
struct Active {
    meta: LaneMeta,
    budget: Budget,
    /// The quota came from it, and the verdict is against it.
    gas_limit: u64,
}

impl LaneState {
    /// The lane switched off.
    pub const fn off() -> Self {
        Self(None)
    }

    /// Reads `0x2007` as of the parent's post-state and derives this block's quota.
    ///
    /// Gate on the **parent** being Jenner-active: the fork installs `0x2007` while the activation
    /// block executes, so `activation + 1` is the first block that reserves. `gas_limit` is
    /// **this** block's, and the sealed one rather than the miner's reservation-adjusted one —
    /// producer and importer must derive the same number.
    pub fn resolve(
        access: &mut impl LaneParentState,
        parent_hash: BlockHash,
        gas_limit: u64,
    ) -> Result<Self, LaneError> {
        let meta = meta::load(access, parent_hash)?;
        let payment_lane_quota = rules::quota(meta.ratio, gas_limit);
        let budget = Budget { payment_lane_quota, payment_lane_used: 0 };
        Ok(Self(Some(Active { meta, budget, gas_limit })))
    }

    /// Whether the lane binds on this block.
    pub fn on(&self) -> bool {
        self.0.is_some()
    }

    /// Which lane this transaction's gas is booked against; `GeneralLane` while the lane is off.
    ///
    /// `live` must be the state as execution has reached this transaction, never the parent
    /// post-state the config came from — see [`LaneLiveState`].
    pub fn classify(
        &self,
        live: &mut impl LaneLiveState,
        is_system: bool,
        tx: &impl Transaction,
    ) -> Result<LaneType, LaneError> {
        let Some(active) = self.0.as_ref() else { return Ok(LaneType::GeneralLane) };
        rules::classify(is_system, tx.to(), tx.ty(), tx.value(), &active.meta.listed, |addr| {
            live.lane_code_is_empty(addr)
        })
    }

    /// Books `delta` gas against `lane`.
    ///
    /// go-bsc's `RecordUsedFrom(lane, gasPool, usedBefore)` subtracts a running total because
    /// geth's `GasPool` is what its executor mutates; here `delta` is the transaction's own gas.
    pub fn record_used(&mut self, lane: LaneType, delta: u64) {
        if let Some(active) = self.0.as_mut() {
            active.budget.record_used(lane, delta);
        }
    }

    /// Whether one more transaction fits, with `shared` the producer's remaining pool. Always
    /// true while the lane is off.
    pub fn admits(&self, shared: u64, lane: LaneType, tx_gas_limit: u64) -> bool {
        self.0.as_ref().is_none_or(|a| a.budget.admits(shared, lane, tx_gas_limit))
    }

    /// `idle <= shared` — the invariant [`Self::admits`] maintains transaction by transaction,
    /// asserted once over a set the BEP-322 builder fixed and the producer never got to filter.
    /// Runs before finalization.
    ///
    /// `shared` is the producer's remaining pool (`GasLimit - reserved - used`), as in
    /// [`Self::admits`] — not the header's gas used, which would hand the bid whatever the system
    /// reservation left unspent. Spelled out rather than `admits(shared, GeneralLane, 0)`, which
    /// is vacuous: a zero-gas transaction fits the zero allowance `admits` saturates to.
    pub fn verify_packed_bid(&self, shared: u64) -> Result<(), LaneError> {
        match self.0.as_ref() {
            Some(a) if a.budget.idle_lane() > shared => {
                Err(LaneError::BidEatsReservation { idle: a.budget.idle_lane(), shared })
            }
            _ => Ok(()),
        }
    }

    /// The verdict on a finished block. `gas_used` must be the header's total, so Parlia's
    /// system gas counts as general. Always `Ok` while the lane is off.
    pub fn verify(&self, gas_used: u64) -> Result<(), LaneError> {
        match self.0.as_ref() {
            Some(a) => a.budget.verify(a.gas_limit, gas_used),
            None => Ok(()),
        }
    }

    /// Hands this block's config to its children. Only for a block that left `0x2007` alone:
    /// after a governance write the config it ran under is not the one its children see.
    pub fn inherit_to(&self, block_hash: BlockHash) {
        if let Some(active) = self.0.as_ref() {
            meta::cache_store(block_hash, &active.meta);
        }
    }

    /// Test-only: an active lane without reading `0x2007`.
    #[cfg(test)]
    pub fn for_test(meta: LaneMeta, budget: Budget, gas_limit: u64) -> Self {
        Self(Some(Active { meta, budget, gas_limit }))
    }

    /// This and the four accessors below are the diagnostics: nothing about the reservation
    /// reaches the header, so two nodes disagreeing can only be compared on these.
    pub fn quota(&self) -> u64 {
        self.0.as_ref().map_or(0, |a| a.budget.payment_lane_quota)
    }

    /// Payment gas booked so far.
    pub fn used(&self) -> u64 {
        self.0.as_ref().map_or(0, |a| a.budget.payment_lane_used)
    }

    /// Reserved gas no payment transaction has claimed.
    pub fn idle_lane(&self) -> u64 {
        self.0.as_ref().map_or(0, |a| a.budget.idle_lane())
    }

    /// The governable ratio this block's quota came from.
    pub fn ratio(&self) -> u64 {
        self.0.as_ref().map_or(0, |a| a.meta.ratio)
    }

    /// How many payment contracts the parent post-state listed.
    pub fn listed_len(&self) -> usize {
        self.0.as_ref().map_or(0, |a| a.meta.listed.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::TxLegacy;
    use alloy_primitives::{Address, Bytes};

    struct NoLiveReads;
    impl LaneLiveState for NoLiveReads {
        fn lane_code_is_empty(&mut self, _: Address) -> Result<bool, LaneError> {
            panic!("the code gate must not run")
        }
    }

    struct NoParentReads;
    impl LaneParentState for NoParentReads {
        fn call_lane_getter(&mut self, _: Address, _: Bytes) -> Result<Bytes, LaneError> {
            panic!("the getter must not run")
        }
    }

    fn active(quota: u64, used: u64, gas_limit: u64) -> LaneState {
        LaneState(Some(Active {
            meta: LaneMeta { ratio: 500, listed: Default::default() },
            budget: Budget { payment_lane_quota: quota, payment_lane_used: used },
            gas_limit,
        }))
    }

    /// Every verb has to answer for a lane that does not bind, or each call site grows its own
    /// pre-Jenner branch and they drift.
    #[test]
    fn an_off_lane_answers_every_verb() {
        let mut off = LaneState::off();
        assert!(!off.on());
        assert_eq!(
            off.classify(&mut NoLiveReads, false, &TxLegacy::default()),
            Ok(LaneType::GeneralLane)
        );
        off.record_used(LaneType::PaymentLane, 21_000);
        assert_eq!(off.used(), 0);
        assert!(off.admits(0, LaneType::GeneralLane, u64::MAX));
        assert_eq!(off.verify_packed_bid(0), Ok(()));
        assert_eq!(off.verify(u64::MAX), Ok(()));
        off.inherit_to(BlockHash::ZERO);
        assert_eq!((off.quota(), off.idle_lane(), off.ratio(), off.listed_len()), (0, 0, 0, 0));
        let _ = NoParentReads; // `resolve` is the one verb an off lane cannot answer.
    }

    /// The bid verdict and the per-transaction gate are one inequality.
    #[test]
    fn a_bid_is_held_to_the_per_transaction_invariant() {
        let lane = active(1_500_000, 21_000, 30_000_000);
        assert_eq!(lane.idle_lane(), 1_479_000);

        // Exactly the reservation left is still leaving it alone; one gas less is not.
        assert_eq!(lane.verify_packed_bid(1_479_000), Ok(()));
        assert_eq!(
            lane.verify_packed_bid(1_478_999),
            Err(LaneError::BidEatsReservation { idle: 1_479_000, shared: 1_478_999 })
        );

        // Agrees with the gate wherever the gate is not vacuous, so a bid and a locally packed
        // block face the same ceiling.
        for shared in [0u64, 1, 21_000, 1_478_999, 1_479_000, 1_500_000, u64::MAX] {
            if lane.admits(shared, LaneType::GeneralLane, 1) {
                assert!(lane.verify_packed_bid(shared).is_ok(), "shared={shared}");
            }
            assert_eq!(lane.verify_packed_bid(shared).is_ok(), lane.idle_lane() <= shared);
            // And why the zero-gas spelling of the same question would not do.
            assert!(lane.admits(shared, LaneType::GeneralLane, 0));
        }
    }

    #[test]
    fn verify_uses_the_blocks_own_gas_limit() {
        let lane = active(1_500_000, 0, 30_000_000);
        assert_eq!(lane.verify(28_500_000), Ok(()));
        assert!(lane.verify(28_500_001).is_err());
    }
}
