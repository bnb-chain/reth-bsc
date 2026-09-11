//! One block's lane, and the only surface the rest of the node talks to.
//!
//! go-bsc's `core/payment_lane.go`: the same six verbs, in the same order of use —
//! [`LaneState::resolve`] once per block, then [`LaneState::classify`] /
//! [`LaneState::record_used`] per transaction, [`LaneState::admits`] or
//! [`LaneState::verify_reservation_intact`] wherever a producer decides what goes in, and
//! [`LaneState::verify`] on the finished block.

use super::{
    meta::{self, LaneMeta},
    rules, Budget, Lane, LaneError, LaneLiveState, LaneParentState,
};
use alloy_primitives::{Address, BlockHash, U256};

/// One block's lane: the `0x2007` snapshot read from the parent's post-state, the quota derived
/// from it, and the payment gas booked as the block runs.
///
/// `None` is "the lane does not bind here" — before Jenner, and on the activation block itself,
/// whose parent is still pre-fork. Every verb then answers as if the lane were switched off, so
/// no call site branches on it; go-bsc gets the same from a nil-safe `*LaneState` plus `On()`.
#[derive(Clone, Debug, Default)]
pub struct LaneState(Option<Active>);

#[derive(Clone, Debug)]
struct Active {
    meta: LaneMeta,
    budget: Budget,
    /// This block's own gas limit: the quota came from it, and the verdict is against it.
    gas_limit: u64,
}

impl LaneState {
    /// The lane switched off.
    pub const fn off() -> Self {
        Self(None)
    }

    /// Reads `0x2007` as of the parent's post-state and derives this block's quota.
    ///
    /// Gate the call on the **parent** being Jenner-active: the fork installs `0x2007` while the
    /// activation block executes, so `activation + 1` is the first block that reserves.
    ///
    /// `gas_limit` is **this** block's (§3.4.1), and the sealed one rather than the miner's
    /// reservation-adjusted one — producer and importer must derive the same number.
    pub fn resolve(
        access: &mut impl LaneParentState,
        parent_hash: BlockHash,
        gas_limit: u64,
    ) -> Result<Self, LaneError> {
        let meta = meta::load(access, parent_hash)?;
        let budget = Budget { quota: rules::quota(meta.ratio, gas_limit), used: 0 };
        Ok(Self(Some(Active { meta, budget, gas_limit })))
    }

    /// Whether the lane binds on this block.
    pub fn on(&self) -> bool {
        self.0.is_some()
    }

    /// Which lane this transaction's gas is booked against; `General` while the lane is off.
    ///
    /// `live` is the state **as execution has reached this transaction**, never the parent
    /// post-state the config came from: §3.2's code gate is settled at the moment the
    /// transaction runs.
    pub fn classify(
        &self,
        live: &mut impl LaneLiveState,
        is_system: bool,
        to: Option<Address>,
        tx_type: u8,
        value: U256,
    ) -> Result<Lane, LaneError> {
        let Some(active) = self.0.as_ref() else { return Ok(Lane::General) };
        rules::classify(is_system, to, tx_type, value, &active.meta.listed, |addr| {
            live.lane_code_is_empty(addr)
        })
    }

    /// Books `delta` gas against `lane`.
    pub fn record_used(&mut self, lane: Lane, delta: u64) {
        if let Some(active) = self.0.as_mut() {
            active.budget.record_used(lane, delta);
        }
    }

    /// Whether one more transaction fits, with `shared` the producer's remaining pool. Always
    /// true while the lane is off.
    pub fn admits(&self, shared: u64, lane: Lane, tx_gas_limit: u64) -> bool {
        self.0.as_ref().is_none_or(|a| a.budget.admits(shared, lane, tx_gas_limit))
    }

    /// Whether the reservation survived a transaction set this node did not pick itself — the
    /// whole-set form of [`Self::admits`], and the only lane check a producer can make when it
    /// cannot drop anything. go-bsc calls it `LaneState.VerifyPackedBid`, and the error reads
    /// word for word the same.
    ///
    /// Used on the BEP-322 bid path (and by `debug_buildCandidateBlock`, which reproduces it):
    /// the builder fixes the transactions, so they are accounted one by one and then judged
    /// once, here, before finalization.
    ///
    /// `shared` is the producer's remaining pool (`GasLimit - reserved - used`), the same unit
    /// [`Self::admits`] takes. Not the block rule: that is the importer's verdict, and counting
    /// the system transactions' actual gas there would hand the set the unused part of the
    /// system reservation.
    pub fn verify_reservation_intact(&self, shared: u64) -> Result<(), LaneError> {
        match self.0.as_ref() {
            Some(a) if !a.budget.reservation_intact(shared) => {
                Err(LaneError::ReservationOverrun { idle: a.budget.idle(), shared })
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
            meta::cache_inherit(block_hash, &active.meta);
        }
    }

    /// Test-only: an active lane without reading `0x2007`.
    #[cfg(test)]
    pub fn for_test(meta: LaneMeta, budget: Budget, gas_limit: u64) -> Self {
        Self(Some(Active { meta, budget, gas_limit }))
    }

    /// Everything the verdict was derived from, for diagnostics: nothing about the reservation
    /// reaches the header, so a disagreement between two nodes can only be read off these.
    pub fn quota(&self) -> u64 {
        self.0.as_ref().map_or(0, |a| a.budget.quota)
    }

    /// Payment gas booked so far.
    pub fn used(&self) -> u64 {
        self.0.as_ref().map_or(0, |a| a.budget.used)
    }

    /// Reserved gas no payment transaction has claimed.
    pub fn idle(&self) -> u64 {
        self.0.as_ref().map_or(0, |a| a.budget.idle())
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
    use alloy_primitives::Bytes;

    /// A live state that fails if it is ever consulted.
    struct NoLiveReads;
    impl LaneLiveState for NoLiveReads {
        fn lane_code_is_empty(&mut self, _: Address) -> Result<bool, LaneError> {
            panic!("the code gate must not run")
        }
    }

    /// A parent state that fails if it is ever consulted.
    struct NoParentReads;
    impl LaneParentState for NoParentReads {
        fn call_lane_getter(&mut self, _: Address, _: Bytes) -> Result<Bytes, LaneError> {
            panic!("the getter must not run")
        }
    }

    fn active(quota: u64, used: u64, gas_limit: u64) -> LaneState {
        LaneState(Some(Active {
            meta: LaneMeta { ratio: 500, listed: Default::default() },
            budget: Budget { quota, used },
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
            off.classify(&mut NoLiveReads, false, Some(Address::ZERO), 0, U256::from(1)),
            Ok(Lane::General)
        );
        off.record_used(Lane::Payment, 21_000);
        assert_eq!(off.used(), 0);
        assert!(off.admits(0, Lane::General, u64::MAX));
        assert_eq!(off.verify_reservation_intact(0), Ok(()));
        assert_eq!(off.verify(u64::MAX), Ok(()));
        off.inherit_to(BlockHash::ZERO);
        assert_eq!((off.quota(), off.idle(), off.ratio(), off.listed_len()), (0, 0, 0, 0));
        let _ = NoParentReads; // `resolve` is the one verb an off lane cannot answer.
    }

    /// The whole-set verdict and the per-transaction gate are one inequality; the error carries
    /// the two numbers a builder needs to resize.
    #[test]
    fn reservation_verdict_matches_the_gate() {
        let lane = active(1_500_000, 21_000, 30_000_000);
        assert_eq!(lane.idle(), 1_479_000);
        assert_eq!(lane.verify_reservation_intact(1_479_000), Ok(()));
        assert_eq!(
            lane.verify_reservation_intact(979_000),
            Err(LaneError::ReservationOverrun { idle: 1_479_000, shared: 979_000 })
        );
        // The wording go-bsc's `VerifyPackedBid` logs, so one grep covers both clients.
        assert_eq!(
            lane.verify_reservation_intact(979_000).unwrap_err().to_string(),
            "payment lane inequality violated: idle lane 1479000 exceeds the 979000 gas left in the pool"
        );
    }

    /// The verdict is against the gas limit the quota was derived from, not one passed in later.
    #[test]
    fn verify_uses_the_blocks_own_gas_limit() {
        let lane = active(1_500_000, 0, 30_000_000);
        assert_eq!(lane.verify(28_500_000), Ok(()));
        assert!(lane.verify(28_500_001).is_err());
    }
}
