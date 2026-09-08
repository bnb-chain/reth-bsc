//! Payment lane arithmetic, classification and the block accounting rule.
//!
//! Mirrors go-bsc's `core/paymentlane` term for term. Every function here is a pure function of
//! its arguments; a single disagreement with go-bsc splits the chain, so the comments mark the
//! places where the obvious simplification is the divergence.

use super::{Budget, Lane, LaneError, MAX_LANE_RATIO, RATIO_DENOM};
use alloy_primitives::{map::HashSet, Address, U256};

/// BEP-703 §3.6.1's guard, applied at the getter's full `uint256` width.
///
/// Narrowing first is the bug the spec calls out: `2^64 + 500` truncates to `500`, which is
/// inside the guard when the value returned was not.
pub fn check_ratio(ratio: U256) -> Result<u64, LaneError> {
    match u64::try_from(ratio) {
        Ok(r) if r > 0 && r <= MAX_LANE_RATIO => Ok(r),
        _ => Err(LaneError::CorruptConfig(format!(
            "payment lane ratio {ratio} outside 0 < ratio <= {MAX_LANE_RATIO}"
        ))),
    }
}

/// `paymentLaneQuota(h) = PAYMENT_LANE_RATIO(h−1) × GasLimit(h) / RATIO_DENOM`.
///
/// 128-bit intermediate, as BEP-703 §3.4.2 requires: consensus bounds `gas_limit` only by
/// `2^63 − 1`, so at the maximum ratio the product needs 73 bits and a 64-bit multiply wraps
/// into a quota nobody else derives.
///
/// Saturating rather than panicking: `check_ratio` already caps the ratio far below
/// `RATIO_DENOM`, so the clamp is unreachable — it exists so a future caller that skips the
/// guard degrades the way go-bsc's `hi >= RatioDenom` branch does instead of aborting the node.
pub fn quota(ratio: u64, gas_limit: u64) -> u64 {
    u64::try_from(ratio as u128 * gas_limit as u128 / RATIO_DENOM as u128).unwrap_or(u64::MAX)
}

/// The block validity rule of BEP-703 §3.3, with `general_gas_used` as the header residual:
///
/// ```text
/// gas_used + max(0, quota - payment_gas_used) <= gas_limit
/// ```
///
/// Parlia's system transactions are general, so their gas sits in `gas_used` and falls on the
/// general side by construction.
///
/// `checked_add`, not `saturating_add`: at `gas_limit == u64::MAX` a saturating sum compares
/// equal and would accept a block go-bsc rejects on carry.
pub fn check_inequality(
    gas_limit: u64,
    gas_used: u64,
    payment_gas_used: u64,
    quota: u64,
) -> Result<(), LaneError> {
    let idle = quota.saturating_sub(payment_gas_used);
    match gas_used.checked_add(idle) {
        Some(sum) if sum <= gas_limit => Ok(()),
        _ => Err(LaneError::Violated { gas_limit, gas_used, quota, payment_gas_used }),
    }
}

/// Classify one user transaction against BEP-703 §3.2's gates, in order.
///
/// Parlia's system transactions never reach here — the caller books them as general, which is
/// go-bsc's `isSystemTransaction` gate.
///
/// `code_at_to_is_empty` must read the **live** state and be built fresh at each call site: the
/// code gate's answer changes within a block, so an address that gains code earlier in the same
/// block is general by the time a transfer to it is classified. Memoizing by address is a fork.
///
/// The closure returns **empty**, not "has code", and two encodings both mean empty: an absent
/// account (reth's `basic()` gives `None`; go-bsc's `GetCodeHash` gives the zero hash, which is
/// *not* `EmptyCodeHash`) and `code_hash == KECCAK_EMPTY`. Testing only `!= KECCAK_EMPTY` drops
/// every transfer to a fresh account out of the lane.
pub fn classify(
    to: Option<Address>,
    tx_type: u8,
    value: U256,
    listed: &HashSet<Address>,
    code_at_to_is_empty: impl FnOnce(Address) -> Result<bool, LaneError>,
) -> Result<Lane, LaneError> {
    let Some(to) = to else { return Ok(Lane::General) };
    // Blob and set-code transactions are excluded: the code test cannot reach the execution
    // an authorisation carried by the transaction itself installs.
    if !matches!(tx_type, 0x00..=0x02) {
        return Ok(Lane::General);
    }
    // Listed destinations are settled by the parent post-state and stop here, so no
    // transaction's lane is decided by both state views.
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
    pub fn idle(&self) -> u64 {
        self.quota.saturating_sub(self.used)
    }

    /// The largest gas limit a single transaction of this lane may declare: payment may take
    /// the entire remainder, general must leave the idle quota untouched even when it is empty.
    fn max_available_gas(&self, shared: u64, lane: Lane) -> u64 {
        match lane {
            Lane::Payment => shared,
            Lane::General => shared.saturating_sub(self.idle()),
        }
    }

    /// Whether this transaction may be included. Producer side only — the importer never
    /// gates a transaction, it only checks the finished block with [`Self::verify`].
    ///
    /// `shared` is the gas still available to any lane, i.e. go-bsc's `gasPool.Gas()`.
    pub fn admits(&self, shared: u64, lane: Lane, tx_gas_limit: u64) -> bool {
        tx_gas_limit <= self.max_available_gas(shared, lane)
    }

    /// Book a transaction's actual gas. Plain `+=`: overflow is unreachable because `used`
    /// tracks a subset of `gas_used`, and a debug panic beats go-bsc's silent wrap.
    pub fn record_used(&mut self, lane: Lane, delta: u64) {
        if lane == Lane::Payment {
            self.used += delta;
        }
    }

    /// Check a finished block. `gas_used` is the header's total, Parlia's system gas included.
    ///
    /// The one verdict on this rule, and the same one on both sides: the importer's ruling on a
    /// received block, and the producer's self-check before it seals.
    pub fn verify(&self, gas_limit: u64, gas_used: u64) -> Result<(), LaneError> {
        check_inequality(gas_limit, gas_used, self.used, self.quota)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::constants::KECCAK_EMPTY;

    /// BEP-703 §3.4's arithmetic, straight from go-bsc's `TestQuotaIsTheRatioOfTheGasLimit`.
    #[test]
    fn payment_lane_quota_is_the_ratio_of_the_gas_limit() {
        let cases: &[(u64, u64, u64, &str)] = &[
            (500, 55_000_000, 2_750_000, "§3.4.4's worked example: the default 5% of mainnet"),
            (1_000, 55_000_000, 5_500_000, "the maximum ratio is 10%"),
            (1, 55_000_000, 5_500, "the smallest settable ratio"),
            (500, 0, 0, "no gas limit, no reservation"),
            (500, 30_000_001, 1_500_000, "truncates toward zero rather than rounding"),
            (
                1_000,
                i64::MAX as u64,
                922_337_203_685_477_580,
                "the product needs 73 bits and must not wrap",
            ),
        ];
        for &(ratio, gas_limit, want, why) in cases {
            assert_eq!(quota(ratio, gas_limit), want, "quota({ratio}, {gas_limit}): {why}");
            // The same value an arbitrary-precision reference computes.
            let reference =
                U256::from(ratio) * U256::from(gas_limit) / U256::from(RATIO_DENOM);
            assert_eq!(U256::from(quota(ratio, gas_limit)), reference, "{why}");
        }
    }

    /// §3.6.1's guard, evaluated on the `uint256` the getter returned.
    #[test]
    fn payment_lane_ratio_guard_is_checked_at_full_width() {
        for ok in [1u64, 500, MAX_LANE_RATIO] {
            assert_eq!(check_ratio(U256::from(ok)), Ok(ok));
        }
        // 2^64 + 500 narrows to 500, which is inside the guard; at full width it is not.
        let wraps = (U256::from(1u64) << 64) + U256::from(500u64);
        for bad in [U256::ZERO, U256::from(MAX_LANE_RATIO + 1), wraps, U256::MAX] {
            assert!(
                matches!(check_ratio(bad), Err(LaneError::CorruptConfig(_))),
                "ratio {bad} must fail the guard"
            );
        }
    }

    /// go-bsc's `TestLaneIsFloorNotCeiling`: the rule's boundary cases, all six of them.
    #[test]
    fn payment_lane_is_a_floor_not_a_ceiling() {
        const LIMIT: u64 = 100;
        const LANE: u64 = 20;
        let cases: &[(u64, u64, bool, &str)] = &[
            (80, 20, false, "payment exactly fills the quota, the terms sum to exactly GasLimit"),
            (79, 21, false, "payment one gas over, general one gas under"),
            (81, 19, true, "payment one gas short does not hand the freed quota to general"),
            (80, 0, false, "with no payment demand the quota idles"),
            (81, 0, true, "general does not get the idling quota"),
            (0, 100, false, "the quota is a floor, not a ceiling: payment may take the block"),
        ];
        for &(general, payment, want_err, why) in cases {
            let got = check_inequality(LIMIT, general + payment, payment, LANE);
            assert_eq!(got.is_err(), want_err, "{why}: {got:?}");
        }
    }

    /// go-bsc's `TestOverflowIsNotAWayIn`: the carry must reject, never wrap into acceptance.
    #[test]
    fn payment_lane_overflow_is_not_a_way_in() {
        const GAS_LIMIT: u64 = 70_000_000;
        let max = u64::MAX;
        for &(gas_used, payment, lane) in &[
            (max, 0u64, 0u64),
            (GAS_LIMIT, 0, max),
            (max / 2 + 1, 0, max / 2 + 1),
            (GAS_LIMIT + 1, 0, 0),
        ] {
            assert!(
                check_inequality(GAS_LIMIT, gas_used, payment, lane).is_err(),
                "gas_used={gas_used} payment={payment} quota={lane} must be a violation"
            );
        }
        // A quota fully consumed by payment gas leaves nothing idle, whatever its size.
        assert!(check_inequality(GAS_LIMIT, 1000, max, max).is_ok());
    }

    /// go-bsc's `TestVerifyFailureTriggers`.
    #[test]
    fn payment_lane_verify_failure_triggers() {
        let cases: &[(Budget, u64, bool, &str)] = &[
            (Budget { quota: 20, used: 20 }, 80, false, "consistent and valid"),
            (Budget { quota: 200, used: 0 }, 0, true, "the quota does not fit this block"),
            (
                Budget { quota: 20, used: 20 },
                101,
                true,
                "system gas overran the reservation and burst the block",
            ),
        ];
        for (budget, gas_used, want_err, why) in cases {
            assert_eq!(budget.verify(100, *gas_used).is_err(), *want_err, "{why}");
        }
    }

    /// go-bsc's `TestAdmissionIsExactlyTight`, shrunk: admission must agree exactly with
    /// post-transaction validity, so a producer never drops a transaction go-bsc would pack and
    /// never packs one go-bsc's own self-check would refuse.
    #[test]
    fn payment_lane_admission_is_exactly_tight() {
        const CAPACITY: u64 = 40;
        for lane_quota in (0..=CAPACITY).step_by(7) {
            for payment_used in (0..=CAPACITY).step_by(3) {
                for general_used in (0..=CAPACITY - payment_used).step_by(3) {
                    let budget = Budget { quota: lane_quota, used: payment_used };
                    // Skip states no packing loop can reach.
                    if check_inequality(
                        CAPACITY,
                        general_used + payment_used,
                        payment_used,
                        lane_quota,
                    )
                    .is_err()
                    {
                        continue;
                    }
                    let shared = CAPACITY - payment_used - general_used;
                    for lane in [Lane::General, Lane::Payment] {
                        for gas in 0..=CAPACITY {
                            let mut after = budget.clone();
                            after.record_used(lane, gas);
                            let after_general =
                                general_used + if lane == Lane::General { gas } else { 0 };
                            let legal = check_inequality(
                                CAPACITY,
                                after_general + after.used,
                                after.used,
                                lane_quota,
                            )
                            .is_ok();
                            assert_eq!(
                                budget.admits(shared, lane, gas), legal,
                                "quota={lane_quota} payment={payment_used} general={general_used} \
                                 lane={lane:?} gas={gas}"
                            );
                        }
                    }
                }
            }
        }
    }

    /// Admission must never widen as a block fills, or go-bsc's `txs.Pop()` — which never
    /// revisits a dropped transaction — would skip one that later became admissible.
    #[test]
    fn payment_lane_max_available_gas_never_rises() {
        let mut budget = Budget { quota: 300, used: 0 };
        let capacity = 1000u64;
        let mut pool_used = 0u64;
        let mut previous = [u64::MAX; 2];
        for (lane, gas) in [
            (Lane::General, 100u64),
            (Lane::Payment, 50),
            (Lane::General, 200),
            (Lane::Payment, 400),
            (Lane::General, 50),
        ] {
            pool_used += gas;
            budget.record_used(lane, gas);
            let shared = capacity - pool_used;
            for (i, l) in [Lane::General, Lane::Payment].into_iter().enumerate() {
                let now = budget.max_available_gas(shared, l);
                assert!(now <= previous[i], "{l:?} rose {} -> {now}", previous[i]);
                previous[i] = now;
            }
            // General traffic must never eat into the idle lane.
            assert!(shared >= budget.idle());
        }
    }

    fn listed_set(addrs: &[Address]) -> HashSet<Address> {
        addrs.iter().copied().collect()
    }

    /// BEP-703 §3.2's gates, in the order the spec states them.
    #[test]
    fn payment_lane_classification_follows_the_spec_gates() {
        let listed_addr = Address::repeat_byte(0xaa);
        let plain = Address::repeat_byte(0xbb);
        let listed = listed_set(&[listed_addr]);
        let empty = |_: Address| Ok(true);
        let has_code = |_: Address| Ok(false);
        let never = |_: Address| -> Result<bool, LaneError> {
            panic!("the code gate must not be reached")
        };

        // A contract creation has no destination to test.
        assert_eq!(classify(None, 0, U256::from(1), &listed, never), Ok(Lane::General));

        // The type allowlist runs before the list, so even a listed destination is general on
        // an excluded type — 0x03 carries blobs, 0x04 installs code before execution begins.
        for ty in [0x03u8, 0x04, 0x05, 0x7e] {
            assert_eq!(
                classify(Some(listed_addr), ty, U256::from(1), &listed, never),
                Ok(Lane::General),
                "type {ty:#x}"
            );
        }

        // A listed destination is payment on every admitted type, whatever the value, and the
        // code gate is never consulted — so a listed contract stays in the lane.
        for ty in [0x00u8, 0x01, 0x02] {
            for value in [U256::ZERO, U256::from(1)] {
                assert_eq!(
                    classify(Some(listed_addr), ty, value, &listed, never),
                    Ok(Lane::Payment),
                    "type {ty:#x} value {value}"
                );
            }
        }

        // A bare transfer needs non-zero value and no code at the destination.
        assert_eq!(classify(Some(plain), 0, U256::from(1), &listed, empty), Ok(Lane::Payment));
        assert_eq!(classify(Some(plain), 0, U256::ZERO, &listed, never), Ok(Lane::General));
        assert_eq!(classify(Some(plain), 0, U256::from(1), &listed, has_code), Ok(Lane::General));

        // A failed state read is surfaced, never rounded to "no code": biasing toward Payment
        // would let an honest block look like it overran the lane.
        assert_eq!(
            classify(Some(plain), 0, U256::from(1), &listed, |_| Err(
                LaneError::StateUnavailable("missing trie node".into())
            )),
            Err(LaneError::StateUnavailable("missing trie node".into()))
        );
    }

    /// The zero-code-hash trap: an account that does not exist yet must still be payment.
    #[test]
    fn payment_lane_absent_account_is_empty_code() {
        let listed = HashSet::default();
        let to = Address::repeat_byte(0xcc);
        // Both encodings of "empty" that reach this closure in the executor.
        for code_hash in [alloy_primitives::B256::ZERO, KECCAK_EMPTY] {
            let is_empty = code_hash.is_zero() || code_hash == KECCAK_EMPTY;
            assert!(is_empty);
            assert_eq!(
                classify(Some(to), 0, U256::from(1), &listed, |_| Ok(is_empty)),
                Ok(Lane::Payment),
                "code hash {code_hash}"
            );
        }
    }
}
