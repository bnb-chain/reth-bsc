//! Miner-owned parent-state cache. Candidate state is never inserted here.

use crate::BscPrimitives;
use alloy_primitives::B256;
use reth_chain_state::ExecutedBlock;
use reth_storage_overlay::OverlayManager;
use std::{
    collections::{HashSet, VecDeque},
    time::Instant,
};

// Bound distinct (durable anchor, parent) views admitted to each manager, rather than
// clearing it on every persistence frontier change. This is an entry/lifetime bound,
// not a byte limit: a captured chain and in-flight task references can be larger.
const MAX_PARENTS_PER_GENERATION: usize = 32;
const MAX_GENERATIONS: usize = 2;

#[derive(Default)]
pub(crate) struct MinerOverlayCache {
    // Most recently selected manager first. Never prune a manager in place: old proof
    // workers may still depend on its captured chain and durable frontier.
    generations: VecDeque<Generation>,
}

struct Generation {
    manager: OverlayManager<BscPrimitives>,
    tip: B256,
    blocks: usize,
    views: HashSet<(B256, B256)>,
}

impl MinerOverlayCache {
    /// Reuse a manager only when its block graph covers the requested chain, or its
    /// tip is an ancestor in the captured snapshot. The underlying Reth cache still
    /// keys each flattened overlay by (anchor, parent); changing frontiers does not
    /// allow a flattened overlay with a different anchor to be reused as-is.
    /// Return evicted managers for destruction away from the mining thread. Readers
    /// keep their own clones, including after eviction or an empty snapshot.
    pub(crate) fn for_parent(
        &mut self,
        blocks: &[ExecutedBlock<BscPrimitives>],
        new_manager: impl FnOnce() -> OverlayManager<BscPrimitives>,
    ) -> (OverlayManager<BscPrimitives>, Vec<OverlayManager<BscPrimitives>>) {
        let started = Instant::now();
        let (Some(tip), Some(oldest)) = (blocks.first(), blocks.last()) else {
            let retired: Vec<_> = self.generations.drain(..).map(|g| g.manager).collect();
            metrics::counter!("bsc_miner_overlay_cache_evictions_total", "reason" => "empty_snapshot")
                .increment(retired.len() as u64);
            self.record("empty", started, 0);
            return (new_manager(), retired);
        };
        let tip = tip.recovered_block().hash();
        let anchor = oldest.recovered_block().parent_hash;
        let key = (anchor, tip);
        let mut capacity_seen = false;
        let mut selected = None;
        for (index, generation) in self.generations.iter().enumerate() {
            let covered = generation.manager.contains_hash(tip, anchor, anchor);
            let added = if covered {
                Some(0)
            } else {
                blocks
                    .iter()
                    .position(|b| b.recovered_block().hash() == generation.tip)
                    .or_else(|| (generation.tip == anchor).then_some(blocks.len()))
            };
            let Some(added) = added else { continue };
            if !generation.views.contains(&key) &&
                generation.views.len() == MAX_PARENTS_PER_GENERATION
            {
                capacity_seen = true;
                continue;
            }
            selected = Some((index, added));
            break;
        }
        if let Some((index, added)) = selected {
            let mut generation =
                self.generations.remove(index).expect("selected generation exists");
            for block in blocks[..added].iter().rev() {
                generation.manager.insert_block(block.clone());
            }
            if added > 0 {
                generation.blocks += added;
                generation.tip = tip;
            }
            let repeated = !generation.views.insert(key);
            let manager = generation.manager.clone();
            self.generations.push_front(generation);
            self.record(
                if index > 0 {
                    "retained"
                } else if repeated {
                    "repeat"
                } else if added > 0 {
                    "extend"
                } else {
                    "covered"
                },
                started,
                added,
            );
            return (manager, Vec::new());
        }

        let manager = new_manager();
        for block in blocks.iter().rev() {
            manager.insert_block(block.clone());
        }
        let reason = if self.generations.is_empty() {
            "cold"
        } else if capacity_seen {
            "capacity"
        } else {
            "chain_miss"
        };
        metrics::counter!("bsc_miner_overlay_cache_generations_total", "reason" => reason)
            .increment(1);
        self.generations.push_front(Generation {
            manager: manager.clone(),
            tip,
            blocks: blocks.len(),
            views: HashSet::from([key]),
        });
        let retired = if self.generations.len() > MAX_GENERATIONS {
            metrics::counter!("bsc_miner_overlay_cache_evictions_total", "reason" => "capacity")
                .increment(1);
            vec![self.generations.pop_back().expect("over capacity").manager]
        } else {
            Vec::new()
        };
        self.record("new", started, blocks.len());
        (manager, retired)
    }

    fn record(&self, result: &'static str, started: Instant, inserted: usize) {
        // Selections are manager reuse, NOT flattened-overlay hits. Views bound admissions;
        // block references may overlap across generations, and neither gauge measures bytes
        // or references held by in-flight tasks after eviction. No parent hashes as labels.
        metrics::counter!("bsc_miner_overlay_cache_selections_total", "result" => result)
            .increment(1);
        metrics::gauge!("bsc_miner_overlay_cache_generations").set(self.generations.len() as f64);
        metrics::gauge!("bsc_miner_overlay_cache_views")
            .set(self.generations.iter().map(|g| g.views.len()).sum::<usize>() as f64);
        metrics::gauge!("bsc_miner_overlay_cache_block_references")
            .set(self.generations.iter().map(|g| g.blocks).sum::<usize>() as f64);
        metrics::histogram!("bsc_miner_overlay_cache_inserted_blocks").record(inserted as f64);
        // Includes the caller's cache lock hold work, but not time acquiring that lock,
        // proof computation, waiting for a flattened overlay, or asynchronous retirement.
        metrics::histogram!("bsc_miner_overlay_cache_selection_seconds")
            .record(started.elapsed().as_secs_f64());
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
    use reth_storage_overlay::StateTrieOverlay;
    use reth_trie_common::{HashedPostState, HashedStorage, SortedTrieData};
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
            SortedTrieData::new(Arc::new(state), Default::default()),
        )
    }

    fn overlay(
        manager: &OverlayManager<BscPrimitives>,
        tip: B256,
        anchor: BlockNumHash,
    ) -> StateTrieOverlay {
        let factory = create_test_provider_factory();
        manager
            .overlay_builder(tip)
            .build_state_trie_overlay_at_frontiers(
                &factory.provider().unwrap(),
                anchor,
                anchor,
                true,
            )
            .unwrap()
    }

    fn root(overlay: &StateTrieOverlay) -> B256 {
        let factory = create_test_provider_factory();
        LatestStateProvider::new(factory.provider().unwrap())
            .state_root(HashedPostState {
                accounts: overlay.input().state.accounts.iter().copied().collect(),
                storages: overlay
                    .input()
                    .state
                    .storages
                    .iter()
                    .map(|(address, storage)| {
                        (
                            *address,
                            HashedStorage {
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

    /// Execution reads and root input for an unpublished parent must agree with the
    /// same parent admitted to a managed graph, without writing canonical DB state.
    #[test]
    fn private_parent_overlay_matches_managed_parent_without_canonical_writes() {
        use alloy_primitives::{keccak256, Address, U256};
        use reth_execution_types::BlockExecutionOutput;
        use reth_provider::{AccountReader, DatabaseProviderROFactory, StateProvider};
        use reth_storage_overlay::OverlayStateProviderFactory;
        use revm::{database::BundleState, state::AccountInfo};

        let factory = reth_provider::test_utils::create_test_provider_factory_with_chain_spec(
            Arc::new(reth_chainspec::ChainSpec::default()),
        );
        let genesis = reth_db_common::init::init_genesis(&factory).unwrap();
        let address = Address::with_last_byte(7);
        let slot = U256::from(9);
        let state = BundleState::builder(1..=1)
            .state_present_account_info(address, AccountInfo { nonce: 4, ..Default::default() })
            .state_storage(address, [(slot, (U256::ZERO, U256::from(77)))].into_iter().collect())
            .build();
        let hashed = HashedPostState::default()
            .with_accounts([(keccak256(address), Some(Account { nonce: 4, ..Default::default() }))])
            .with_storages([(
                keccak256(address),
                HashedStorage::from_iter([(keccak256(B256::from(slot)), U256::from(77))]),
            )]);
        let parent: ExecutedBlock<BscPrimitives> = ExecutedBlock::new(
            Arc::new(RecoveredBlock::new_unhashed(
                BscBlock {
                    header: Header { number: 1, parent_hash: genesis, ..Default::default() },
                    body: Default::default(),
                },
                Vec::new(),
            )),
            Arc::new(BlockExecutionOutput { state, result: Default::default() }),
            SortedTrieData::new(Arc::new(hashed.into_sorted()), Default::default()),
        );
        let private_manager = OverlayManager::default();
        let private = private_manager.overlay_builder(genesis).with_appended_block(parent.clone());
        let managed = OverlayManager::default();
        managed.insert_block(parent.clone());
        let ordinary = managed.overlay_builder(parent.recovered_block().hash());
        let db = factory.provider().unwrap();
        let private_reads = OverlayStateProviderFactory::new(factory.clone(), private.clone())
            .database_provider_ro()
            .unwrap();
        let ordinary_reads = OverlayStateProviderFactory::new(factory.clone(), ordinary.clone())
            .database_provider_ro()
            .unwrap();
        assert_eq!(private_reads.basic_account(&address).unwrap().unwrap().nonce, 4);
        assert_eq!(private_reads.storage(address, B256::from(slot)).unwrap(), Some(U256::from(77)));
        assert_eq!(
            private_reads.basic_account(&address).unwrap(),
            ordinary_reads.basic_account(&address).unwrap()
        );
        assert_eq!(
            private_reads.storage(address, B256::from(slot)).unwrap(),
            ordinary_reads.storage(address, B256::from(slot)).unwrap()
        );
        let anchor = BlockNumHash::new(0, genesis);
        let private_root =
            private.build_state_trie_overlay_at_frontiers(&db, anchor, anchor, true).unwrap();
        let ordinary_root =
            ordinary.build_state_trie_overlay_at_frontiers(&db, anchor, anchor, true).unwrap();
        assert_eq!(root(&private_root), root(&ordinary_root));
        assert!(!private_manager.contains_hash(parent.recovered_block().hash(), genesis, genesis));
        assert!(LatestStateProvider::new(factory.provider().unwrap())
            .basic_account(&address)
            .unwrap()
            .is_none());
    }

    /// An empty candidate's system writes and a normal candidate's user writes share
    /// parent overlays, but must produce independent roots even when running together.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn empty_and_normal_root_tasks_share_only_parent_overlay() {
        use crate::node::miner::proof_provider::{
            MinerProofOverlayCache, MinerProofProviderFactory,
        };
        use alloy_primitives::{keccak256, Address, U256};
        use reth_engine_tree::tree::{
            state_root_strategy::spawn_payload_builder_state_root, TreeConfig,
        };
        use reth_evm::OnStateHook;
        use reth_tasks::{RayonConfig, RuntimeBuilder, RuntimeConfig, TokioConfig};
        use revm::state::{AccountInfo, AccountStatus, EvmState, EvmStorageSlot, TransactionId};

        let factory = reth_provider::test_utils::create_test_provider_factory_with_chain_spec(
            Arc::new(reth_chainspec::ChainSpec::default()),
        );
        let genesis_hash = reth_db_common::init::init_genesis(&factory).unwrap();
        let anchor = BlockNumHash::new(0, genesis_hash);
        let address = Address::with_last_byte(7);
        let key = keccak256(address);
        let initial = HashedPostState::default()
            .with_accounts([(key, Some(Account { nonce: 1, ..Default::default() }))])
            .with_storages([(
                key,
                HashedStorage::from_iter([(keccak256(B256::ZERO), U256::from(1))]),
            )]);
        let serial_root = |state| {
            LatestStateProvider::new(factory.provider().unwrap()).state_root(state).unwrap()
        };
        let parent_root = serial_root(initial.clone());
        let parent = ExecutedBlock::new(
            Arc::new(RecoveredBlock::new_unhashed(
                BscBlock {
                    header: Header {
                        number: 1,
                        parent_hash: genesis_hash,
                        state_root: parent_root,
                        ..Default::default()
                    },
                    body: Default::default(),
                },
                Vec::new(),
            )),
            Default::default(),
            SortedTrieData::new(Arc::new(initial.clone().into_sorted()), Default::default()),
        );
        let runtime = RuntimeBuilder::new(
            RuntimeConfig::default()
                .with_tokio(TokioConfig::existing_handle(tokio::runtime::Handle::current()))
                .with_rayon(RayonConfig {
                    cpu_threads: Some(2),
                    proof_storage_worker_threads: Some(2),
                    proof_account_worker_threads: Some(2),
                    state_trie_overlay_worker_threads: Some(2),
                    ..Default::default()
                }),
        )
        .with_worker_namespace("empty-root-test")
        .build()
        .unwrap();
        let empty_runtime = RuntimeBuilder::new(
            RuntimeConfig::default()
                .with_tokio(TokioConfig::existing_handle(tokio::runtime::Handle::current()))
                .with_rayon(RayonConfig {
                    cpu_threads: Some(1),
                    proof_storage_worker_threads: Some(1),
                    proof_account_worker_threads: Some(1),
                    ..Default::default()
                }),
        )
        .with_worker_namespace("empty-fallback-test")
        .build()
        .unwrap();
        let mut cache = MinerOverlayCache::default();
        let (normal_manager, _) = cache.for_parent(std::slice::from_ref(&parent), || {
            OverlayManager::new(runtime.state_trie_overlay_worker_pool())
        });
        let parent_hash = parent.recovered_block().hash();
        let first = normal_manager
            .overlay_builder(parent_hash)
            .build_state_trie_overlay_at_frontiers(
                &factory.provider().unwrap(),
                anchor,
                anchor,
                true,
            )
            .unwrap();
        let (empty_manager, _) = cache.for_parent(std::slice::from_ref(&parent), || {
            panic!("empty fallback must reuse the parent's manager")
        });
        let second = empty_manager
            .overlay_builder(parent_hash)
            .build_state_trie_overlay_at_frontiers(
                &factory.provider().unwrap(),
                anchor,
                anchor,
                true,
            )
            .unwrap();
        assert!(Arc::ptr_eq(&first.input().state, &second.input().state));
        let proof_cache = Arc::new(MinerProofOverlayCache::default());
        let spawn = |manager: &OverlayManager<BscPrimitives>, executor, fallback| {
            spawn_payload_builder_state_root(
                executor,
                manager,
                MinerProofProviderFactory::new(
                    factory.clone(),
                    manager.overlay_builder(parent_hash),
                    parent_hash,
                    proof_cache.clone(),
                    fallback,
                ),
                parent.recovered_block().sealed_header().clone(),
                None,
                &TreeConfig::default(),
                None,
            )
        };
        let mut normal = spawn(&normal_manager, &runtime, false);
        let mut empty = spawn(&empty_manager, &empty_runtime, true);
        let update = |nonce: u64| {
            let mut account = revm::state::Account::default();
            account.info = AccountInfo { nonce, ..Default::default() };
            account.status = AccountStatus::Touched;
            account.storage.insert(
                U256::ZERO,
                EvmStorageSlot::new_changed(U256::from(1), U256::from(nonce), TransactionId::ZERO),
            );
            EvmState::from_iter([(address, account)])
        };
        let mut normal_hook = normal.take_execution_hook();
        let mut empty_hook = empty.take_execution_hook();
        normal_hook.on_state(update(3));
        empty_hook.on_state(update(2));
        drop(empty_hook);
        let expected = |nonce| {
            serial_root(
                initial
                    .clone()
                    .with_accounts([(key, Some(Account { nonce, ..Default::default() }))])
                    .with_storages([(
                        key,
                        HashedStorage::from_iter([(keccak256(B256::ZERO), U256::from(nonce))]),
                    )]),
            )
        };
        // The normal stream remains open while the fallback completes: sharing its
        // named root/hash/proof workers would deadlock this bounded receive.
        let empty_root = empty
            .take_state_root_rx()
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap()
            .unwrap()
            .state_root;
        drop(empty);
        drop(normal_hook);
        let normal_root = normal
            .take_state_root_rx()
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap()
            .unwrap()
            .state_root;
        drop(normal);
        assert_eq!(normal_root, expected(3));
        assert_eq!(empty_root, expected(2));
        assert_ne!(normal_root, empty_root);
        // Neither candidate may poison the reusable parent overlay.
        let after = empty_manager
            .overlay_builder(parent_hash)
            .build_state_trie_overlay_at_frontiers(
                &factory.provider().unwrap(),
                anchor,
                anchor,
                true,
            )
            .unwrap();
        assert_eq!(after.input().state, first.input().state);

        // A cancelled normal producer must release its sessions so the fallback can finish.
        let mut cancelled = spawn(&normal_manager, &runtime, false);
        let cancelled_hook = cancelled.take_execution_hook();
        let mut fallback = spawn(&empty_manager, &empty_runtime, true);
        let mut hook = fallback.take_execution_hook();
        hook.on_state(update(2));
        drop(hook);
        drop(cancelled_hook);
        drop(cancelled);
        let outcome = fallback
            .take_state_root_rx()
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap()
            .unwrap();
        drop(fallback);
        tokio::task::block_in_place(|| {
            for executor in [&runtime, &empty_runtime] {
                for name in ["sparse-trie", "trie-hashing", "account-workers", "storage-workers"] {
                    executor.spawn_blocking_named(name, || {}).get();
                }
            }
        });
        assert_eq!(outcome.state_root, expected(2));
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
        assert!(Arc::ptr_eq(&original.input().state, &retried.input().state));

        let chain = [b.clone(), a.clone()];
        let extended = cache.for_parent(&chain, || panic!("linear extension must reuse manager")).0;
        let result = overlay(&extended, b.recovered_block().hash(), anchor);
        let baseline = overlay(&fresh(&chain), b.recovered_block().hash(), anchor);
        assert_eq!(result.input().state, baseline.input().state);
        assert_eq!(result.input().nodes, baseline.input().nodes);
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
        assert_eq!(after.input().state, baseline.input().state);
        assert_eq!(root(&after), root(&baseline));
        assert_eq!(root(&overlay(&old, b.recovered_block().hash(), anchor)), root(&before));
        cache.for_parent(&[], OverlayManager::default);
        assert!(cache.generations.is_empty());
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
        assert_eq!(cache.generations.front().unwrap().views.len(), 1);
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
            assert_eq!(result.input().state, expected.input().state);
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
    #[test]
    fn frontier_progress_reuses_manager_but_not_wrong_anchor_overlay() {
        let anchor = BlockNumHash::new(0, B256::ZERO);
        let a = block(1, anchor.hash, 1);
        let b = block(2, a.recovered_block().hash(), 2);
        let c = block(3, b.recovered_block().hash(), 3);
        let mut cache = MinerOverlayCache::default();
        let first = cache.for_parent(&[b.clone(), a.clone()], OverlayManager::default).0;
        let old = overlay(&first, b.recovered_block().hash(), anchor);
        let frontier = BlockNumHash::new(1, a.recovered_block().hash());
        let progressed = cache
            .for_parent(&[c.clone(), b.clone()], || {
                panic!("frontier must not retire a covering manager")
            })
            .0;
        let actual = overlay(&progressed, c.recovered_block().hash(), frontier);
        let expected =
            overlay(&fresh(&[c.clone(), b.clone()]), c.recovered_block().hash(), frontier);
        assert_eq!(actual.input().state, expected.input().state);
        assert_eq!(actual.input().nodes, expected.input().nodes);
        assert_eq!(root(&actual), root(&expected));
        let again =
            cache.for_parent(&[b.clone(), a.clone()], || panic!("older snapshot is retained")).0;
        let retried = overlay(&again, b.recovered_block().hash(), anchor);
        assert!(Arc::ptr_eq(&old.input().state, &retried.input().state));
        assert_eq!(cache.generations.len(), 1);
    }

    #[test]
    fn returning_to_retained_fork_reuses_completed_overlay() {
        let anchor = BlockNumHash::new(0, B256::ZERO);
        let a = block(1, anchor.hash, 1);
        let b = block(2, a.recovered_block().hash(), 2);
        let fork = block(2, a.recovered_block().hash(), 99);
        let mut cache = MinerOverlayCache::default();
        let old = cache.for_parent(&[b.clone(), a.clone()], OverlayManager::default).0;
        let original = overlay(&old, b.recovered_block().hash(), anchor);
        cache.for_parent(&[fork.clone(), a.clone()], OverlayManager::default);
        let reused =
            cache.for_parent(&[b.clone(), a.clone()], || panic!("retained fork must be found")).0;
        let retried = overlay(&reused, b.recovered_block().hash(), anchor);
        assert!(Arc::ptr_eq(&original.input().state, &retried.input().state));
        assert_eq!(cache.generations.len(), 2);
    }

    #[test]
    fn eviction_bounds_owners_without_invalidating_old_readers() {
        let anchor = BlockNumHash::new(0, B256::ZERO);
        let a = block(1, anchor.hash, 1);
        let mut cache = MinerOverlayCache::default();
        let old = cache.for_parent(std::slice::from_ref(&a), OverlayManager::default).0;
        let before = overlay(&old, a.recovered_block().hash(), anchor);
        let mut evicted = 0;
        for nonce in 2..=5 {
            let fork = block(1, anchor.hash, nonce);
            let (_, retired) = cache.for_parent(&[fork], OverlayManager::default);
            evicted += retired.len();
            drop(retired);
            assert!(cache.generations.len() <= MAX_GENERATIONS);
        }
        assert_eq!(evicted, 3);
        assert_eq!(root(&overlay(&old, a.recovered_block().hash(), anchor)), root(&before));
    }
}
