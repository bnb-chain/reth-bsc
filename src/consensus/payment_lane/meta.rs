//! Decoding the governable lane parameters and the payment contract list.
//!
//! Two getters, both `view`: the eight parameters, and the payment contract list walked one
//! page at a time. Every value the lane rules consume comes from here — the defaults of
//! BEP-703 §3.6.1 live in the contract, not in this client, so there is one source of truth.
//!
//! This module decodes and validates; it does not call. The caller supplies each page's raw
//! return data, which keeps every reject condition below testable without an EVM.

use super::{GovernanceParams, LaneError, MAX_LISTED_CONTRACTS, PAGE_SIZE};
use alloy_primitives::{map::HashSet, Address, Bytes, U256};
use alloy_sol_types::{sol, SolCall};
use std::sync::Arc;

sol! {
    /// Field order is the contract's slot order and `abi_decode_returns` is positional, so
    /// all eight must be spelled out here in that order. Abbreviating hands the wire format
    /// to luck.
    #[derive(Debug)]
    struct PaymentLaneParams {
        uint256 paymentLaneMinRatio;
        uint256 paymentLaneMaxRatio;
        uint256 expandTriggerRatio;
        uint256 shrinkTriggerRatio;
        uint256 expandStepRatio;
        uint256 shrinkStepRatio;
        uint256 paymentLaneMin;
        uint256 paymentLaneMax;
    }

    #[derive(Debug)]
    function getPaymentLaneParams() external view returns (PaymentLaneParams);

    /// The returns must stay named: unnamed ones generate fields `_0`/`_1` instead.
    #[derive(Debug)]
    function getPaymentContracts(uint256 offset, uint256 limit)
        external
        view
        returns (address[] paymentContracts, uint256 totalLength);
}

/// Every failure in this module is a verdict on the block, so they share one constructor.
fn corrupt(msg: String) -> LaneError {
    LaneError::CorruptConfig(msg)
}

/// Everything the lane rules need from `0x2007`, read as of one block's parent post-state.
#[derive(Clone, Debug)]
pub struct LaneMeta {
    pub params: GovernanceParams,
    /// Shared: the cache hands out a clone per block, and the per-transaction classifier holds
    /// it while the executor reads state.
    pub listed: Arc<HashSet<Address>>,
}

/// Calldata for `getPaymentLaneParams()`.
pub fn params_calldata() -> Bytes {
    getPaymentLaneParamsCall {}.abi_encode().into()
}

/// Calldata for one page of `getPaymentContracts(offset, limit)`.
///
/// `limit` is always [`PAGE_SIZE`], never 0: the contract reads 0 as "return everything left",
/// which would slip past the one-page-is-at-most-`PAGE_SIZE` check below.
pub fn contracts_calldata(offset: u64) -> Bytes {
    getPaymentContractsCall { offset: U256::from(offset), limit: U256::from(PAGE_SIZE) }
        .abi_encode()
        .into()
}

/// Decodes the parameter getter's return data.
///
/// The only client-side validation BEP-703 permits is "does it fit in a `u64`". The six
/// invariants of BEP-703 §3.6.2 are the contract's to enforce on write; re-checking them here would
/// stall this client on a configuration go-bsc happily accepts.
pub fn decode_params(ret: &[u8]) -> Result<GovernanceParams, LaneError> {
    // Deliberately no length check. The struct return is encoded inline — eight words, no
    // outer offset — and the decoder rejects anything short. It ignores trailing words, and so
    // does go-bsc, which has no length check either: were a later contract upgrade to append a
    // ninth field, geth would keep importing while a stricter reth-bsc rejected every block
    // from that point on. Matching go-bsc's tolerance is the safe direction.
    let p = getPaymentLaneParamsCall::abi_decode_returns(ret)
        .map_err(|e| corrupt(format!("getPaymentLaneParams decode: {e}")))?;

    let fit = |name: &str, v: U256| {
        u64::try_from(v)
            .map_err(|_| corrupt(format!("payment lane param {name} does not fit u64: {v}")))
    };
    Ok(GovernanceParams {
        min_ratio: fit("paymentLaneMinRatio", p.paymentLaneMinRatio)?,
        max_ratio: fit("paymentLaneMaxRatio", p.paymentLaneMaxRatio)?,
        expand_trigger: fit("expandTriggerRatio", p.expandTriggerRatio)?,
        shrink_trigger: fit("shrinkTriggerRatio", p.shrinkTriggerRatio)?,
        expand_step: fit("expandStepRatio", p.expandStepRatio)?,
        shrink_step: fit("shrinkStepRatio", p.shrinkStepRatio)?,
        min_gas: fit("paymentLaneMin", p.paymentLaneMin)?,
        max_gas: fit("paymentLaneMax", p.paymentLaneMax)?,
    })
}

/// Folds the paged `getPaymentContracts` walk into one set, rejecting every inconsistency.
///
/// The walk is driven by the page lengths the contract returns, and [`Self::accept`] only ever
/// reports an offset strictly greater than the one it was given, so it terminates.
///
/// Single-shot: [`Self::accept`] inserts as it goes, so an error leaves the set half-filled.
/// Drop the walk and start over rather than retrying a page.
#[derive(Debug, Default)]
pub struct PageWalk {
    /// `totalLength` as reported by the first page; every later page must agree.
    total: Option<u64>,
    listed: HashSet<Address>,
}

impl PageWalk {
    /// Decodes one page read at `offset` and folds it in.
    ///
    /// Returns the offset of the next page, or `None` when the list is complete.
    pub fn accept(&mut self, offset: u64, ret: &[u8]) -> Result<Option<u64>, LaneError> {
        let r = getPaymentContractsCall::abi_decode_returns(ret)
            .map_err(|e| corrupt(format!("getPaymentContracts decode: {e}")))?;
        let total = u64::try_from(r.totalLength)
            .map_err(|_| corrupt(format!("payment contract count exceeds u64: {}", r.totalLength)))?;

        match self.total {
            // The ceiling is checked before anything else is trusted.
            None if total > MAX_LISTED_CONTRACTS => {
                return Err(corrupt(format!(
                    "payment contract count {total} exceeds the {MAX_LISTED_CONTRACTS} ceiling"
                )))
            }
            None => self.total = Some(total),
            Some(first) if total != first => {
                return Err(corrupt(format!(
                    "payment contract count changed mid-walk: {first} then {total}"
                )))
            }
            Some(_) => {}
        }

        // An empty list is normal — that is how the fork starts.
        if total == 0 {
            return Ok(None);
        }
        let page = &r.paymentContracts;
        let n = page.len() as u64;
        if n == 0 {
            return Err(corrupt(format!("empty payment contract page at offset {offset} of {total}")));
        }
        if n > PAGE_SIZE {
            return Err(corrupt(format!(
                "payment contract page holds {n} entries, limit was {PAGE_SIZE}"
            )));
        }
        // Before the subtraction below can be trusted.
        if offset > total {
            return Err(corrupt(format!(
                "payment contract page offset {offset} past the {total} reported"
            )));
        }
        if n > total - offset {
            return Err(corrupt(format!(
                "payment contract page of {n} overruns {total} entries from offset {offset}"
            )));
        }
        for addr in page {
            // Against the cumulative set, so a page repeating an earlier page is caught too.
            if !self.listed.insert(*addr) {
                return Err(corrupt(format!("payment contract {addr} listed twice")));
            }
        }

        let next = offset + n;
        Ok(if next >= total { None } else { Some(next) })
    }

    /// Completes the walk. Fails if the pages did not add up to the reported count.
    pub fn finish(self) -> Result<HashSet<Address>, LaneError> {
        let total =
            self.total.ok_or_else(|| corrupt("payment contract walk read no pages".into()))?;
        if self.listed.len() as u64 != total {
            return Err(corrupt(format!(
                "payment contract walk collected {} of {total} entries",
                self.listed.len()
            )));
        }
        Ok(self.listed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::hex;

    fn word(v: u64) -> Vec<u8> {
        U256::from(v).to_be_bytes::<32>().to_vec()
    }

    /// `getPaymentContracts` return data: a head of two offsets, then the array.
    fn page(total: u64, addrs: &[Address]) -> Vec<u8> {
        let mut r = word(0x40);
        r.extend_from_slice(&word(total));
        r.extend_from_slice(&word(addrs.len() as u64));
        for a in addrs {
            r.extend_from_slice(a.into_word().as_slice());
        }
        r
    }

    fn addrs(range: std::ops::Range<u64>) -> Vec<Address> {
        range.map(|i| Address::from_word(U256::from(i + 1).into())).collect()
    }

    /// Walks a whole list, `PAGE_SIZE` at a time, the way `load_lane_meta` does.
    fn walk(total: u64, all: &[Address]) -> Result<HashSet<Address>, LaneError> {
        let mut w = PageWalk::default();
        let mut offset = 0u64;
        loop {
            let end = ((offset + PAGE_SIZE) as usize).min(all.len());
            let ret = page(total, &all[offset as usize..end]);
            match w.accept(offset, &ret)? {
                Some(next) => offset = next,
                None => break,
            }
        }
        w.finish()
    }

    #[test]
    fn payment_lane_getter_selectors_match_the_contract() {
        assert_eq!(getPaymentLaneParamsCall::SELECTOR, hex!("ff620147"));
        assert_eq!(getPaymentContractsCall::SELECTOR, hex!("08fcc45a"));
    }

    #[test]
    fn payment_lane_page_limit_is_never_zero() {
        // The contract reads limit 0 as "everything left", which would defeat the
        // one-page-is-at-most-PAGE_SIZE check.
        let call = getPaymentContractsCall::abi_decode(&contracts_calldata(7)).unwrap();
        assert_eq!(call.offset, U256::from(7));
        assert_eq!(call.limit, U256::from(PAGE_SIZE));
    }

    /// Eight distinct sentinels, deliberately not `DEFAULT_PARAMS`: the defaults repeat 200 for
    /// both `min_ratio` and `expand_step`, so transposing those two fields would pass unnoticed.
    #[test]
    fn payment_lane_params_decode_in_contract_order() {
        let mut ret = Vec::new();
        for v in 1..=8u64 {
            ret.extend_from_slice(&word(v));
        }
        assert_eq!(
            decode_params(&ret).unwrap(),
            GovernanceParams {
                min_ratio: 1,
                max_ratio: 2,
                expand_trigger: 3,
                shrink_trigger: 4,
                expand_step: 5,
                shrink_step: 6,
                min_gas: 7,
                max_gas: 8,
            }
        );
    }

    #[test]
    fn payment_lane_params_tolerate_trailing_words_but_not_short_data() {
        let eight = [0u8; 32 * 8];
        assert!(decode_params(&eight).is_ok());
        // A ninth word is ignored, matching go-bsc. Rejecting it would halt this client on a
        // contract upgrade geth keeps importing through.
        let mut nine = eight.to_vec();
        nine.extend_from_slice(&word(1));
        assert!(decode_params(&nine).is_ok());
        // Short data is the decoder's own error, and stays an error.
        assert!(matches!(decode_params(&eight[..32 * 7]), Err(LaneError::CorruptConfig(_))));
        assert!(matches!(decode_params(&[]), Err(LaneError::CorruptConfig(_))));
    }

    #[test]
    fn payment_lane_params_reject_values_over_u64() {
        // Every field, not just the first: a truncated `max_gas` yields a different quota
        // ceiling and every block then fails on the derived-vs-committed comparison.
        for i in 0..8 {
            let mut ret = [0u8; 32 * 8];
            ret[i * 32..(i + 1) * 32].copy_from_slice(&U256::MAX.to_be_bytes::<32>());
            assert!(
                matches!(decode_params(&ret), Err(LaneError::CorruptConfig(_))),
                "field {i} accepted a value over u64"
            );
        }
    }

    #[test]
    fn payment_lane_params_are_not_checked_against_bep_invariants() {
        // BEP-703 §3.6.2's invariants are the contract's to enforce on write. Checking them here
        // would stall this client on a configuration go-bsc accepts.
        let bad = [9_000u64, 9_000, 9_000, 1, 10_000, 10_000, u64::MAX, 0];
        let mut ret = Vec::new();
        for v in bad {
            ret.extend_from_slice(&word(v));
        }
        let p = decode_params(&ret).unwrap();
        assert_eq!(p.max_ratio + p.expand_trigger, 18_000); // way over RATIO_DENOM
        assert!(p.min_gas > p.max_gas);
    }

    #[test]
    fn payment_lane_rejects_a_count_over_u64() {
        // Exactly 2^64, not U256::MAX: MAX truncates to u64::MAX and the ceiling check catches
        // it anyway, while 2^64 truncates to 0 and would take the "an empty list is normal"
        // path — silently discarding the whole allowlist and booking every payment
        // transaction to the general lane.
        let mut ret = word(0x40);
        ret.extend_from_slice(&(U256::from(1u64) << 64u32).to_be_bytes::<32>());
        ret.extend_from_slice(&word(0));
        assert!(matches!(
            PageWalk::default().accept(0, &ret),
            Err(LaneError::CorruptConfig(_))
        ));
    }

    #[test]
    fn payment_lane_walk_needs_at_least_one_page() {
        // Not an empty list: a walk that read nothing must not pass for one.
        assert!(matches!(PageWalk::default().finish(), Err(LaneError::CorruptConfig(_))));
    }

    #[test]
    fn payment_lane_empty_list_is_normal() {
        assert!(walk(0, &[]).unwrap().is_empty());
    }

    #[test]
    fn payment_lane_walks_multiple_pages() {
        let all = addrs(0..300);
        let got = walk(300, &all).unwrap();
        assert_eq!(got.len(), 300);
        assert_eq!(got, all.into_iter().collect::<HashSet<_>>());
    }

    #[test]
    fn payment_lane_walk_stops_exactly_on_a_page_boundary() {
        let all = addrs(0..PAGE_SIZE);
        assert_eq!(walk(PAGE_SIZE, &all).unwrap().len(), PAGE_SIZE as usize);
    }

    #[test]
    fn payment_lane_list_ceiling_is_checked_before_anything_else() {
        let mut w = PageWalk::default();
        // The page is ALSO empty while entries remain, so both conditions apply and only the
        // order decides which surfaces. With a valid page the test would pass from any position.
        let err = w.accept(0, &page(MAX_LISTED_CONTRACTS + 1, &[])).unwrap_err();
        assert!(format!("{err}").contains("ceiling"), "{err}");
    }

    #[test]
    fn payment_lane_walk_rejects_inconsistent_pages() {
        let a = addrs(0..PAGE_SIZE + 1);
        // Each case asserts on the MESSAGE: `is_err()` alone would still pass if these
        // conditions were reordered, or if one case tripped a different check than it names.
        let because = |r: Result<Option<u64>, LaneError>, want: &str| {
            let msg = r.expect_err("must reject").to_string();
            assert!(msg.contains(want), "want {want:?}, got {msg:?}");
        };

        // total changes mid-walk
        let mut w = PageWalk::default();
        assert_eq!(w.accept(0, &page(200, &a[..PAGE_SIZE as usize])).unwrap(), Some(PAGE_SIZE));
        because(w.accept(PAGE_SIZE, &page(201, &a[PAGE_SIZE as usize..])), "count changed mid-walk");

        // a page that is empty while entries remain
        let mut w = PageWalk::default();
        because(w.accept(0, &page(5, &[])), "empty payment contract page");

        // a page longer than the limit asked for
        let mut w = PageWalk::default();
        because(w.accept(0, &page(500, &addrs(0..PAGE_SIZE + 1))), "limit was");

        // an offset past the reported count
        let mut w = PageWalk::default();
        assert_eq!(w.accept(0, &page(200, &a[..PAGE_SIZE as usize])).unwrap(), Some(PAGE_SIZE));
        because(w.accept(500, &page(200, &a[PAGE_SIZE as usize..])), "past the");

        // a page that overruns the count from its offset
        let mut w = PageWalk::default();
        because(w.accept(0, &page(3, &addrs(0..4))), "overruns");

        // a duplicate inside one page
        let mut w = PageWalk::default();
        let dup = vec![Address::repeat_byte(1), Address::repeat_byte(1)];
        because(w.accept(0, &page(2, &dup)), "listed twice");

        // a duplicate across pages — caught only because the set is cumulative
        let mut w = PageWalk::default();
        assert_eq!(w.accept(0, &page(200, &a[..PAGE_SIZE as usize])).unwrap(), Some(PAGE_SIZE));
        because(w.accept(PAGE_SIZE, &page(200, &a[..1])), "listed twice");

        // garbage return data
        let mut w = PageWalk::default();
        because(w.accept(0, &[0u8; 16]), "decode");
    }

    #[test]
    fn payment_lane_walk_rejects_a_short_total() {
        // Pages agree with each other but not with the count: nothing above catches this,
        // only the tally in `finish`.
        let mut w = PageWalk::default();
        assert_eq!(w.accept(0, &page(200, &addrs(0..PAGE_SIZE))).unwrap(), Some(PAGE_SIZE));
        assert!(matches!(w.finish(), Err(LaneError::CorruptConfig(_))));
    }
}
