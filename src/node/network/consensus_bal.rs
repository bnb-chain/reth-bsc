//! Bounded retention of full BAL attachments. A commitment match is not execution validation.

use alloy_consensus::Header;
use alloy_eips::eip7928::{bal::Bal, BlockAccessList};
use alloy_primitives::{keccak256, Bytes, B256};
use parking_lot::Mutex;
use std::{collections::VecDeque, sync::LazyLock};

// Local retention limits, not consensus limits. Eviction always permits sequential execution.
const CACHE_BYTES: usize = 128 * 1024 * 1024;
const CACHE_ENTRIES: usize = 128;

/// Prototype handshake marker in the existing BSC capability packet's opaque extension.
pub(crate) const CAPABILITY: &[u8] = b"bep732-bal/1";

#[derive(Default)]
struct BalCache {
    entries: VecDeque<(B256, Bytes)>,
    bytes: usize,
}

impl BalCache {
    fn insert(&mut self, hash: B256, raw: Bytes) {
        if raw.len() > CACHE_BYTES || self.entries.iter().any(|(key, _)| *key == hash) {
            return;
        }
        while self.entries.len() >= CACHE_ENTRIES || self.bytes + raw.len() > CACHE_BYTES {
            if let Some((_, old)) = self.entries.pop_front() {
                self.bytes -= old.len();
            }
        }
        self.bytes += raw.len();
        self.entries.push_back((hash, raw));
    }
}

static CACHE: LazyLock<Mutex<BalCache>> = LazyLock::new(|| Mutex::new(BalCache::default()));

/// Retain only canonical, structurally valid bytes matching the sealed block's commitment.
/// Returns whether validation succeeded; valid entries can still exceed the local cache budget.
/// Failure discards the attachment, never the block; execution remains authoritative.
pub(crate) fn retain(header: &Header, transaction_count: usize, raw: Bytes) -> bool {
    if header.block_access_list_hash != Some(keccak256(&raw)) {
        return false;
    }
    let Ok(list) = alloy_rlp::decode_exact::<BlockAccessList>(&raw) else { return false };
    let bal = Bal::from(list);
    if bal.validate_structure(transaction_count).is_err() ||
        bal.validate_gas_limit(header.gas_limit).is_err()
    {
        return false;
    }
    CACHE.lock().insert(header.hash_slow(), raw);
    true
}

/// Retrieve a retained attachment for this exact block, including noncanonical branches.
pub(crate) fn get(hash: B256) -> Option<Bytes> {
    CACHE.lock().entries.iter().find(|(key, _)| *key == hash).map(|(_, raw)| raw.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BscBlock;

    #[test]
    fn cache_evicts_oldest_and_does_not_duplicate() {
        let mut cache = BalCache::default();
        for index in 0..=CACHE_ENTRIES {
            cache.insert(
                B256::from_slice(&alloy_primitives::U256::from(index).to_be_bytes::<32>()),
                Bytes::from_static(&[0xc0]),
            );
        }
        assert_eq!(cache.entries.len(), CACHE_ENTRIES);
        assert_eq!(cache.bytes, CACHE_ENTRIES);
        assert!(!cache.entries.iter().any(|(key, _)| *key == B256::ZERO));
        let last = cache.entries.back().unwrap().0;
        cache.insert(last, Bytes::from_static(&[0xc0]));
        assert_eq!(cache.bytes, CACHE_ENTRIES);
    }

    #[test]
    fn cache_enforces_byte_budget_and_skips_oversized_entries() {
        let mut cache = BalCache::default();
        let raw = Bytes::from(vec![0; CACHE_BYTES / 2 + 1]);
        cache.insert(B256::repeat_byte(1), raw.clone());
        cache.insert(B256::repeat_byte(2), raw);
        assert_eq!(cache.entries.len(), 1);
        assert_eq!(cache.entries[0].0, B256::repeat_byte(2));
        assert_eq!(cache.bytes, CACHE_BYTES / 2 + 1);
        cache.insert(B256::repeat_byte(3), Bytes::from(vec![0; CACHE_BYTES + 1]));
        assert_eq!(cache.entries.len(), 1);
        assert_eq!(cache.entries[0].0, B256::repeat_byte(2));
        assert_eq!(cache.bytes, CACHE_BYTES / 2 + 1);
    }

    #[test]
    fn invalid_attachment_does_not_replace_retained_bal() {
        let mut block = BscBlock::default();
        let header = &mut block.header;
        header.number = 732;
        header.block_access_list_hash = Some(keccak256([0xc0]));
        assert!(retain(header, 0, Bytes::from_static(&[0xc0])));
        assert!(!retain(header, 0, Bytes::from_static(&[0x80])));
        assert_eq!(get(header.hash_slow()), Some(Bytes::from_static(&[0xc0])));
        header.block_access_list_hash = Some(keccak256([0x80]));
        assert!(!retain(header, 0, Bytes::from_static(&[0x80])));
        assert_eq!(get(header.hash_slow()), None);
    }
}
