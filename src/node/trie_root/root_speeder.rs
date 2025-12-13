use super::trie_overlay::{init_trie_overlay_cache, trie_overlay_cache, TrieOverlayEntry};
use alloy_primitives::{map::B256Set, B256};
use reth_chain_state::{ExecutedTrieUpdates, NewCanonicalChain};
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
    HashedPostState, MultiProofTargets, Nibbles, TrieInput,
};
use reth_trie_db::{DatabaseHashedCursorFactory, DatabaseTrieCursorFactory};
use reth_trie_parallel::root::ParallelStateRoot;
use reth_trie_sparse::{
    provider::TrieNodeProviderFactory, SerialSparseTrie, SparseStateTrie, SparseTrie,
    SparseTrieInterface,
};
use reth_trie_sparse_parallel::ParallelSparseTrie;
use std::sync::Arc;
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
            let multiproof = Proof::new(trie_cursor_factory.clone(), hashed_cursor_factory.clone())
                .with_prefix_sets_mut(prefix_sets.clone())
                .with_branch_node_masks(true)
                .multiproof(proof_targets)
                .map_err(|e| BlockExecutionError::other(std::io::Error::other(e)))?;

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

