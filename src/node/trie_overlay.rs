use alloy_primitives::B256;
use parking_lot::RwLock;
use reth_trie::{updates::TrieUpdates, HashedPostState};
use std::{ops::RangeInclusive, sync::Arc};
use std::sync::OnceLock;

/// Cached per-block overlay needed to bridge DB lag for parallel state root.
///
/// When the canonical chain head is ahead of the persisted DB tip, we can still run
/// `ParallelStateRoot` by supplying the missing canonical blocks' hashed state + trie updates as
/// an overlay via `TrieInput`.
#[derive(Debug, Clone)]
pub struct TrieOverlayEntry {
    pub number: u64,
    pub hash: B256,
    pub hashed_state: Arc<HashedPostState>,
    pub trie_updates: Option<Arc<TrieUpdates>>,
}

#[derive(Debug, Default)]
pub struct TrieOverlayCache {
    // Keyed by block number.
    by_number: std::collections::BTreeMap<u64, TrieOverlayEntry>,
    // Soft bound to keep memory predictable.
    capacity: usize,
}

impl TrieOverlayCache {
    pub fn new(capacity: usize) -> Self {
        Self { by_number: Default::default(), capacity }
    }

    pub fn insert(&mut self, entry: TrieOverlayEntry) {
        self.by_number.insert(entry.number, entry);
        self.trim();
    }

    pub fn remove_range(&mut self, range: RangeInclusive<u64>) {
        let keys: Vec<u64> = self.by_number.range(range).map(|(n, _)| *n).collect();
        for k in keys {
            self.by_number.remove(&k);
        }
    }

    pub fn get_range(&self, range: RangeInclusive<u64>) -> Vec<TrieOverlayEntry> {
        self.by_number
            .range(range)
            .map(|(_, v)| v.clone())
            .collect()
    }

    pub fn get(&self, number: u64) -> Option<TrieOverlayEntry> {
        self.by_number.get(&number).cloned()
    }

    fn trim(&mut self) {
        if self.capacity == 0 {
            self.by_number.clear();
            return;
        }
        while self.by_number.len() > self.capacity {
            // Drop the oldest.
            let Some(oldest) = self.by_number.keys().next().copied() else { break };
            self.by_number.remove(&oldest);
        }
    }
}

static TRIE_OVERLAY: OnceLock<Arc<RwLock<TrieOverlayCache>>> = OnceLock::new();

/// Initialize the global trie overlay cache.
pub fn init_trie_overlay_cache(capacity: usize) -> Arc<RwLock<TrieOverlayCache>> {
    TRIE_OVERLAY
        .get_or_init(|| Arc::new(RwLock::new(TrieOverlayCache::new(capacity))))
        .clone()
}

/// Get the global trie overlay cache if initialized.
pub fn trie_overlay_cache() -> Option<&'static Arc<RwLock<TrieOverlayCache>>> {
    TRIE_OVERLAY.get()
}

