//! Bounded, root-checked ownership handoffs between completed miner builds.
use alloy_consensus::Header;
use alloy_primitives::B256;
use reth_chain_state::PreservedSparseTrie;
use reth_engine_tree::tree::state_root_strategy::PayloadBuilderTrieCache;
use reth_primitives_traits::SealedHeader;
use std::{collections::VecDeque, sync::OnceLock};

const MAX_PENDING_TREES: usize = 4;

#[derive(Default)]
pub(crate) struct MinerRootCache {
    entries: VecDeque<(B256, PayloadBuilderTrieCache)>,
}
impl MinerRootCache {
    /// Match both the producing attempt's parent and the accepted block's final state root.
    /// A re-seal may change the block hash without changing its post-state; bind ownership
    /// to the exact sealed parent supplied by this next build, never a guessed header hash.
    pub(crate) fn take(&mut self, parent: &SealedHeader<Header>) -> Option<PreservedSparseTrie> {
        let found = self.entries.iter_mut().enumerate().find_map(|(index, (base, cache))| {
            (*base == parent.parent_hash)
                .then(|| cache.try_take(parent.hash(), parent.state_root))
                .flatten()
                .map(|tree| (index, tree))
        });
        let result = if let Some((index, tree)) = found {
            self.entries.remove(index);
            Some(tree)
        } else {
            None
        };
        metrics::gauge!("bsc.miner_root_cache_pending").set(self.entries.len() as f64);
        metrics::counter!("bsc.miner_root_cache_total", "result" => if result.is_some() { "handoff" } else { "miss" }).increment(1);
        result
    }
    /// Return evictions for destruction on the existing drop worker, outside the cache lock.
    pub(crate) fn insert(
        &mut self,
        parent: B256,
        cache: PayloadBuilderTrieCache,
    ) -> Option<PayloadBuilderTrieCache> {
        let retired =
            (self.entries.len() == MAX_PENDING_TREES).then(|| self.entries.pop_front().unwrap().1);
        self.entries.push_back((parent, cache));
        metrics::gauge!("bsc.miner_root_cache_pending").set(self.entries.len() as f64);
        retired
    }
}
pub(crate) fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("BSC_MINER_SPARSE_REUSE_CANONICAL").is_ok_and(|value| {
            matches!(value.trim().to_ascii_lowercase().as_str(), "1" | "true" | "on")
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{node::primitives::BscPrimitives, BscBlock};
    use alloy_primitives::{Address, U256};
    use reth_engine_tree::tree::{
        state_root_strategy::spawn_payload_builder_state_root_with_cache, TreeConfig,
    };
    use reth_execution_types::BlockExecutionOutput;
    use reth_primitives_traits::RecoveredBlock;
    use reth_provider::{DatabaseProviderROFactory, HashedPostStateProvider, StateRootProvider};
    use reth_storage_overlay::{OverlayManager, OverlayStateProviderFactory};
    use reth_trie_common::SortedTrieData;
    use revm::{
        database::BundleState,
        state::{Account, AccountInfo, AccountStatus, EvmState, EvmStorageSlot, TransactionId},
        OnStateHook,
    };
    use std::{
        sync::Arc,
        time::{Duration, Instant},
    };

    #[test]
    fn private_chain_tree_handoff_matches_serial_roots_across_slot_updates() {
        let factory = reth_provider::test_utils::create_test_provider_factory_with_chain_spec(
            Arc::new(reth_chainspec::ChainSpec::default()),
        );
        let genesis = reth_db_common::init::init_genesis(&factory).unwrap();
        let runtime = reth_tasks::Runtime::test();
        let manager = OverlayManager::<BscPrimitives>::default();
        let mut cache = MinerRootCache::default();
        let mut parent = SealedHeader::new(Header::default(), genesis);
        let mut previous = U256::ZERO;
        let mut blocks = Vec::new();
        let address = Address::with_last_byte(7);
        let config = TreeConfig::default().with_has_enough_parallelism(true);
        for number in 1..=3u64 {
            let next = if number == 3 { U256::ZERO } else { U256::from(number * 11) };
            let info = AccountInfo { nonce: number, balance: U256::from(1), ..Default::default() };
            let bundle = BundleState::builder(number..=number)
                .state_present_account_info(address, info.clone())
                .state_storage(address, [(U256::ZERO, (previous, next))].into_iter().collect())
                .build();
            let factory_for_parent = OverlayStateProviderFactory::new(
                factory.clone(),
                manager.overlay_builder(parent.hash()),
            );
            let provider = factory_for_parent.database_provider_ro().unwrap();
            let hashed = provider.hashed_post_state(&bundle).unwrap();
            let serial = provider.state_root_with_updates(hashed.clone()).unwrap().0;
            drop(provider);
            let preserved = cache.take(&parent);
            assert_eq!(preserved.is_some(), number > 1);
            let preserved = preserved.map(|mut pending| {
                // Only the test waits to force the ready-cache path. Production always falls
                // back without waiting when the previous task is still finalizing.
                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    match pending.try_into_ready() {
                        Ok(ready) => break ready,
                        Err(not_ready) => pending = not_ready,
                    }
                    assert!(Instant::now() < deadline);
                    std::thread::yield_now();
                }
            });
            let (mut handle, pending) =
                spawn_payload_builder_state_root_with_cache::<BscPrimitives, _>(
                    &runtime,
                    factory_for_parent,
                    parent.clone(),
                    Some(1),
                    &config,
                    Some(blocks.iter().rev().cloned().collect()),
                    preserved,
                    |factory, anchor, roots| {
                        factory.with_skip_overlay_for_reused_sparse_trie(anchor, roots)
                    },
                );
            let mut account = Account::default();
            account.info = info;
            account.status = AccountStatus::Touched;
            account.transaction_id = TransactionId::ZERO;
            account.storage.insert(
                U256::ZERO,
                EvmStorageSlot::new_changed(previous, next, TransactionId::ZERO),
            );
            let mut update = EvmState::default();
            update.insert(address, account);
            let mut hook = handle.take_execution_hook();
            hook.on_state(update);
            drop(hook);
            let outcome = handle.state_root().unwrap();
            assert_eq!(outcome.state_root, serial);
            let block = Arc::new(RecoveredBlock::new_unhashed(
                BscBlock {
                    header: Header {
                        number,
                        parent_hash: parent.hash(),
                        state_root: serial,
                        ..Default::default()
                    },
                    body: Default::default(),
                },
                Vec::new(),
            ));
            let next_parent = block.clone_sealed_header();
            assert!(cache.insert(parent.hash(), pending).is_none());
            // Same root on an unrelated parent must never take this cache entry.
            let unrelated = SealedHeader::new(
                Header {
                    parent_hash: B256::with_last_byte(99),
                    state_root: serial,
                    ..Default::default()
                },
                B256::with_last_byte(100),
            );
            assert!(cache.take(&unrelated).is_none());
            let executed = reth_chain_state::ExecutedBlock::new(
                block,
                Arc::new(BlockExecutionOutput { state: bundle, result: Default::default() }),
                SortedTrieData::new(
                    Arc::new(hashed.into_sorted()),
                    Arc::new(outcome.trie_updates.as_ref().clone().into_sorted()),
                ),
            );
            manager.insert_block(executed.clone());
            blocks.push(executed);
            parent = next_parent;
            previous = next;
        }
        for name in ["sparse-trie", "trie-hashing", "storage-workers", "account-workers", "drop"] {
            runtime.spawn_blocking_named(name, || {}).get();
        }
    }
}
