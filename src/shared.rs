use crate::consensus::parlia::SnapshotProvider;
use std::sync::{Arc, OnceLock};
use alloy_consensus::{Header, BlockHeader};
use alloy_primitives::B256;
use reth_provider::{HeaderProvider, BlockNumReader};
use crate::node::network::BscNetworkPrimitives;
use reth_network::NetworkHandle;
use crate::node::network::block_import::service::{IncomingBlock, IncomingMinedBlock};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::broadcast;
use reth_network_api::PeerId;
use parking_lot::Mutex;
use std::collections::VecDeque;
use reth_payload_builder_primitives::Events;
use crate::node::engine_api::payload::BscPayloadTypes;
use crate::node::primitives::BscBlock;
use std::sync::RwLock;
use schnellru::{LruMap, ByLength};

/// Function type for HeaderProvider::header() access (by hash)
type HeaderByHashFn = Arc<dyn Fn(&B256) -> Option<Header> + Send + Sync>;

/// Function type for HeaderProvider::header_by_number() access (by number)  
type HeaderByNumberFn = Arc<dyn Fn(u64) -> Option<Header> + Send + Sync>;

/// Global shared access to the snapshot provider for RPC
static SNAPSHOT_PROVIDER: OnceLock<Arc<dyn SnapshotProvider + Send + Sync>> = OnceLock::new();

/// Global header provider function - HeaderProvider::header() by hash
static HEADER_BY_HASH_PROVIDER: OnceLock<HeaderByHashFn> = OnceLock::new();

/// Global header provider function - HeaderProvider::header_by_number() by number  
static HEADER_BY_NUMBER_PROVIDER: OnceLock<HeaderByNumberFn> = OnceLock::new();

/// Function type for BlockNumReader::best_block_number()
type BestBlockNumberFn = Arc<dyn Fn() -> Option<u64> + Send + Sync>;

/// Global best block number function
static BEST_BLOCK_NUMBER_PROVIDER: OnceLock<BestBlockNumberFn> = OnceLock::new();

/// Function type for best total difficulty (u128 approximation)
type BestTdFn = Arc<dyn Fn() -> Option<u128> + Send + Sync>;

/// Global best total difficulty provider
static BEST_TD_PROVIDER: OnceLock<BestTdFn> = OnceLock::new();

/// Global sender for submitting mined blocks to the import service
static BLOCK_IMPORT_MINED_SENDER: OnceLock<UnboundedSender<IncomingMinedBlock>> = OnceLock::new();

/// Global sender for submitting built payload to the import service
static BLOCK_IMPORT_SENDER: OnceLock<UnboundedSender<IncomingBlock>> = OnceLock::new();

/// Global local peer ID for network identification
static LOCAL_PEER_ID: OnceLock<PeerId> = OnceLock::new();

/// Global queue for bid packages (thread-safe)
static BID_PACKAGE_QUEUE: OnceLock<Arc<Mutex<VecDeque<crate::node::miner::bid_simulator::Bid>>>> = OnceLock::new();

/// Global network handle to interact with P2P (reth).
static NETWORK_HANDLE: OnceLock<NetworkHandle<BscNetworkPrimitives>> = OnceLock::new();

/// Global payload events broadcast sender
static PAYLOAD_EVENTS_TX: OnceLock<broadcast::Sender<Events<BscPayloadTypes>>> = OnceLock::new();
/// Broadcast channel for notifying about successfully imported block hashes
static IMPORTED_BLOCKS_TX: OnceLock<broadcast::Sender<B256>> = OnceLock::new();

/// Set global imported blocks broadcast sender.
pub fn set_imported_blocks_tx(tx: broadcast::Sender<B256>) -> Result<(), broadcast::Sender<B256>> {
    IMPORTED_BLOCKS_TX.set(tx)
}

/// Get global imported blocks broadcast sender if initialized.
pub fn get_imported_blocks_tx() -> Option<&'static broadcast::Sender<B256>> {
    IMPORTED_BLOCKS_TX.get()
}

/// Trait for fork choice engine operations that can be stored globally
pub trait ForkChoiceEngineTrait: Send + Sync {
    fn update_forkchoice<'a>(&'a self, header: &'a Header) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), crate::consensus::ParliaConsensusErr>> + Send + 'a>>;
    fn is_need_reorg<'a>(&'a self, incoming_header: &'a Header, current_header: &'a Header) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool, crate::consensus::ParliaConsensusErr>> + Send + 'a>>;
}

impl<P> ForkChoiceEngineTrait for crate::node::consensus::BscForkChoiceEngine<P>
where
    P: HeaderProvider<Header = Header> + BlockNumReader + Clone + Send + Sync,
{
    fn update_forkchoice<'a>(&'a self, header: &'a Header) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), crate::consensus::ParliaConsensusErr>> + Send + 'a>> {
        Box::pin(self.update_forkchoice(header))
    }
    
    fn is_need_reorg<'a>(&'a self, incoming_header: &'a Header, current_header: &'a Header) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool, crate::consensus::ParliaConsensusErr>> + Send + 'a>> {
        Box::pin(self.is_need_reorg(incoming_header, current_header))
    }
}

/// Global fork choice engine instance
static FORK_CHOICE_ENGINE: OnceLock<Box<dyn ForkChoiceEngineTrait>> = OnceLock::new();

/// Trait for full block access (header + body + sidecars)
pub trait FullBlockProvider: Send + Sync {
    fn block_by_hash(&self, hash: &B256) -> Option<BscBlock>;
    fn block_by_number(&self, number: u64) -> Option<BscBlock>;
}

/// Global full block provider instance
static FULL_BLOCK_PROVIDER: OnceLock<Arc<dyn FullBlockProvider + Send + Sync>> = OnceLock::new();

/// In-memory cache for recently seen full blocks (hash -> block), and number -> hash mapping.
/// This allows answering range requests with full bodies if they were recently imported.
static BODY_CACHE: OnceLock<RwLock<BodyCache>> = OnceLock::new();

/// Max number of full blocks to store in the in-memory body cache
const BODY_CACHE_CAPACITY: usize = 512;

struct BodyCache {
    by_hash: LruMap<B256, BscBlock, ByLength>,
    by_number: LruMap<u64, B256, ByLength>,
}

impl Default for BodyCache {
    fn default() -> Self {
        Self {
            by_hash: LruMap::new(ByLength::new(BODY_CACHE_CAPACITY.try_into().unwrap())),
            by_number: LruMap::new(ByLength::new(BODY_CACHE_CAPACITY.try_into().unwrap())),
        }
    }
}

/// Store the snapshot provider globally
pub fn set_snapshot_provider(provider: Arc<dyn SnapshotProvider + Send + Sync>) -> Result<(), Arc<dyn SnapshotProvider + Send + Sync>> {
    SNAPSHOT_PROVIDER.set(provider)
}

/// Get the global snapshot provider
pub fn get_snapshot_provider() -> Option<&'static Arc<dyn SnapshotProvider + Send + Sync>> {
    SNAPSHOT_PROVIDER.get()
}

/// Store the header provider globally
/// Creates functions that directly call HeaderProvider::header() and HeaderProvider::header_by_number()
pub fn set_header_provider<T>(provider: Arc<T>) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
where
    T: HeaderProvider<Header = Header> + BlockNumReader + Send + Sync + 'static,
{
    // Create function for header by hash
    let provider_clone = provider.clone();
    let header_by_hash_fn = Arc::new(move |block_hash: &B256| -> Option<Header> {
        match provider_clone.header(block_hash) {
            Ok(Some(header)) => Some(header),
            _ => None,
        }
    });
    
    // Create function for header by number
    let provider_clone2 = provider.clone();
    let header_by_number_fn = Arc::new(move |block_number: u64| -> Option<Header> {
        match provider_clone2.header_by_number(block_number) {
            Ok(Some(header)) => Some(header),
            _ => None,
        }
    });
    
    // Set both functions
    HEADER_BY_HASH_PROVIDER.set(header_by_hash_fn).map_err(|_| "Failed to set hash provider")?;
    HEADER_BY_NUMBER_PROVIDER.set(header_by_number_fn).map_err(|_| "Failed to set number provider")?;

    // Create function for best block number
    let provider_clone3 = provider.clone();
    let best_block_number_fn = Arc::new(move || -> Option<u64> { provider_clone3.best_block_number().ok() });
    BEST_BLOCK_NUMBER_PROVIDER
        .set(best_block_number_fn)
        .map_err(|_| "Failed to set best block number provider")?;

    // Create function for best total difficulty (u128 approximation)
    let provider_clone4 = provider.clone();
    let best_td_fn = Arc::new(move || -> Option<u128> {
        match provider_clone4.best_block_number() {
            Ok(n) => match provider_clone4.header_td_by_number(n) {
                Ok(Some(td)) => {
                    // Convert to u128; safe approximation for small deltas (thresholds are small)
                    Some(td.to::<u128>())
                }
                _ => None,
            },
            _ => None,
        }
    });
    BEST_TD_PROVIDER.set(best_td_fn).map_err(|_| "Failed to set best td provider")?;

    Ok(())
}

/// Get header by hash from the global header provider
/// Directly calls the stored HeaderProvider::header() function
pub fn get_canonical_header_by_hash_from_provider(block_hash: &B256) -> Option<Header> {
    let provider_fn = HEADER_BY_HASH_PROVIDER.get()?;
    provider_fn(block_hash)
}

/// Get header by number from the global header provider
/// Directly calls the stored HeaderProvider::header_by_number() function
pub fn get_canonical_header_by_number_from_provider(block_number: u64) -> Option<Header> {
    let provider_fn = HEADER_BY_NUMBER_PROVIDER.get()?;
    provider_fn(block_number)
}

/// Get header by hash - simplified interface
pub fn get_canonical_header_by_hash(block_hash: &B256) -> Option<Header> {
    get_canonical_header_by_hash_from_provider(block_hash)
}

/// Get header by number - simplified interface
pub fn get_canonical_header_by_number(block_number: u64) -> Option<Header> {
    get_canonical_header_by_number_from_provider(block_number)
}

/// Get the best block number from the global provider if initialized
pub fn get_best_canonical_block_number() -> Option<u64> {
    BEST_BLOCK_NUMBER_PROVIDER.get().and_then(|f| f())
}

/// Get the best total difficulty (u128 approximation) if available
pub fn get_best_canonical_td() -> Option<u128> {
    BEST_TD_PROVIDER.get().and_then(|f| f())
}

/// Store the block import sender globally. Returns an error if it was set before.
pub fn set_block_import_mined_sender(sender: UnboundedSender<IncomingMinedBlock>) -> Result<(), UnboundedSender<IncomingMinedBlock>> {
    BLOCK_IMPORT_MINED_SENDER.set(sender)
}

/// Get a reference to the global block import sender, if initialized.
pub fn get_block_import_mined_sender() -> Option<&'static UnboundedSender<IncomingMinedBlock>> {
    BLOCK_IMPORT_MINED_SENDER.get()
}

/// Store the block import sender globally. Returns an error if it was set before.
pub fn set_block_import_sender(sender: UnboundedSender<IncomingBlock>) -> Result<(), UnboundedSender<IncomingBlock>> {
    BLOCK_IMPORT_SENDER.set(sender)
}

/// Get a reference to the global block import sender, if initialized.
pub fn get_block_import_sender() -> Option<&'static UnboundedSender<IncomingBlock>> {
    BLOCK_IMPORT_SENDER.get()
}

/// Store the local peer ID globally. Returns an error if it was set before.
pub fn set_local_peer_id(peer_id: PeerId) -> Result<(), PeerId> {
    LOCAL_PEER_ID.set(peer_id)
}

/// Get the global local peer ID, or return a default PeerId if not set.
pub fn get_local_peer_id_or_default() -> PeerId {
    LOCAL_PEER_ID.get().cloned().unwrap_or_default()
}

/// Initialize the bid package queue (should be called once at startup)
pub fn init_bid_package_queue() {
    let _ = BID_PACKAGE_QUEUE.set(Arc::new(Mutex::new(VecDeque::new())));
}

/// Push a bid package to the global queue
pub fn push_bid_package(package: crate::node::miner::bid_simulator::Bid) -> Result<(), &'static str> {
    if let Some(queue) = BID_PACKAGE_QUEUE.get() {
        queue.lock().push_back(package);
        Ok(())
    } else {
        Err("Bid package queue not initialized")
    }
}

/// Pop a bid package from the global queueBid
pub fn pop_bid_package() -> Option<crate::node::miner::bid_simulator::Bid> {
    BID_PACKAGE_QUEUE.get().and_then(|queue| queue.lock().pop_front())
}

/// Get the count of pending bid packages in the queue
pub fn bid_package_queue_len() -> usize {
    BID_PACKAGE_QUEUE.get().map(|queue| queue.lock().len()).unwrap_or(0)
}

/// Store the reth `NetworkHandle` globally for dynamic peer actions.
pub fn set_network_handle(handle: NetworkHandle<BscNetworkPrimitives>) -> Result<(), NetworkHandle<BscNetworkPrimitives>> {
    NETWORK_HANDLE.set(handle)
}

/// Get a clone of the global network handle if available.
pub fn get_network_handle() -> Option<NetworkHandle<BscNetworkPrimitives>> {
    NETWORK_HANDLE.get().cloned()
}

/// Set global payload events broadcast sender.
pub fn set_payload_events_tx(tx: broadcast::Sender<Events<BscPayloadTypes>>) -> Result<(), broadcast::Sender<Events<BscPayloadTypes>>> {
    PAYLOAD_EVENTS_TX.set(tx)
}

/// Get global payload events broadcast sender if initialized.
pub fn get_payload_events_tx() -> Option<&'static broadcast::Sender<Events<BscPayloadTypes>>> {
    PAYLOAD_EVENTS_TX.get()
}

/// Store the fork choice engine globally.
/// 
/// This stores a `BscForkChoiceEngine` instance to provide global access for fork choice operations.
pub fn set_fork_choice_engine<P>(engine: crate::node::consensus::BscForkChoiceEngine<P>) 
    -> Result<(), Box<dyn std::error::Error>>
where
    P: HeaderProvider<Header = Header> + BlockNumReader + Clone + Send + Sync + 'static,
{
    let boxed: Box<dyn ForkChoiceEngineTrait> = Box::new(engine);
    FORK_CHOICE_ENGINE.set(boxed).map_err(|_| "Failed to set fork choice engine")?;
    Ok(())
}

/// Get a reference to the global fork choice engine.
pub fn get_fork_choice_engine() -> Option<&'static dyn ForkChoiceEngineTrait> {
    FORK_CHOICE_ENGINE.get().map(|b| &**b)
}

/// Set the global full block provider
pub fn set_full_block_provider(provider: Arc<dyn FullBlockProvider + Send + Sync>) -> Result<(), Arc<dyn FullBlockProvider + Send + Sync>> {
    FULL_BLOCK_PROVIDER.set(provider)
}

/// Try to get a full block by hash from the global provider
pub fn get_full_block_by_hash(hash: &B256) -> Option<BscBlock> {
    FULL_BLOCK_PROVIDER.get().and_then(|p| p.block_by_hash(hash))
}

/// Try to get a full block by number from the global provider
pub fn get_full_block_by_number(number: u64) -> Option<BscBlock> {
    FULL_BLOCK_PROVIDER.get().and_then(|p| p.block_by_number(number))
}

/// A closure-based full block provider for easy integration.
pub struct ClosureFullBlockProvider<ByHash, ByNumber>
where
    ByHash: Fn(&B256) -> Option<BscBlock> + Send + Sync + 'static,
    ByNumber: Fn(u64) -> Option<BscBlock> + Send + Sync + 'static,
{
    by_hash: ByHash,
    by_number: ByNumber,
}

impl<ByHash, ByNumber> ClosureFullBlockProvider<ByHash, ByNumber>
where
    ByHash: Fn(&B256) -> Option<BscBlock> + Send + Sync + 'static,
    ByNumber: Fn(u64) -> Option<BscBlock> + Send + Sync + 'static,
{
    pub fn new(by_hash: ByHash, by_number: ByNumber) -> Self { Self { by_hash, by_number } }
}

impl<ByHash, ByNumber> FullBlockProvider for ClosureFullBlockProvider<ByHash, ByNumber>
where
    ByHash: Fn(&B256) -> Option<BscBlock> + Send + Sync + 'static,
    ByNumber: Fn(u64) -> Option<BscBlock> + Send + Sync + 'static,
{
    fn block_by_hash(&self, hash: &B256) -> Option<BscBlock> { (self.by_hash)(hash) }
    fn block_by_number(&self, number: u64) -> Option<BscBlock> { (self.by_number)(number) }
}

/// Helper to install a closure-based full block provider.
pub fn set_full_block_provider_from_closures<ByHash, ByNumber>(
    by_hash: ByHash,
    by_number: ByNumber,
) -> Result<(), Arc<dyn FullBlockProvider + Send + Sync>>
where
    ByHash: Fn(&B256) -> Option<BscBlock> + Send + Sync + 'static,
    ByNumber: Fn(u64) -> Option<BscBlock> + Send + Sync + 'static,
{
    set_full_block_provider(Arc::new(ClosureFullBlockProvider::new(by_hash, by_number)))
}

/// Inserts a full block into the in-memory body cache.
pub fn cache_full_block(block: BscBlock) {
    let cache = BODY_CACHE.get_or_init(|| RwLock::new(BodyCache::default()));
    if let Ok(mut guard) = cache.write() {
        let hash = block.header.hash_slow();
        let number = block.header.number();
        guard.by_number.insert(number, hash);
        guard.by_hash.insert(hash, block);
    }
}

/// Fetch a full block from the in-memory body cache by hash.
pub fn get_cached_block_by_hash(hash: &B256) -> Option<BscBlock> {
    let cache = BODY_CACHE.get_or_init(|| RwLock::new(BodyCache::default()));
    if let Ok(mut guard) = cache.write() {
        if let Some(block) = guard.by_hash.get(hash) {
            return Some(block.clone());
        }
    }
    None
}

/// Fetch a full block from the in-memory body cache by number.
pub fn get_cached_block_by_number(number: u64) -> Option<BscBlock> {
    let cache = BODY_CACHE.get_or_init(|| RwLock::new(BodyCache::default()));
    if let Ok(mut guard) = cache.write() {
        if let Some(h_ref) = guard.by_number.get(&number) {
            let h = *h_ref;
            if let Some(block) = guard.by_hash.get(&h) {
                return Some(block.clone());
            }
        }
    }
    None
}

/// Clear the body cache (primarily for testing)
#[cfg(test)]
pub fn clear_body_cache() {
    let cache = BODY_CACHE.get_or_init(|| RwLock::new(BodyCache::default()));
    if let Ok(mut guard) = cache.write() {
        *guard = BodyCache::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::Header;
    use crate::BscBlockBody;

    fn mk_block(num: u64, parent: B256) -> BscBlock {
        let header = Header { parent_hash: parent, number: num, ..Default::default() };
        BscBlock { header, body: BscBlockBody { inner: reth_ethereum_primitives::BlockBody::default(), sidecars: None } }
    }

    #[test]
    fn test_body_cache_put_and_get() {
        let genesis = mk_block(0, B256::ZERO);
        let ghash = genesis.header.hash_slow();
        cache_full_block(genesis.clone());
        assert_eq!(get_cached_block_by_hash(&ghash).unwrap().header.hash_slow(), ghash);
        assert_eq!(get_cached_block_by_number(0).unwrap().header.hash_slow(), ghash);
    }

    // Note: eviction behavior depends on access patterns; an exhaustive eviction
    // test would be flaky here without introspecting the LRU. The cache is covered
    // by basic put/get tests above.
}
