use super::trie_overlay::{init_trie_overlay_cache, trie_overlay_cache, TrieOverlayEntry};
use alloy_primitives::{keccak256, map::B256Set, B256};
use rayon::iter::{ParallelBridge, ParallelIterator};
use reth_chain_state::{ExecutedTrieUpdates, NewCanonicalChain};
use reth_evm::OnStateHook;
use reth_evm::execute::BlockExecutionError;
use reth_provider::{
    providers::ConsistentDbView, BlockNumReader, BlockReader, DatabaseProviderFactory, HeaderProvider,
    NewCanonicalChainSubscriptions, StateProvider, DBProvider, NodePrimitivesProvider,
};
use reth_trie::{
    hashed_cursor::{HashedCursor, HashedCursorFactory, HashedPostStateCursorFactory},
    prefix_set::TriePrefixSetsMut,
    updates::TrieUpdates,
    DecodedMultiProof, HashedPostState, MultiProofTargets, Nibbles, TrieInput,
};
use reth_trie_db::DatabaseHashedCursorFactory;
use reth_trie_parallel::{
    proof::ParallelProof,
    proof_task::{ProofTaskCtx, ProofTaskManager},
    root::ParallelStateRoot,
};
use reth_trie_sparse::{
    provider::TrieNodeProviderFactory, SerialSparseTrie, SparseStateTrie, SparseTrie,
    SparseTrieInterface,
};
use reth_trie_sparse_parallel::ParallelSparseTrie;
use parking_lot::Mutex;
use revm::state::EvmState;
use std::{collections::HashMap, sync::{Arc, OnceLock}, time::Duration};
use tokio::sync::broadcast;

/// Same idea as engine-tree payload processor: compute proofs in small account chunks to keep
/// per-proof overhead bounded and allow better scheduling.
const MULTIPROOF_TARGETS_CHUNK_SIZE: usize = 10;

/// Which algorithm produced the state root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootSpeederMode {
    Sparse,
    Parallel,
    Serial,
}

/// Trie root accelerator facade.
///
/// The goal is to keep all complexity localized here so callers remain simple and can always
/// downgrade to serial computation.
#[derive(Debug, Default, Clone, Copy)]
pub struct RootSpeeder;

/// Key for caching prefetched multiproofs while building a payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct PrefetchKey {
    parent_number: u64,
    parent_hash: B256,
}

#[derive(Debug, Clone)]
struct PrefetchedMultiproof {
    targets: MultiProofTargets,
    multiproof: DecodedMultiProof,
}

static PREFETCHED_MULTIPROOFS: OnceLock<Mutex<HashMap<PrefetchKey, PrefetchedMultiproof>>> = OnceLock::new();

fn prefetched_multiproofs() -> &'static Mutex<HashMap<PrefetchKey, PrefetchedMultiproof>> {
    PREFETCHED_MULTIPROOFS.get_or_init(|| Mutex::new(HashMap::new()))
}

static ROOT_SPEEDER_PROOF_RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

fn root_speeder_proof_runtime() -> &'static tokio::runtime::Runtime {
    ROOT_SPEEDER_PROOF_RT.get_or_init(|| {
        // Dedicated runtime so RootSpeeder can use ProofTaskManager even when called from a plain
        // std::thread (e.g. background compare).
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            // Keep this small to avoid starving the foreground sealing path.
            .worker_threads(2)
            .thread_name("root-speeder-proof")
            .build()
            .expect("root-speeder proof runtime build")
    })
}

fn chunk_multiproof_targets(
    targets: &MultiProofTargets,
    chunk_size: usize,
) -> Vec<MultiProofTargets> {
    let mut chunks = Vec::new();
    let mut current = MultiProofTargets::default();
    let mut count = 0usize;

    for (addr, slots) in targets.iter() {
        current.insert(*addr, slots.clone());
        count += 1;
        if count >= chunk_size {
            chunks.push(current);
            current = MultiProofTargets::default();
            count = 0;
        }
    }

    if !current.is_empty() {
        chunks.push(current);
    }

    chunks
}

fn make_hashed_state_subset_for_targets(
    hashed_state: &HashedPostState,
    targets: &MultiProofTargets,
) -> HashedPostState {
    let mut subset = HashedPostState::default();

    for (hashed_address, _) in targets.iter() {
        if let Some(account) = hashed_state.accounts.get(hashed_address) {
            subset.accounts.insert(*hashed_address, account.clone());
        }
        if let Some(storage) = hashed_state.storages.get(hashed_address) {
            subset.storages.insert(*hashed_address, storage.clone());
        }
    }

    subset
}

fn should_cross_check(parent_hash: B256) -> bool {
    // ~1/128 sampling based on hash prefix.
    parent_hash.as_slice()[0] & 0x7f == 0
}

/// Apply a hashed state update to a revealed sparse state trie.
///
/// Mirrors engine-tree's update ordering:
/// - Update storage tries first (in parallel).
/// - Update account leaves, ensuring storage roots are reflected even when only storage changed.
fn apply_hashed_state_update_to_sparse_trie<BPF>(
    trie: &mut SparseStateTrie<ParallelSparseTrie, SerialSparseTrie>,
    mut state: HashedPostState,
    blinded_provider_factory: &BPF,
) -> Result<(), BlockExecutionError>
where
    BPF: TrieNodeProviderFactory + Send + Sync + Clone,
    BPF::AccountNodeProvider: reth_trie_sparse::provider::TrieNodeProvider + Send + Sync,
    BPF::StorageNodeProvider: reth_trie_sparse::provider::TrieNodeProvider + Send + Sync,
{
    // Update storage slots + compute per-account storage roots in parallel.
    let (tx, rx) = std::sync::mpsc::channel();
    state
        .storages
        .drain()
        .map(|(address, storage)| (address, storage, trie.take_storage_trie(&address)))
        .par_bridge()
        .map(|(address, storage, storage_trie)| {
            let storage_provider = blinded_provider_factory.storage_node_provider(address);
            let mut storage_trie = storage_trie.ok_or_else(|| {
                BlockExecutionError::other(std::io::Error::other(format!(
                    "sparse storage trie not revealed for account {address:?}"
                )))
            })?;

            if storage.wiped {
                storage_trie
                    .wipe()
                    .map_err(|e| BlockExecutionError::other(std::io::Error::other(format!("{e:?}"))))?;
            }

            // Defer removals until after updates/additions.
            let mut removed_slots: Vec<Nibbles> = Vec::new();
            for (slot, value) in storage.storage {
                let slot_nibbles = Nibbles::unpack(slot);
                if value.is_zero() {
                    removed_slots.push(slot_nibbles);
                    continue;
                }

                storage_trie
                    .update_leaf(
                        slot_nibbles,
                        alloy_rlp::encode_fixed_size(&value).to_vec(),
                        &storage_provider,
                    )
                    .map_err(|e| BlockExecutionError::other(std::io::Error::other(format!("{e:?}"))))?;
            }
            for slot_nibbles in removed_slots {
                storage_trie
                    .remove_leaf(&slot_nibbles, &storage_provider)
                    .map_err(|e| BlockExecutionError::other(std::io::Error::other(format!("{e:?}"))))?;
            }

            storage_trie.root();

            Ok::<_, BlockExecutionError>((address, storage_trie))
        })
        .for_each_init(|| tx.clone(), |tx, result| {
            let _ = tx.send(result);
        });
    drop(tx);

    // Defer account removals until after updates.
    let mut removed_accounts: Vec<B256> = Vec::new();

    // First apply any storage-root changes to accounts.
    for result in rx {
        let (address, storage_trie) = result?;
        trie.insert_storage_trie(address, storage_trie);

        if let Some(account) = state.accounts.remove(&address) {
            match account {
                Some(account) => {
                    let keep = trie
                        .update_account(address, account, blinded_provider_factory)
                        .map_err(|e| {
                            BlockExecutionError::other(std::io::Error::other(format!("{e:?}")))
                        })?;
                    if !keep {
                        removed_accounts.push(address);
                    }
                }
                None => {
                    // Ensure storage trie deletion is reflected in trie updates.
                    if trie.storage_trie_ref(&address).is_none() {
                        let mut wiped = SparseTrie::Revealed(Box::new(
                            SerialSparseTrie::default().with_updates(true),
                        ));
                        wiped
                            .wipe()
                            .map_err(|e| {
                                BlockExecutionError::other(std::io::Error::other(format!("{e:?}")))
                            })?;
                        trie.insert_storage_trie(address, wiped);
                    } else {
                        let t = trie.storage_trie_mut(&address).expect("checked above");
                        t.wipe();
                    }

                    removed_accounts.push(address);
                }
            }
        } else if trie.is_account_revealed(address) {
            // Otherwise, if the account is revealed, update only its storage root.
            let keep = trie
                .update_account_storage_root(address, blinded_provider_factory)
                .map_err(|e| BlockExecutionError::other(std::io::Error::other(format!("{e:?}"))))?;
            if !keep {
                removed_accounts.push(address);
            }
        }
    }

    // Apply remaining account changes.
    for (address, account) in state.accounts.drain() {
        match account {
            Some(account) => {
                let keep = trie
                    .update_account(address, account, blinded_provider_factory)
                    .map_err(|e| BlockExecutionError::other(std::io::Error::other(format!("{e:?}"))))?;
                if !keep {
                    removed_accounts.push(address);
                }
            }
            None => {
                removed_accounts.push(address);
            }
        }
    }

    // Remove accounts.
    for address in removed_accounts {
        let nibbles = Nibbles::unpack(address);
        trie.remove_account_leaf(&nibbles, blinded_provider_factory)
            .map_err(|e| BlockExecutionError::other(std::io::Error::other(format!("{e:?}"))))?;
    }

    Ok(())
}

/// Collects state changes during payload execution and opportunistically prefetches multiproofs.
///
/// This is intentionally best-effort: if the prefetched targets don't exactly match the final
/// block's proof targets, `RootSpeeder` will ignore it and compute the multiproof synchronously.
#[derive(Debug, Clone)]
pub struct RootSpeederPrefetch {
    key: PrefetchKey,
    /// Hashed account -> hashed slots.
    targets: Arc<Mutex<MultiProofTargets>>,
    /// Hashed accounts whose storage was likely wiped (selfdestruct/clear); used for target expansion.
    wiped_accounts: Arc<Mutex<B256Set>>,
}

/// Shared state used to correlate serial/accelerated state-root computations.
#[derive(Debug, Default, Clone, Copy)]
pub struct StateRootCompareState {
    pub block_hash: Option<B256>,
    pub user_tx_len: Option<usize>,
    pub system_tx_len: Option<usize>,
    pub total_tx_len: Option<usize>,
    pub serial_root: Option<B256>,
    pub serial_duration_ms: Option<u128>,
}

#[derive(Debug, Default)]
struct StateRootCompareStats {
    samples: u64,
    serial_faster: u64,
    accelerated_faster: u64,
    tie: u64,

    sum_total_tx_len: u128,
    sum_serial_ms: u128,
    sum_accel_ms: u128,
}

static STATE_ROOT_COMPARE_STATS: OnceLock<Arc<Mutex<StateRootCompareStats>>> = OnceLock::new();
static STATE_ROOT_COMPARE_PRINTER_STARTED: OnceLock<()> = OnceLock::new();

impl RootSpeederPrefetch {
    pub fn new(parent_number: u64, parent_hash: B256) -> Self {
        Self {
            key: PrefetchKey { parent_number, parent_hash },
            targets: Arc::new(Mutex::new(MultiProofTargets::default())),
            wiped_accounts: Arc::new(Mutex::new(B256Set::default())),
        }
    }

    /// Hook (1): invoked by the executor after each tx/system transition with the current `EvmState`.
    ///
    /// We conservatively treat any touched account + all cached storage keys as proof targets.
    pub fn state_hook(&self) -> Box<dyn OnStateHook> {
        let targets = self.targets.clone();
        let wiped = self.wiped_accounts.clone();
        Box::new(move |source, state: &EvmState| {
            // Only transaction sources matter for payload building targets.
            let reth_evm::block::StateChangeSource::Transaction(_) = source else { return };

            let mut targets = targets.lock();
            let mut wiped_accounts = wiped.lock();

            for (address, account) in state {
                // Account trie key = keccak(address)
                let hashed_address = keccak256(address.as_slice());

                // Any touched account is potentially relevant, even if storage is empty.
                if account.is_touched() || account.is_selfdestructed() || !account.storage.is_empty() {
                    targets.entry(hashed_address).or_default();
                }

                // Mark likely storage wipe to trigger expansion in prefetch task.
                if account.is_selfdestructed() {
                    wiped_accounts.insert(hashed_address);
                }

                // Storage trie keys = keccak(storage_key)
                if !account.storage.is_empty() {
                    let mut slots = targets.get(&hashed_address).cloned().unwrap_or_default();
                    for (slot_key, _) in &account.storage {
                        let hashed_slot = keccak256(slot_key.to_be_bytes::<32>());
                        slots.insert(hashed_slot);
                    }
                    targets.insert(hashed_address, slots);
                }
            }
        })
    }

    /// Hook (2): snapshot of the not-yet-finished state.
    ///
    /// For now this is the same signal as `state_hook` provides (post-tx), but kept as a separate
    /// API so we can evolve to more granular snapshots (pre-tx / mid-tx) without changing callers.
    pub fn state_snapshot_hook(&self) -> Box<dyn OnStateHook> {
        self.state_hook()
    }

    /// Best-effort: kick off a background multiproof prefetch for current targets.
    ///
    /// Safe to call multiple times; the latest completed proof overwrites older ones for the same key.
    pub fn kick_prefetch<Factory>(&self, provider_factory: Factory)
    where
        Factory: DatabaseProviderFactory<Provider: BlockNumReader + HeaderProvider + BlockReader>
            + Clone
            + Send
            + Sync
            + 'static,
    {
        let key = self.key;
        let targets_snapshot = { self.targets.lock().clone() };
        let wiped_snapshot = { self.wiped_accounts.lock().clone() };

        // Avoid spawning if we have no targets yet.
        if targets_snapshot.is_empty() {
            return;
        }

        tokio::task::spawn_blocking(move || {
            let res = (|| -> Result<PrefetchedMultiproof, BlockExecutionError> {
                let provider_ro = provider_factory
                    .database_provider_ro()
                    .map_err(BlockExecutionError::other)?;
                let db_last = provider_ro.best_block_number().map_err(BlockExecutionError::other)?;
                if db_last > key.parent_number {
                    return Err(BlockExecutionError::other(std::io::Error::other(format!(
                        "db tip ({db_last}) ahead of parent ({})",
                        key.parent_number
                    ))));
                }
                let db_tip = provider_ro
                    .sealed_header(db_last)
                    .map_err(BlockExecutionError::other)?
                    .ok_or_else(|| BlockExecutionError::other(std::io::Error::other("db tip missing")))?;

                let consistent_view =
                    ConsistentDbView::new(provider_factory.clone(), Some((db_tip.hash(), db_last)));

                // Build base input (DB + overlay up to parent).
                let mut base_input = TrieInput::default();
                if db_last < key.parent_number {
                    let cache = trie_overlay_cache().ok_or_else(|| {
                        BlockExecutionError::other(std::io::Error::other("trie overlay cache not initialized"))
                    })?;
                    let needed_range = (db_last + 1)..=key.parent_number;
                    let overlays = cache.read().get_range(needed_range.clone());
                    if overlays.len() != (key.parent_number - db_last) as usize {
                        return Err(BlockExecutionError::other(std::io::Error::other(format!(
                            "missing trie overlay blocks for range {:?} (have {})",
                            needed_range,
                            overlays.len()
                        ))));
                    }
                    for entry in overlays {
                        if entry.number == key.parent_number && entry.hash != key.parent_hash {
                            return Err(BlockExecutionError::other(std::io::Error::other(format!(
                                "parent hash mismatch in overlay cache: expected={:?} got={:?}",
                                key.parent_hash, entry.hash
                            ))));
                        }
                        let Some(nodes) = entry.trie_updates.as_deref() else {
                            return Err(BlockExecutionError::other(std::io::Error::other(format!(
                                "missing trie_updates for overlay block {}",
                                entry.number
                            ))));
                        };
                        base_input.append_cached_ref(nodes, &entry.hashed_state);
                    }
                }

                let nodes_sorted = Arc::new(base_input.nodes.clone().into_sorted());
                let state_sorted = Arc::new(base_input.state.clone().into_sorted());

                // Cursor factories against DB + overlay.
                let provider_ro = consistent_view.provider_ro().map_err(BlockExecutionError::other)?;
                let hashed_cursor_factory = HashedPostStateCursorFactory::new(
                    DatabaseHashedCursorFactory::new(provider_ro.tx_ref()),
                    &state_sorted,
                );

                // Expand wiped accounts: include all existing hashed slots from base view.
                let mut expanded_targets = targets_snapshot.clone();
                for hashed_address in wiped_snapshot {
                    let mut slots: B256Set =
                        expanded_targets.get(&hashed_address).cloned().unwrap_or_default();
                    let mut storage_cursor = hashed_cursor_factory
                        .hashed_storage_cursor(hashed_address)
                        .map_err(|e| BlockExecutionError::other(std::io::Error::other(e)))?;
                    let mut current_entry = storage_cursor
                        .seek(B256::ZERO)
                        .map_err(|e| BlockExecutionError::other(std::io::Error::other(e)))?;
                    while let Some((hashed_slot, _)) = current_entry {
                        slots.insert(hashed_slot);
                        current_entry = storage_cursor
                            .next()
                            .map_err(|e| BlockExecutionError::other(std::io::Error::other(e)))?;
                    }
                    expanded_targets.insert(hashed_address, slots);
                }

                // Use the same parallel proof pipeline as engine-tree so prefetch aligns with
                // the actual sparse computation path.
                let prefix_sets = Arc::new(TriePrefixSetsMut::default());
                let task_ctx = ProofTaskCtx::new(nodes_sorted.clone(), state_sorted.clone(), prefix_sets.clone());
                let rt = root_speeder_proof_runtime();
                let handle = rt.handle().clone();
                let proof_task = ProofTaskManager::new(handle.clone(), consistent_view.clone(), task_ctx, 64);
                let proof_handle = proof_task.handle();
                handle.spawn_blocking(move || {
                    let _ = proof_task.run();
                });

                let decoded = ParallelProof::new(
                    consistent_view.clone(),
                    nodes_sorted,
                    state_sorted,
                    prefix_sets,
                    proof_handle.clone(),
                )
                .with_branch_node_masks(true)
                .decoded_multiproof(expanded_targets.clone())
                .map_err(|e| BlockExecutionError::other(std::io::Error::other(format!("{e:?}"))))?;

                Ok(PrefetchedMultiproof { targets: expanded_targets, multiproof: decoded })
            })();

            if let Ok(prefetched) = res {
                prefetched_multiproofs().lock().insert(key, prefetched);
            }
        });
    }
}

impl RootSpeeder {
    fn compare_stats() -> Arc<Mutex<StateRootCompareStats>> {
        STATE_ROOT_COMPARE_STATS
            .get_or_init(|| Arc::new(Mutex::new(StateRootCompareStats::default())))
            .clone()
    }

    fn start_compare_printer_thread_once() {
        STATE_ROOT_COMPARE_PRINTER_STARTED.get_or_init(|| {
            let stats = Self::compare_stats();
            std::thread::spawn(move || loop {
                std::thread::sleep(Duration::from_secs(30));
                let snapshot = {
                    let s = stats.lock();
                    (
                        s.samples,
                        s.serial_faster,
                        s.accelerated_faster,
                        s.tie,
                        s.sum_total_tx_len,
                        s.sum_serial_ms,
                        s.sum_accel_ms,
                    )
                };

                let (samples, serial_faster, accelerated_faster, tie, sum_total_tx_len, sum_serial_ms, sum_accel_ms) =
                    snapshot;

                if samples == 0 {
                    continue;
                }

                let avg_tx = (sum_total_tx_len as f64) / (samples as f64);
                let avg_serial_ms = (sum_serial_ms as f64) / (samples as f64);
                let avg_accel_ms = (sum_accel_ms as f64) / (samples as f64);

                tracing::info!(
                    target: "bsc::builder",
                    samples,
                    serial_faster,
                    accelerated_faster,
                    tie,
                    avg_total_tx_len = avg_tx,
                    avg_serial_ms = avg_serial_ms,
                    avg_accel_ms = avg_accel_ms,
                    "State root compare summary (last 30s interval is cumulative)"
                );
            });
        });
    }

    fn wait_for_compare_state(
        state: &Mutex<StateRootCompareState>,
    ) -> StateRootCompareState {
        let mut tries = 0usize;
        loop {
            let snapshot = *state.lock();
            if snapshot.block_hash.is_some()
                && snapshot.serial_root.is_some()
                && snapshot.serial_duration_ms.is_some()
            {
                return snapshot
            }
            tries += 1;
            if tries >= 500 {
                // ~5s with 10ms sleep
                return snapshot
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn record_compare_sample(
        snapshot: StateRootCompareState,
        accel_ms: u128,
    ) {
        let Some(serial_ms) = snapshot.serial_duration_ms else { return };
        let total_tx_len = snapshot.total_tx_len.unwrap_or(0) as u128;

        let stats_arc = Self::compare_stats();
        let mut stats = stats_arc.lock();
        stats.samples += 1;
        stats.sum_total_tx_len += total_tx_len;
        stats.sum_serial_ms += serial_ms;
        stats.sum_accel_ms += accel_ms;

        if serial_ms < accel_ms {
            stats.serial_faster += 1;
        } else if serial_ms > accel_ms {
            stats.accelerated_faster += 1;
        } else {
            stats.tie += 1;
        }
    }

    /// Spawn an accelerated (sparse/parallel) state-root computation for **comparison only**.
    ///
    /// The caller supplies the authoritative serial root result. We compute the accelerated root
    /// on a detached thread, then log both durations + whether roots match, keyed by `block_hash`.
    pub fn spawn_accelerated_compare<Factory>(
        provider_factory: Factory,
        parent_number: u64,
        parent_hash: B256,
        hashed_state: HashedPostState,
        compare_state: Arc<Mutex<StateRootCompareState>>,
    ) where
        Factory: DatabaseProviderFactory<Provider: BlockNumReader + HeaderProvider + BlockReader>
            + Clone
            + Send
            + Sync
            + 'static,
    {
        // Ensure the periodic summary thread is running.
        Self::start_compare_printer_thread_once();

        std::thread::spawn(move || {
            let start = std::time::Instant::now();
            let res = Self::compute_accelerated_db_overlay_only(
                provider_factory,
                parent_number,
                parent_hash,
                &hashed_state,
            );
            let accel_dur = start.elapsed();
            let snapshot = Self::wait_for_compare_state(&compare_state);
            let block_hash = snapshot.block_hash;
            let user_tx_len = snapshot.user_tx_len;
            let system_tx_len = snapshot.system_tx_len;
            let total_tx_len = snapshot.total_tx_len;
            let serial_state_root = snapshot.serial_root;
            let serial_state_root_duration_ms = snapshot.serial_duration_ms;
            let accel_state_root_duration_ms = accel_dur.as_millis();

            // Update global stats (only if we have the serial duration).
            Self::record_compare_sample(snapshot, accel_state_root_duration_ms);

            let (faster_side, faster_by_ms) = match serial_state_root_duration_ms {
                Some(serial_ms) => {
                    if serial_ms < accel_state_root_duration_ms {
                        (Some("serial"), Some(accel_state_root_duration_ms - serial_ms))
                    } else if serial_ms > accel_state_root_duration_ms {
                        (Some("accelerated"), Some(serial_ms - accel_state_root_duration_ms))
                    } else {
                        (Some("tie"), Some(0))
                    }
                }
                None => (None, None),
            };

            match res {
                Ok((accel_root, accel_mode)) => {
                    let roots_equal = serial_state_root.is_some_and(|r| r == accel_root);
                    let approx_change_units: usize = hashed_state.accounts.len()
                        + hashed_state
                            .storages
                            .values()
                            .map(|s| s.storage.len())
                            .sum::<usize>();
                    let proof_targets = hashed_state.multi_proof_targets();
                    let proof_target_accounts = proof_targets.len();
                    let proof_target_slots: usize =
                        proof_targets.values().map(|s| s.len()).sum::<usize>();
                    let proof_target_chunks = (proof_target_accounts + MULTIPROOF_TARGETS_CHUNK_SIZE - 1)
                        / MULTIPROOF_TARGETS_CHUNK_SIZE;

                    if !roots_equal {
                        tracing::error!(
                            target: "bsc::builder",
                            block_number = parent_number + 1,
                            block_hash = ?block_hash,
                            user_tx_len = ?user_tx_len,
                            system_tx_len = ?system_tx_len,
                            total_tx_len = ?total_tx_len,
                            parent_hash = ?parent_hash,
                            approx_change_units,
                            proof_target_accounts,
                            proof_target_slots,
                            proof_target_chunks,
                            serial_state_root = ?serial_state_root,
                            serial_state_root_duration_ms = ?serial_state_root_duration_ms,
                            accel_state_root = ?accel_root,
                            accel_state_root_duration_ms,
                            accel_state_root_mode = ?accel_mode,
                            faster_side = ?faster_side,
                            faster_by_ms = ?faster_by_ms,
                            "State root MISMATCH (serial vs accelerated)"
                        );
                    } else {
                        tracing::debug!(
                            target: "bsc::builder",
                            block_number = parent_number + 1,
                            block_hash = ?block_hash,
                            user_tx_len = ?user_tx_len,
                            system_tx_len = ?system_tx_len,
                            total_tx_len = ?total_tx_len,
                            parent_hash = ?parent_hash,
                            approx_change_units,
                            proof_target_accounts,
                            proof_target_slots,
                            proof_target_chunks,
                            serial_state_root = ?serial_state_root,
                            serial_state_root_duration_ms = ?serial_state_root_duration_ms,
                            accel_state_root = ?accel_root,
                            accel_state_root_duration_ms,
                            accel_state_root_mode = ?accel_mode,
                            faster_side = ?faster_side,
                            faster_by_ms = ?faster_by_ms,
                            roots_equal,
                            "State root comparison (serial vs accelerated)"
                        );
                    }
                }
                Err((sparse_err, parallel_err)) => {
                    tracing::debug!(
                        target: "bsc::builder",
                        block_number = parent_number + 1,
                        block_hash = ?block_hash,
                        user_tx_len = ?user_tx_len,
                        system_tx_len = ?system_tx_len,
                        total_tx_len = ?total_tx_len,
                        parent_hash = ?parent_hash,
                        serial_state_root = ?serial_state_root,
                        serial_state_root_duration_ms = ?serial_state_root_duration_ms,
                        accel_state_root_duration_ms,
                        faster_side = ?faster_side,
                        faster_by_ms = ?faster_by_ms,
                        sparse_err = %sparse_err,
                        parallel_err = %parallel_err,
                        "State root comparison failed (accelerated unavailable)"
                    );
                }
            }
        });
    }

    fn compute_accelerated_db_overlay_only<Factory>(
        provider_factory: Factory,
        parent_number: u64,
        parent_hash: B256,
        hashed_state: &HashedPostState,
    ) -> Result<(B256, RootSpeederMode), (BlockExecutionError, BlockExecutionError)>
    where
        Factory: DatabaseProviderFactory<Provider: BlockNumReader + HeaderProvider + BlockReader>
            + Clone
            + Send
            + Sync
            + 'static,
    {
        // Fast path: sparse multiproof + sparse trie has a fixed overhead that often dominates for
        // very small blocks (few changed accounts/slots). In `sparse_case4.log` this shows up as
        // serial (0ms) vs sparse (1ms) for thousands of blocks.
        //
        // Heuristic: if the post-state touches only a small number of accounts/slots, skip the
        // sparse path and try the parallel state-root directly.
        let approx_change_units: usize = hashed_state.accounts.len()
            + hashed_state
                .storages
                .values()
                .map(|s| s.storage.len())
                .sum::<usize>();
        let skip_sparse_small_block = approx_change_units < 256;

        // Try sparse first by reusing the exact same code path pieces as the main implementation.
        let sparse_res = (|| -> Result<B256, BlockExecutionError> {
            if skip_sparse_small_block {
                return Err(BlockExecutionError::other(std::io::Error::other(format!(
                    "skip sparse: small post-state (change_units={approx_change_units})"
                ))));
            }
            let (root, _updates, _db_last, _overlay_blocks, _account_nodes, _wiped_storage_slots) =
                (|| -> Result<(B256, TrieUpdates, u64, usize, usize, usize), BlockExecutionError> {
                    // This is intentionally identical to the "attempt_sparse" closure in the main
                    // implementation (minus logging).
                    let provider_ro = provider_factory
                        .database_provider_ro()
                        .map_err(BlockExecutionError::other)?;
                    let db_last = provider_ro.best_block_number().map_err(BlockExecutionError::other)?;
                    if db_last > parent_number {
                        return Err(BlockExecutionError::other(std::io::Error::other(format!(
                            "db tip ({db_last}) ahead of parent ({parent_number})"
                        ))));
                    }
                    let db_tip = provider_ro
                        .sealed_header(db_last)
                        .map_err(BlockExecutionError::other)?
                        .ok_or_else(|| BlockExecutionError::other(std::io::Error::other("db tip missing")))?;

                    let consistent_view =
                        ConsistentDbView::new(provider_factory.clone(), Some((db_tip.hash(), db_last)));

                    let mut base_input = TrieInput::default();
                    let mut overlay_blocks = 0usize;
                    if db_last < parent_number {
                        let cache = trie_overlay_cache().ok_or_else(|| {
                            BlockExecutionError::other(std::io::Error::other("trie overlay cache not initialized"))
                        })?;
                        let needed_range = (db_last + 1)..=parent_number;
                        let overlays = cache.read().get_range(needed_range.clone());
                        overlay_blocks = overlays.len();
                        if overlays.len() != (parent_number - db_last) as usize {
                            return Err(BlockExecutionError::other(std::io::Error::other(format!(
                                "missing trie overlay blocks for range {:?} (have {})",
                                needed_range,
                                overlays.len()
                            ))));
                        }
                        for entry in overlays {
                            if entry.number == parent_number && entry.hash != parent_hash {
                                return Err(BlockExecutionError::other(std::io::Error::other(format!(
                                    "parent hash mismatch in overlay cache: expected={:?} got={:?}",
                                    parent_hash, entry.hash
                                ))));
                            }
                            let Some(nodes) = entry.trie_updates.as_deref() else {
                                return Err(BlockExecutionError::other(std::io::Error::other(format!(
                                    "missing trie_updates for overlay block {}",
                                    entry.number
                                ))));
                            };
                            base_input.append_cached_ref(nodes, &entry.hashed_state);
                        }
                    }

                    let nodes_sorted = Arc::new(base_input.nodes.clone().into_sorted());
                    let state_sorted = Arc::new(base_input.state.clone().into_sorted());

                    let provider_ro = consistent_view.provider_ro().map_err(BlockExecutionError::other)?;
                    let hashed_cursor_factory = HashedPostStateCursorFactory::new(
                        DatabaseHashedCursorFactory::new(provider_ro.tx_ref()),
                        &state_sorted,
                    );

                    let mut proof_targets: MultiProofTargets = hashed_state.multi_proof_targets();
                    let mut wiped_storage_slots = 0usize;
                    for (hashed_address, storage) in &hashed_state.storages {
                        if !storage.wiped {
                            continue;
                        }
                        let mut slots: B256Set =
                            proof_targets.get(hashed_address).cloned().unwrap_or_default();
                        let mut storage_cursor = hashed_cursor_factory
                            .hashed_storage_cursor(*hashed_address)
                            .map_err(|e| BlockExecutionError::other(std::io::Error::other(e)))?;
                        let mut current_entry = storage_cursor
                            .seek(B256::ZERO)
                            .map_err(|e| BlockExecutionError::other(std::io::Error::other(e)))?;
                        while let Some((hashed_slot, _)) = current_entry {
                            wiped_storage_slots += 1;
                            slots.insert(hashed_slot);
                            current_entry = storage_cursor
                                .next()
                                .map_err(|e| BlockExecutionError::other(std::io::Error::other(e)))?;
                        }
                        proof_targets.insert(*hashed_address, slots);
                    }

                    // engine-tree style: parallel decoded multiproofs + correct sparse update order.
                    let prefix_sets = Arc::new(hashed_state.construct_prefix_sets());
                    let task_ctx = ProofTaskCtx::new(nodes_sorted.clone(), state_sorted.clone(), prefix_sets.clone());
                    let rt = root_speeder_proof_runtime();
                    let handle = rt.handle().clone();
                    let proof_task = ProofTaskManager::new(handle.clone(), consistent_view.clone(), task_ctx, 64);
                    let proof_handle = proof_task.handle();
                    handle.spawn_blocking(move || {
                        let _ = proof_task.run();
                    });

                    let mut sparse =
                        SparseStateTrie::<ParallelSparseTrie, SerialSparseTrie>::new().with_updates(true);

                    // Try to reuse a prefetched full decoded multiproof first (fastest, best for fairness).
                    let key = PrefetchKey { parent_number, parent_hash };
                    let prefetched = prefetched_multiproofs().lock().remove(&key);
                    if let Some(prefetched) = prefetched.filter(|p| p.targets == proof_targets) {
                        sparse
                            .reveal_decoded_multiproof(prefetched.multiproof)
                            .map_err(|e| BlockExecutionError::other(std::io::Error::other(format!("{e:?}"))))?;
                        apply_hashed_state_update_to_sparse_trie(
                            &mut sparse,
                            hashed_state.clone(),
                            &proof_handle,
                        )?;
                    } else {
                        for chunk_targets in
                            chunk_multiproof_targets(&proof_targets, MULTIPROOF_TARGETS_CHUNK_SIZE)
                        {
                            let decoded = ParallelProof::new(
                                consistent_view.clone(),
                                nodes_sorted.clone(),
                                state_sorted.clone(),
                                prefix_sets.clone(),
                                proof_handle.clone(),
                            )
                            .with_branch_node_masks(true)
                            .decoded_multiproof(chunk_targets.clone())
                            .map_err(|e| {
                                BlockExecutionError::other(std::io::Error::other(format!("{e:?}")))
                            })?;

                            sparse
                                .reveal_decoded_multiproof(decoded)
                                .map_err(|e| {
                                    BlockExecutionError::other(std::io::Error::other(format!("{e:?}")))
                                })?;

                            let subset =
                                make_hashed_state_subset_for_targets(hashed_state, &chunk_targets);
                            apply_hashed_state_update_to_sparse_trie(
                                &mut sparse,
                                subset,
                                &proof_handle,
                            )?;
                        }
                    }

                    let (state_root, trie_updates) = sparse
                        .root_with_updates(proof_handle.clone())
                        .map_err(|e| BlockExecutionError::other(std::io::Error::other(format!("{e:?}"))))?;

                    // Cross-check (sampled): compare sparse root against parallel root on the same input.
                    if should_cross_check(parent_hash) {
                        let mut trie_input = base_input.clone();
                        trie_input.append(hashed_state.clone());
                        let (parallel_root, _parallel_updates) =
                            ParallelStateRoot::new(consistent_view.clone(), trie_input)
                                .incremental_root_with_updates()
                                .map_err(BlockExecutionError::other)?;
                        if parallel_root != state_root {
                            return Err(BlockExecutionError::other(std::io::Error::other(format!(
                                "cross-check mismatch: sparse_root={state_root:?} parallel_root={parallel_root:?}"
                            ))));
                        }
                    }

                    Ok((
                        state_root,
                        trie_updates,
                        db_last,
                        overlay_blocks,
                        nodes_sorted.account_nodes.len(),
                        wiped_storage_slots,
                    ))
                })()?;
            Ok(root)
        })();

        if let Ok(root) = sparse_res {
            return Ok((root, RootSpeederMode::Sparse))
        }

        let sparse_err = sparse_res.err().expect("checked above");

        let parallel_res = (|| -> Result<B256, BlockExecutionError> {
            let provider_ro = provider_factory
                .database_provider_ro()
                .map_err(BlockExecutionError::other)?;
            let db_last = provider_ro.best_block_number().map_err(BlockExecutionError::other)?;
            if db_last > parent_number {
                return Err(BlockExecutionError::other(std::io::Error::other(format!(
                    "db tip ({db_last}) ahead of parent ({parent_number})"
                ))));
            }
            let db_tip = provider_ro
                .sealed_header(db_last)
                .map_err(BlockExecutionError::other)?
                .ok_or_else(|| BlockExecutionError::other(std::io::Error::other("db tip missing")))?;
            let consistent_view =
                ConsistentDbView::new(provider_factory.clone(), Some((db_tip.hash(), db_last)));

            let mut trie_input = TrieInput::default();
            if db_last < parent_number {
                let cache = trie_overlay_cache().ok_or_else(|| {
                    BlockExecutionError::other(std::io::Error::other("trie overlay cache not initialized"))
                })?;
                let needed_range = (db_last + 1)..=parent_number;
                let overlays = cache.read().get_range(needed_range.clone());
                if overlays.len() != (parent_number - db_last) as usize {
                    return Err(BlockExecutionError::other(std::io::Error::other(format!(
                        "missing trie overlay blocks for range {:?} (have {})",
                        needed_range,
                        overlays.len()
                    ))));
                }
                for entry in overlays {
                    if entry.number == parent_number && entry.hash != parent_hash {
                        return Err(BlockExecutionError::other(std::io::Error::other(format!(
                            "parent hash mismatch in overlay cache: expected={:?} got={:?}",
                            parent_hash, entry.hash
                        ))));
                    }
                    let Some(nodes) = entry.trie_updates.as_deref() else {
                        return Err(BlockExecutionError::other(std::io::Error::other(format!(
                            "missing trie_updates for overlay block {}",
                            entry.number
                        ))));
                    };
                    trie_input.append_cached_ref(nodes, &entry.hashed_state);
                }
            }

            trie_input.append(hashed_state.clone());
            let (state_root, _trie_updates) = ParallelStateRoot::new(consistent_view, trie_input)
                .incremental_root_with_updates()
                .map_err(BlockExecutionError::other)?;
            Ok(state_root)
        })();

        match parallel_res {
            Ok(root) => Ok((root, RootSpeederMode::Parallel)),
            Err(par_err) => Err((sparse_err, par_err)),
        }
    }

    /// Computes `(state_root, trie_updates, mode)` for a block being built on `parent`.
    ///
    /// - Attempts sparse trie computation first (multiproof + sparse trie with parallel accounts trie)
    /// - Falls back to `ParallelStateRoot`
    /// - Falls back to serial `StateProvider::state_root_with_updates`
    pub fn compute_state_root_with_updates<Factory, SP>(
        provider_factory: Factory,
        parent_number: u64,
        parent_hash: B256,
        hashed_state: &HashedPostState,
        state_provider: &SP,
    ) -> Result<(B256, TrieUpdates, RootSpeederMode), BlockExecutionError>
    where
        Factory: DatabaseProviderFactory<Provider: BlockNumReader + HeaderProvider + BlockReader>
            + Clone
            + Send
            + Sync
            + 'static,
        SP: StateProvider,
    {
        let attempt_sparse = (|| -> Result<(B256, TrieUpdates, u64, usize, usize, usize), BlockExecutionError> {
            let provider_ro = provider_factory
                .database_provider_ro()
                .map_err(BlockExecutionError::other)?;
            // Use the highest fully-available DB tip for trie reads.
            // During persistence, static-file tip can temporarily move ahead of DB tables.
            let db_last = provider_ro.best_block_number().map_err(BlockExecutionError::other)?;

            // If the database is ahead of the requested parent (e.g. we're building on an ancestor
            // due to reorg/stale work), then a forward overlay is not sufficient.
            if db_last > parent_number {
                return Err(BlockExecutionError::other(std::io::Error::other(format!(
                    "db tip ({db_last}) ahead of parent ({parent_number})"
                ))));
            }

            let db_tip = provider_ro
                .sealed_header(db_last)
                .map_err(BlockExecutionError::other)?
                .ok_or_else(|| BlockExecutionError::other(std::io::Error::other("db tip missing")))?;

            let consistent_view =
                ConsistentDbView::new(provider_factory.clone(), Some((db_tip.hash(), db_last)));

            // Build a base DB+overlay trie input that represents the *parent* state.
            let mut base_input = TrieInput::default();
            let mut overlay_blocks = 0usize;

            if db_last < parent_number {
                let cache = trie_overlay_cache().ok_or_else(|| {
                    BlockExecutionError::other(std::io::Error::other("trie overlay cache not initialized"))
                })?;
                let needed_range = (db_last + 1)..=parent_number;
                let overlays = cache.read().get_range(needed_range.clone());
                overlay_blocks = overlays.len();

                // Require full coverage.
                if overlays.len() != (parent_number - db_last) as usize {
                    return Err(BlockExecutionError::other(std::io::Error::other(format!(
                        "missing trie overlay blocks for range {:?} (have {})",
                        needed_range,
                        overlays.len()
                    ))));
                }

                for entry in overlays {
                    // Ensure hash matches expected canonical chain (best-effort safety).
                    if entry.number == parent_number && entry.hash != parent_hash {
                        return Err(BlockExecutionError::other(std::io::Error::other(format!(
                            "parent hash mismatch in overlay cache: expected={:?} got={:?}",
                            parent_hash, entry.hash
                        ))));
                    }

                    let Some(nodes) = entry.trie_updates.as_deref() else {
                        return Err(BlockExecutionError::other(std::io::Error::other(format!(
                            "missing trie_updates for overlay block {}",
                            entry.number
                        ))));
                    };
                    base_input.append_cached_ref(nodes, &entry.hashed_state);
                }
            }

            let nodes_sorted = Arc::new(base_input.nodes.clone().into_sorted());
            let state_sorted = Arc::new(base_input.state.clone().into_sorted());

            // Generate multiproof against DB + overlay up to parent.
            let provider_ro = consistent_view.provider_ro().map_err(BlockExecutionError::other)?;
            let hashed_cursor_factory = HashedPostStateCursorFactory::new(
                DatabaseHashedCursorFactory::new(provider_ro.tx_ref()),
                &state_sorted,
            );

            // Build proof targets, expanding wiped storage to include all existing slots from the
            // base view (DB + overlay up to parent).
            let mut proof_targets: MultiProofTargets = hashed_state.multi_proof_targets();
            let mut wiped_storage_slots = 0usize;
            for (hashed_address, storage) in &hashed_state.storages {
                if !storage.wiped {
                    continue;
                }
                let mut slots: B256Set =
                    proof_targets.get(hashed_address).cloned().unwrap_or_default();
                let mut storage_cursor = hashed_cursor_factory
                    .hashed_storage_cursor(*hashed_address)
                    .map_err(|e| BlockExecutionError::other(std::io::Error::other(e)))?;
                let mut current_entry = storage_cursor
                    .seek(B256::ZERO)
                    .map_err(|e| BlockExecutionError::other(std::io::Error::other(e)))?;
                while let Some((hashed_slot, _)) = current_entry {
                    wiped_storage_slots += 1;
                    slots.insert(hashed_slot);
                    current_entry = storage_cursor
                        .next()
                        .map_err(|e| BlockExecutionError::other(std::io::Error::other(e)))?;
                }
                proof_targets.insert(*hashed_address, slots);
            }

            // Try to reuse a prefetched multiproof (built concurrently during tx execution).
            let key = PrefetchKey { parent_number, parent_hash };
            let prefetched = prefetched_multiproofs().lock().remove(&key);
            // Build a single proof-task manager and reuse its handle for:
            // - decoding multiproof (if needed)
            // - serving blind node fetches during sparse updates
            let prefix_sets = Arc::new(hashed_state.construct_prefix_sets());
            let task_ctx = ProofTaskCtx::new(nodes_sorted.clone(), state_sorted.clone(), prefix_sets.clone());
            let rt = root_speeder_proof_runtime();
            let handle = rt.handle().clone();
            let proof_task = ProofTaskManager::new(handle.clone(), consistent_view.clone(), task_ctx, 64);
            let proof_handle = proof_task.handle();
            handle.spawn_blocking(move || {
                let _ = proof_task.run();
            });

            let decoded_multiproof = match prefetched {
                Some(prefetched) if prefetched.targets == proof_targets => prefetched.multiproof,
                _ => ParallelProof::new(
                    consistent_view.clone(),
                    nodes_sorted.clone(),
                    state_sorted.clone(),
                    prefix_sets.clone(),
                    proof_handle.clone(),
                )
                .with_branch_node_masks(true)
                .decoded_multiproof(proof_targets.clone())
                .map_err(|e| BlockExecutionError::other(std::io::Error::other(format!("{e:?}"))))?,
            };

            // Sparse state trie, with parallel sparse accounts trie.
            let mut sparse = SparseStateTrie::<ParallelSparseTrie, SerialSparseTrie>::new().with_updates(true);
            sparse
                .reveal_decoded_multiproof(decoded_multiproof)
                .map_err(|e| BlockExecutionError::other(std::io::Error::other(format!("{e:?}"))))?;

            apply_hashed_state_update_to_sparse_trie(&mut sparse, hashed_state.clone(), &proof_handle)?;

            let (state_root, trie_updates) = sparse
                .root_with_updates(proof_handle.clone())
                .map_err(|e| BlockExecutionError::other(std::io::Error::other(format!("{e:?}"))))?;

            // Cross-check (sampled): compare sparse root against parallel root on the same input.
            if should_cross_check(parent_hash) {
                let mut trie_input = base_input.clone();
                trie_input.append(hashed_state.clone());
                let (parallel_root, _parallel_updates) =
                    ParallelStateRoot::new(consistent_view.clone(), trie_input)
                        .incremental_root_with_updates()
                        .map_err(BlockExecutionError::other)?;
                if parallel_root != state_root {
                    return Err(BlockExecutionError::other(std::io::Error::other(format!(
                        "cross-check mismatch: sparse_root={state_root:?} parallel_root={parallel_root:?}"
                    ))));
                }
            }

            Ok((
                state_root,
                trie_updates,
                db_last,
                overlay_blocks,
                nodes_sorted.account_nodes.len(),
                wiped_storage_slots,
            ))
        })();

        let attempt_parallel = (|| -> Result<(B256, TrieUpdates, u64, usize), BlockExecutionError> {
            let provider_ro = provider_factory
                .database_provider_ro()
                .map_err(BlockExecutionError::other)?;
            let db_last = provider_ro.best_block_number().map_err(BlockExecutionError::other)?;

            if db_last > parent_number {
                return Err(BlockExecutionError::other(std::io::Error::other(format!(
                    "db tip ({db_last}) ahead of parent ({parent_number})"
                ))));
            }

            let db_tip = provider_ro
                .sealed_header(db_last)
                .map_err(BlockExecutionError::other)?
                .ok_or_else(|| BlockExecutionError::other(std::io::Error::other("db tip missing")))?;

            let consistent_view =
                ConsistentDbView::new(provider_factory.clone(), Some((db_tip.hash(), db_last)));

            let mut trie_input = TrieInput::default();
            let mut overlay_blocks = 0usize;

            if db_last < parent_number {
                let cache = trie_overlay_cache().ok_or_else(|| {
                    BlockExecutionError::other(std::io::Error::other("trie overlay cache not initialized"))
                })?;
                let needed_range = (db_last + 1)..=parent_number;
                let overlays = cache.read().get_range(needed_range.clone());
                overlay_blocks = overlays.len();

                if overlays.len() != (parent_number - db_last) as usize {
                    return Err(BlockExecutionError::other(std::io::Error::other(format!(
                        "missing trie overlay blocks for range {:?} (have {})",
                        needed_range,
                        overlays.len()
                    ))));
                }

                for entry in overlays {
                    if entry.number == parent_number && entry.hash != parent_hash {
                        return Err(BlockExecutionError::other(std::io::Error::other(format!(
                            "parent hash mismatch in overlay cache: expected={:?} got={:?}",
                            parent_hash, entry.hash
                        ))));
                    }
                    let Some(nodes) = entry.trie_updates.as_deref() else {
                        return Err(BlockExecutionError::other(std::io::Error::other(format!(
                            "missing trie_updates for overlay block {}",
                            entry.number
                        ))));
                    };
                    trie_input.append_cached_ref(nodes, &entry.hashed_state);
                }
            }

            trie_input.append(hashed_state.clone());

            let (state_root, trie_updates) = ParallelStateRoot::new(consistent_view, trie_input)
                .incremental_root_with_updates()
                .map_err(BlockExecutionError::other)?;

            Ok((state_root, trie_updates, db_last, overlay_blocks))
        })();

        match attempt_sparse {
            Ok((state_root, trie_updates, db_last, overlay_blocks, account_nodes, wiped_storage_slots)) => {
                tracing::debug!(
                    target: "bsc::trie_root",
                    parent_num = parent_number,
                    parent_hash = ?parent_hash,
                    db_last,
                    overlay_blocks,
                    account_nodes,
                    wiped_storage_slots,
                    "Sparse state root succeeded"
                );
                Ok((state_root, trie_updates, RootSpeederMode::Sparse))
            }
            Err(sparse_err) => {
                tracing::debug!(
                    target: "bsc::trie_root",
                    parent_num = parent_number,
                    parent_hash = ?parent_hash,
                    %sparse_err,
                    "Sparse state root unavailable, trying parallel state root"
                );

                match attempt_parallel {
                    Ok((state_root, trie_updates, db_last, overlay_blocks)) => {
                        tracing::debug!(
                            target: "bsc::trie_root",
                            parent_num = parent_number,
                            parent_hash = ?parent_hash,
                            db_last,
                            overlay_blocks,
                            "Parallel state root succeeded"
                        );
                        Ok((state_root, trie_updates, RootSpeederMode::Parallel))
                    }
                    Err(par_err) => {
                        tracing::debug!(
                            target: "bsc::trie_root",
                            parent_num = parent_number,
                            parent_hash = ?parent_hash,
                            %par_err,
                            "Parallel state root unavailable, falling back to serial"
                        );
                        let (state_root, trie_updates) = state_provider
                            .state_root_with_updates(hashed_state.clone())
                            .map_err(BlockExecutionError::other)?;
                        Ok((state_root, trie_updates, RootSpeederMode::Serial))
                    }
                }
            }
        }
    }
}

/// A helper that owns the canonical-chain subscription and keeps the overlay cache up to date.
///
/// This is intended to be used by the miner: miner calls a single `drain()` method per loop.
pub struct RootSpeederUpdater<P>
where
    P: NewCanonicalChainSubscriptions + NodePrimitivesProvider + Clone + Send + Sync + 'static,
{
    rx: broadcast::Receiver<NewCanonicalChain<<P as NodePrimitivesProvider>::Primitives>>,
}

impl<P> RootSpeederUpdater<P>
where
    P: NewCanonicalChainSubscriptions + NodePrimitivesProvider + Clone + Send + Sync + 'static,
{
    /// Initializes overlay cache and subscribes to new canonical chain updates.
    pub fn new(provider: &P, overlay_capacity: usize) -> Self {
        let _ = init_trie_overlay_cache(overlay_capacity);
        let rx = provider.subscribe_to_new_canonical_chain();
        Self { rx }
    }

    /// Drain pending canonical chain updates and apply them to the overlay cache.
    pub fn drain(&mut self) {
        loop {
            match self.rx.try_recv() {
                Ok(update) => Self::apply_update(&update),
                Err(broadcast::error::TryRecvError::Empty) => break,
                Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
                Err(broadcast::error::TryRecvError::Closed) => break,
            }
        }
    }

    fn apply_update<N: reth_primitives_traits::NodePrimitives>(event: &NewCanonicalChain<N>) {
        let Some(cache) = trie_overlay_cache() else { return };
        let mut w = cache.write();
        match event {
            NewCanonicalChain::Commit { new } => {
                for exec in new {
                    let num = exec.block.block_number();
                    let hash = exec.block.recovered_block.hash();
                    let hashed_state = Arc::clone(&exec.block.hashed_state);
                    let trie_updates = match &exec.trie {
                        ExecutedTrieUpdates::Present(updates) => Some(Arc::clone(updates)),
                        ExecutedTrieUpdates::Missing => None,
                    };
                    w.insert(TrieOverlayEntry { number: num, hash, hashed_state, trie_updates });
                }
            }
            NewCanonicalChain::Reorg { new, old } => {
                for exec in old {
                    let num = exec.block_number();
                    w.remove_range(num..=num);
                }
                for exec in new {
                    let num = exec.block.block_number();
                    let hash = exec.block.recovered_block.hash();
                    let hashed_state = Arc::clone(&exec.block.hashed_state);
                    let trie_updates = match &exec.trie {
                        ExecutedTrieUpdates::Present(updates) => Some(Arc::clone(updates)),
                        ExecutedTrieUpdates::Missing => None,
                    };
                    w.insert(TrieOverlayEntry { number: num, hash, hashed_state, trie_updates });
                }
            }
        }
    }
}

