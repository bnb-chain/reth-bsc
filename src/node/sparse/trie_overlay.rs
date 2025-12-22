// This file was moved from `src/node/sparse_integrator/trie_overlay.rs`.

use alloy_consensus::BlockHeader;
use alloy_primitives::B256;
use reth_chain_state::ExecutedTrieUpdates;
use reth_trie::{updates::TrieUpdates, HashedPostState};
use std::{ops::RangeInclusive, sync::Arc};

/// Cached per-block overlay needed to bridge DB lag for trie root computation.
///
/// When the canonical chain head is ahead of the persisted DB tip, we can still run sparse/parallel
/// state root algorithms by supplying the missing canonical blocks' hashed state + trie updates as
/// an overlay via `TrieInput`.
#[derive(Debug, Clone)]
pub struct TrieOverlayEntry {
    /// Block number for this overlay entry.
    pub number: u64,
    /// Canonical block hash at this number.
    pub hash: B256,
    /// Hashed post state overlay for this block.
    pub hashed_state: Arc<HashedPostState>,
    /// Cached trie updates (intermediate nodes) for this block, if available.
    pub trie_updates: Option<Arc<TrieUpdates>>,
}

/// A small in-memory cache keyed by block number storing overlay entries.
#[derive(Debug, Default)]
pub struct TrieOverlayCache {
    /// Keyed by block number.
    by_number: std::collections::BTreeMap<u64, TrieOverlayEntry>,
    /// Soft bound to keep memory predictable.
    capacity: usize,
}

impl TrieOverlayCache {
    /// Create a new cache with the given capacity.
    ///
    /// Capacity is a soft bound: when exceeded, oldest entries are evicted.
    pub fn new(capacity: usize) -> Self {
        Self { by_number: Default::default(), capacity }
    }

    /// Insert/replace an entry.
    pub fn insert(&mut self, entry: TrieOverlayEntry) {
        self.by_number.insert(entry.number, entry);
        self.trim();
    }

    /// Build and insert a [`TrieOverlayEntry`] from an executed block update.
    ///
    /// Prefer the per-block `hashed_state` and (optional) `trie_updates` coming from
    /// `ExecutedBlockWithTrieUpdates`.
    pub fn insert_from_executed<N: reth_primitives_traits::NodePrimitives>(
        &mut self,
        block: &reth_chain_state::ExecutedBlockWithTrieUpdates<N>,
    ) {
        let number = block.recovered_block().number() as u64;
        let hash = block.recovered_block().hash();

        let trie_updates: Option<Arc<TrieUpdates>> = match &block.trie {
            ExecutedTrieUpdates::Present(updates) => Some(updates.clone()),
            ExecutedTrieUpdates::Missing => None,
        };

        self.insert(TrieOverlayEntry {
            number,
            hash,
            hashed_state: block.block.hashed_state.clone(),
            trie_updates,
        });
    }

    /// Remove entries in the given range.
    pub fn remove_range(&mut self, range: RangeInclusive<u64>) {
        let keys: Vec<u64> = self.by_number.range(range).map(|(n, _)| *n).collect();
        for k in keys {
            self.by_number.remove(&k);
        }
    }

    /// Get all entries in the given range, in ascending block number order.
    pub fn get_range(&self, range: RangeInclusive<u64>) -> Vec<TrieOverlayEntry> {
        self.by_number.range(range).map(|(_, v)| v.clone()).collect()
    }

    /// Get the entry for the given block number.
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


