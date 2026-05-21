//! Miner-owned cross-block execution cache.
//!
//! Encapsulates: cache data structures, the single-writer updater task,
//! the state-provider wrapper, and the global handle. See
//! `docs/superpowers/specs/2026-05-21-miner-cross-block-cache-design.md`
//! for correctness invariants and scenario analysis.

#![allow(dead_code)]

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
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

impl MinerExecCache {
    pub(super) fn new() -> Self {
        Self {
            accounts: Cache::builder()
                .max_capacity(ACCOUNTS_CAPACITY_BYTES)
                .build(),
            storage: Cache::builder()
                .max_capacity(STORAGE_OUTER_CAPACITY_BYTES)
                .build(),
            code: Cache::builder()
                .max_capacity(CODE_CAPACITY_BYTES)
                .build(),
            chain_epoch: AtomicU64::new(0),
            version: AtomicU64::new(0),
            last_apply_at_ms: AtomicI64::new(0),
        }
    }

    /// Applies a post-block `BundleState` into the cache.
    ///
    /// # Ordering invariant (spec §6.1, §9.4 S2)
    ///
    /// Version is bumped **after** all moka writes (Release ordering). If we
    /// bumped first, a concurrent reader could observe `v_at_borrow = new_v`
    /// before the writes land, read a stale entry for a key in the bundle,
    /// and incorrectly accept it as current.
    pub(super) fn apply_bundle(&self, bundle: &revm::database::BundleState) {
        let chain_epoch = self.chain_epoch.load(Ordering::Acquire); // chain_epoch snapshot
        let new_v = self.version.load(Ordering::Acquire) + 1;

        // Bytecode is content-addressable (code_hash → bytes); no destruction logic
        // applies. Tag every entry with (new_v, chain_epoch) for reader-side filter
        // consistency (spec §6.1).
        for (code_hash, bytecode) in &bundle.contracts {
            self.code.insert(*code_hash, (Some(Bytecode(bytecode.clone())), new_v, chain_epoch));
        }

        for (addr, account) in &bundle.state {
            if account.was_destroyed() {
                // CRITICAL: insert-replace, NOT invalidate. moka invalidate is
                // eventually consistent — get(K) may keep returning the old value
                // for an unbounded period. insert is per-key linearizable, so the
                // empty Arc is immediately visible to subsequent callers (spec §6.1).
                // The accounts tombstone (None) lets Task 10's lookup_storage
                // short-circuit destroyed-account storage lookups (spec §6.3).
                self.storage.insert(*addr, Arc::new(MinerStorageCache::new()));
                self.accounts.insert(*addr, (None, new_v, chain_epoch));
                continue;
            }
            // `Account` stores only (balance, nonce, bytecode_hash); inline bytecode
            // is cached separately by Task 6 via bundle.contracts.
            let reth_account = account.info.as_ref().map(Account::from);
            self.accounts.insert(*addr, (reth_account, new_v, chain_epoch));

            // Storage slots — non-destruction path (spec §6.1).
            //
            // Reuse the existing per-account Arc when present; only create a new
            // empty one if no entry exists yet. NEVER replace an existing populated
            // Arc — that would silently drop unrelated cached slots written by earlier
            // blocks.
            //
            // Safety: apply_bundle is only ever called by the single updater task
            // (spec §5), so there is no concurrent writer that could race between
            // our .get() and .insert() below.
            if !account.storage.is_empty() {
                let storage_arc = match self.storage.get(addr) {
                    Some(arc) => arc,
                    None => {
                        let new_arc = Arc::new(MinerStorageCache::new());
                        self.storage.insert(*addr, new_arc.clone());
                        new_arc
                    }
                };
                for (key, slot) in &account.storage {
                    // revm StorageKey is U256; our slots cache is keyed by
                    // alloy_primitives::StorageKey (= B256). Convert via big-endian bytes.
                    let slot_key = B256::from(key.to_be_bytes::<32>());
                    storage_arc.slots.insert(
                        slot_key,
                        (Some(slot.present_value), new_v, chain_epoch),
                    );
                }
            }
            // Code writes land in Task 6.
        }

        self.last_apply_at_ms.store(now_ms(), Ordering::Relaxed);
        // MUST be last: bump version only after all moka writes are visible.
        self.version.store(new_v, Ordering::Release);
    }
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
    use std::sync::atomic::Ordering;

    use alloy_primitives::U256;
    use revm::database::{
        states::StorageSlot, AccountStatus, BundleAccount, BundleState, StorageWithOriginalValues,
    };
    use revm::state::AccountInfo;

    // ------------------------------------------------------------------
    // Test helpers
    // ------------------------------------------------------------------

    fn mk_account_info(balance: u64) -> AccountInfo {
        AccountInfo {
            balance: U256::from(balance),
            nonce: 0,
            code_hash: B256::ZERO,
            account_id: None,
            code: None,
        }
    }

    fn mk_bundle_with_destroyed(addr: Address) -> BundleState {
        use std::collections::HashMap;
        let mut state = HashMap::default();
        state.insert(
            addr,
            BundleAccount {
                info: None,
                original_info: None,
                storage: StorageWithOriginalValues::default(),
                status: AccountStatus::Destroyed,
            },
        );
        BundleState {
            state,
            contracts: HashMap::default(),
            reverts: Default::default(),
            state_size: 0,
            reverts_size: 0,
        }
    }

    fn mk_bundle_with_account(addr: Address, balance: u64) -> BundleState {
        use std::collections::HashMap;
        let mut state = HashMap::default();
        state.insert(
            addr,
            BundleAccount {
                info: Some(mk_account_info(balance)),
                original_info: None,
                storage: StorageWithOriginalValues::default(),
                status: AccountStatus::Changed,
            },
        );
        BundleState {
            state,
            contracts: HashMap::default(),
            reverts: Default::default(),
            state_size: 0,
            reverts_size: 0,
        }
    }

    fn mk_bundle_with_slot(addr: Address, slot_key: U256, slot_value: U256) -> BundleState {
        use std::collections::HashMap;
        let mut storage = StorageWithOriginalValues::default();
        storage.insert(
            slot_key,
            StorageSlot {
                previous_or_original_value: U256::ZERO,
                present_value: slot_value,
            },
        );
        let mut state = HashMap::default();
        state.insert(
            addr,
            BundleAccount {
                info: Some(mk_account_info(0)),
                original_info: None,
                storage,
                status: AccountStatus::Changed,
            },
        );
        BundleState {
            state,
            contracts: HashMap::default(),
            reverts: Default::default(),
            state_size: 0,
            reverts_size: 0,
        }
    }

    // ------------------------------------------------------------------
    // Basic sanity tests
    // ------------------------------------------------------------------

    #[test]
    fn skeleton_compiles() {
        // Trivial test to confirm module compiles.
        let _cache_size = ACCOUNTS_CAPACITY_BYTES;
        const { assert!(STALENESS_THRESHOLD_MS > 0) };
    }

    #[test]
    fn new_initializes_atomics_to_zero() {
        let cache = MinerExecCache::new();
        assert_eq!(cache.chain_epoch.load(Ordering::Acquire), 0);
        assert_eq!(cache.version.load(Ordering::Acquire), 0);
        assert_eq!(cache.last_apply_at_ms.load(Ordering::Relaxed), 0);
    }

    // ------------------------------------------------------------------
    // apply_bundle tests
    // ------------------------------------------------------------------

    #[test]
    fn apply_bundle_inserts_non_destroyed_account() {
        let cache = MinerExecCache::new();
        let addr = Address::from([0xAB; 20]);
        let bundle = mk_bundle_with_account(addr, 100);

        let v_before = cache.version.load(Ordering::Acquire);
        cache.apply_bundle(&bundle);
        let v_after = cache.version.load(Ordering::Acquire);

        assert_eq!(v_after, v_before + 1, "version must be bumped exactly once");
        let entry = cache.accounts.get(&addr).expect("account should be cached");
        let (value, write_v, ce) = entry;
        assert!(value.is_some(), "value should not be a tombstone");
        assert_eq!(write_v, v_after, "entry write_version must equal new version");
        assert_eq!(
            ce,
            cache.chain_epoch.load(Ordering::Acquire),
            "entry chain_epoch must equal current chain_epoch"
        );
    }

    #[test]
    fn apply_bundle_bumps_version_sequentially() {
        let cache = MinerExecCache::new();
        let addr = Address::from([0xCD; 20]);

        for i in 1u64..=3 {
            cache.apply_bundle(&mk_bundle_with_account(addr, i * 10));
            assert_eq!(cache.version.load(Ordering::Acquire), i, "version step {i}");
        }
    }

    #[test]
    fn apply_bundle_destruction_tombstones_and_replaces_storage() {
        let cache = MinerExecCache::new();
        let addr = Address::from([0xEF; 20]);

        // Seed: account exists. Task 3 only inserts the account entry, not the
        // storage Arc, so storage_arc_before will be None here. The assertion
        // below (option a) checks that a fresh Arc IS present after destruction,
        // rather than testing Arc ptr-inequality. After Task 5 lands (which adds
        // storage writes in the non-destruction branch), this can be strengthened.
        cache.apply_bundle(&mk_bundle_with_account(addr, 99));

        // Destroy.
        cache.apply_bundle(&mk_bundle_with_destroyed(addr));

        // Accounts: tombstone (None) at latest version.
        let (value, write_v, _ce) = cache.accounts.get(&addr).expect("must have tombstone");
        assert!(value.is_none(), "destruction must store None tombstone");
        assert_eq!(
            write_v,
            cache.version.load(Ordering::Acquire),
            "tombstone write_version must equal current version"
        );

        // Storage: outer cache contains a fresh empty Arc, NOT invalidated.
        // (Option a: just assert presence; Arc ptr-inequality test requires Task 5.)
        let _storage_arc_after =
            cache.storage.get(&addr).expect("storage entry must be inserted (not removed) on destruction");
    }

    #[test]
    fn apply_bundle_writes_code() {
        use std::collections::HashMap;
        let cache = MinerExecCache::new();
        let code_hash = B256::from([0xAA; 32]);
        let bytecode = Bytecode::new_raw(vec![0x60, 0x00, 0x60, 0x00, 0xfd].into());

        let mut contracts = HashMap::default();
        contracts.insert(code_hash, bytecode.0.clone());
        let bundle = revm::database::BundleState {
            state: HashMap::default(),
            contracts,
            reverts: Default::default(),
            state_size: 0,
            reverts_size: 0,
        };

        cache.apply_bundle(&bundle);

        let entry = cache.code.get(&code_hash).expect("code cached");
        let (cached_code, write_v, _ce) = entry;
        assert_eq!(cached_code, Some(bytecode));
        assert_eq!(write_v, cache.version.load(Ordering::Acquire));
    }

    #[test]
    fn apply_bundle_writes_storage_slots() {
        let cache = MinerExecCache::new();
        let addr = Address::from([0x11; 20]);
        let key = U256::from(42);
        let value = U256::from(777);

        cache.apply_bundle(&mk_bundle_with_slot(addr, key, value));

        let storage = cache.storage.get(&addr).expect("storage container exists");
        let slot_key = B256::from(key.to_be_bytes::<32>());
        let entry = storage.slots.get(&slot_key).expect("slot cached");
        let (cached_val, write_v, _ce) = entry;
        assert_eq!(cached_val, Some(value));
        assert_eq!(write_v, cache.version.load(Ordering::Acquire));
    }
}
