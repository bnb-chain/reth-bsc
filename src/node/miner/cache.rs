//! Miner-owned cross-block execution cache.
//!
//! Encapsulates: cache data structures, the single-writer updater task,
//! the state-provider wrapper, and the global handle. See
//! `docs/superpowers/specs/2026-05-21-miner-cross-block-cache-design.md`
//! for correctness invariants and scenario analysis.

#![allow(dead_code)]

use std::sync::atomic::{AtomicI64, AtomicU64};
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use alloy_primitives::{Address, StorageKey, StorageValue, B256};
use moka::sync::Cache;
use reth_primitives_traits::{Account, Bytecode};

// ===========================================================================
// Constants
// ===========================================================================

/// Heartbeat staleness threshold. `peek_for` returns `None` if no apply_bundle
/// or on_reorg has run within this window.
const STALENESS_THRESHOLD_MS: i64 = 5_000;

/// Cache sizing (bytes, rough budgets — moka uses size_weighted LRU).
const ACCOUNTS_CAPACITY_BYTES: u64 = 1024 * 1024 * 1024; // 1 GB
const STORAGE_OUTER_CAPACITY_BYTES: u64 = 2_500 * 1024 * 1024; // 2.5 GB
const STORAGE_INNER_CAPACITY_BYTES: u64 = 16 * 1024 * 1024; // 16 MB per account
const CODE_CAPACITY_BYTES: u64 = 500 * 1024 * 1024; // 500 MB

// ===========================================================================
// Private types
// ===========================================================================

/// Entry value type for `accounts` and `code` caches.
/// `value` is the cached state (`None` is a tombstone for destroyed accounts).
type AccountEntry = (Option<Account>, u64, u64); // (value, write_version, chain_epoch)
type CodeEntry = (Option<Bytecode>, u64, u64);
type SlotEntry = (Option<StorageValue>, u64, u64);

/// Per-account storage cache. Replaced (not invalidated) on account destruction.
struct MinerStorageCache {
    slots: Cache<StorageKey, SlotEntry>,
}

impl MinerStorageCache {
    fn new() -> Self {
        Self {
            slots: Cache::builder()
                .max_capacity(STORAGE_INNER_CAPACITY_BYTES)
                .build(),
        }
    }
}

/// Top-level cache. Single instance, `Arc`-wrapped, registered in `EXEC_CACHE`.
pub(super) struct MinerExecCache {
    accounts: Cache<Address, AccountEntry>,
    storage: Cache<Address, Arc<MinerStorageCache>>,
    code: Cache<B256, CodeEntry>,
    chain_epoch: AtomicU64,
    version: AtomicU64,
    last_apply_at_ms: AtomicI64,
}

/// Snapshot handle taken at `peek_for`. RAII: drop releases nothing
/// (we don't use SavedCache.usage_guard semantics).
pub(super) struct MinerCacheBorrow {
    cache: Arc<MinerExecCache>,
    chain_epoch_at_borrow: u64,
    v_at_borrow: u64,
}

// ===========================================================================
// Global handle
// ===========================================================================

static EXEC_CACHE: OnceLock<Arc<MinerExecCache>> = OnceLock::new();

// ===========================================================================
// Helpers
// ===========================================================================

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skeleton_compiles() {
        // Trivial test to confirm module compiles.
        let _cache_size = ACCOUNTS_CAPACITY_BYTES;
        const { assert!(STALENESS_THRESHOLD_MS > 0) };
    }
}
