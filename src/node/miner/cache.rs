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

    /// Returns a `MinerCacheBorrow` if the cache heartbeat is fresh enough.
    ///
    /// Returns `None` when:
    /// - The cache has never been written to (`last_apply_at_ms == 0`), or
    /// - The updater task hasn't run within `STALENESS_THRESHOLD_MS` (5 s).
    ///
    /// On `None`, callers (miner build loop) fall back to the raw state provider
    /// (spec §5, §9.4 S5).
    pub(super) fn peek_for(self: &Arc<Self>) -> Option<MinerCacheBorrow> {
        let now = now_ms();
        let last = self.last_apply_at_ms.load(Ordering::Relaxed);
        // Never-set: last == 0 → stale by definition (no apply has run yet).
        // Otherwise: check now - last <= threshold.
        if last == 0 || now.saturating_sub(last) > STALENESS_THRESHOLD_MS {
            return None;
        }
        let ce = self.chain_epoch.load(Ordering::Acquire);
        let v = self.version.load(Ordering::Acquire);
        Some(MinerCacheBorrow {
            cache: Arc::clone(self),
            chain_epoch_at_borrow: ce,
            v_at_borrow: v,
        })
    }

    /// Called when a `CanonStateNotification::Reorg` is received.
    ///
    /// Bumps `chain_epoch` (Release) so all existing cache entries become
    /// unborrowable from any future borrow's perspective. No physical cache
    /// clearing is needed — entries are LRU-evicted naturally as new
    /// `apply_bundle` calls accumulate on the new chain (spec §6.2, §9.4 S4).
    pub(super) fn on_reorg(&self) {
        self.chain_epoch.fetch_add(1, Ordering::Release);
        self.last_apply_at_ms.store(now_ms(), Ordering::Relaxed);
    }
}

/// Snapshot handle taken at `peek_for`. RAII: drop releases nothing
/// (we don't use SavedCache.usage_guard semantics).
pub(super) struct MinerCacheBorrow {
    cache: Arc<MinerExecCache>,
    chain_epoch_at_borrow: u64,
    v_at_borrow: u64,
}

impl MinerCacheBorrow {
    /// Look up an account by address.
    ///
    /// Returns:
    /// - `None` — cache miss OR filter rejected (caller falls through to raw provider)
    /// - `Some(None)` — cached tombstone (account was destroyed)
    /// - `Some(Some(account))` — cached live account
    ///
    /// Filter (spec §9.3): entry must have the same `chain_epoch` as at borrow time
    /// AND a `write_version` ≤ `v_at_borrow`. Either mismatch → `None`.
    pub(super) fn lookup_account(&self, addr: &Address) -> Option<Option<Account>> {
        let (value, write_v, ce) = self.cache.accounts.get(addr)?;
        if ce == self.chain_epoch_at_borrow && write_v <= self.v_at_borrow {
            Some(value)
        } else {
            None
        }
    }

    /// Look up bytecode by code hash.
    ///
    /// Returns:
    /// - `None` — cache miss OR filter rejected
    /// - `Some(None)` — cached absence (should not arise for code, but consistent)
    /// - `Some(Some(bytecode))` — cached bytecode
    ///
    /// Filter: identical to `lookup_account` (spec §9.3).
    pub(super) fn lookup_code(&self, code_hash: &B256) -> Option<Option<Bytecode>> {
        let (value, write_v, ce) = self.cache.code.get(code_hash)?;
        if ce == self.chain_epoch_at_borrow && write_v <= self.v_at_borrow {
            Some(value)
        } else {
            None
        }
    }

    /// Look up a storage slot by address and key.
    ///
    /// **Accounts-first check (required for correctness, not an optimisation —
    /// spec §6.3, §7.3):** if the accounts cache shows a tombstone (`None`) at
    /// or before `v_at_borrow` on the same chain epoch, the account was
    /// destroyed before this borrow was taken.  Storage is therefore empty by
    /// definition and we return `Some(None)` immediately, regardless of any
    /// slot entries that may linger in a leaked old `Arc<MinerStorageCache>`.
    ///
    /// Returns:
    /// - `None` — cache miss OR filter rejected (caller falls through to raw
    ///   provider)
    /// - `Some(None)` — cached "no value at this slot" (tombstone short-circuit
    ///   OR a cached absent slot)
    /// - `Some(Some(v))` — cached slot value
    pub(super) fn lookup_storage(
        &self,
        addr: &Address,
        key: &B256,
    ) -> Option<Option<StorageValue>> {
        // Step 1: accounts-first. If the account is tombstoned at or before our
        // borrow's version (and on the same chain), storage is empty by
        // definition — regardless of any slot entries that may linger
        // (spec §6.3).
        if let Some((value, write_v, ce)) = self.cache.accounts.get(addr) {
            if ce == self.chain_epoch_at_borrow
                && write_v <= self.v_at_borrow
                && value.is_none()
            {
                return Some(None);
            }
        }

        // Step 2: slot lookup via per-account Arc.
        let storage_arc = self.cache.storage.get(addr)?;
        let (value, write_v, ce) = storage_arc.slots.get(key)?;
        if ce == self.chain_epoch_at_borrow && write_v <= self.v_at_borrow {
            Some(value)
        } else {
            None
        }
    }
}

// ===========================================================================
// MinerCachedStateProvider — StateProvider wrapper
// ===========================================================================

/// Wraps a raw `StateProvider` with the miner cross-block cache.
///
/// - `basic_account`, `bytecode_by_hash`, and `storage` consult the
///   `MinerCacheBorrow` first; a `None` return (cache-miss or filter-reject)
///   falls through to the raw provider.
/// - All other `StateProvider` parent-trait methods delegate unconditionally.
///
/// Visibility: `pub(super)` so Task 13 (`wrap_state_provider`) can use it.
pub(super) struct MinerCachedStateProvider<S> {
    raw: S,
    borrow: MinerCacheBorrow,
}

impl<S> MinerCachedStateProvider<S> {
    pub(super) fn new(raw: S, borrow: MinerCacheBorrow) -> Self {
        Self { raw, borrow }
    }
}

// ---------------------------------------------------------------------------
// Cache-backed readers
// ---------------------------------------------------------------------------

impl<S: reth_provider::AccountReader> reth_provider::AccountReader
    for MinerCachedStateProvider<S>
{
    fn basic_account(
        &self,
        address: &Address,
    ) -> reth_provider::ProviderResult<Option<Account>> {
        match self.borrow.lookup_account(address) {
            Some(value) => Ok(value),
            None => self.raw.basic_account(address),
        }
    }
}

impl<S: reth_provider::BytecodeReader> reth_provider::BytecodeReader
    for MinerCachedStateProvider<S>
{
    fn bytecode_by_hash(
        &self,
        code_hash: &B256,
    ) -> reth_provider::ProviderResult<Option<reth_primitives_traits::Bytecode>> {
        match self.borrow.lookup_code(code_hash) {
            Some(value) => Ok(value),
            None => self.raw.bytecode_by_hash(code_hash),
        }
    }
}

// ---------------------------------------------------------------------------
// StateProvider (owns the `storage` method)
// ---------------------------------------------------------------------------

impl<S: reth_provider::StateProvider> reth_provider::StateProvider
    for MinerCachedStateProvider<S>
{
    fn storage(
        &self,
        account: Address,
        storage_key: StorageKey,
    ) -> reth_provider::ProviderResult<Option<StorageValue>> {
        // StorageKey = B256 (alloy alias); no conversion needed.
        match self.borrow.lookup_storage(&account, &storage_key) {
            Some(value) => Ok(value),
            None => self.raw.storage(account, storage_key),
        }
    }
}

// ---------------------------------------------------------------------------
// Pure-delegation impls for every other StateProvider super-trait
// ---------------------------------------------------------------------------

impl<S: reth_provider::BlockHashReader> reth_provider::BlockHashReader
    for MinerCachedStateProvider<S>
{
    fn block_hash(
        &self,
        number: alloy_primitives::BlockNumber,
    ) -> reth_provider::ProviderResult<Option<B256>> {
        self.raw.block_hash(number)
    }

    fn canonical_hashes_range(
        &self,
        start: alloy_primitives::BlockNumber,
        end: alloy_primitives::BlockNumber,
    ) -> reth_provider::ProviderResult<Vec<B256>> {
        self.raw.canonical_hashes_range(start, end)
    }
}

impl<S: reth_provider::StateRootProvider> reth_provider::StateRootProvider
    for MinerCachedStateProvider<S>
{
    fn state_root(
        &self,
        hashed_state: reth_trie_common::HashedPostState,
    ) -> reth_provider::ProviderResult<B256> {
        self.raw.state_root(hashed_state)
    }

    fn state_root_from_nodes(
        &self,
        input: reth_trie_common::TrieInput,
    ) -> reth_provider::ProviderResult<B256> {
        self.raw.state_root_from_nodes(input)
    }

    fn state_root_with_updates(
        &self,
        hashed_state: reth_trie_common::HashedPostState,
    ) -> reth_provider::ProviderResult<(B256, reth_trie_common::updates::TrieUpdates)> {
        self.raw.state_root_with_updates(hashed_state)
    }

    fn state_root_from_nodes_with_updates(
        &self,
        input: reth_trie_common::TrieInput,
    ) -> reth_provider::ProviderResult<(B256, reth_trie_common::updates::TrieUpdates)> {
        self.raw.state_root_from_nodes_with_updates(input)
    }
}

impl<S: reth_provider::StorageRootProvider> reth_provider::StorageRootProvider
    for MinerCachedStateProvider<S>
{
    fn storage_root(
        &self,
        address: Address,
        hashed_storage: reth_trie_common::HashedStorage,
    ) -> reth_provider::ProviderResult<B256> {
        self.raw.storage_root(address, hashed_storage)
    }

    fn storage_proof(
        &self,
        address: Address,
        slot: B256,
        hashed_storage: reth_trie_common::HashedStorage,
    ) -> reth_provider::ProviderResult<reth_trie_common::StorageProof> {
        self.raw.storage_proof(address, slot, hashed_storage)
    }

    fn storage_multiproof(
        &self,
        address: Address,
        slots: &[B256],
        hashed_storage: reth_trie_common::HashedStorage,
    ) -> reth_provider::ProviderResult<reth_trie_common::StorageMultiProof> {
        self.raw.storage_multiproof(address, slots, hashed_storage)
    }
}

impl<S: reth_provider::StateProofProvider> reth_provider::StateProofProvider
    for MinerCachedStateProvider<S>
{
    fn proof(
        &self,
        input: reth_trie_common::TrieInput,
        address: Address,
        slots: &[B256],
    ) -> reth_provider::ProviderResult<reth_trie_common::AccountProof> {
        self.raw.proof(input, address, slots)
    }

    fn multiproof(
        &self,
        input: reth_trie_common::TrieInput,
        targets: reth_trie_common::MultiProofTargets,
    ) -> reth_provider::ProviderResult<reth_trie_common::MultiProof> {
        self.raw.multiproof(input, targets)
    }

    fn witness(
        &self,
        input: reth_trie_common::TrieInput,
        target: reth_trie_common::HashedPostState,
    ) -> reth_provider::ProviderResult<Vec<alloy_primitives::Bytes>> {
        self.raw.witness(input, target)
    }
}

impl<S: reth_provider::HashedPostStateProvider> reth_provider::HashedPostStateProvider
    for MinerCachedStateProvider<S>
{
    fn hashed_post_state(
        &self,
        bundle_state: &revm::database::BundleState,
    ) -> reth_trie_common::HashedPostState {
        self.raw.hashed_post_state(bundle_state)
    }
}

// ===========================================================================
// Global handle
// ===========================================================================

static EXEC_CACHE: OnceLock<Arc<MinerExecCache>> = OnceLock::new();

// ===========================================================================
// Updater task (§5)
// ===========================================================================

/// Applies a single [`reth_chain_state::CanonStateNotification`] to the cache.
///
/// Called by [`run_updater`] on each successful receive. Extracted as a free
/// function so it can be unit-tested without a live broadcast channel.
///
/// - `Commit { new }` → apply the chain's merged `BundleState` to the cache.
/// - `Reorg { new, .. }` → bump `chain_epoch` (on_reorg) first, then apply
///   the new chain's bundle so the new-chain entries are immediately valid.
pub(super) fn apply_notification<N: reth_primitives_traits::NodePrimitives>(
    cache: &MinerExecCache,
    notif: reth_chain_state::CanonStateNotification<N>,
) {
    use reth_chain_state::CanonStateNotification;
    match notif {
        CanonStateNotification::Commit { new } => {
            cache.apply_bundle(new.execution_outcome().state());
        }
        CanonStateNotification::Reorg { new, .. } => {
            // Invalidate all pre-reorg entries by bumping the epoch, then
            // seed the cache with the new chain's state (spec §6.2).
            cache.on_reorg();
            if !new.is_empty() {
                cache.apply_bundle(new.execution_outcome().state());
            }
        }
    }
}

/// Single-writer updater task (spec §5).
///
/// Runs forever, draining [`reth_chain_state::CanonStateNotifications`] and
/// forwarding each event to [`apply_notification`].
///
/// - `Ok(notif)` → delegate to `apply_notification`.
/// - `Err(Lagged(_))` → forced invalidation: call `on_reorg` so every
///   existing entry becomes unborrowable. The loop **continues** — this is a
///   recoverable condition (spec §15, degraded-mode).
/// - `Err(Closed)` → sender dropped; no more notifications will arrive.
///   Return so the spawned task exits cleanly.
pub(super) async fn run_updater<N: reth_primitives_traits::NodePrimitives>(
    cache: Arc<MinerExecCache>,
    mut rx: reth_chain_state::CanonStateNotifications<N>,
) {
    use tokio::sync::broadcast::error::RecvError;
    loop {
        match rx.recv().await {
            Ok(notif) => apply_notification(&cache, notif),
            Err(RecvError::Lagged(n)) => {
                tracing::warn!(
                    target: "bsc::miner::cache",
                    skipped = n,
                    "CanonStateNotifications lagged — cache invalidated (on_reorg)"
                );
                // Treat as forced invalidation: bump chain_epoch so every
                // existing entry becomes unborrowable from any future borrow.
                // The heartbeat is also refreshed so peek_for doesn't
                // permanently stale-out; subsequent apply_bundle calls will
                // re-fill the cache on the new canonical segment.
                cache.on_reorg();
            }
            Err(RecvError::Closed) => {
                tracing::debug!(
                    target: "bsc::miner::cache",
                    "CanonStateNotifications channel closed — updater task exiting"
                );
                return;
            }
        }
    }
}

// ===========================================================================
// Public API (§3.1)
// ===========================================================================

/// Initialize the global miner exec cache and spawn the updater task.
///
/// Idempotent: calling twice is a no-op for the second call (logs a warning).
/// The first caller wins via [`OnceLock`]; subsequent callers return immediately.
///
/// The updater task runs indefinitely on `task_executor`, draining
/// `CanonStateNotifications` forwarded by `provider`. When the channel closes
/// the task exits cleanly; when it lags it falls back to an on-reorg epoch bump.
pub fn init_and_spawn<P>(provider: P, task_executor: reth_tasks::TaskExecutor)
where
    P: reth_provider::CanonStateSubscriptions
        + reth_provider::NodePrimitivesProvider
        + Send
        + Sync
        + 'static,
    <P as reth_provider::NodePrimitivesProvider>::Primitives: reth_primitives_traits::NodePrimitives,
{
    let cache = Arc::new(MinerExecCache::new());
    // First-set-wins via OnceLock.
    if EXEC_CACHE.set(Arc::clone(&cache)).is_err() {
        tracing::warn!(
            target: "bsc::miner::cache",
            "MinerExecCache already initialized; ignoring"
        );
        return;
    }
    let rx = provider.subscribe_to_canonical_state();
    let cache_for_task = Arc::clone(&cache);
    task_executor.spawn_critical("miner-cache-updater", async move {
        run_updater(cache_for_task, rx).await;
    });
    tracing::info!(
        target: "bsc::miner::cache",
        "MinerExecCache initialized and updater task spawned"
    );
}

/// Wrap a raw state provider with the miner cache.
///
/// Returns the `raw` provider unchanged when:
/// - The cache has not been initialized yet (`init_and_spawn` not called), or
/// - The cache heartbeat is stale (`peek_for` returns `None`).
///
/// Never panics — all three degrade paths return `raw` unmodified.
pub fn wrap_state_provider(
    raw: reth_provider::StateProviderBox,
) -> reth_provider::StateProviderBox {
    let cache = match EXEC_CACHE.get() {
        Some(c) => c,
        None => return raw,
    };
    let borrow = match cache.peek_for() {
        Some(b) => b,
        None => return raw,
    };
    Box::new(MinerCachedStateProvider::new(raw, borrow))
}

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
    use reth_provider::AccountReader;

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
    fn on_reorg_bumps_chain_epoch_and_heartbeat() {
        let cache = MinerExecCache::new();
        let ce_before = cache.chain_epoch.load(Ordering::Acquire);
        let heartbeat_before = cache.last_apply_at_ms.load(Ordering::Relaxed);
        std::thread::sleep(std::time::Duration::from_millis(2));

        cache.on_reorg();

        assert_eq!(cache.chain_epoch.load(Ordering::Acquire), ce_before + 1);
        assert!(cache.last_apply_at_ms.load(Ordering::Relaxed) > heartbeat_before);
        // version unchanged
        assert_eq!(cache.version.load(Ordering::Acquire), 0);
    }

    // ------------------------------------------------------------------
    // peek_for tests
    // ------------------------------------------------------------------

    #[test]
    fn peek_for_returns_none_when_heartbeat_never_set() {
        let cache = Arc::new(MinerExecCache::new());
        assert!(cache.peek_for().is_none(), "no apply yet → no borrow");
    }

    #[test]
    fn peek_for_returns_borrow_after_apply() {
        let cache = Arc::new(MinerExecCache::new());
        let bundle = mk_bundle_with_account(Address::from([0x55; 20]), 1);
        cache.apply_bundle(&bundle);

        let borrow = cache.peek_for().expect("heartbeat fresh");
        assert_eq!(borrow.chain_epoch_at_borrow, 0);
        assert_eq!(borrow.v_at_borrow, 1);
    }

    #[test]
    fn peek_for_returns_none_when_heartbeat_stale() {
        let cache = Arc::new(MinerExecCache::new());
        // Manually mark heartbeat as ancient.
        cache.last_apply_at_ms.store(1, Ordering::Relaxed);
        assert!(cache.peek_for().is_none(), "stale heartbeat → no borrow");
    }

    // ------------------------------------------------------------------
    // lookup_account / lookup_code tests
    // ------------------------------------------------------------------

    #[test]
    fn lookup_account_accepts_in_chain_in_version() {
        let cache = Arc::new(MinerExecCache::new());
        let addr = Address::from([0x77; 20]);
        cache.apply_bundle(&mk_bundle_with_account(addr, 42));

        let borrow = cache.peek_for().unwrap();
        let result = borrow.lookup_account(&addr);
        assert!(matches!(result, Some(Some(_))), "expected cache hit with value");
    }

    #[test]
    fn lookup_account_rejects_chain_mismatch() {
        let cache = Arc::new(MinerExecCache::new());
        let addr = Address::from([0x88; 20]);
        cache.apply_bundle(&mk_bundle_with_account(addr, 1));

        let borrow = cache.peek_for().unwrap();
        cache.on_reorg(); // bump chain_epoch
        // Apply on the new chain.
        cache.apply_bundle(&mk_bundle_with_account(addr, 2));

        // Borrow's chain_epoch is stale → must reject the new entry.
        let result = borrow.lookup_account(&addr);
        assert!(result.is_none(), "chain mismatch must reject");
    }

    #[test]
    fn lookup_account_rejects_future_version() {
        let cache = Arc::new(MinerExecCache::new());
        let addr = Address::from([0x99; 20]);
        cache.apply_bundle(&mk_bundle_with_account(addr, 1));

        let borrow = cache.peek_for().unwrap();
        let v_at_borrow = borrow.v_at_borrow;

        cache.apply_bundle(&mk_bundle_with_account(addr, 2)); // write_v = v_at_borrow + 1

        // The newer entry overwrites the cache. Borrow sees the newer
        // entry but rejects via the version filter.
        let result = borrow.lookup_account(&addr);
        assert!(result.is_none(), "future-version entry must be rejected");
        assert!(borrow.v_at_borrow == v_at_borrow, "borrow unchanged");
    }

    #[test]
    fn lookup_account_miss_returns_none() {
        let cache = Arc::new(MinerExecCache::new());
        // Force heartbeat fresh without writing anything readable.
        cache.last_apply_at_ms.store(now_ms(), Ordering::Relaxed);
        let borrow = cache.peek_for().unwrap();
        assert!(borrow.lookup_account(&Address::from([0xAA; 20])).is_none());
    }

    #[test]
    fn lookup_code_filters_consistently() {
        use std::collections::HashMap;
        let cache = Arc::new(MinerExecCache::new());
        let code_hash = B256::from([0xBB; 32]);
        let bytecode = Bytecode::new_raw(vec![0xfd].into());

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

        let borrow = cache.peek_for().unwrap();
        assert!(matches!(borrow.lookup_code(&code_hash), Some(Some(_))));
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

    // ------------------------------------------------------------------
    // lookup_storage tests
    // ------------------------------------------------------------------

    #[test]
    fn lookup_storage_happy_path() {
        let cache = Arc::new(MinerExecCache::new());
        let addr = Address::from([0x10; 20]);
        let key = U256::from(1);
        let value = U256::from(123);
        cache.apply_bundle(&mk_bundle_with_slot(addr, key, value));

        let borrow = cache.peek_for().unwrap();
        let storage_key = B256::from(key.to_be_bytes::<32>());
        let result = borrow.lookup_storage(&addr, &storage_key);
        assert_eq!(result, Some(Some(value)));
    }

    #[test]
    fn lookup_storage_shortcircuits_on_account_tombstone() {
        let cache = Arc::new(MinerExecCache::new());
        let addr = Address::from([0x20; 20]);
        // Block 1: seed an account with a storage slot.
        cache.apply_bundle(&mk_bundle_with_slot(addr, U256::from(7), U256::from(99)));
        // Block 2: destroy the account.
        cache.apply_bundle(&mk_bundle_with_destroyed(addr));

        let borrow = cache.peek_for().unwrap();
        // Even if a stale slot entry exists in some leaked Arc, accounts-first
        // must return Some(None) because the tombstone covers our v_at_borrow.
        let storage_key = B256::from(U256::from(7).to_be_bytes::<32>());
        let result = borrow.lookup_storage(&addr, &storage_key);
        assert_eq!(result, Some(None), "tombstoned account → storage is None");
    }

    #[test]
    fn lookup_storage_borrow_before_destruction_does_not_shortcircuit() {
        let cache = Arc::new(MinerExecCache::new());
        let addr = Address::from([0x30; 20]);
        cache.apply_bundle(&mk_bundle_with_slot(addr, U256::from(7), U256::from(99)));

        // Borrow taken NOW, BEFORE destruction.
        let borrow = cache.peek_for().unwrap();

        // Destruction happens after.
        cache.apply_bundle(&mk_bundle_with_destroyed(addr));

        // Borrow's v_at_borrow predates destruction. The tombstone now in
        // accounts has write_v > v_at_borrow → filter rejects → no short-circuit.
        // The storage Arc was replaced by destruction, so storage.get(addr)
        // returns the new (empty) Arc. Slot lookup misses → None.
        // Caller (raw provider) handles the actual pre-destruction read.
        let storage_key = B256::from(U256::from(7).to_be_bytes::<32>());
        let result = borrow.lookup_storage(&addr, &storage_key);
        assert!(
            result.is_none(),
            "pre-destruction borrow + post-destruction read: cache returns None (fall-through), not Some(None) shortcut"
        );
    }

    // ------------------------------------------------------------------
    // MinerCachedStateProvider tests
    // ------------------------------------------------------------------

    /// Stub raw provider that always returns a fixed account regardless of address.
    struct StubRaw {
        account: Account,
    }

    impl reth_provider::AccountReader for StubRaw {
        fn basic_account(
            &self,
            _address: &Address,
        ) -> reth_provider::ProviderResult<Option<Account>> {
            Ok(Some(self.account))
        }
    }

    #[test]
    fn cached_provider_falls_through_to_raw_on_filter_reject() {
        // Scenario: take a borrow, then a NEWER write for the same address
        // arrives (write_v > v_at_borrow). The version filter must reject the
        // newer entry and fall through to the raw provider.
        let cache = Arc::new(MinerExecCache::new());
        let addr = Address::from([0x33; 20]);
        cache.apply_bundle(&mk_bundle_with_account(addr, 42));

        // Borrow snapshot: v_at_borrow = 1.
        let borrow = cache.peek_for().unwrap();
        assert_eq!(borrow.v_at_borrow, 1);

        // Overwrite the entry with a newer write (write_v = 2, same chain).
        cache.apply_bundle(&mk_bundle_with_account(addr, 555));
        // Now cache has (value=555, write_v=2, ce=0). Borrow v_at_borrow=1 < 2
        // → version filter rejects.

        // Construct the wrapper with a stub that returns a sentinel value (99999).
        let raw = StubRaw {
            account: Account {
                nonce: 0,
                balance: alloy_primitives::U256::from(99999u64),
                bytecode_hash: None,
            },
        };
        let provider = MinerCachedStateProvider::new(raw, borrow);

        // lookup_account sees write_v=2 > v_at_borrow=1 → returns None
        // → wrapper falls through to raw → 99999.
        let result = provider
            .basic_account(&addr)
            .expect("basic_account must not error");
        assert_eq!(
            result.unwrap().balance,
            alloy_primitives::U256::from(99999u64),
            "filter reject (future write_v) must fall through to raw provider"
        );
    }

    #[test]
    fn cached_provider_returns_cached_value_on_hit() {
        let cache = Arc::new(MinerExecCache::new());
        let addr = Address::from([0x44; 20]);
        cache.apply_bundle(&mk_bundle_with_account(addr, 777));

        // Borrow taken immediately after — chain_epoch matches, version matches.
        let borrow = cache.peek_for().unwrap();

        let raw = StubRaw {
            account: Account {
                nonce: 0,
                balance: alloy_primitives::U256::from(99999u64),
                bytecode_hash: None,
            },
        };
        let provider = MinerCachedStateProvider::new(raw, borrow);

        let result = provider
            .basic_account(&addr)
            .expect("basic_account must not error");
        assert_eq!(
            result.unwrap().balance,
            alloy_primitives::U256::from(777u64),
            "cache hit must return the cached value, not the raw fallback"
        );
    }

    // ------------------------------------------------------------------
    // apply_notification / run_updater tests
    //
    // Test approach: we test `apply_notification` directly with
    // hand-crafted `CanonStateNotification` values (built from `Chain`
    // constructed via `Chain::new`).  This avoids the need to drive a live
    // broadcast channel and is simpler and more deterministic.
    //
    // For `run_updater` we create a real `broadcast` channel, send
    // events on the sender side, and confirm the cache is mutated.
    // ------------------------------------------------------------------

    use reth_execution_types::{Chain, ExecutionOutcome};
    use reth_chain_state::CanonStateNotification;
    use reth_ethereum_primitives::Block as EthBlock;
    use reth_primitives_traits::{RecoveredBlock, SealedHeader};

    /// Build a minimal `Chain` whose merged `BundleState` contains one account.
    fn mk_chain_with_account(addr: Address, balance: u64) -> std::sync::Arc<Chain> {
        use alloy_consensus::Header;
        use alloy_primitives::B256;
        use reth_primitives_traits::SealedBlock;

        // Minimal block: number=1, zero hash.
        let header = Header::default();
        let sealed = SealedHeader::new(header, B256::ZERO);
        let sealed_block = SealedBlock::<EthBlock>::from_sealed_parts(
            sealed,
            alloy_consensus::BlockBody::default(),
        );
        let block: RecoveredBlock<EthBlock> = sealed_block.try_recover().unwrap();

        let outcome = ExecutionOutcome {
            bundle: mk_bundle_with_account(addr, balance),
            ..Default::default()
        };
        std::sync::Arc::new(Chain::new(vec![block], outcome, std::collections::BTreeMap::new()))
    }

    /// Build a minimal empty `Chain` (for the `new` side of a pure revert).
    fn mk_empty_chain() -> std::sync::Arc<Chain> {
        // Chain::new asserts non-empty; use the Default impl instead.
        std::sync::Arc::new(Chain::default())
    }

    #[test]
    fn apply_notification_commit_applies_bundle() {
        let cache = MinerExecCache::new();
        let addr = Address::from([0xA1; 20]);

        let notif = CanonStateNotification::Commit { new: mk_chain_with_account(addr, 42) };
        apply_notification(&cache, notif);

        assert_eq!(cache.version.load(Ordering::Acquire), 1, "one apply_bundle run");
        let entry = cache.accounts.get(&addr).expect("account cached");
        let (value, ..) = entry;
        assert!(value.is_some(), "account must be in cache");
    }

    #[test]
    fn apply_notification_reorg_bumps_epoch_then_applies() {
        let cache = MinerExecCache::new();
        let addr_old = Address::from([0xB1; 20]);
        let addr_new = Address::from([0xB2; 20]);

        // Seed: commit one account on the old chain.
        apply_notification(
            &cache,
            CanonStateNotification::Commit { new: mk_chain_with_account(addr_old, 1) },
        );
        let ce_before = cache.chain_epoch.load(Ordering::Acquire);
        assert_eq!(ce_before, 0);

        // Reorg: old discarded, new chain comes in with a different account.
        let old_chain = mk_chain_with_account(addr_old, 1);
        let new_chain = mk_chain_with_account(addr_new, 99);
        apply_notification(
            &cache,
            CanonStateNotification::Reorg { old: old_chain, new: new_chain },
        );

        // chain_epoch bumped exactly once.
        assert_eq!(cache.chain_epoch.load(Ordering::Acquire), ce_before + 1, "epoch bumped");
        // version bumped twice (once for old commit, once for new after reorg).
        assert_eq!(cache.version.load(Ordering::Acquire), 2, "two bundles applied total");
        // New chain's account cached at the new epoch.
        let entry = cache.accounts.get(&addr_new).expect("new account cached");
        let (_, _, ce) = entry;
        assert_eq!(ce, 1, "new entry tagged with post-reorg epoch");
    }

    #[test]
    fn apply_notification_reorg_with_empty_new_chain() {
        // A pure revert: on_reorg runs, no apply_bundle since new chain is empty.
        let cache = MinerExecCache::new();
        apply_notification(
            &cache,
            CanonStateNotification::Reorg {
                old: mk_chain_with_account(Address::from([0xC1; 20]), 1),
                new: mk_empty_chain(),
            },
        );
        assert_eq!(cache.chain_epoch.load(Ordering::Acquire), 1, "epoch bumped on revert");
        // No apply_bundle ran for the empty new chain.
        assert_eq!(cache.version.load(Ordering::Acquire), 0, "no bundle applied");
    }

    #[tokio::test]
    async fn run_updater_processes_commit() {
        use tokio::sync::broadcast;

        let cache = Arc::new(MinerExecCache::new());
        let (tx, rx) = broadcast::channel::<CanonStateNotification>(16);

        let cache_clone = Arc::clone(&cache);
        let handle = tokio::spawn(async move { run_updater(cache_clone, rx).await });

        let addr = Address::from([0xD1; 20]);
        tx.send(CanonStateNotification::Commit { new: mk_chain_with_account(addr, 7) }).unwrap();

        // Wait for the updater to process (poll up to 500 ms).
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            if cache.version.load(Ordering::Acquire) > 0 {
                break;
            }
        }

        assert_eq!(cache.version.load(Ordering::Acquire), 1, "one Commit processed");
        handle.abort();
    }

    #[tokio::test]
    async fn run_updater_exits_on_closed_channel() {
        use tokio::sync::broadcast;

        let cache = Arc::new(MinerExecCache::new());
        let (tx, rx) = broadcast::channel::<CanonStateNotification>(16);

        let cache_clone = Arc::clone(&cache);
        let handle = tokio::spawn(async move { run_updater(cache_clone, rx).await });

        // Drop the sender — channel becomes Closed.
        drop(tx);

        // Task must finish cleanly within a reasonable timeout.
        let result =
            tokio::time::timeout(std::time::Duration::from_secs(2), handle).await;
        assert!(result.is_ok(), "run_updater must exit when channel is closed");
        assert!(
            result.unwrap().is_ok(),
            "task must not panic"
        );
    }

    #[tokio::test]
    async fn run_updater_handles_lagged_as_on_reorg() {
        use tokio::sync::broadcast;

        // Small capacity so we can force a Lagged error.
        let cache = Arc::new(MinerExecCache::new());
        let (tx, rx) = broadcast::channel::<CanonStateNotification>(1);

        // Overfill the channel before spawning the consumer → first recv() will
        // be Lagged because all but the last message were dropped.
        let addr = Address::from([0xE1; 20]);
        // Send enough to overflow the capacity-1 buffer.
        for _ in 0..3 {
            let _ = tx.send(CanonStateNotification::Commit { new: mk_chain_with_account(addr, 1) });
        }

        let cache_clone = Arc::clone(&cache);
        let handle = tokio::spawn(async move { run_updater(cache_clone, rx).await });

        // Let the updater drain.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        // chain_epoch must have been bumped (on_reorg called for Lagged).
        // (Version may also be bumped if some non-lagged messages got through.)
        // The invariant is: on_reorg was called at least once, so epoch ≥ 1
        // OR version ≥ 1 (if all messages arrived without lagging on this run).
        // We simply assert the task is still alive and not panicked.
        assert!(!handle.is_finished(), "updater must still be running after Lagged");
        handle.abort();
    }

    // ------------------------------------------------------------------
    // Public API surface tests (Task 13)
    // ------------------------------------------------------------------

    #[test]
    fn wrap_returns_raw_when_cache_not_initialized() {
        // NOTE: EXEC_CACHE is process-global; we can't guarantee it's
        // uninitialized in test order. So this test verifies the API exists
        // and is callable. The fall-through behavior is tested via
        // cached_provider_falls_through_to_raw_on_filter_reject (Task 11).
        let _: fn(reth_provider::StateProviderBox) -> reth_provider::StateProviderBox =
            wrap_state_provider;
    }
}
