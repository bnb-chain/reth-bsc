//! Payment lane arithmetic, classification and the commitment codec.
//!
//! The quota is an accumulator: a single disagreement with go-bsc never reconverges, so the
//! comments below mark the places where the obvious simplification is the divergence.

use super::{
    Bounds, Budget, Commitment, GovernanceParams, Lane, LaneError, OmmersHashError, ParentSignal,
    RATIO_DENOM, SYSTEM_TXS_GAS_HARD_LIMIT, Signal,
};
use alloy_consensus::{Header, constants::EMPTY_OMMER_ROOT_HASH};
use alloy_primitives::{Address, B256, U256, map::HashSet};

/// `floor(a * b / d)` over 128 bits, saturating.
///
/// `d == 0` returns `u64::MAX` to match go-bsc's `hi >= d` guard; Rust would panic.
fn mul_div_floor(a: u64, b: u64, d: u64) -> u64 {
    if d == 0 {
        return u64::MAX;
    }
    u64::try_from(a as u128 * b as u128 / d as u128).unwrap_or(u64::MAX)
}

/// `a * b >= c * d`, exactly, without dividing.
///
/// 128-bit because both products wrap at a consensus-legal gas limit: `8000 * 2^62` is zero
/// in 64 bits, which turns every contraction into an expansion.
fn gte(a: u64, b: u64, c: u64, d: u64) -> bool {
    a as u128 * b as u128 >= c as u128 * d as u128
}

/// Upper clamp: the tighter of the ratio bound and the absolute bound.
fn lane_ceiling(p: &GovernanceParams, gas_limit: u64) -> u64 {
    mul_div_floor(p.max_ratio, gas_limit, RATIO_DENOM).min(p.max_gas)
}

/// Lower clamp, itself clamped to the ceiling.
///
/// `min_gas` can exceed `max_ratio * gas_limit`, so without the inner `min` the floor would
/// exceed the ceiling and the `min(max(..))` chains below would panic. `payment_lane_clamp_grid`.
fn lane_floor(p: &GovernanceParams, gas_limit: u64) -> u64 {
    mul_div_floor(p.min_ratio, gas_limit, RATIO_DENOM)
        .max(p.min_gas)
        .min(lane_ceiling(p, gas_limit))
}

/// The gas the quota must leave for Parlia's system transactions.
///
/// Applied last, and allowed below the floor: at a low gas limit a breathe block would not
/// otherwise fit, and that halt does not clear on its own.
fn reserve_cap(gas_limit: u64) -> u64 {
    gas_limit.saturating_sub(SYSTEM_TXS_GAS_HARD_LIMIT)
}

/// The three clamps `next_lane_quota` applies.
pub fn bounds(p: &GovernanceParams, gas_limit: u64) -> Bounds {
    Bounds {
        floor: lane_floor(p, gas_limit),
        ceiling: lane_ceiling(p, gas_limit),
        reserve_cap: reserve_cap(gas_limit),
    }
}

/// The block validity rule: `general_gas_used + max(payment_gas_used, quota) <= gas_limit`,
/// rewritten with `general_gas_used` as the header residual.
///
/// Not self-sufficient. The equivalence holds only while `payment_gas_used <= gas_used`, so
/// callers must run that bound first — see [`Commitment::check_header_bounds`].
///
/// `checked_add`, not `saturating_add`: at `gas_limit == u64::MAX` a saturating sum compares
/// equal and would accept a block go-bsc rejects on carry.
fn check_inequality(
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

impl Commitment {
    /// `[0..8]` quota, `[8..16]` payment gas used, `[16..32]` zero.
    pub fn encode(&self) -> B256 {
        let mut out = [0u8; 32];
        out[0..8].copy_from_slice(&self.quota.to_be_bytes());
        out[8..16].copy_from_slice(&self.payment_gas_used.to_be_bytes());
        B256::from(out)
    }

    /// Inverse of [`Self::encode`]. Rejects a non-zero reserved tail, which is what keeps a
    /// commitment from ever equalling `EMPTY_OMMER_ROOT_HASH`.
    pub fn decode(h: B256) -> Result<Self, LaneError> {
        if h[16..32].iter().any(|b| *b != 0) {
            return Err(LaneError::BadCommitment(h));
        }
        Ok(Self {
            quota: u64::from_be_bytes(h[0..8].try_into().expect("8 bytes")),
            payment_gas_used: u64::from_be_bytes(h[8..16].try_into().expect("8 bytes")),
        })
    }

    /// Whether this header claims an empty ommers list. Looser than [`Self::decode`]: it also
    /// accepts `EMPTY_OMMER_ROOT_HASH`, so a caller with no parent header can check the shape
    /// without rejecting the activation block, which is post-Jenner yet still carries that hash.
    pub fn commits_no_uncles(h: B256) -> bool {
        h == EMPTY_OMMER_ROOT_HASH || h[16..32].iter().all(|b| *b == 0)
    }

    /// The order is load-bearing — [`check_inequality`] is only equivalent to the spec's rule
    /// once `payment_gas_used <= gas_used` has been established.
    pub fn check_header_bounds(&self, gas_used: u64, gas_limit: u64) -> Result<(), LaneError> {
        if self.payment_gas_used > gas_used {
            return Err(LaneError::Untruthy { committed: self.payment_gas_used, actual: gas_used });
        }
        if self.quota > gas_limit {
            return Err(LaneError::Violated {
                gas_limit,
                gas_used,
                quota: self.quota,
                payment_gas_used: self.payment_gas_used,
            });
        }
        check_inequality(gas_limit, gas_used, self.payment_gas_used, self.quota)
    }
}

impl ParentSignal {
    /// Whether the parent's congestion reached `trigger` basis points of its own gas limit.
    fn reached(&self, trigger: u64) -> bool {
        gte(self.signal_gas_used, RATIO_DENOM, trigger, self.gas_limit)
    }

    /// The parent's quota after one expand / shrink / hold step.
    ///
    /// `else if`, not two `if`s: BEP-703 §3.6 accepts parameters that violate its own
    /// invariants, and with `shrink_trigger > expand_trigger` both predicates hold at once.
    /// A zero gas limit is the bootstrap seed rather than a quiet parent — no signal, so no
    /// step, but the quota carries over.
    fn stepped(&self, p: &GovernanceParams, gas_limit: u64) -> u64 {
        if self.gas_limit == 0 {
            return self.lane_quota;
        }
        if self.reached(p.expand_trigger) {
            self.lane_quota.saturating_add(mul_div_floor(p.expand_step, gas_limit, RATIO_DENOM))
        } else if !self.reached(p.shrink_trigger) {
            self.lane_quota.saturating_sub(mul_div_floor(p.shrink_step, gas_limit, RATIO_DENOM))
        } else {
            self.lane_quota
        }
    }
}

impl Signal {
    /// Read the parent's congestion off its own header.
    ///
    /// Bootstrap is decided by the positive test `ommers_hash == EMPTY_OMMER_ROOT_HASH`, never
    /// by a failed decode: reading a decode failure as bootstrap silently resets the
    /// accumulator to the floor.
    pub fn from_parent(parent: &Header) -> Result<Self, LaneError> {
        if parent.ommers_hash == EMPTY_OMMER_ROOT_HASH {
            return Ok(Self(None));
        }
        let c = Commitment::decode(parent.ommers_hash)?;
        // Gas the parent spent outside the reservation. Plain `+`: the two terms sum to at
        // most `max(parent.gas_used, c.payment_gas_used)`, so they cannot carry.
        let signal_gas_used = parent.gas_used.saturating_sub(c.payment_gas_used)
            + c.payment_gas_used.saturating_sub(c.quota);
        Ok(Self(Some(ParentSignal {
            lane_quota: c.quota,
            signal_gas_used,
            gas_limit: parent.gas_limit,
        })))
    }

    /// The quota for the block after the one this signal describes.
    ///
    /// `gas_limit` is *that* block's: the thresholds divide by the parent's gas limit because
    /// that is the block whose congestion is measured, while the step and all three clamps
    /// scale by this block's because that is the block whose space is reserved.
    pub fn next_lane_quota(&self, p: &GovernanceParams, gas_limit: u64) -> u64 {
        let stepped = self.0.map_or(0, |parent| parent.stepped(p, gas_limit));
        let b = bounds(p, gas_limit);
        // Clamped every block, not only when a step fires, because the bounds track this
        // block's gas limit. `reserve_cap` comes last and may land below the floor.
        stepped.max(b.floor).min(b.ceiling).min(b.reserve_cap)
    }

    /// Adjudicate a committed quota. Decidable before any transaction runs, which is what
    /// lets a validator check it before blind-signing a builder's block.
    pub fn check_next_lane_quota(
        &self,
        committed: u64,
        p: &GovernanceParams,
        gas_limit: u64,
    ) -> Result<(), LaneError> {
        let derived = self.next_lane_quota(p, gas_limit);
        if committed != derived {
            return Err(LaneError::QuotaMismatch { committed, derived });
        }
        Ok(())
    }
}

/// The whole `header.ommers_hash` rule for a caller that has the parent — checks #1-#4 once the
/// parent is under the lane, and the empty ommers root before it.
///
/// Both arms are one decision, which is why they live in one function: a caller that keeps only
/// the first accepts a commitment on the activation block, and go-bsc rejects that. The import
/// path and the BidBlock admission path both call this and map the failure to their own error
/// type.
pub fn check_ommers_hash_against_parent(
    parent_commits_lane: bool,
    header: &Header,
) -> Result<(), OmmersHashError> {
    if parent_commits_lane {
        Commitment::decode(header.ommers_hash)
            .and_then(|c| c.check_header_bounds(header.gas_used, header.gas_limit))?;
        Ok(())
    } else if header.ommers_hash == EMPTY_OMMER_ROOT_HASH {
        Ok(())
    } else {
        Err(OmmersHashError::CommitmentBeforeFork(header.ommers_hash))
    }
}

/// Classify one user transaction. Parlia's system transactions never reach here.
///
/// Gates run before any state read, and `code_at_to_is_empty` must be built fresh
/// at each call site: the code gate's answer changes within a block — an address that gains
/// code earlier in the same block is general by the time a transfer to it is classified.
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
    if code_at_to_is_empty(to)? { Ok(Lane::Payment) } else { Ok(Lane::General) }
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

    /// Whether this transaction may be included. Producer side only — the importer's only
    /// lane gate is [`Self::verify_commitment`].
    pub fn admits(&self, shared: u64, lane: Lane, tx_gas_limit: u64) -> bool {
        tx_gas_limit <= self.max_available_gas(shared, lane)
    }

    /// Book a transaction's actual gas. Plain `+=`: overflow is unreachable, and a debug panic
    /// beats go-bsc's silent wrap.
    pub fn record_used(&mut self, lane: Lane, delta: u64) {
        if lane == Lane::Payment {
            self.used += delta;
        }
    }

    /// Check a finished block. `gas_used` is the header's total, system gas included.
    ///
    /// The first branch catches swapped arguments; it is unreachable while `used` only
    /// accumulates from user transactions.
    pub fn verify(&self, gas_limit: u64, gas_used: u64) -> Result<(), LaneError> {
        if self.used > gas_used {
            return Err(LaneError::Untruthy { committed: self.used, actual: gas_used });
        }
        check_inequality(gas_limit, gas_used, self.used, self.quota)
    }

    /// The authoritative check on a committed figure: compare it against local replay, so
    /// `used` must come from replaying this block, never from the commitment.
    ///
    /// The quota comparison has no go-bsc counterpart — it leaves quota to
    /// `check_next_lane_quota`. It only ever rejects more, and catches a commitment that was
    /// not derived from this budget.
    pub fn verify_commitment(
        &self,
        gas_limit: u64,
        gas_used: u64,
        c: &Commitment,
    ) -> Result<(), LaneError> {
        if c.quota != self.quota {
            return Err(LaneError::QuotaMismatch { committed: c.quota, derived: self.quota });
        }
        if c.payment_gas_used != self.used {
            return Err(LaneError::Untruthy {
                committed: c.payment_gas_used,
                actual: self.used,
            });
        }
        self.verify(gas_limit, gas_used)
    }

}

#[cfg(test)]
#[allow(clippy::absurd_extreme_comparisons, clippy::if_same_then_else, clippy::manual_div_ceil)]
mod tests {
    use super::*;
    use crate::consensus::payment_lane::DEFAULT_PARAMS;
    use alloy_consensus::constants::KECCAK_EMPTY;
    use alloy_primitives::b256;

    fn parent_with(quota: u64, payment: u64, gas_limit: u64, gas_used: u64) -> Header {
        Header {
            ommers_hash: Commitment { quota, payment_gas_used: payment }.encode(),
            gas_limit,
            gas_used,
            ..Default::default()
        }
    }

    /// Build a Signal with an exact `signal_gas_used` by choosing parent gas_used.
    /// signal = satSub(gasUsed, payment) + satSub(payment, quota); with payment=0 it is gasUsed.
    fn signal_of(quota: u64, signal_gas_used: u64, parent_gas_limit: u64) -> Signal {
        Signal::from_parent(&parent_with(quota, 0, parent_gas_limit, signal_gas_used)).unwrap()
    }

    #[test]
    fn payment_lane_trigger_thresholds() {
        let gl = 55_000_000u64;
        assert_eq!(mul_div_floor(DEFAULT_PARAMS.expand_step, gl, RATIO_DENOM), 1_100_000);
        assert_eq!(mul_div_floor(DEFAULT_PARAMS.shrink_step, gl, RATIO_DENOM), 275_000);
        let cases: &[(u64, u64)] = &[
            (8000, 4_100_000),
            (7999, 3_000_000),
            (7000, 3_000_000),
            (6999, 2_725_000),
            (0, 2_725_000),
        ];
        for &(bps, want) in cases {
            // parametrisation A: parentGasLimit = 10_000 so signal == bps numerically
            let a = signal_of(3_000_000, bps, 10_000).next_lane_quota(&DEFAULT_PARAMS, gl);
            // parametrisation B: parentGasLimit = 55_000_000, signal scaled
            let sig = mul_div_floor(bps, 55_000_000, RATIO_DENOM);
            let b = signal_of(3_000_000, sig, 55_000_000).next_lane_quota(&DEFAULT_PARAMS, gl);
            assert_eq!(a, want, "parametrisation A, bps {bps}");
            assert_eq!(b, want, "parametrisation B, bps {bps}");
        }
        // the doc's warning: the A==B equivalence needs 10000 | gasLimit. Show it breaking.
        let odd = 55_009_999u64;
        let a = signal_of(3_000_000, 8000, 10_000).next_lane_quota(&DEFAULT_PARAMS, odd);
        let sig = mul_div_floor(8000, odd, RATIO_DENOM);
        let b = signal_of(3_000_000, sig, odd).next_lane_quota(&DEFAULT_PARAMS, odd);
        assert_eq!(a, 4_100_199, "hand-computed: 3_000_000 + mul_div_floor(200, 55_009_999, 10000)");
        assert_ne!(a, b, "the doc says this equivalence fails when 10000 does not divide gasLimit");
        assert_eq!(b, 3_000_000, "signal lands one wei below the expand threshold => hysteresis");
    }

    #[test]
    fn payment_lane_mul_div_floor() {
        assert_eq!(mul_div_floor(200, 55_009_999, 10000), 1_100_199);
        assert_eq!(mul_div_floor(800, 55_009_999, 10000), 4_400_799);
        assert_eq!(mul_div_floor(2000, 55_009_999, 10000), 11_001_999);
        assert_eq!(mul_div_floor(800, 54_999_999, 10000), 4_399_999);
        assert_eq!(mul_div_floor(u64::MAX, u64::MAX, 10000), u64::MAX);
        assert_eq!(mul_div_floor(1, 1, 0), u64::MAX);
    }

    #[test]
    fn payment_lane_bootstrap_and_cap() {
        assert_eq!(Signal(None).next_lane_quota(&DEFAULT_PARAMS, 55_000_000), 2_000_000);
        assert_eq!(Signal(None).next_lane_quota(&DEFAULT_PARAMS, SYSTEM_TXS_GAS_HARD_LIMIT), 0);
        assert_eq!(bounds(&DEFAULT_PARAMS, 70_000_000).ceiling, 5_600_000);
        assert_eq!(
            Signal(None).check_next_lane_quota(0, &DEFAULT_PARAMS, SYSTEM_TXS_GAS_HARD_LIMIT),
            Ok(())
        );
        // boundary 7: parent HAS a commitment but gas_limit == 0
        let s = Signal::from_parent(&parent_with(3_000_000, 0, 0, 12_345_678)).unwrap();
        let got = s.next_lane_quota(&DEFAULT_PARAMS, 55_000_000);
        let b = bounds(&DEFAULT_PARAMS, 55_000_000);
        assert_eq!(got, 3_000_000u64.max(b.floor).min(b.ceiling));
        assert_ne!(got, b.floor.min(b.reserve_cap), "must NOT restart from next = 0");
    }

    /// A parent with a commitment but `gas_limit == 0` keeps its quota; it is not a bootstrap.
    ///
    /// A parent that carries a commitment but has `gas_limit == 0` keeps its quota: only the
    /// step is skipped. Collapsing that into bootstrap would start from zero and derive the
    /// floor instead, which is a permanent split.
    #[test]
    fn payment_lane_zero_parent_gas_limit_keeps_quota() {
        let parent = parent_with(3_000_000, 0, 0, 0);
        let signal = Signal::from_parent(&parent).unwrap();

        let gl = 55_000_000u64;
        let b = bounds(&DEFAULT_PARAMS, gl);
        assert_eq!(signal.next_lane_quota(&DEFAULT_PARAMS, gl), 3_000_000);
        assert_ne!(
            signal.next_lane_quota(&DEFAULT_PARAMS, gl),
            b.floor.min(b.reserve_cap),
            "must not collapse to min(floor, cap) = 2_000_000"
        );

        // The same parent with a real gas limit sees a zero signal, so it shrinks.
        let parent = parent_with(3_000_000, 0, 55_000_000, 0);
        assert_eq!(
            Signal::from_parent(&parent).unwrap().next_lane_quota(&DEFAULT_PARAMS, gl),
            2_725_000
        );
    }

    /// The reserve cap is the only clamp allowed to push the quota below its own floor, and
    /// at a low enough gas limit it does.
    #[test]
    fn payment_lane_reserve_cap_pushes_below_floor() {
        let gl = 21_000_000u64;
        let b = bounds(&DEFAULT_PARAMS, gl);
        assert_eq!((b.floor, b.ceiling, b.reserve_cap), (1_680_000, 1_680_000, 1_000_000));

        let signal = Signal::from_parent(&parent_with(3_000_000, 0, 0, 0)).unwrap();
        let quota = signal.next_lane_quota(&DEFAULT_PARAMS, gl);
        assert_eq!(quota, 1_000_000);
        assert!(quota < b.floor, "the outer min must push below the floor");
    }

    /// The signal's second term: gas the parent spent *beyond* its quota.
    ///
    /// The lane is a floor, not a ceiling — a block may spend its whole gas limit on payments —
    /// so that overspill is congestion and has to reach the trigger comparison. No other vector
    /// has `payment_gas_used > quota`, which means deleting the term outright goes unnoticed
    /// everywhere else, including against go-bsc's own generated chain.
    #[test]
    fn payment_lane_signal_counts_gas_spent_beyond_the_quota() {
        // parent gas limit 55M: expand at signal >= 44_000_000, shrink below 38_500_000.
        let parent = parent_with(3_000_000, 20_000_000, 55_000_000, 47_000_000);
        let signal = Signal::from_parent(&parent).unwrap();

        // 27_000_000 general + 17_000_000 over the 3M quota, landing exactly on the trigger.
        assert_eq!(signal.0.unwrap().signal_gas_used, 44_000_000);

        // Both branches land strictly inside [floor, ceiling], so the clamp cannot hide the
        // difference: with the overspill counted the quota expands, without it it shrinks.
        let with_overspill = signal.next_lane_quota(&DEFAULT_PARAMS, 55_000_000);
        assert_eq!(with_overspill, 4_100_000, "expand: 3M + 200 * 55M / 10000");

        let general_only = signal_of(3_000_000, 27_000_000, 55_000_000);
        assert_eq!(
            general_only.next_lane_quota(&DEFAULT_PARAMS, 55_000_000),
            2_725_000,
            "shrink: 3M - 50 * 55M / 10000 — what dropping the overspill term would give"
        );
    }

    #[test]
    fn payment_lane_signal_derivation() {
        // gas_used 30_001_000 less the 1_000 of payment gas, plus nothing above the quota.
        let parent = parent_with(2_400_000, 1_000, 40_000_000, 30_001_000);
        let signal = Signal::from_parent(&parent).unwrap();
        let parent_signal = signal.0.expect("parent carries a commitment");

        assert_eq!(parent_signal.signal_gas_used, 30_000_000);
        assert_eq!(parent_signal.lane_quota, 2_400_000);
        assert_eq!(parent_signal.gas_limit, 40_000_000);
        // 30e6 sits inside [70%, 80%) of 40e6, so the quota holds.
        assert_eq!(signal.next_lane_quota(&DEFAULT_PARAMS, 55_000_000), 2_400_000);
    }

    #[test]
    fn payment_lane_commitment() {
        let c = Commitment { quota: 0x0102030405060708, payment_gas_used: 0x1112131415161718 };
        assert_eq!(
            c.encode(),
            b256!("01020304050607081112131415161718" "0000000000000000" "0000000000000000")
        );
        assert_eq!(Commitment::default().encode(), B256::ZERO);
        assert_eq!(Commitment::decode(B256::ZERO).unwrap(), Commitment::default());
        assert!(matches!(
            Commitment::decode(EMPTY_OMMER_ROOT_HASH),
            Err(LaneError::BadCommitment(_))
        ));
        assert!(Commitment::commits_no_uncles(EMPTY_OMMER_ROOT_HASH));
        assert!(Commitment::commits_no_uncles(B256::ZERO), "all-zero is a legal commitment");
        assert!(
            !Commitment::commits_no_uncles(b256!(
                "0x1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49348"
            )),
            "a real uncle hash must not pass the shape check"
        );
        assert!(Commitment::commits_no_uncles(c.encode()));
        // all 128 reserved bits, one at a time
        for byte in 16..32usize {
            for bit in 0..8u32 {
                let mut h = c.encode().0;
                h[byte] |= 1 << bit;
                assert!(
                    Commitment::decode(B256::from(h)).is_err(),
                    "reserved byte {byte} bit {bit} must be rejected"
                );
            }
        }
    }

    #[test]
    fn payment_lane_header_bounds() {
        // Both bounds fail here, so only the branch order decides which error surfaces.
        assert!(matches!(
            Commitment { quota: 200, payment_gas_used: 150 }.check_header_bounds(100, 100),
            Err(LaneError::Untruthy { .. })
        ));

        let (gu, gl) = (3_000_000u64, 55_000_000u64);
        let ok = |q, p| Commitment { quota: q, payment_gas_used: p }.check_header_bounds(gu, gl);
        assert_eq!(ok(0, 0), Ok(()));
        assert_eq!(ok(2_000_000, 900_000), Ok(()));
        assert_eq!(ok(2_000_000, 3_000_000), Ok(()));
        assert_eq!(ok(52_000_000, 0), Ok(()));
        assert!(matches!(ok(0, 3_000_001), Err(LaneError::Untruthy { .. })));
        assert!(matches!(ok(55_000_001, 0), Err(LaneError::Violated { .. })));
        assert!(matches!(ok(52_000_001, 0), Err(LaneError::Violated { .. })));
    }

    #[test]
    fn payment_lane_is_a_floor_not_a_ceiling() {
        let gl = 100u64;
        let q = 20u64;
        let case = |general: u64, payment: u64| check_inequality(gl, general + payment, payment, q);
        assert_eq!(case(80, 20), Ok(()));
        assert_eq!(case(79, 21), Ok(()));
        assert!(case(81, 19).is_err());
        assert_eq!(case(80, 0), Ok(()));
        assert!(case(81, 0).is_err());
        assert_eq!(case(0, 100), Ok(()));
    }

    #[test]
    fn payment_lane_inequality_overflow() {
        let half = u64::MAX / 2 + 1;
        assert!(
            check_inequality(u64::MAX, half, 0, half).is_err(),
            "checked_add must reject; saturating_add would accept (MAX <= MAX)"
        );
        // demonstrate the saturating variant would pass
        let sat = half.saturating_add(half) <= u64::MAX;
        assert!(sat);
        assert!(check_inequality(70_000_000, u64::MAX, 0, 0).is_err());
        assert!(check_inequality(70_000_000, 70_000_000, 0, u64::MAX).is_err());
        assert!(check_inequality(70_000_000, 70_000_001, 0, 0).is_err());
        assert_eq!(check_inequality(70_000_000, 1000, u64::MAX, u64::MAX), Ok(()));
    }

    struct FakeDb {
        code_hash: B256,
        exists: bool,
        reads: std::cell::Cell<u32>,
    }
    impl FakeDb {
        fn code_is_empty(&self) -> Result<bool, LaneError> {
            self.reads.set(self.reads.get() + 1);
            Ok(!self.exists || self.code_hash == KECCAK_EMPTY || self.code_hash.is_zero())
        }
    }

    #[test]
    fn payment_lane_classify_gate_order_and_state_reads() {
        let target = Address::repeat_byte(0x11);
        let empty_list: HashSet<Address> = HashSet::default();
        let listed: HashSet<Address> = [target].into_iter().collect();

        // four code-hash encodings
        let encodings: &[(&str, B256, bool, Lane)] = &[
            ("zero hash / account absent", B256::ZERO, false, Lane::Payment),
            ("KECCAK_EMPTY (EOA)", KECCAK_EMPTY, true, Lane::Payment),
            ("0x..0beef (has code)", b256!("000000000000000000000000000000000000000000000000000000000000beef"), true, Lane::General),
            ("EIP-7702 delegation marker", b256!("eadcdba66a79ab5dce91622d1d75c8cff5cff0b96944c3bf1072cd08ce018329"), true, Lane::General),
        ];
        for &(name, ch, exists, want) in encodings {
            let db = FakeDb { code_hash: ch, exists, reads: 0.into() };
            let got = classify(Some(target), 2, U256::from(1), &empty_list, |_| db.code_is_empty()).unwrap();
            assert_eq!(got, want, "{name}");
            assert_eq!(db.reads.get(), 1);
        }

        // gate exhaustion: 5 tx types x 4 targets x {listed, value}
        let mut table = Vec::new();
        for ty in [0u8, 1, 2, 3, 4] {
            for to in [None, Some(target)] {
                for is_listed in [false, true] {
                    for value in [U256::ZERO, U256::from(1)] {
                        let l = if is_listed { &listed } else { &empty_list };
                        let db = FakeDb { code_hash: KECCAK_EMPTY, exists: true, reads: 0.into() };
                        let got = classify(to, ty, value, l, |_| db.code_is_empty()).unwrap();
                        let reads = db.reads.get();
                        // expected, from the gate order in `classify`
                        let (want, want_reads) = if to.is_none() {
                            (Lane::General, 0)
                        } else if !matches!(ty, 0..=2) {
                            (Lane::General, 0)
                        } else if is_listed {
                            (Lane::Payment, 0)
                        } else if value.is_zero() {
                            (Lane::General, 0)
                        } else {
                            (Lane::Payment, 1)
                        };
                        assert_eq!((got, reads), (want, want_reads), "ty={ty} to={to:?} listed={is_listed} value={value}");
                        table.push((ty, to.is_some(), is_listed, !value.is_zero(), got, reads));
                    }
                }
            }
        }

        // listed hit must NOT read state — pinned with a panicking closure
        let got = classify(Some(target), 0, U256::ZERO, &listed, |_| panic!("must not read state")).unwrap();
        assert_eq!(got, Lane::Payment);
    }

    #[test]
    fn payment_lane_classify_rereads_code_when_it_changes() {
        let target = Address::repeat_byte(0x22);
        let empty: HashSet<Address> = HashSet::default();
        let reads = std::cell::Cell::new(0u32);
        let code_hash = std::cell::Cell::new(B256::ZERO); // no code yet
        let call = || {
            classify(Some(target), 2, U256::from(1), &empty, |_| {
                reads.set(reads.get() + 1);
                let ch = code_hash.get();
                Ok(ch.is_zero() || ch == KECCAK_EMPTY)
            })
        };
        assert_eq!(call().unwrap(), Lane::Payment);
        code_hash.set(b256!("eadcdba66a79ab5dce91622d1d75c8cff5cff0b96944c3bf1072cd08ce018329"));
        assert_eq!(call().unwrap(), Lane::General);
        assert_eq!(reads.get(), 2, "exactly two state reads; a cached answer would give one");
    }

    #[test]
    fn payment_lane_admits_implies_legal() {
        // Only "admitted => still legal" holds. The converse fails by exactly the miner's
        // system-tx reserve, because the packing budget subtracts that reserve while the quota
        // is capped by the 20M protocol constant. go-bsc is conservative in the same way, so
        // this is the shape of the rule, not a divergence.
        let mut one_way = 0u32;
        let mut rejected_but_legal = 0u32;
        for reserved in [0u64, 1, 5] {
            for quota in [0u64, 5, 20, 40, 100] {
                for used in [0u64, 3, 20, 50] {
                    for gas_used in [0u64, 10, 50, 99, 100] {
                        let gl = 100u64;
                        let b = Budget { quota, used };
                        // Only meaningful over legal pre-states: the block so far is valid.
                        if b.verify(gl, gas_used).is_err() {
                            continue;
                        }
                        let shared = gl.saturating_sub(gas_used).saturating_sub(reserved);
                        for lane in [Lane::General, Lane::Payment] {
                            for g in 0..=shared {
                                let admits = b.admits(shared, lane, g);
                                let mut after = b.clone();
                                after.record_used(lane, g);
                                let legal = after.verify(gl, gas_used + g).is_ok();

                                assert!(
                                    !admits || legal,
                                    "admitted an illegal block: reserved={reserved} quota={quota} \
                                     used={used} gas_used={gas_used} lane={lane:?} g={g}"
                                );
                                if reserved == 0 {
                                    // With no miner reserve the two coincide, which is the
                                    // form go-bsc pins.
                                    assert_eq!(admits, legal, "reserved=0 quota={quota} used={used} gas_used={gas_used} lane={lane:?} g={g}");
                                }
                                one_way += 1;
                                if !admits && legal {
                                    rejected_but_legal += 1;
                                }
                            }
                        }
                    }
                }
            }
        }
        assert!(one_way > 0);
        assert!(
            rejected_but_legal > 0,
            "with a non-zero reserve the converse must fail somewhere, or this test is vacuous"
        );
    }

    #[test]
    fn payment_lane_max_available_gas_is_monotone() {
        let gl = 1_000_000u64;
        let mut b = Budget { quota: 300_000, used: 0 };
        let mut gas_used = 0u64;
        let mut prev = (b.max_available_gas(gl, Lane::General), b.max_available_gas(gl, Lane::Payment));
        let mut steps = 0;
        for (i, g) in (0..40).map(|i| (i, 7_000u64 + (i as u64 * 1_301) % 9_000)) {
            let lane = if i % 3 == 0 { Lane::Payment } else { Lane::General };
            if !b.admits(gl - gas_used, lane, g) {
                continue;
            }
            gas_used += g;
            b.record_used(lane, g);
            let now = (
                b.max_available_gas(gl - gas_used, Lane::General),
                b.max_available_gas(gl - gas_used, Lane::Payment),
            );
            assert!(now.0 <= prev.0 && now.1 <= prev.1, "step {i}: {prev:?} -> {now:?}");
            prev = now;
            steps += 1;
        }
        assert!(steps > 5, "the loop must actually admit transactions, not skip them all");
    }

    /// `verify` runs `used > gas_used` before the accounting rule. Two separately-failing
    /// examples cannot test an order, so this input fails both and only the order decides
    /// which error surfaces.
    #[test]
    fn payment_lane_verify_error_order() {
        let b = Budget { quota: 200, used: 1 };
        assert!(matches!(b.verify(100, 0), Err(LaneError::Untruthy { .. })));
        assert!(matches!(
            check_inequality(100, 0, 1, 200),
            Err(LaneError::Violated { .. })
        ));
    }

    fn legal_param_grid() -> Vec<GovernanceParams> {
        let mut v = Vec::new();
        for min_ratio in [0u64, 200, 1000] {
            for max_ratio in [200u64, 800, 2000] {
                if min_ratio > max_ratio {
                    continue;
                }
                for (shrink_trigger, expand_trigger) in
                    [(0u64, 1u64), (3000, 5000), (7000, 8000), (7000, 9999)]
                {
                    for expand_step in [1u64, 50, 200, 1000] {
                        for shrink_step in [1u64, 50, 200] {
                            for (min_gas, max_gas) in
                                [(0u64, 1u64), (0, 8_000_000), (2_000_000, 8_000_000), (2_000_000, 2_000_000)]
                            {
                                v.push(GovernanceParams {
                                    min_ratio,
                                    max_ratio,
                                    expand_trigger,
                                    shrink_trigger,
                                    expand_step,
                                    shrink_step,
                                    min_gas,
                                    max_gas,
                                });
                            }
                        }
                    }
                }
            }
        }
        v
    }

    #[test]
    fn payment_lane_clamp_grid() {
        let gas_limits = [
            0u64, 1, 21_000, 1_000_000, SYSTEM_TXS_GAS_HARD_LIMIT, 20_000_001, 21_700_000,
            40_000_000, 55_000_000, 55_009_999, 70_000_000, 140_000_000, u64::MAX,
        ];
        let grid = legal_param_grid();
        for p in &grid {
            for &gl in &gas_limits {
                let b = bounds(p, gl);
                assert!(b.floor <= b.ceiling, "floor {} > ceiling {} at gl {gl} p {p:?}", b.floor, b.ceiling);
                for sig in [Signal(None), signal_of(3_000_000, 0, 10_000), signal_of(3_000_000, u64::MAX, 10_000)] {
                    let q = sig.next_lane_quota(p, gl);
                    assert!(q <= b.reserve_cap, "quota {q} > reserveCap {}", b.reserve_cap);
                    assert!(q <= mul_div_floor(2000, gl, RATIO_DENOM), "quota {q} exceeds 2000bps of gl {gl}");
                    if b.reserve_cap >= b.ceiling {
                        assert!(q >= b.floor && q <= b.ceiling, "quota {q} outside [{}, {}]", b.floor, b.ceiling);
                    }
                }
                // in-window and not stepping => unchanged
                let mid = (b.floor + b.ceiling) / 2;
                if b.reserve_cap >= b.ceiling && p.shrink_trigger < p.expand_trigger {
                    let s = signal_of(mid, mul_div_floor(p.shrink_trigger, 10_000, RATIO_DENOM), 10_000);
                    let q = s.next_lane_quota(p, gl);
                    assert_eq!(q, mid.max(b.floor).min(b.ceiling), "hysteresis must not move quota");
                }
            }
        }
    }

    #[test]
    fn payment_lane_reserve_cap_crossover() {
        // reserve_cap strictly decides the quota while gas_limit - 20M < 0.08 * gas_limit,
        // i.e. 0.92 * gas_limit < 20M. Integer truncation puts the last such gas limit at
        // 21_739_129, one below the real-valued 21_739_130.4.
        assert!(reserve_cap(21_739_129) < lane_floor(&DEFAULT_PARAMS, 21_739_129));
        assert!(reserve_cap(21_739_130) >= lane_floor(&DEFAULT_PARAMS, 21_739_130));

        // A falling gas limit must stay clamped every block, and cross into the cap-decided
        // region without the quota ever exceeding it.
        let mut gas_limit = 70_000_000u64;
        let mut quota = Signal(None).next_lane_quota(&DEFAULT_PARAMS, gas_limit);
        let mut reached_cap_region = false;
        for _ in 0..1500 {
            gas_limit -= gas_limit / 1024;
            let parent = parent_with(quota, 0, gas_limit, gas_limit);
            let b = bounds(&DEFAULT_PARAMS, gas_limit);
            quota = Signal::from_parent(&parent).unwrap().next_lane_quota(&DEFAULT_PARAMS, gas_limit);

            assert!(quota <= b.reserve_cap);
            if b.reserve_cap < b.floor {
                reached_cap_region = true;
                assert!(quota < b.floor, "past the crossover the cap must push below the floor");
            }
        }
        assert!(reached_cap_region, "1500 blocks from 70M must cross the ~21.7M threshold");
    }

    #[test]
    fn payment_lane_saturation_and_no_carry() {
        // satAdd wrap guard: next near MAX + step must clamp to ceiling, not wrap
        let p = GovernanceParams { max_ratio: 2000, max_gas: u64::MAX, ..DEFAULT_PARAMS };
        let s = signal_of(u64::MAX - 1, u64::MAX, 10_000);
        let gl = 55_000_000u64;
        let got = s.next_lane_quota(&p, gl);
        let b = bounds(&p, gl);
        assert_eq!(got, b.ceiling.min(b.reserve_cap));
        assert_ne!(got, (u64::MAX - 1).wrapping_add(mul_div_floor(200, gl, RATIO_DENOM)));

        // signal's two terms never carry
        let vals = [0u64, 1, 21_000, 55_000_000, u64::MAX / 2, u64::MAX - 1, u64::MAX];
        for &gas_used in &vals {
            for &payment in &vals {
                for &quota in &vals {
                    let a = gas_used.saturating_sub(payment);
                    let bb = payment.saturating_sub(quota);
                    assert!(a.checked_add(bb).is_some(), "carry at ({gas_used},{payment},{quota})");
                    assert!(a + bb <= gas_used.max(payment), "bound violated");
                }
            }
        }

        // gasLimit exactly 2^62: 8000 * gl == 0 in u64 but gte must still be right
        let gl = 1u64 << 62;
        assert_eq!(8000u64.wrapping_mul(gl), 0, "naive u64 product is zero");
        assert!(!gte(1, RATIO_DENOM, 8000, gl), "tiny signal must NOT expand");
        assert!(gte(u64::MAX, RATIO_DENOM, 8000, gl), "huge signal must expand");
    }

    #[test]
    fn payment_lane_accepts_invariant_violating_params() {
        // maxRatio + expandTrigger > 10000, min_ratio > max_ratio, triggers inverted,
        // min_gas > max_gas — all accepted, quota still derived.
        let bad = GovernanceParams {
            min_ratio: 9000,
            max_ratio: 8000,
            expand_trigger: 3000,
            shrink_trigger: 9000,
            expand_step: 5000,
            shrink_step: 5000,
            min_gas: 9_000_000,
            max_gas: 1_000_000,
        };
        assert!(bad.max_ratio + bad.expand_trigger > 10_000);
        let gl = 55_000_000u64;
        let b = bounds(&bad, gl);
        assert!(b.floor <= b.ceiling, "the inner min still keeps floor <= ceiling: {b:?}");
        let q = Signal(None).next_lane_quota(&bad, gl);
        assert_eq!(q, b.floor.min(b.reserve_cap));
    }

    // invariant-violating params (shrinkTrigger > expandTrigger), which BEP-703 §3.6 accepts.
    #[test]
    fn payment_lane_step_branches_are_exclusive() {
        // An `else if` and two
        // independent `if`s differ whenever both predicates hold at once.
        let p = GovernanceParams { expand_trigger: 3000, shrink_trigger: 9000, ..DEFAULT_PARAMS };
        let (gl, pgl, quota, bps) = (55_000_000u64, 10_000u64, 3_000_000u64, 5000u64);
        assert!(gte(bps, RATIO_DENOM, p.expand_trigger, pgl), "expand predicate holds");
        assert!(!gte(bps, RATIO_DENOM, p.shrink_trigger, pgl), "shrink predicate ALSO holds");

        let as_else_if = signal_of(quota, bps, pgl).next_lane_quota(&p, gl);

        // the same thing with two independent ifs, spelled out
        let mut next = quota;
        if gte(bps, RATIO_DENOM, p.expand_trigger, pgl) {
            next = next.saturating_add(mul_div_floor(p.expand_step, gl, RATIO_DENOM));
        }
        if !gte(bps, RATIO_DENOM, p.shrink_trigger, pgl) {
            next = next.saturating_sub(mul_div_floor(p.shrink_step, gl, RATIO_DENOM));
        }
        let b = bounds(&p, gl);
        let as_two_ifs = next.max(b.floor).min(b.ceiling).min(b.reserve_cap);

        // `as_else_if` came out of production `next_lane_quota`, so a difference here is
        // proof that production takes the `else if` form — matching go-bsc's `switch`.
        assert_ne!(
            as_else_if, as_two_ifs,
            "production must not apply both steps: else-if gives {as_else_if}, two ifs give {as_two_ifs}"
        );
    }

    /// `quota <= 2000 * gas_limit / 10000` holds only while the parameters are contract-legal
    /// (`max_ratio <= 2000`), and BEP-703 §3.6 forbids the client from enforcing that. So the
    /// bound must never be asserted over arbitrary getter output — here it is broken on purpose.
    #[test]
    fn payment_lane_ratio_bound_breaks_outside_the_legal_grid() {
        let p = GovernanceParams { max_ratio: 5000, max_gas: u64::MAX, ..DEFAULT_PARAMS };
        let gl = 55_000_000u64;
        let mut q = 3_000_000u64;
        for _ in 0..40 {
            q = signal_of(q, u64::MAX, 10_000).next_lane_quota(&p, gl); // always expanding
        }
        assert!(q > mul_div_floor(2000, gl, RATIO_DENOM));
    }
}
