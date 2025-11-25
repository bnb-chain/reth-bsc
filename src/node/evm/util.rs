use alloy_consensus::{BlockHeader, Header};
use alloy_eips::BlockId;
use alloy_primitives::{B256, BlockHash, BlockNumber, Bytes};
use alloy_rpc_types::{
    Block as RpcBlock, BlockOverrides, Header as RpcHeader, Receipt as RpcReceipt, Transaction as RpcTransaction, TransactionRequest as RpcTransactionRequest, state::StateOverride
};
use reth_primitives::Transaction;
use schnellru::{ByLength, LruMap};
use std::sync::{LazyLock, Mutex, OnceLock};

pub fn set_nonce(transaction: Transaction, nonce: u64) -> Transaction {
    match transaction {
        Transaction::Legacy(mut tx) => {
            tx.nonce = nonce;
            Transaction::Legacy(tx)
        }
        Transaction::Eip2930(mut tx) => {
            tx.nonce = nonce;
            Transaction::Eip2930(tx)
        }
        Transaction::Eip1559(mut tx) => {
            tx.nonce = nonce;
            Transaction::Eip1559(tx)
        }
        Transaction::Eip4844(mut tx) => {
            tx.nonce = nonce;
            Transaction::Eip4844(tx)
        }
        Transaction::Eip7702(mut tx) => {
            tx.nonce = nonce;
            Transaction::Eip7702(tx)
        }
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
            tracing::trace!("Get header from cache, block_number: {:?}", header.number());
            return Some(header.clone());
        }
        if let Some(header) =
            crate::shared::get_canonical_header_by_number_from_provider(block_number)
        {
            tracing::trace!("Get header from provider, block_number: {:?}", header.number());
            return Some(header);
        }

        tracing::warn!(
            "Failed to get header from cache and provider, block_number: {:?}",
            block_number
        );
        None
    }

    pub fn get_header_by_hash(&mut self, block_hash: &B256) -> Option<Header> {
        if let Some(header) = self.blockhash_to_header.get(block_hash) {
            return Some(header.clone());
        }
        if let Some(header) = crate::shared::get_canonical_header_by_hash_from_provider(block_hash)
        {
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
        tracing::trace!(
            "Insert header to cache, block_number: {:?}, block_hash: {:?}, header: {:?}",
            block_number,
            block_hash,
            header_clone_for_log
        );
    }
}

pub static HEADER_CACHE_READER: LazyLock<Mutex<HeaderCacheReader>> =
    LazyLock::new(|| Mutex::new(HeaderCacheReader::new(100000)));

/// Get header by hash from the global header provider
pub fn get_header_by_hash_from_cache(block_hash: &BlockHash) -> Option<Header> {
    let header = HEADER_CACHE_READER.lock().unwrap().get_header_by_hash(block_hash);
    tracing::trace!(
        "Succeed to fetch header by hash, is_none: {} for hash {}",
        header.is_none(),
        block_hash
    );
    header
}

/// Get canonical header by number from the global header provider
pub fn get_cannonical_header_from_cache(number: BlockNumber) -> Option<Header> {
    let header = HEADER_CACHE_READER.lock().unwrap().get_header_by_number(number);
    tracing::debug!(
        "Succeed to fetch canonical header by number, is_none: {} for number {}",
        header.is_none(),
        number
    );
    header
}

/// Insert header to cache
pub fn insert_header_to_cache(header: Header) {
    HEADER_CACHE_READER.lock().unwrap().insert_header_to_cache(header);
}

pub static RPC_CLIENT: OnceLock<jsonrpsee::http_client::HttpClient> = OnceLock::new();

pub fn set_rpc_client(url: String) -> Result<(), eyre::Error> {
    let client = jsonrpsee::http_client::HttpClientBuilder::default().build(url).
        map_err(|e| eyre::eyre!("Failed to build RPC client: {:?}", e))?;
    RPC_CLIENT.set(client).map_err(|e| eyre::eyre!("Failed to set RPC client: {:?}", e))?;
    Ok(())
}

pub fn get_rpc_client() -> Option<jsonrpsee::http_client::HttpClient> {
    RPC_CLIENT.get().cloned()
}

pub async fn rpc_eth_call(
    req: RpcTransactionRequest,
    block_id: Option<BlockId>,
    state_overrides: Option<StateOverride>,
    block_overrides: Option<Box<BlockOverrides>>,
) -> Result<Bytes, eyre::Error> {
    let client = get_rpc_client().ok_or(eyre::eyre!("Failed to get RPC client"))?;
    Ok(reth_rpc_eth_api::EthApiClient::<
        RpcTransactionRequest,
        RpcTransaction,
        RpcBlock,
        RpcReceipt,
        RpcHeader,
    >::call(&client, req, block_id, state_overrides, block_overrides)
    .await
    .map_err(|e| eyre::eyre!("failed to query chain id from healthy node: {e}"))?)
}
