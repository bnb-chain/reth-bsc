//! Per-block metadata, transaction admission/accounting and final validation.

use super::{
    meta::{self, LaneMeta},
    rules, Budget, LaneError, LaneLiveState, LaneParentState, LaneType,
};
use alloy_consensus::Transaction;
use alloy_primitives::BlockHash;

/// An inactive lane imposes no restrictions, including on the Jenner activation block.
#[derive(Clone, Debug, Default)]
pub struct LaneState(Option<Active>);

#[derive(Clone, Debug)]
struct Active {
    meta: LaneMeta,
    budget: Budget,
    gas_limit: u64,
}

impl LaneState {
    pub const fn off() -> Self {
        Self(None)
    }

    /// Loads parent metadata and derives quota from this block's full gas limit, before reserves.
    /// The caller must gate on the parent being Jenner-active.
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

    /// Classifies using parent membership and live code; returns general while inactive.
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

    /// Books this transaction's gas, not a cumulative total.
    pub fn record_used(&mut self, lane: LaneType, delta: u64) {
        if let Some(active) = self.0.as_mut() {
            active.budget.record_used(lane, delta);
        }
    }

    /// Checks a transaction's declared gas against the remaining producer pool; true if inactive.
    pub fn admits(&self, shared: u64, lane: LaneType, tx_gas_limit: u64) -> bool {
        self.0.as_ref().is_none_or(|a| a.budget.admits(shared, lane, tx_gas_limit))
    }

    /// Checks the complete bid, including pool additions, before finalization.
    /// `shared` excludes system reserves. Unlike `admits(shared, GeneralLane, 0)`,
    /// this rejects an idle reservation larger than the remaining pool.
    pub fn verify_packed_bid(&self, shared: u64) -> Result<(), LaneError> {
        match self.0.as_ref() {
            Some(a) if a.budget.idle_lane() > shared => {
                Err(LaneError::BidEatsReservation { idle: a.budget.idle_lane(), shared })
            }
            _ => Ok(()),
        }
    }

    /// Checks total block gas, including system gas as general; succeeds if inactive.
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

    #[cfg(test)]
    pub(crate) fn for_test(meta: LaneMeta, budget: Budget, gas_limit: u64) -> Self {
        Self(Some(Active { meta, budget, gas_limit }))
    }

    /// Gas reserved for payment transactions.
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
    use alloy_primitives::Address;

    struct NoLiveReads;
    impl LaneLiveState for NoLiveReads {
        fn lane_code_is_empty(&mut self, _: Address) -> Result<bool, LaneError> {
            panic!("the code gate must not run")
        }
    }

    fn active(quota: u64, used: u64, gas_limit: u64) -> LaneState {
        LaneState(Some(Active {
            meta: LaneMeta { ratio: 500, listed: Default::default() },
            budget: Budget { payment_lane_quota: quota, payment_lane_used: used },
            gas_limit,
        }))
    }

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
    }

    #[test]
    fn a_bid_is_held_to_the_per_transaction_invariant() {
        let lane = active(1_500_000, 21_000, 30_000_000);
        assert_eq!(lane.idle_lane(), 1_479_000);

        assert_eq!(lane.verify_packed_bid(1_479_000), Ok(()));
        assert_eq!(
            lane.verify_packed_bid(1_478_999),
            Err(LaneError::BidEatsReservation { idle: 1_479_000, shared: 1_478_999 })
        );

        // Admission implies the bid invariant, except for a zero-gas transaction.
        for shared in [0u64, 1, 21_000, 1_478_999, 1_479_000, 1_500_000, u64::MAX] {
            if lane.admits(shared, LaneType::GeneralLane, 1) {
                assert!(lane.verify_packed_bid(shared).is_ok(), "shared={shared}");
            }
            assert_eq!(lane.verify_packed_bid(shared).is_ok(), lane.idle_lane() <= shared);
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
