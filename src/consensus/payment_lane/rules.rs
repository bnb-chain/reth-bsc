//! The lane arithmetic and the gates — go-bsc `core/paymentlane`.

use super::{Budget, LaneError, LaneType, MAX_LANE_RATIO, RATIO_DENOM};
use alloy_primitives::{map::HashSet, Address, U256};

/// Guards at the getter's full `uint256` width: a value narrowed to 64 bits first can land inside
/// the bound when the value returned did not.
pub fn check_ratio(ratio: U256) -> Result<u64, LaneError> {
    match u64::try_from(ratio) {
        Ok(narrowed) if narrowed > 0 && narrowed <= MAX_LANE_RATIO => Ok(narrowed),
        _ => Err(LaneError::CorruptConfig(format!(
            "payment lane ratio {ratio} outside 0 < ratio <= {MAX_LANE_RATIO}"
        ))),
    }
}

/// In 128 bits: at the maximum ratio a `2^63` gas limit makes a 73-bit product, which a 64-bit
/// multiply would wrap.
pub fn quota(ratio: u64, gas_limit: u64) -> u64 {
    u64::try_from(ratio as u128 * gas_limit as u128 / RATIO_DENOM as u128).unwrap_or_else(|_| {
        panic!("quota overflowed u64: ratio {ratio} gas_limit {gas_limit} denom {RATIO_DENOM}")
    })
}

/// The block validity rule. `gas_used` must be the header's total, so Parlia's system gas counts
/// as general.
///
/// Checked, not saturating: at `gas_limit == u64::MAX` a saturating sum compares equal and would
/// admit a block that overflowed.
pub fn check_inequality(
    gas_limit: u64,
    gas_used: u64,
    payment_gas_used: u64,
    payment_lane_quota: u64,
) -> Result<(), LaneError> {
    let idle = payment_lane_quota.saturating_sub(payment_gas_used);
    match gas_used.checked_add(idle) {
        Some(total) if total <= gas_limit => Ok(()),
        _ => Err(LaneError::Violated {
            gas_limit,
            gas_used,
            quota: payment_lane_quota,
            payment_gas_used,
        }),
    }
}

/// Every gate, in consensus order, so no call site can implement half the rule.
///
/// `code_at_to_is_empty` must read live state and count an absent account as empty — see
/// [`super::LaneLiveState`].
pub fn classify(
    is_system: bool,
    to: Option<Address>,
    tx_type: u8,
    value: U256,
    listed: &HashSet<Address>,
    code_at_to_is_empty: impl FnOnce(Address) -> Result<bool, LaneError>,
) -> Result<LaneType, LaneError> {
    // Consensus mechanics, not user traffic. First, because a `deposit` passes the payment gates.
    if is_system {
        return Ok(LaneType::GeneralLane);
    }
    let Some(to) = to else { return Ok(LaneType::GeneralLane) };
    // The code gate cannot see code that the transaction's own set-code authorisation installs.
    if !matches!(tx_type, 0x00..=0x02) {
        return Ok(LaneType::GeneralLane);
    }
    // From the parent post-state, so it answers before the live-state gates below: one
    // transaction's lane must not depend on two views.
    if listed.contains(&to) {
        return Ok(LaneType::PaymentLane);
    }
    if value.is_zero() {
        return Ok(LaneType::GeneralLane);
    }
    if code_at_to_is_empty(to)? {
        Ok(LaneType::PaymentLane)
    } else {
        Ok(LaneType::GeneralLane)
    }
}

impl Budget {
    /// Reserved gas no payment transaction has claimed.
    pub fn idle_lane(&self) -> u64 {
        self.payment_lane_quota.saturating_sub(self.payment_lane_used)
    }

    /// The largest gas limit a *single* transaction of this lane may declare, with `shared` the
    /// gas still available to any lane. Payment may take the whole remainder; general must leave
    /// the idle reservation untouched.
    pub fn max_available_gas(&self, shared: u64, lane: LaneType) -> u64 {
        match lane {
            LaneType::PaymentLane => shared,
            LaneType::GeneralLane => shared.saturating_sub(self.idle_lane()),
        }
    }

    /// Producer side only: the importer gates nothing, it rules on the finished block.
    pub fn admits(&self, shared: u64, lane: LaneType, tx_gas_limit: u64) -> bool {
        tx_gas_limit <= self.max_available_gas(shared, lane)
    }

    /// Plain `+=`: the payment total tracks a subset of the block's gas, so it cannot overflow.
    pub fn record_used(&mut self, lane: LaneType, delta: u64) {
        if lane == LaneType::PaymentLane {
            self.payment_lane_used += delta;
        }
    }

    /// This budget held to [`check_inequality`].
    pub fn verify(&self, gas_limit: u64, gas_used: u64) -> Result<(), LaneError> {
        check_inequality(gas_limit, gas_used, self.payment_lane_used, self.payment_lane_quota)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn budget(quota: u64, used: u64) -> Budget {
        Budget { payment_lane_quota: quota, payment_lane_used: used }
    }

    #[test]
    fn quota_is_a_ratio_of_the_gas_limit() {
        assert_eq!(quota(500, 55_000_000), 2_750_000);
        assert_eq!(quota(1_000, 55_000_000), 5_500_000);
        assert_eq!(quota(1, 55_000_000), 5_500);
        assert_eq!(quota(500, 0), 0);
        // Truncates toward zero, and survives a 73-bit product.
        assert_eq!(quota(500, 30_000_001), 1_500_000);
        assert_eq!(quota(1_000, i64::MAX as u64), 922_337_203_685_477_580);
    }

    #[test]
    fn lane_is_a_floor_not_a_ceiling() {
        // (general gas, payment gas, must reject) against gas_limit 100 and quota 20.
        let cases: &[(u64, u64, bool)] = &[
            (80, 20, false),
            (79, 21, false),
            (81, 19, true), // one gas of unclaimed quota is not general's to take
            (80, 0, false), // an idle quota still fits
            (81, 0, true),
            (0, 100, false), // payment may take everything
        ];
        for &(general, payment, want_err) in cases {
            let got = budget(20, payment).verify(100, general + payment);
            assert_eq!(got.is_err(), want_err, "general={general} payment={payment}: {got:?}");
        }
        // A quota larger than the block it reserves from can never be satisfied.
        assert!(budget(200, 0).verify(100, 0).is_err());
    }

    #[test]
    fn overflow_is_not_a_way_in() {
        const LIMIT: u64 = 70_000_000;
        let half = u64::MAX / 2 + 1;
        for &(gas_used, quota) in &[(u64::MAX, 0), (LIMIT, u64::MAX), (half, half), (LIMIT + 1, 0)]
        {
            let got = budget(quota, 0).verify(LIMIT, gas_used);
            assert!(got.is_err(), "gas_used={gas_used} quota={quota} must be a violation");
        }
        // A fully claimed quota leaves nothing idle, whatever its size.
        assert!(budget(u64::MAX, u64::MAX).verify(LIMIT, 1000).is_ok());
    }

    /// Must agree exactly with post-transaction validity, or a producer drops a transaction its
    /// own final check would have accepted.
    #[test]
    fn admission_is_exactly_tight() {
        const CAPACITY: u64 = 40;
        for quota in (0..=CAPACITY).step_by(7) {
            for used in (0..=CAPACITY).step_by(3) {
                for general in (0..=CAPACITY - used).step_by(3) {
                    let before = budget(quota, used);
                    // Skip states no packing loop can reach.
                    if before.verify(CAPACITY, general + used).is_err() {
                        continue;
                    }
                    let shared = CAPACITY - used - general;
                    for lane in [LaneType::GeneralLane, LaneType::PaymentLane] {
                        for gas in 0..=CAPACITY {
                            let mut after = before.clone();
                            after.record_used(lane, gas);
                            let general_after =
                                general + if lane == LaneType::GeneralLane { gas } else { 0 };
                            let total = general_after + after.payment_lane_used;
                            let legal = after.verify(CAPACITY, total).is_ok();
                            assert_eq!(
                                before.admits(shared, lane, gas),
                                legal,
                                "quota={quota} used={used} general={general} {lane:?} gas={gas}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn classification_follows_the_gates() {
        let listed_addr = Address::repeat_byte(0xaa);
        let plain = Address::repeat_byte(0xbb);
        let listed: HashSet<Address> = [listed_addr].into_iter().collect();
        let one = U256::from(1);
        let empty = |_: Address| Ok(true);
        let has_code = |_: Address| Ok(false);
        let never = |_: Address| -> Result<bool, LaneError> { panic!("code gate must not run") };

        // The same transaction with and without the flag, so system is not general by accident.
        let listed_tx = |is_system| classify(is_system, Some(listed_addr), 0, one, &listed, never);
        assert_eq!(listed_tx(true), Ok(LaneType::GeneralLane));
        assert_eq!(listed_tx(false), Ok(LaneType::PaymentLane));

        // A creation has no destination to test.
        assert_eq!(classify(false, None, 0, one, &listed, never), Ok(LaneType::GeneralLane));

        // Excluded types are general even when listed: 0x03 carries blobs, 0x04 installs code.
        for ty in [0x03u8, 0x04, 0x05, 0x7e] {
            let lane = classify(false, Some(listed_addr), ty, one, &listed, never);
            assert_eq!(lane, Ok(LaneType::GeneralLane), "type {ty:#x}");
        }

        // Listed: payment at any admitted type and any value, without ever reaching the probe.
        for ty in [0x00u8, 0x01, 0x02] {
            for value in [U256::ZERO, one] {
                let lane = classify(false, Some(listed_addr), ty, value, &listed, never);
                assert_eq!(lane, Ok(LaneType::PaymentLane), "type {ty:#x} value {value}");
            }
        }

        // An unlisted destination needs a bare transfer: non-zero value, no code.
        let to = Some(plain);
        assert_eq!(classify(false, to, 0, one, &listed, empty), Ok(LaneType::PaymentLane));
        assert_eq!(classify(false, to, 0, U256::ZERO, &listed, never), Ok(LaneType::GeneralLane));
        assert_eq!(classify(false, to, 0, one, &listed, has_code), Ok(LaneType::GeneralLane));

        // Never rounded to "no code": that would make an honest block look like it overran.
        let unavailable = |_: Address| Err(LaneError::StateUnavailable("missing node".into()));
        assert!(matches!(
            classify(false, Some(plain), 0, one, &listed, unavailable),
            Err(LaneError::StateUnavailable(_))
        ));
    }
}
