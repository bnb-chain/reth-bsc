use std::collections::HashMap;
use std::sync::Arc;
use alloy_primitives::B256;
use reth_provider::{ProviderFactory, ProviderError};
use reth_provider::providers::ProviderNodeTypes;
use revm::state::EvmState;
use alloy_primitives::map::B256Map;
use reth_trie::{
    trie_cursor::TrieCursorFactory,
    walker::TrieWalker,
    HashedPostState,
};
use tokio::task::JoinSet;
use tokio::sync::mpsc;
use tracing::{debug, trace};
use thiserror::Error;
use reth::builder::NodeTypesWithDB;
use reth_db::database::Database;
use reth_trie::prefix_set::PrefixSet;
use reth_trie::HashedStorage;
use reth_primitives_traits::Account;

#[derive(Error, Debug)]
pub enum TriePrefetchError {
    #[error("Provider error: {0}")]
    Provider(#[from] ProviderError),
    #[error("Database error: {0}")]
    Database(String),
}

#[derive(Debug, Clone)]
pub struct TriePrefetcher {
    /// Cached accounts.
    cached_accounts: HashMap<B256, bool>, // hashed_address -> is_cached
    /// Cached storages.
    cached_storages: HashMap<B256, HashMap<B256, bool>>, // hashed_address -> hashed_key -> is_cached
}

impl Default for TriePrefetcher {
    fn default() -> Self {
        Self {
            cached_accounts: HashMap::new(),
            cached_storages: HashMap::new(),
        }
    }
}

impl TriePrefetcher {
    pub async fn run<N>(
        &mut self,
        provider_factory: ProviderFactory<N>,
        mut prefetch_rx: mpsc::Receiver<EvmState>,
        mut interrupt_rx: mpsc::Receiver<()>,
    ) where
        N: ProviderNodeTypes + 'static,
        <<N as NodeTypesWithDB>::DB as reth_db::Database>::TXMut: TrieCursorFactory,
    {
        let mut join_set: JoinSet<()> = JoinSet::new();
        
        loop {
            tokio::select! {
                state = prefetch_rx.recv() => {
                    if let Some(state) = state {
                        let provider_factory = provider_factory.clone();
                        let hashed_state = self.deduplicate_and_update_cached(state);

                        let self_clone = Arc::new(self.clone());
                        join_set.spawn(async move {
                            if let Err(e) = self_clone.prefetch_once(provider_factory, hashed_state).await {
                                debug!(target: "trie::trie_prefetch", ?e, "Error while prefetching trie storage");
                            };
                        });
                    }
                }
                _ = interrupt_rx.recv() => {
                    debug!(target: "trie::trie_prefetch", "Interrupted trie prefetch task. Unprocessed tx {:?}", prefetch_rx.len());
                    join_set.abort_all();
                    return;
                }
            }
        }
    }

    /// Deduplicate `hashed_state` based on `cached` and update `cached`.
    fn deduplicate_and_update_cached(&mut self, state: EvmState) -> HashedPostState {
        let mut new_hashed_state = HashedPostState::default();

        // 遍历 EvmState 中的账户
        for (address, account) in &state {
            let hashed_address = B256::from_slice(&address.as_slice());
            let mut has_storage_changes = false;
            
            // 检查存储变更
            let mut new_storage = B256Map::default();
            let cached_entry = self.cached_storages.entry(hashed_address).or_default();
            
            // 遍历账户的存储变更
            for (key, value) in &account.storage {
                let hashed_key = B256::from_slice(&key.as_le_slice());
                if !cached_entry.contains_key(&hashed_key) {
                    cached_entry.insert(hashed_key, true);
                    new_storage.insert(hashed_key, value.present_value());
                    has_storage_changes = true;
                }
            }

            // 如果有存储变更，添加到结果中
            if has_storage_changes {
                let mut hashed_storage = HashedStorage::new(false);
                hashed_storage.storage = new_storage;
                new_hashed_state.storages.insert(hashed_address, hashed_storage);
            }

            // 检查账户是否应该被包含（有存储变更或账户本身未缓存）
            let should_include_account = has_storage_changes || 
                !self.cached_accounts.contains_key(&hashed_address);
            
            if should_include_account {
                self.cached_accounts.insert(hashed_address, true);
                let acc = Account {
                    nonce: account.info.nonce,
                    balance: account.info.balance,
                    bytecode_hash: Some(account.info.code_hash),
                };
                new_hashed_state.accounts.insert(hashed_address, Some(acc));
            }
        }

        new_hashed_state
    }

    /// Prefetch trie storage using Provider interface.
    pub async fn prefetch_once<N>(
        self: Arc<Self>,
        provider_factory: ProviderFactory<N>,
        state: HashedPostState,
    ) -> Result<(), TriePrefetchError>
    where
        N: ProviderNodeTypes,
        <<N as NodeTypesWithDB>::DB as reth_db::Database>::TXMut: TrieCursorFactory,
    {
        trace!(target: "trie::trie_prefetch", "start prefetching trie storages");
        
        // 1. Prefetch storage tries
        self.prefetch_storage_tries(&provider_factory, &state).await?;
        
        // 2. Prefetch account tries
        self.prefetch_account_tries(&provider_factory, &state).await?;
        
        trace!(target: "trie::trie_prefetch", "finished prefetching trie storages");
        Ok(())
    }

    /// Prefetch account tries using trie walker.
    async fn prefetch_account_tries<N>(
        &self,
        provider_factory: &ProviderFactory<N>,
        state: &HashedPostState,
    ) -> Result<(), TriePrefetchError>
    where
        N: ProviderNodeTypes,
        <<N as NodeTypesWithDB>::DB as Database>::TXMut: TrieCursorFactory,
    {
        let provider = provider_factory.provider_rw()?;
        
        // 创建空的 PrefixSet 用于 TrieWalker
        let prefix_set = PrefixSet::default();
        
        // 创建 trie cursor
        let trie_cursor = provider.tx_ref().account_trie_cursor()
            .map_err(|e| TriePrefetchError::Database(e.to_string()))?;
        
        // 使用公共构造函数创建 TrieWalker
        let mut walker = TrieWalker::state_trie(trie_cursor, prefix_set);
        
        let mut branch_count = 0;
        let mut leaf_count = 0;
        
        // 使用 TrieWalker 遍历 trie 节点
        loop {
            // 获取当前节点的 key
            if let Some(key) = walker.key() {
                trace!(target: "trie::trie_prefetch", "current key: {:?}", key);
                
                // 检查是否是叶子节点（有 hash）
                if let Some(hash) = walker.hash() {
                    leaf_count += 1;
                    trace!(target: "trie::trie_prefetch", "prefetched leaf node: {:?}", hash);
                    
                    // 对于叶子节点，也预取其存储
                    if !self.cached_storages.contains_key(&hash) {
                        if let Ok(storage_cursor) = provider.tx_ref().storage_trie_cursor(hash) {
                            let mut storage_walker = TrieWalker::storage_trie(storage_cursor, PrefixSet::default());
                            while let Ok(()) = storage_walker.advance() {
                                if let Some(storage_key) = storage_walker.key() {
                                    trace!(target: "trie::trie_prefetch", "prefetched storage: {:?} -> {:?}", hash, storage_key);
                                }
                            }
                        }
                    }
                } else {
                    branch_count += 1;
                    trace!(target: "trie::trie_prefetch", "prefetched branch node");
                }
            }
            
            // 前进到下一个节点
            match walker.advance() {
                Ok(()) => {
                    // 继续遍历
                }
                Err(_) => {
                    // 遍历完成或出错，退出循环
                    break;
                }
            }
        }
        
        trace!(
            target: "trie::trie_prefetch",
            branches = branch_count,
            leaves = leaf_count,
            "prefetched account trie"
        );

        Ok(())
    }

    /// Prefetch storage tries for all accounts in the state.
    async fn prefetch_storage_tries<N>(
        &self,
        provider_factory: &ProviderFactory<N>,
        state: &HashedPostState,
    ) -> Result<(), TriePrefetchError>
    where
        N: ProviderNodeTypes,
        <<N as NodeTypesWithDB>::DB as Database>::TXMut: TrieCursorFactory,
    {
        let provider = provider_factory.provider_rw()?;
        
        for (address, _) in &state.storages {
            let hashed_address = B256::from_slice(&address.as_slice());
            
            // Skip if already cached
            if self.cached_storages.contains_key(&hashed_address) {
                continue;
            }
            
            // 使用 TrieWalker 访问存储 trie
            match provider.tx_ref().storage_trie_cursor(hashed_address) {
                Ok(storage_cursor) => {
                    let mut walker = TrieWalker::storage_trie(storage_cursor, PrefixSet::default());
                    
                    // 遍历存储 trie
                    while let Ok(()) = walker.advance() {
                        if let Some(key) = walker.key() {
                            trace!(target: "trie::trie_prefetch", "prefetched storage key: {:?}", key);
                        }
                    }
                }
                Err(e) => {
                    debug!(target: "trie::trie_prefetch", ?e, "Failed to access storage trie for address: {:?}", address);
                }
            }
        }
        
        Ok(())
    }
}
        