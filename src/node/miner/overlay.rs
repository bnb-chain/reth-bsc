//! Miner-owned parent-state cache. Candidate state is never inserted here.

use crate::BscPrimitives;
use alloy_primitives::B256;
use reth_chain_state::ExecutedBlock;
use reth_storage_overlay::OverlayManager;

// Persistence normally rolls the generation sooner. Bound retained parent overlays even if
// persistence stalls; this limits cache growth, not the ancestor data required by a root task.
const MAX_PARENTS_PER_GENERATION: usize = 32;

#[derive(Default)]
pub(crate) struct MinerOverlayCache {
    current: Option<Generation>,
}

struct Generation {
    manager: OverlayManager<BscPrimitives>,
    anchor: B256,
    tip: B256,
    parents: usize,
}

impl MinerOverlayCache {
    /// Select a cache using a captured parent-chain snapshot, ordered newest to oldest.
    ///
    /// Only append to a generation. Never prune it in place: proof workers may still be using
    /// its old parent while persistence, a reorg, or another build changes the current chain.
    /// Return the retired manager so the caller can release it off the mining thread; existing
    /// factories/tasks retain their clones until their work ends.
    pub(crate) fn for_parent(
        &mut self,
        blocks: &[ExecutedBlock<BscPrimitives>],
        new_manager: impl FnOnce() -> OverlayManager<BscPrimitives>,
    ) -> (OverlayManager<BscPrimitives>, Option<OverlayManager<BscPrimitives>>) {
        let (Some(tip), Some(oldest)) = (blocks.first(), blocks.last()) else {
            let retired = self.current.take().map(|generation| generation.manager);
            return (new_manager(), retired);
        };
        let tip = tip.recovered_block().hash();
        let anchor = oldest.recovered_block().parent_hash;

        // A previous tip must be on the requested parent chain. This also handles rollback:
        // asking for an ancestor of our previous tip starts a separate generation.
        let extension = self.current.as_ref().and_then(|current| {
            (current.anchor == anchor).then(|| {
                blocks.iter().position(|block| block.recovered_block().hash() == current.tip)
            })?
        });
        let reuse = extension.is_some_and(|added| {
            let current = self.current.as_ref().expect("extension requires a generation");
            added == 0 || current.parents < MAX_PARENTS_PER_GENERATION
        });

        if reuse {
            let current = self.current.as_mut().expect("reuse requires a generation");
            let added = extension.expect("reuse requires a parent on the chain");
            for block in blocks[..added].iter().rev() {
                current.manager.insert_block(block.clone());
            }
            if added > 0 {
                current.parents += 1;
                current.tip = tip;
            }
            return (current.manager.clone(), None);
        }

        let manager = new_manager();
        for block in blocks.iter().rev() {
            manager.insert_block(block.clone());
        }
        let retired = self
            .current
            .replace(Generation { manager: manager.clone(), anchor, tip, parents: 1 })
            .map(|generation| generation.manager);
        (manager, retired)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BscBlock;
    use alloy_consensus::Header;
    use alloy_eips::BlockNumHash;
    use reth_primitives_traits::{Account, RecoveredBlock};
    use reth_provider::{
        providers::LatestStateProvider, test_utils::create_test_provider_factory, StateRootProvider,
    };
    use reth_storage_overlay::Overlay;
    use reth_trie_common::{ComputedTrieData, HashedPostState, HashedStorage};
    use std::sync::Arc;

    fn block(number: u64, parent_hash: B256, nonce: u64) -> ExecutedBlock<BscPrimitives> {
        let block = RecoveredBlock::new_unhashed(
            BscBlock {
                header: Header { number, parent_hash, timestamp: nonce, ..Default::default() },
                body: Default::default(),
            },
            Vec::new(),
        );
        // Repeated writes to one account detect wrong ordering and state leaking across forks.
        let state = HashedPostState::default()
            .with_accounts([
                (B256::with_last_byte(1), Some(Account { nonce, ..Default::default() })),
                (B256::with_last_byte(number as u8 + 2), Some(Account::default())),
            ])
            .with_storages([(
                B256::with_last_byte(1),
                HashedStorage::from_iter([
                    (B256::ZERO, alloy_primitives::U256::from(nonce)),
                    (B256::with_last_byte(number as u8), alloy_primitives::U256::from(nonce)),
                ]),
            )])
            .into_sorted();
        ExecutedBlock::new(
            Arc::new(block),
            Default::default(),
            ComputedTrieData::new(Arc::new(state), Default::default()),
        )
    }

    fn overlay(
        manager: &OverlayManager<BscPrimitives>,
        tip: B256,
        anchor: BlockNumHash,
    ) -> Overlay {
        let factory = create_test_provider_factory();
        manager
            .overlay_builder(tip)
            .build_overlay_at_frontiers(&factory.provider().unwrap(), anchor, anchor)
            .unwrap()
    }

    fn root(overlay: &Overlay) -> B256 {
        let factory = create_test_provider_factory();
        LatestStateProvider::new(factory.provider().unwrap())
            .state_root(HashedPostState {
                accounts: overlay.hashed_post_state.accounts.iter().copied().collect(),
                storages: overlay
                    .hashed_post_state
                    .storages
                    .iter()
                    .map(|(address, storage)| {
                        (
                            *address,
                            HashedStorage {
                                wiped: storage.wiped,
                                storage: storage.storage_slots.iter().copied().collect(),
                            },
                        )
                    })
                    .collect(),
            })
            .unwrap()
    }

    fn fresh(blocks: &[ExecutedBlock<BscPrimitives>]) -> OverlayManager<BscPrimitives> {
        MinerOverlayCache::default().for_parent(blocks, OverlayManager::default).0
    }

    #[test]
    fn retries_share_computed_overlay_and_extension_matches_fresh_root() {
        let anchor = BlockNumHash::new(0, B256::ZERO);
        let a = block(1, anchor.hash, 1);
        let b = block(2, a.recovered_block().hash(), 2);
        let mut cache = MinerOverlayCache::default();
        let first = cache.for_parent(std::slice::from_ref(&a), OverlayManager::default).0;
        let original = overlay(&first, a.recovered_block().hash(), anchor);
        let retry =
            cache.for_parent(std::slice::from_ref(&a), || panic!("retry must reuse manager")).0;
        let retried = overlay(&retry, a.recovered_block().hash(), anchor);
        assert!(Arc::ptr_eq(&original.hashed_post_state, &retried.hashed_post_state));

        let chain = [b.clone(), a.clone()];
        let extended = cache.for_parent(&chain, || panic!("linear extension must reuse manager")).0;
        let result = overlay(&extended, b.recovered_block().hash(), anchor);
        let baseline = overlay(&fresh(&chain), b.recovered_block().hash(), anchor);
        assert_eq!(result.hashed_post_state, baseline.hashed_post_state);
        assert_eq!(result.trie_updates, baseline.trie_updates);
        assert_eq!(root(&result), root(&baseline));
        assert_ne!(root(&result), root(&original));
        // An older task still sees its own parent, even after a new child is inserted.
        assert_eq!(root(&overlay(&first, a.recovered_block().hash(), anchor)), root(&original));
    }

    #[test]
    fn reorg_and_rollback_leave_inflight_parent_intact() {
        let anchor = BlockNumHash::new(0, B256::ZERO);
        let a = block(1, anchor.hash, 1);
        let b = block(2, a.recovered_block().hash(), 2);
        let fork = block(2, a.recovered_block().hash(), 99);
        let mut cache = MinerOverlayCache::default();
        let old = cache.for_parent(&[b.clone(), a.clone()], OverlayManager::default).0;
        let old_root = root(&overlay(&old, b.recovered_block().hash(), anchor));
        let fork_chain = [fork.clone(), a.clone()];
        let new = cache.for_parent(&fork_chain, OverlayManager::default).0;
        let fork_root = root(&overlay(&new, fork.recovered_block().hash(), anchor));
        assert_eq!(
            fork_root,
            root(&overlay(&fresh(&fork_chain), fork.recovered_block().hash(), anchor))
        );
        assert_ne!(fork_root, old_root);
        assert_eq!(root(&overlay(&old, b.recovered_block().hash(), anchor)), old_root);
        let rolled_back = cache.for_parent(std::slice::from_ref(&a), OverlayManager::default).0;
        assert_eq!(
            root(&overlay(&rolled_back, a.recovered_block().hash(), anchor)),
            root(&overlay(&fresh(std::slice::from_ref(&a)), a.recovered_block().hash(), anchor))
        );
        // No factory/receiver cancellation is allowed to delete data used by an older task.
        drop(new);
        drop(cache);
        assert_eq!(root(&overlay(&old, b.recovered_block().hash(), anchor)), old_root);
    }

    #[test]
    fn persistence_frontier_change_rotates_without_pruning_old_task() {
        let anchor = BlockNumHash::new(0, B256::ZERO);
        let a = block(1, anchor.hash, 1);
        let b = block(2, a.recovered_block().hash(), 2);
        let mut cache = MinerOverlayCache::default();
        let old = cache.for_parent(&[b.clone(), a.clone()], OverlayManager::default).0;
        let before = overlay(&old, b.recovered_block().hash(), anchor);
        let next_anchor = BlockNumHash::new(1, a.recovered_block().hash());
        let new = cache.for_parent(std::slice::from_ref(&b), OverlayManager::default).0;
        let after = overlay(&new, b.recovered_block().hash(), next_anchor);
        let baseline =
            overlay(&fresh(std::slice::from_ref(&b)), b.recovered_block().hash(), next_anchor);
        assert_eq!(after.hashed_post_state, baseline.hashed_post_state);
        assert_eq!(root(&after), root(&baseline));
        assert_eq!(root(&overlay(&old, b.recovered_block().hash(), anchor)), root(&before));
        cache.for_parent(&[], OverlayManager::default);
        assert!(cache.current.is_none());
        assert_eq!(root(&overlay(&new, b.recovered_block().hash(), next_anchor)), root(&after));
    }

    #[test]
    fn stalled_persistence_bounds_generation_lifetime() {
        let mut cache = MinerOverlayCache::default();
        let mut chain = Vec::new();
        let mut tip = B256::ZERO;
        let mut created = 0;
        let mut old = None;
        for number in 1..=MAX_PARENTS_PER_GENERATION + 1 {
            let next = block(number as u64, tip, number as u64);
            tip = next.recovered_block().hash();
            chain.insert(0, next);
            let manager = cache
                .for_parent(&chain, || {
                    created += 1;
                    OverlayManager::default()
                })
                .0;
            if number == 1 {
                old = Some(manager);
            }
        }
        assert_eq!(created, 2);
        assert_eq!(cache.current.as_ref().unwrap().parents, 1);
        let first = chain.last().unwrap();
        let anchor = BlockNumHash::new(0, B256::ZERO);
        let retained = overlay(&old.unwrap(), first.recovered_block().hash(), anchor);
        assert_eq!(
            root(&retained),
            root(&overlay(
                &fresh(std::slice::from_ref(first)),
                first.recovered_block().hash(),
                anchor
            ))
        );
    }

    #[test]
    fn skipped_heads_and_changed_database_frontier_match_fresh_overlay() {
        let anchor = BlockNumHash::new(0, B256::ZERO);
        let a = block(1, anchor.hash, 1);
        let b = block(2, a.recovered_block().hash(), 2);
        let c = block(3, b.recovered_block().hash(), 3);
        let mut cache = MinerOverlayCache::default();
        let first = cache.for_parent(std::slice::from_ref(&a), OverlayManager::default).0;
        let before = overlay(&first, a.recovered_block().hash(), anchor);
        let chain = [c.clone(), b.clone(), a.clone()];
        let extended =
            cache.for_parent(&chain, || panic!("skipped heads still extend the chain")).0;
        for frontier in [anchor, BlockNumHash::new(1, a.recovered_block().hash())] {
            // Persistence can advance while the same in-memory snapshot is retained.
            let result = overlay(&extended, c.recovered_block().hash(), frontier);
            let expected = overlay(&fresh(&chain), c.recovered_block().hash(), frontier);
            assert_eq!(result.hashed_post_state, expected.hashed_post_state);
            assert_eq!(root(&result), root(&expected));
        }
        assert_eq!(root(&overlay(&first, a.recovered_block().hash(), anchor)), root(&before));
    }

    #[test]
    fn old_worker_can_finish_after_concurrent_generation_replacement() {
        let anchor = BlockNumHash::new(0, B256::ZERO);
        let a = block(1, anchor.hash, 1);
        let b = block(2, a.recovered_block().hash(), 2);
        let fork = block(2, a.recovered_block().hash(), 99);
        let mut cache = MinerOverlayCache::default();
        let old = cache.for_parent(&[b.clone(), a.clone()], OverlayManager::default).0;
        let expected = root(&overlay(&old, b.recovered_block().hash(), anchor));
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let worker_barrier = barrier.clone();
        let worker = std::thread::spawn(move || {
            worker_barrier.wait();
            root(&overlay(&old, b.recovered_block().hash(), anchor))
        });
        let (new, retired) = cache.for_parent(&[fork, a], OverlayManager::default);
        // Drop both the cache owner's reference and the replacement; the in-flight task owns
        // everything needed for its captured parent, without any cleanup/notification race.
        drop(retired);
        drop(new);
        drop(cache);
        barrier.wait();
        assert_eq!(worker.join().unwrap(), expected);
    }
}
