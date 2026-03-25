use alloy_primitives::{Address, B256, U256};
use reth_engine_tree::tree::ExecutionCache;
use revm::{bytecode::Bytecode, state::AccountInfo, DatabaseRef};

/// A [`DatabaseRef`] that optionally uses the engine's [`ExecutionCache`] as an intermediate
/// caching layer between [`reth_revm::cached::CachedReads`] and the underlying database.
///
/// When `cache` is `Some`, reads are attempted from the moka caches first; on a miss (or when
/// `cache` is `None`) the request is forwarded to `inner`.
///
/// Because the moka caches are `Arc`-backed, cloning an [`ExecutionCache`] is O(1) (reference
/// count bump only).
///
/// DB stack: `CachedReads → ExecutionCacheDb → StateProviderDatabase(MDBX)`
#[derive(Debug)]
pub struct ExecutionCacheDb<DB> {
    /// Snapshot of the engine's execution cache for the parent block, if available.
    pub cache: Option<ExecutionCache>,
    /// The underlying database, typically `StateProviderDatabase`.
    pub inner: DB,
}

impl<DB: DatabaseRef> DatabaseRef for ExecutionCacheDb<DB> {
    type Error = DB::Error;

    /// Returns account info from the engine cache (if present) or falls through to `inner`.
    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        if let Some(cache) = &self.cache {
            // cache.account_cache stores Option<Account>:
            //   None outer  → cache miss → fall through
            //   Some(None)  → account confirmed non-existent
            //   Some(Some)  → account exists
            if let Some(maybe_account) = cache.account_cache.get(&address) {
                return Ok(maybe_account.map(Into::into));
            }
        }
        self.inner.basic_ref(address)
    }

    /// Returns bytecode from the engine cache (if present) or falls through to `inner`.
    fn code_by_hash_ref(&self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        if let Some(cache) = &self.cache {
            // code_cache stores Option<reth_primitives_traits::Bytecode>:
            //   None outer  → cache miss → fall through
            //   Some(None)  → no bytecode for this hash
            //   Some(Some)  → bytecode found; .0 unwraps to revm Bytecode
            if let Some(maybe_bytecode) = cache.code_cache.get(&code_hash) {
                return Ok(maybe_bytecode.map(|b| b.0).unwrap_or_default());
            }
        }
        self.inner.code_by_hash_ref(code_hash)
    }

    /// Returns a storage slot value from the engine cache (if present) or falls through to `inner`.
    fn storage_ref(&self, address: Address, index: U256) -> Result<U256, Self::Error> {
        if let Some(cache) = &self.cache {
            // StorageKey is B256 (big-endian bytes of U256 slot index).
            let key = B256::new(index.to_be_bytes());
            if let Some(acct_storage) = cache.storage_cache.get(&address) {
                // acct_storage.slots: Cache<StorageKey, Option<StorageValue>>
                //   None outer  → slot not cached → fall through
                //   Some(None)  → slot confirmed zero/empty
                //   Some(Some)  → slot has a value
                match acct_storage.slots.get(&key) {
                    Some(Some(v)) => return Ok(v),
                    Some(None) => return Ok(U256::ZERO),
                    None => {}
                }
            }
        }
        self.inner.storage_ref(address, index)
    }

    /// Block hash is not cached in the engine cache; always delegate.
    fn block_hash_ref(&self, number: u64) -> Result<B256, Self::Error> {
        self.inner.block_hash_ref(number)
    }
}
