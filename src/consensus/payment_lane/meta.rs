//! Reading the governable lane ratio and the payment contract list out of `0x2007`, and caching
//! the result. go-bsc's `core/paymentlanemeta`.

use super::{
    LaneError, LaneParentState, MAX_LANE_RATIO, MAX_LISTED_CONTRACTS, PAGE_SIZE,
    PAYMENT_LANE_CONTRACT, RATIO_DENOM,
};
use alloy_primitives::{map::HashSet, Address, BlockHash, Bytes, U256};
use alloy_sol_types::{sol, SolCall};
use schnellru::{ByLength, LruMap};
use std::sync::{Arc, LazyLock, Mutex};

sol! {
    /// The getters on payment lane contract `0x2007`.
    #[derive(Debug)]
    function getPaymentLaneRatio() external view returns (uint256);

    #[derive(Debug)]
    function getPaymentContracts(uint256 offset, uint256 limit)
        external
        view
        returns (address[] paymentContracts, uint256 totalLength);
}

fn corrupt(msg: String) -> LaneError {
    LaneError::CorruptConfig(msg)
}

/// What the lane rules need from `0x2007`, as of one block's parent post-state.
#[derive(Clone, Debug)]
pub struct LaneMeta {
    /// The governable reservation, as a fraction of the gas limit against `RATIO_DENOM`.
    pub ratio: u64,
    /// Shared: the cache clones it per block, and the classifier holds it during execution.
    pub listed: Arc<HashSet<Address>>,
}

impl LaneMeta {
    pub fn quota(&self, gas_limit: u64) -> u64 {
        super::rules::quota(self.ratio, gas_limit)
    }
}

pub fn ratio_calldata() -> Bytes {
    getPaymentLaneRatioCall {}.abi_encode().into()
}

pub fn contracts_calldata(offset: u64) -> Bytes {
    getPaymentContractsCall { offset: U256::from(offset), limit: U256::from(PAGE_SIZE) }
        .abi_encode()
        .into()
}

/// Applies the ratio guard at the full `uint256` width.
pub fn decode_ratio(ret: &[u8]) -> Result<u64, LaneError> {
    let value = getPaymentLaneRatioCall::abi_decode_returns(ret)
        .map_err(|e| corrupt(format!("getPaymentLaneRatio decode: {e}")))?;
    match u64::try_from(value) {
        Ok(ratio) if ratio > 0 && ratio <= MAX_LANE_RATIO => Ok(ratio),
        _ => Err(corrupt(format!(
            "payment lane ratio {value} outside 0 < ratio <= {MAX_LANE_RATIO}"
        ))),
    }
}

/// Folds the paged contract list into one set, rejecting every inconsistency.
///
/// [`Self::accept`] only ever reports an offset greater than the one it was given, so the walk
/// terminates; it inserts as it goes, so an error leaves the set half-filled — drop the walk
/// rather than retrying a page.
#[derive(Debug, Default)]
pub struct PageWalk {
    /// `totalLength` as reported by the first page; every later page must agree.
    total: Option<u64>,
    listed: HashSet<Address>,
}

impl PageWalk {
    /// Folds in the page read at `offset`, returning the next offset or `None` when done.
    pub fn accept(&mut self, offset: u64, ret: &[u8]) -> Result<Option<u64>, LaneError> {
        let r = getPaymentContractsCall::abi_decode_returns(ret)
            .map_err(|e| corrupt(format!("getPaymentContracts decode: {e}")))?;
        let total = u64::try_from(r.totalLength).map_err(|_| {
            corrupt(format!("payment contract count exceeds u64: {}", r.totalLength))
        })?;

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
            return Err(corrupt(format!(
                "empty payment contract page at offset {offset} of {total}"
            )));
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

    /// Completes the walk, failing if the pages did not add up to the reported count.
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

/// The lane config as of the post-state of one block, keyed by that block's hash: a block whose
/// parent is `P` reads the entry for `P`.
///
/// go-bsc keys the same cache by `0x2007`'s `(codeHash, storageRoot)`, which is content-addressed
/// and needs no propagation. revm's account carries no storage root, and computing one means
/// walking the contract's storage trie every block, so this keys by hash instead and moves an
/// entry along a real parent-child edge ([`cache_inherit`]) whenever the block left `0x2007`
/// alone. Same guarantee, reth's primitives: a sibling branch's config can never be read here,
/// because an entry only ever reaches a hash whose parent it was read at.
static CACHE: LazyLock<Mutex<LruMap<BlockHash, LaneMeta, ByLength>>> =
    LazyLock::new(|| Mutex::new(LruMap::new(ByLength::new(1024))));

/// Reads the ratio and the contract list as of `parent_hash`'s post-state, memoized by that hash.
///
/// `access` must still be looking at that post-state; it is the implementor's job to refuse once
/// it is not (see [`LaneParentState`]). A late read would be cached under `parent_hash` and
/// poison every sibling block with that parent.
pub fn load(
    access: &mut impl LaneParentState,
    parent_hash: BlockHash,
) -> Result<LaneMeta, LaneError> {
    if let Some(hit) = CACHE.lock().unwrap().get(&parent_hash) {
        return Ok(hit.clone());
    }

    let started = std::time::Instant::now();
    let ratio = decode_ratio(&access.call_lane_getter(PAYMENT_LANE_CONTRACT, ratio_calldata())?)?;

    let mut walk = PageWalk::default();
    let mut offset = 0u64;
    let mut pages = 0u64;
    loop {
        let ret = access.call_lane_getter(PAYMENT_LANE_CONTRACT, contracts_calldata(offset))?;
        pages += 1;
        match walk.accept(offset, &ret)? {
            Some(next) => offset = next,
            None => break,
        }
    }
    let listed = walk.finish()?;
    tracing::info!(
        target: "bsc::payment_lane",
        contract = %PAYMENT_LANE_CONTRACT,
        parent = %parent_hash,
        ratio,
        denom = RATIO_DENOM,
        listed = listed.len(),
        pages,
        elapsed_ms = started.elapsed().as_millis(),
        "payment lane config loaded"
    );

    let meta = LaneMeta { ratio, listed: Arc::new(listed) };
    CACHE.lock().unwrap().insert(parent_hash, meta.clone());
    Ok(meta)
}

/// Carries a block's config forward to its own hash, so the walk costs one read per governance
/// change rather than one per block.
///
/// The caller must not call this for a block that wrote to `0x2007`: the config it ran under is
/// no longer the one its children will see.
pub fn cache_inherit(block_hash: BlockHash, meta: &LaneMeta) {
    CACHE.lock().unwrap().insert(block_hash, meta.clone());
}

/// Test-only: what the cache holds for `block_hash`.
#[cfg(test)]
pub fn cache_peek(block_hash: BlockHash) -> Option<LaneMeta> {
    CACHE.lock().unwrap().get(&block_hash).cloned()
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

    /// Walks a whole list `PAGE_SIZE` at a time, the way [`load`] does.
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

    /// Pinned against the deployed dispatcher: a wrong selector reads a different function and
    /// the whole lane silently changes meaning.
    #[test]
    fn getter_selectors_match_the_contract() {
        assert_eq!(getPaymentLaneRatioCall::SELECTOR, hex!("c988aaf7"));
        assert_eq!(getPaymentContractsCall::SELECTOR, hex!("08fcc45a"));
    }

    #[test]
    fn page_limit_is_never_zero() {
        let call = getPaymentContractsCall::abi_decode(&contracts_calldata(7)).unwrap();
        assert_eq!(call.offset, U256::from(7));
        assert_eq!(call.limit, U256::from(PAGE_SIZE));
    }

    #[test]
    fn ratio_decodes_and_is_guarded() {
        // The contract applies the default itself, so 500 is what an untouched slot returns.
        assert_eq!(decode_ratio(&word(500)), Ok(500));
        assert_eq!(decode_ratio(&word(1_000)), Ok(1_000));

        // Zero never reaches a node: the getter maps it to the default. If one ever did, it must
        // be rejected rather than turned into a lane of nothing.
        for bad in [word(0), word(1_001), U256::MAX.to_be_bytes::<32>().to_vec()] {
            assert!(matches!(decode_ratio(&bad), Err(LaneError::CorruptConfig(_))), "{bad:?}");
        }
        // 2^64 + 500 narrows to a legal 500, so the guard must run before any narrowing.
        let wraps = ((U256::from(1u64) << 64u32) + U256::from(500u64)).to_be_bytes::<32>();
        assert!(matches!(decode_ratio(&wraps), Err(LaneError::CorruptConfig(_))));

        // Short or absent return data stays the decoder's error.
        assert!(matches!(decode_ratio(&[]), Err(LaneError::CorruptConfig(_))));
        assert!(matches!(decode_ratio(&word(500)[..31]), Err(LaneError::CorruptConfig(_))));
    }

    #[test]
    fn walk_folds_every_page() {
        // A walk that read no page at all is not an empty list.
        assert!(PageWalk::default().finish().is_err());

        assert!(walk(0, &[]).unwrap().is_empty());
        assert_eq!(walk(PAGE_SIZE, &addrs(0..PAGE_SIZE)).unwrap().len(), PAGE_SIZE as usize);
        let all = addrs(0..300);
        assert_eq!(walk(300, &all).unwrap(), all.into_iter().collect::<HashSet<_>>());
    }

    /// A count of exactly 2^64 truncates to 0 and would take the "empty list is normal" path,
    /// silently discarding the allowlist and booking every payment transaction as general.
    #[test]
    fn rejects_a_count_over_u64() {
        let mut ret = word(0x40);
        ret.extend_from_slice(&(U256::from(1u64) << 64u32).to_be_bytes::<32>());
        ret.extend_from_slice(&word(0));
        assert!(matches!(PageWalk::default().accept(0, &ret), Err(LaneError::CorruptConfig(_))));
    }

    #[test]
    fn walk_rejects_inconsistent_pages() {
        let a = addrs(0..PAGE_SIZE + 1);
        // Each case asserts on the MESSAGE: `is_err()` alone would still pass if these
        // conditions were reordered, or if a case tripped a different check than it names.
        let because = |r: Result<Option<u64>, LaneError>, want: &str| {
            let msg = r.expect_err("must reject").to_string();
            assert!(msg.contains(want), "want {want:?}, got {msg:?}");
        };
        let first_page = |w: &mut PageWalk| {
            assert_eq!(w.accept(0, &page(200, &a[..PAGE_SIZE as usize])).unwrap(), Some(PAGE_SIZE));
        };

        // the ceiling, checked before anything else is trusted: this page is also empty while
        // entries remain, so only the order of the checks decides which one surfaces
        because(
            PageWalk::default().accept(0, &page(MAX_LISTED_CONTRACTS + 1, &[])),
            "ceiling",
        );

        // total changes mid-walk
        let mut w = PageWalk::default();
        first_page(&mut w);
        because(w.accept(PAGE_SIZE, &page(201, &a[PAGE_SIZE as usize..])), "count changed");

        // a page that is empty while entries remain
        because(PageWalk::default().accept(0, &page(5, &[])), "empty payment contract page");

        // a page longer than the limit asked for
        because(PageWalk::default().accept(0, &page(500, &addrs(0..PAGE_SIZE + 1))), "limit was");

        // an offset past the reported count
        let mut w = PageWalk::default();
        first_page(&mut w);
        because(w.accept(500, &page(200, &a[PAGE_SIZE as usize..])), "past the");

        // a page that overruns the count from its offset
        because(PageWalk::default().accept(0, &page(3, &addrs(0..4))), "overruns");

        // a duplicate inside one page
        let dup = vec![Address::repeat_byte(1), Address::repeat_byte(1)];
        because(PageWalk::default().accept(0, &page(2, &dup)), "listed twice");

        // a duplicate across pages, caught only because the set is cumulative
        let mut w = PageWalk::default();
        first_page(&mut w);
        because(w.accept(PAGE_SIZE, &page(200, &a[..1])), "listed twice");

        // garbage return data
        because(PageWalk::default().accept(0, &[0u8; 16]), "decode");

        // pages that agree with each other but not with the count: only the tally catches it
        let mut w = PageWalk::default();
        first_page(&mut w);
        assert!(w.finish().is_err());
    }
}
