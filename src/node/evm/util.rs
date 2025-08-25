use reth_primitives::Transaction;
use alloy_consensus::{Header, BlockHeader};
use alloy_primitives::B256;
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

    pub fn get_header_by_number(&mut self, block_number: u64) -> Option<Header> {
        if let Some(header) = self.blocknumber_to_header.get(&block_number) {
            tracing::debug!("Get header from cache, block_number: {:?}", header.number());
            return Some(header.clone());
        }
        if let Some(header) = crate::shared::get_header_by_number_from_provider(block_number) {
            tracing::debug!("Get header from provider, block_number: {:?}", header.number());
            return Some(header);
        }

        tracing::warn!("Failed to get header from cache and provider, block_number: {:?}", block_number);
        None
    }

    pub fn get_header_by_hash(&mut self, block_hash: &B256) -> Option<Header> {
        if let Some(header) = self.blockhash_to_header.get(block_hash) {
            return Some(header.clone());
        }
        if let Some(header) = crate::shared::get_header_by_hash_from_provider(block_hash) {
            return Some(header);
        }

        None
    }

    pub fn insert_header_to_cache(&mut self, header: Header) {
        let block_number = header.number();
        let block_hash = header.hash_slow();
        let header_clone_for_log = header.clone();
        self.blocknumber_to_header.insert(block_number, header.clone());
        self.blockhash_to_header.insert(block_hash, header);
        tracing::debug!("Insert header to cache, block_number: {:?}, block_hash: {:?}, header: {:?}", block_number, block_hash, header_clone_for_log);
    }
}

pub static HEADER_CACHE_READER: LazyLock<Mutex<HeaderCacheReader>> = LazyLock::new(|| {
    Mutex::new(HeaderCacheReader::new(100000))
});