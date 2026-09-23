//! ABI loading and caching of the parent-state lane ratio and payment contract list.

use super::{
    rules, LaneError, LaneParentState, MAX_LISTED_CONTRACTS, PAGE_SIZE, PAYMENT_LANE_CONTRACT,
    RATIO_DENOM,
};
use alloy_primitives::{map::HashSet, Address, BlockHash, Bytes, U256};
use alloy_sol_types::{sol, SolCall};
use schnellru::{ByLength, LruMap};
use std::sync::{Arc, LazyLock, Mutex};

sol! {
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

/// Parent-state governance metadata.
#[derive(Clone, Debug)]
pub struct LaneMeta {
    /// Reserved fraction of block gas: `ratio / RATIO_DENOM`.
    pub ratio: u64,
    /// Shared across cache entries and executors without copying the set.
    pub listed: Arc<HashSet<Address>>,
}

fn ratio_calldata() -> Bytes {
    getPaymentLaneRatioCall {}.abi_encode().into()
}

fn contracts_calldata(offset: u64) -> Bytes {
    getPaymentContractsCall { offset: U256::from(offset), limit: U256::from(PAGE_SIZE) }
        .abi_encode()
        .into()
}

fn decode_ratio(ret: &[u8]) -> Result<u64, LaneError> {
    let value = getPaymentLaneRatioCall::abi_decode_returns(ret)
        .map_err(|e| corrupt(format!("getPaymentLaneRatio decode: {e}")))?;
    rules::check_ratio(value)
}

/// Accumulates validated pages. Discard the walk on error; do not retry a partial page.
#[derive(Debug, Default)]
struct PageWalk {
    /// `totalLength` as reported by the first page; every later page must agree.
    total: Option<u64>,
    listed: HashSet<Address>,
}

impl PageWalk {
    /// Folds in the page read at `offset`, returning the next offset or `None` when done.
    fn accept(&mut self, offset: u64, ret: &[u8]) -> Result<Option<u64>, LaneError> {
        let r = getPaymentContractsCall::abi_decode_returns(ret)
            .map_err(|e| corrupt(format!("getPaymentContracts decode: {e}")))?;
        let total = u64::try_from(r.totalLength).map_err(|_| {
            corrupt(format!("payment contract count exceeds u64: {}", r.totalLength))
        })?;

        match self.total {
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
            if !self.listed.insert(*addr) {
                return Err(corrupt(format!("payment contract {addr} listed twice")));
            }
        }

        let next = offset + n;
        Ok(if next >= total { None } else { Some(next) })
    }

    /// Completes the walk, failing if the pages did not add up to the reported count.
    fn finish(self) -> Result<HashSet<Address>, LaneError> {
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

/// Post-state metadata keyed by block hash. The executor's revm account interface does not
/// expose the storage root used by go-bsc's cache key. Instead, unchanged metadata is inherited
/// along verified parent-child edges; a changed contract forces children to reload.
static CACHE: LazyLock<Mutex<LruMap<BlockHash, LaneMeta, ByLength>>> =
    LazyLock::new(|| Mutex::new(LruMap::new(ByLength::new(1024))));

/// Returns cached parent metadata, or loads it through [`LaneParentState`] on a miss.
pub(super) fn load(
    access: &mut impl LaneParentState,
    parent_hash: BlockHash,
) -> Result<LaneMeta, LaneError> {
    if let Some(hit) = cache_get(parent_hash) {
        return Ok(hit);
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
    cache_store(parent_hash, &meta);
    Ok(meta)
}

pub(super) fn cache_store(hash: BlockHash, meta: &LaneMeta) {
    CACHE.lock().unwrap().insert(hash, meta.clone());
}

pub(crate) fn cache_get(hash: BlockHash) -> Option<LaneMeta> {
    CACHE.lock().unwrap().get(&hash).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::hex;

    fn word(v: u64) -> Vec<u8> {
        U256::from(v).to_be_bytes::<32>().to_vec()
    }

    // Independent ABI fixture: array offset and total count, followed by the array.
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
    fn getter_calldata_matches_the_contract() {
        assert_eq!(getPaymentLaneRatioCall::SELECTOR, hex!("c988aaf7"));
        assert_eq!(getPaymentContractsCall::SELECTOR, hex!("08fcc45a"));
        let call = getPaymentContractsCall::abi_decode(&contracts_calldata(7)).unwrap();
        assert_eq!(call.offset, U256::from(7));
        assert_eq!(call.limit, U256::from(PAGE_SIZE));
    }

    #[test]
    fn ratio_decodes_and_is_guarded() {
        // The getter supplies the default, not the loader.
        assert_eq!(decode_ratio(&word(500)), Ok(500));
        assert_eq!(decode_ratio(&word(1_000)), Ok(1_000));

        for bad in [word(0), word(1_001), U256::MAX.to_be_bytes::<32>().to_vec()] {
            assert!(matches!(decode_ratio(&bad), Err(LaneError::CorruptConfig(_))), "{bad:?}");
        }
        // 2^64 + 500 narrows to a legal 500, so the guard must run before any narrowing.
        let wraps = ((U256::from(1u64) << 64u32) + U256::from(500u64)).to_be_bytes::<32>();
        assert!(matches!(decode_ratio(&wraps), Err(LaneError::CorruptConfig(_))));

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

    // 2^64 must not truncate to an empty list.
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
        // Pin the reason so a different failing check cannot make the case pass.
        let because = |r: Result<Option<u64>, LaneError>, want: &str| {
            let msg = r.expect_err("must reject").to_string();
            assert!(msg.contains(want), "want {want:?}, got {msg:?}");
        };
        let first_page = |w: &mut PageWalk| {
            assert_eq!(w.accept(0, &page(200, &a[..PAGE_SIZE as usize])).unwrap(), Some(PAGE_SIZE));
        };

        // Check the ceiling before rejecting the empty page.
        because(PageWalk::default().accept(0, &page(MAX_LISTED_CONTRACTS + 1, &[])), "ceiling");

        let mut w = PageWalk::default();
        first_page(&mut w);
        because(w.accept(PAGE_SIZE, &page(201, &a[PAGE_SIZE as usize..])), "count changed");

        because(PageWalk::default().accept(0, &page(5, &[])), "empty payment contract page");

        because(PageWalk::default().accept(0, &page(500, &addrs(0..PAGE_SIZE + 1))), "limit was");

        let mut w = PageWalk::default();
        first_page(&mut w);
        because(w.accept(500, &page(200, &a[PAGE_SIZE as usize..])), "past the");

        because(PageWalk::default().accept(0, &page(3, &addrs(0..4))), "overruns");

        let dup = vec![Address::repeat_byte(1), Address::repeat_byte(1)];
        because(PageWalk::default().accept(0, &page(2, &dup)), "listed twice");

        // Duplicate across pages.
        let mut w = PageWalk::default();
        first_page(&mut w);
        because(w.accept(PAGE_SIZE, &page(200, &a[..1])), "listed twice");

        because(PageWalk::default().accept(0, &[0u8; 16]), "decode");

        let mut w = PageWalk::default();
        first_page(&mut w);
        assert!(w.finish().is_err());
    }
}
