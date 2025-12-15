use super::trie_overlay::{init_trie_overlay_cache, trie_overlay_cache, TrieOverlayEntry};
use alloy_primitives::{keccak256, map::B256Set, B256};
use reth_chain_state::{ExecutedTrieUpdates, NewCanonicalChain};
use reth_evm::OnStateHook;
use reth_evm::execute::BlockExecutionError;
use reth_provider::{
    providers::ConsistentDbView, BlockNumReader, BlockReader, DatabaseProviderFactory, HeaderProvider,
    NewCanonicalChainSubscriptions, StateProvider, DBProvider, NodePrimitivesProvider,
};
use reth_trie::{
    hashed_cursor::{HashedCursor, HashedCursorFactory, HashedPostStateCursorFactory},
    proof::{Proof, ProofTrieNodeProviderFactory},
    trie_cursor::InMemoryTrieCursorFactory,
    updates::TrieUpdates,
    HashedPostState, MultiProof, MultiProofTargets, Nibbles, TrieInput,
};
use reth_trie_db::{DatabaseHashedCursorFactory, DatabaseTrieCursorFactory};
use reth_trie_parallel::root::ParallelStateRoot;
use reth_trie_sparse::{
    provider::TrieNodeProviderFactory, SerialSparseTrie, SparseStateTrie, SparseTrie,
    SparseTrieInterface,
};
use reth_trie_sparse_parallel::ParallelSparseTrie;
use parking_lot::Mutex;
use revm::state::EvmState;
use std::{collections::HashMap, sync::{Arc, OnceLock}};
use tokio::sync::broadcast;

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
    multiproof: MultiProof,
}

static PREFETCHED_MULTIPROOFS: OnceLock<Mutex<HashMap<PrefetchKey, PrefetchedMultiproof>>> = OnceLock::new();

fn prefetched_multiproofs() -> &'static Mutex<HashMap<PrefetchKey, PrefetchedMultiproof>> {
    PREFETCHED_MULTIPROOFS.get_or_init(|| Mutex::new(HashMap::new()))
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
                let trie_cursor_factory = InMemoryTrieCursorFactory::new(
                    DatabaseTrieCursorFactory::new(provider_ro.tx_ref()),
                    &nodes_sorted,
                );
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

                let multiproof = Proof::new(trie_cursor_factory, hashed_cursor_factory)
                    .with_branch_node_masks(true)
                    .multiproof(expanded_targets.clone())
                    .map_err(|e| BlockExecutionError::other(std::io::Error::other(e)))?;

                Ok(PrefetchedMultiproof { targets: expanded_targets, multiproof })
            })();

            if let Ok(prefetched) = res {
                prefetched_multiproofs().lock().insert(key, prefetched);
            }
        });
    }
}

impl RootSpeeder {
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
            let trie_cursor_factory = InMemoryTrieCursorFactory::new(
                DatabaseTrieCursorFactory::new(provider_ro.tx_ref()),
                &nodes_sorted,
            );
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

            let prefix_sets = hashed_state.construct_prefix_sets();

            // Try to reuse a prefetched multiproof (built concurrently during tx execution).
            let key = PrefetchKey { parent_number, parent_hash };
            let prefetched = prefetched_multiproofs().lock().remove(&key);
            let multiproof = match prefetched {
                Some(prefetched) if prefetched.targets == proof_targets => prefetched.multiproof,
                _ => Proof::new(trie_cursor_factory.clone(), hashed_cursor_factory.clone())
                    .with_prefix_sets_mut(prefix_sets.clone())
                    .with_branch_node_masks(true)
                    .multiproof(proof_targets)
                    .map_err(|e| BlockExecutionError::other(std::io::Error::other(e)))?,
            };

            // Sparse state trie, with parallel sparse accounts trie.
            let mut sparse = SparseStateTrie::<ParallelSparseTrie, SerialSparseTrie>::new().with_updates(true);
            sparse
                .reveal_multiproof(multiproof)
                .map_err(|e| BlockExecutionError::other(std::io::Error::other(format!("{e:?}"))))?;

            // Provider factory for on-demand reveals during sparse updates.
            let blinded_provider_factory = ProofTrieNodeProviderFactory::new(
                trie_cursor_factory,
                hashed_cursor_factory,
                Arc::new(prefix_sets),
            );

            // Apply storage changes first so account updates can compute storage roots.
            for (hashed_address, storage) in &hashed_state.storages {
                let Some(storage_trie) = sparse.storage_trie_mut(hashed_address) else {
                    return Err(BlockExecutionError::other(std::io::Error::other(format!(
                        "sparse storage trie not revealed for account {hashed_address:?}"
                    ))));
                };

                if storage.wiped {
                    storage_trie.wipe();
                }

                // Defer removals until after updates.
                let mut removed_slots: Vec<Nibbles> = Vec::new();
                for (slot, value) in &storage.storage {
                    let slot_nibbles = Nibbles::unpack(slot);
                    if value.is_zero() {
                        removed_slots.push(slot_nibbles);
                        continue;
                    }
                    storage_trie
                        .update_leaf(
                            slot_nibbles,
                            alloy_rlp::encode_fixed_size(value).to_vec(),
                            blinded_provider_factory.storage_node_provider(*hashed_address),
                        )
                        .map_err(|e| BlockExecutionError::other(std::io::Error::other(format!("{e:?}"))))?;
                }
                for slot_nibbles in removed_slots {
                    storage_trie
                        .remove_leaf(
                            &slot_nibbles,
                            blinded_provider_factory.storage_node_provider(*hashed_address),
                        )
                        .map_err(|e| BlockExecutionError::other(std::io::Error::other(format!("{e:?}"))))?;
                }
                storage_trie.root();
            }

            // Apply account changes.
            for (hashed_address, maybe_account) in &hashed_state.accounts {
                let nibbles = Nibbles::unpack(hashed_address);
                match maybe_account {
                    Some(account) => {
                        let keep = sparse
                            .update_account(*hashed_address, account.clone(), &blinded_provider_factory)
                            .map_err(|e| BlockExecutionError::other(std::io::Error::other(format!("{e:?}"))))?;
                        if !keep {
                            sparse
                                .remove_account_leaf(&nibbles, &blinded_provider_factory)
                                .map_err(|e| BlockExecutionError::other(std::io::Error::other(format!("{e:?}"))))?;
                        }
                    }
                    None => {
                        // Ensure storage trie deletion is reflected in trie updates.
                        if sparse.storage_trie_ref(hashed_address).is_none() {
                            let mut wiped = SparseTrie::Revealed(Box::new(
                                SerialSparseTrie::default().with_updates(true),
                            ));
                            wiped
                                .wipe()
                                .map_err(|e| BlockExecutionError::other(std::io::Error::other(format!("{e:?}"))))?;
                            sparse.insert_storage_trie(*hashed_address, wiped);
                        } else {
                            let trie = sparse.storage_trie_mut(hashed_address).expect("checked above");
                            trie.wipe();
                        }

                        sparse
                            .remove_account_leaf(&nibbles, &blinded_provider_factory)
                            .map_err(|e| BlockExecutionError::other(std::io::Error::other(format!("{e:?}"))))?;
                    }
                }
            }

            let (state_root, trie_updates) = sparse
                .root_with_updates(blinded_provider_factory)
                .map_err(|e| BlockExecutionError::other(std::io::Error::other(format!("{e:?}"))))?;

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

