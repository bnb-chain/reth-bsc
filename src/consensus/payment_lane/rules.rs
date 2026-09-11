//! Lane arithmetic, transaction classification and the block accounting rule.

use super::{Budget, Lane, LaneError, RATIO_DENOM};
use alloy_primitives::{map::HashSet, Address, U256};

/// `quota = ratio * gas_limit / RATIO_DENOM`, in 128 bits: `gas_limit` reaches `2^63 - 1`, so at
/// the maximum ratio the product needs 73 bits and a 64-bit multiply would wrap.
pub fn quota(ratio: u64, gas_limit: u64) -> u64 {
    u64::try_from(ratio as u128 * gas_limit as u128 / RATIO_DENOM as u128).unwrap_or_else(|_| {
        panic!("quota overflowed u64: ratio {ratio} gas_limit {gas_limit} denom {RATIO_DENOM}")
    })
}

/// Classify one transaction. The gates run in the order written, and all of them live here so no
/// call site can implement half the rule.
///
/// `code_at_to_is_empty` must read **live** state: an address that gains code earlier in the same
/// block is general by the time a transfer to it is classified, so memoizing by address is a
/// fork. Both an absent account and `KECCAK_EMPTY` count as empty — testing `!= KECCAK_EMPTY`
/// alone would drop every transfer to a fresh account out of the lane.
pub fn classify(
    is_system: bool,
    to: Option<Address>,
    tx_type: u8,
    value: U256,
    listed: &HashSet<Address>,
    code_at_to_is_empty: impl FnOnce(Address) -> Result<bool, LaneError>,
) -> Result<Lane, LaneError> {
    // Consensus mechanics, not user traffic: general whatever the destination, and a listed
    // destination must not rescue them. First, because a `deposit` passes the payment gates.
    if is_system {
        return Ok(Lane::General);
    }
    let Some(to) = to else { return Ok(Lane::General) };
    // Blob and set-code transactions are excluded: the code gate cannot see code that an
    // authorisation carried by the transaction itself installs.
    if !matches!(tx_type, 0x00..=0x02) {
        return Ok(Lane::General);
    }
    // Settled by the parent post-state; stopping here keeps one transaction's lane from
    // depending on two different state views.
    if listed.contains(&to) {
        return Ok(Lane::Payment);
    }
    if value.is_zero() {
        return Ok(Lane::General);
    }
    if code_at_to_is_empty(to)? {
        Ok(Lane::Payment)
    } else {
        Ok(Lane::General)
    }
}

impl Budget {
    /// Reserved gas no payment transaction has claimed.
    pub fn idle(&self) -> u64 {
        self.quota.saturating_sub(self.used)
    }

    /// Whether this transaction fits, with `shared` the gas still available to any lane. Payment
    /// may take the whole remainder; general must leave the idle reservation untouched.
    ///
    /// Producer side only: the importer gates nothing, it rules on the finished block.
    pub fn admits(&self, shared: u64, lane: Lane, tx_gas_limit: u64) -> bool {
        tx_gas_limit <=
            match lane {
                Lane::Payment => shared,
                Lane::General => shared.saturating_sub(self.idle()),
            }
    }

    /// Whether `shared` gas still covers the reservation — the invariant [`Self::admits`]
    /// maintains one transaction at a time, stated directly so it can also be checked in one
    /// go over a transaction set that was never filtered.
    ///
    /// `shared` is the producer's remaining pool (`GasLimit - reserved - used`), never the
    /// block's raw headroom: the block rule is the importer's verdict and counts the system
    /// transactions' *actual* gas, which would hand user traffic the unused part of the
    /// reservation.
    ///
    /// Spelled out rather than written as `admits(shared, General, 0)`: that would be vacuous,
    /// because the saturating subtraction inside `admits` turns an over-committed pool into a
    /// zero allowance, which a zero-gas transaction still fits into.
    pub fn reservation_intact(&self, shared: u64) -> bool {
        self.idle() <= shared
    }

    /// Plain `+=`: `used` tracks a subset of the block's gas, so it cannot overflow.
    pub fn record_used(&mut self, lane: Lane, delta: u64) {
        if lane == Lane::Payment {
            self.used += delta;
        }
    }

    /// `gas_used` must be the header's total, so Parlia's system gas counts as general.
    ///
    /// `checked_add`, not saturating: at `gas_limit == u64::MAX` a saturating sum compares equal
    /// and would accept a block that overflowed.
    pub fn verify(&self, gas_limit: u64, gas_used: u64) -> Result<(), LaneError> {
        match gas_used.checked_add(self.idle()) {
            Some(total) if total <= gas_limit => Ok(()),
            _ => Err(LaneError::Violated {
                gas_limit,
                gas_used,
                quota: self.quota,
                payment_gas_used: self.used,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn budget(quota: u64, used: u64) -> Budget {
        Budget { quota, used }
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

    /// The reservation is a floor, not a ceiling: unclaimed quota stays out of general's reach,
    /// while payment traffic may take the whole block.
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

    /// The carry must reject, never wrap into acceptance.
    #[test]
    fn overflow_is_not_a_way_in() {
        const LIMIT: u64 = 70_000_000;
        let half = u64::MAX / 2 + 1;
        for &(gas_used, quota) in
            &[(u64::MAX, 0), (LIMIT, u64::MAX), (half, half), (LIMIT + 1, 0)]
        {
            let got = budget(quota, 0).verify(LIMIT, gas_used);
            assert!(got.is_err(), "gas_used={gas_used} quota={quota} must be a violation");
        }
        // A fully claimed quota leaves nothing idle, whatever its size.
        assert!(budget(u64::MAX, u64::MAX).verify(LIMIT, 1000).is_ok());
    }

    /// Admission must agree exactly with post-transaction validity, or a producer drops a
    /// transaction that its own final check would have accepted.
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
                    for lane in [Lane::General, Lane::Payment] {
                        for gas in 0..=CAPACITY {
                            let mut after = before.clone();
                            after.record_used(lane, gas);
                            let general_after =
                                general + if lane == Lane::General { gas } else { 0 };
                            let legal =
                                after.verify(CAPACITY, general_after + after.used).is_ok();
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

    /// The whole-set check is the packing loop's invariant, and it must agree with the
    /// per-transaction gate everywhere that gate is not vacuous.
    #[test]
    fn reservation_check_is_the_packing_loop_invariant() {
        for quota in [0u64, 1, 7, 1_500_000, u64::MAX] {
            for used in [0u64, 1, 21_000, 1_500_000] {
                let b = budget(quota, used);
                for shared in [0u64, 1, 21_000, 1_479_000, 1_500_000, u64::MAX] {
                    assert_eq!(b.reservation_intact(shared), b.idle() <= shared);
                    // A pool that admits any real general transaction has an intact
                    // reservation; the converse needs `shared > idle`, which is where the two
                    // differ by exactly one gas.
                    if b.admits(shared, Lane::General, 1) {
                        assert!(b.reservation_intact(shared), "quota={quota} used={used} shared={shared}");
                    }
                }
            }
        }
        // The boundary: the reservation is intact when nothing but it is left.
        assert!(budget(20, 0).reservation_intact(20));
        assert!(!budget(20, 0).reservation_intact(19));
        // And why the zero-gas spelling would not do.
        assert!(budget(20, 0).admits(19, Lane::General, 0));
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

        // A system transaction is general where every payment gate would otherwise pass — and
        // the same transaction without the flag is payment, so this is not passing by accident.
        assert_eq!(classify(true, Some(listed_addr), 0, one, &listed, never), Ok(Lane::General));
        assert_eq!(classify(false, Some(listed_addr), 0, one, &listed, never), Ok(Lane::Payment));

        // A creation has no destination to test.
        assert_eq!(classify(false, None, 0, one, &listed, never), Ok(Lane::General));

        // Excluded types are general even when listed: 0x03 carries blobs, 0x04 installs code.
        for ty in [0x03u8, 0x04, 0x05, 0x7e] {
            let lane = classify(false, Some(listed_addr), ty, one, &listed, never);
            assert_eq!(lane, Ok(Lane::General), "type {ty:#x}");
        }

        // A listed destination is payment at any admitted type and any value, and without the
        // probe — so a listed contract stays in the lane.
        for ty in [0x00u8, 0x01, 0x02] {
            for value in [U256::ZERO, one] {
                let lane = classify(false, Some(listed_addr), ty, value, &listed, never);
                assert_eq!(lane, Ok(Lane::Payment), "type {ty:#x} value {value}");
            }
        }

        // An unlisted destination needs a bare transfer: non-zero value, no code.
        assert_eq!(classify(false, Some(plain), 0, one, &listed, empty), Ok(Lane::Payment));
        assert_eq!(classify(false, Some(plain), 0, U256::ZERO, &listed, never), Ok(Lane::General));
        assert_eq!(classify(false, Some(plain), 0, one, &listed, has_code), Ok(Lane::General));

        // A failed read is surfaced, never rounded to "no code" — that would make an honest
        // block look like it overran the lane.
        let unavailable = |_: Address| Err(LaneError::StateUnavailable("missing node".into()));
        assert!(matches!(
            classify(false, Some(plain), 0, one, &listed, unavailable),
            Err(LaneError::StateUnavailable(_))
        ));
    }
}
