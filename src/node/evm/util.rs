use reth_ethereum_primitives::Transaction;
use alloy_consensus::{Header, BlockHeader};
use alloy_primitives::{B256, BlockHash, BlockNumber};
use schnellru::{ByLength, LruMap};
use std::sync::{LazyLock, Mutex};

pub fn set_nonce(transaction: Transaction, nonce: u64) -> Transaction {
    match transaction {
        Transaction::Legacy(mut tx) => {
            tx.nonce = nonce;
            Transaction::Legacy(tx)
        },
        Transaction::Eip2930(mut tx) => {
            tx.nonce = nonce;
            Transaction::Eip2930(tx)
        },
        Transaction::Eip1559(mut tx) => {
            tx.nonce = nonce;
            Transaction::Eip1559(tx)
        },
        Transaction::Eip4844(mut tx) => {
            tx.nonce = nonce;
            Transaction::Eip4844(tx)
        },
        Transaction::Eip7702(mut tx) => {
            tx.nonce = nonce;
            Transaction::Eip7702(tx)
        },
    }
}

// HeaderReader add a cache layer on the provider.
#[derive(Debug)]
pub struct HeaderCacheReader {
    pub blocknumber_to_header: LruMap<u64, Header, ByLength>,
    pub blockhash_to_header: LruMap<B256, Header, ByLength>,
}

impl HeaderCacheReader {
    pub fn new(cache_size: u32) -> Self {
        Self {
            blocknumber_to_header: LruMap::new(ByLength::new(cache_size)),
            blockhash_to_header: LruMap::new(ByLength::new(cache_size)),
        }
    }

    /// Cache-only lookup. The provider fallback lives in the free functions below, so the
    /// mutex is released before any database read.
    pub fn cached_header_by_number(&mut self, block_number: u64) -> Option<Header> {
        self.blocknumber_to_header.get(&block_number).cloned()
    }

    /// Cache-only lookup; see [`Self::cached_header_by_number`].
    pub fn cached_header_by_hash(&mut self, block_hash: &B256) -> Option<Header> {
        self.blockhash_to_header.get(block_hash).cloned()
    }

    pub fn insert_header_to_cache(&mut self, header: Header) {
        self.insert_header_to_cache_with_hash(header, None);
    }

    pub fn insert_header_to_cache_with_hash(&mut self, header: Header, block_hash: Option<BlockHash>) {
        let block_number = header.number();
        let block_hash = block_hash.unwrap_or_else(|| header.hash_slow());
        let header_clone_for_log = header.clone();
        self.blocknumber_to_header.insert(block_number, header.clone());
        self.blockhash_to_header.insert(block_hash, header);
        tracing::trace!("Insert header to cache, block_number: {:?}, block_hash: {:?}, header: {:?}", block_number, block_hash, header_clone_for_log);
    }
}

pub static HEADER_CACHE_READER: LazyLock<Mutex<HeaderCacheReader>> = LazyLock::new(|| {
    Mutex::new(HeaderCacheReader::new(100000))
});

/// Lock the header cache, recovering from a poisoned mutex.
///
/// The cache is a plain LRU: a panic while it was held leaves it structurally intact, and
/// killing every later header lookup because some unrelated task panicked is worse than
/// reusing it.
fn lock_header_cache() -> std::sync::MutexGuard<'static, HeaderCacheReader> {
    HEADER_CACHE_READER.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Get header by hash from the global header provider
pub fn get_header_by_hash_from_cache(block_hash: &BlockHash) -> Option<Header> {
    // Cache first, and drop the guard before falling back to the provider: the fallback is a
    // real database read, and holding the global header lock across it stalls every other
    // header lookup in the node.
    if let Some(header) = lock_header_cache().cached_header_by_hash(block_hash) {
        return Some(header);
    }
    let header = crate::shared::get_canonical_header_by_hash_from_provider(block_hash);
    tracing::trace!("Succeed to fetch header by hash, is_none: {} for hash {}", header.is_none(), block_hash);
    header
}

/// Get canonical header by number from the global header provider
pub fn get_cannonical_header_from_cache(number: BlockNumber) -> Option<Header> {
    if let Some(header) = lock_header_cache().cached_header_by_number(number) {
        return Some(header);
    }
    let header = crate::shared::get_canonical_header_by_number_from_provider(number);
    if header.is_none() {
        tracing::warn!("Failed to get header from cache and provider, block_number: {:?}", number);
    }
    tracing::debug!("Succeed to fetch canonical header by number, is_none: {} for number {}", header.is_none(), number);
    header
}

/// Insert header to cache
pub fn insert_header_to_cache(header: Header) {
    lock_header_cache().insert_header_to_cache(header);
}

/// Insert header with a known hash to avoid re-hashing.
pub fn insert_header_to_cache_with_hash(header: Header, block_hash: Option<BlockHash>) {
    lock_header_cache().insert_header_to_cache_with_hash(header, block_hash);
}
