use crate::consensus::parlia::SnapshotProvider;
use crate::node::engine_api::payload::BscPayloadTypes;
use crate::node::network::block_import::service::{
    IncomingBidBlock, IncomingBlock, IncomingMinedBlock,
};
use crate::node::network::BscNetworkPrimitives;
use crate::node::primitives::BscBlock;
use alloy_consensus::{BlockHeader, Header};
use alloy_eips::BlockId;
use alloy_primitives::{Bytes, B256, U256};
use alloy_rlp::Encodable;
use alloy_rpc_types::{
    state::StateOverride, Block as RpcBlock, BlockOverrides, Header as RpcHeader,
    Receipt as RpcReceipt, Transaction as RpcTransaction,
    TransactionRequest as RpcTransactionRequest,
};
use parking_lot::Mutex;
use reth::api::NodeTypesWithDBAdapter;
use reth_engine_tree::engine::EngineApiRequest;
use reth_network::NetworkHandle;
use reth_network_api::PeerId;
use reth_provider::providers::BlockchainProvider;
use reth_payload_builder_primitives::Events;
use reth_ethereum_primitives::TransactionSigned;
use reth_provider::{BlockNumReader, HeaderProvider};
use schnellru::{ByLength, LruMap};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::RwLock;
use std::sync::{Arc, OnceLock};
use tokio::sync::broadcast;
use tokio::sync::mpsc::UnboundedSender;

/// Public type alias for the BSC engine API sender (replaces private reth EngineApiTx).
pub type BscEngineApiTx = UnboundedSender<
    EngineApiRequest<
        crate::node::engine_api::payload::BscPayloadTypes,
        crate::BscPrimitives,
        BlockchainProvider<NodeTypesWithDBAdapter<crate::node::BscNode, reth_db::DatabaseEnv>>,
        crate::node::evm::config::BscEvmConfig,
    >,
>;

/// Function type for HeaderProvider::header() access (by hash)
type HeaderByHashFn = Arc<dyn Fn(&B256) -> Option<Header> + Send + Sync>;

/// Function type for HeaderProvider::header_by_number() access (by number)  
type HeaderByNumberFn = Arc<dyn Fn(u64) -> Option<Header> + Send + Sync>;

/// Global shared access to the snapshot provider for RPC
static SNAPSHOT_PROVIDER: OnceLock<Arc<dyn SnapshotProvider + Send + Sync>> = OnceLock::new();

/// Function type for spawning a sparse-trie state-root background task.
///
/// Takes the parent block's hash and state root, returns an opaque handle that the
/// miner can:
///   1. attach as a `state_hook` on the BSC executor (streams per-tx state diffs to the task)
///   2. block on after execution to receive the precomputed `(state_root, trie_updates)`
///
/// Two `B256` parameters:
///   * `parent_hash`: block hash of the parent — used as the `anchor_hash` for the
///     `OverlayStateProviderFactory` so the sparse trie can resolve historical trie
///     nodes via the changeset cache.
///   * `parent_state_root`: state root of the parent — the sparse trie's starting
///     anchor for incremental hashing.
///
/// Registered by the engine launch path when
/// `--mining.use-sparse-trie-state-root` is enabled. Returns `None` if the engine
/// has not been wired (graceful fallback to legacy `state_root_with_updates`).
pub type SparseTrieSpawnFn = Arc<
    dyn Fn(B256, B256) -> Option<reth_engine_tree::tree::multiproof::StateRootHandle>
        + Send
        + Sync,
>;

/// Global sparse-trie state-root spawner. See [`SparseTrieSpawnFn`].
static SPARSE_TRIE_SPAWN_FN: OnceLock<SparseTrieSpawnFn> = OnceLock::new();

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

/// Global sender for submitting a selected (sealed, unexecuted) BEP-675 BidBlock to the import
/// service, which broadcasts it then verifies it on import (zero-simulate).
static BID_BLOCK_IMPORT_SENDER: OnceLock<UnboundedSender<IncomingBidBlock>> = OnceLock::new();

/// Global local peer ID for network identification
static LOCAL_PEER_ID: OnceLock<PeerId> = OnceLock::new();

/// Global queue for bid packages (thread-safe)
static BID_PACKAGE_QUEUE: OnceLock<Arc<Mutex<VecDeque<crate::node::miner::bid_simulator::Bid>>>> =
    OnceLock::new();

/// Global BidBlock builder permission manager (BEP-675). Shared between the `mev_sendBidBlock`
/// admission path (which rejects revoked builders) and the miner, which revokes builders after a
/// failed BidBlock verification.
static BID_BLOCK_PERMISSION_MANAGER: OnceLock<
    Arc<crate::node::miner::bid_block_permission::BidBlockPermissionManager>,
> = OnceLock::new();

/// Global intake queue for admitted BEP-675 BidBlocks. `mev_sendBidBlock` pushes a decoded block
/// here after admission; the miner pops it to pre-seal verify, execute, and select against the
/// local block (mirrors the legacy [`BID_PACKAGE_QUEUE`] SendBid intake).
static BID_BLOCK_QUEUE: OnceLock<
    Arc<Mutex<VecDeque<crate::node::miner::bid_block::DecodedBidBlock>>>,
> = OnceLock::new();

/// Function type for the [`RECENT_MINED_BLOCKS`] cache: `block_number -> parent_hashes already
/// sealed for it`.
type RecentMinedBlocksCache = Arc<Mutex<lru::LruCache<u64, Vec<B256>>>>;

/// LRU cache backing [`check_and_record_mined_block`]. Shared by every block-submission path —
/// local/legacy payload (via `ResultWorkWorker::submit_payload`) and BEP-675 BidBlock (via
/// `BscPayloadJob::try_submit_winning_bid_block`) — so the validator's double-sign guard covers
/// both, not just whichever path happens to hold its own private cache.
static RECENT_MINED_BLOCKS: OnceLock<RecentMinedBlocksCache> = OnceLock::new();

/// Matches go-bsc's mined-block history depth used for the same purpose.
const RECENT_MINED_BLOCKS_CACHE_SIZE: usize = 100;

/// Global network handle to interact with P2P (reth).
static NETWORK_HANDLE: OnceLock<NetworkHandle<BscNetworkPrimitives>> = OnceLock::new();

/// Global payload events broadcast sender
static PAYLOAD_EVENTS_TX: OnceLock<broadcast::Sender<Events<BscPayloadTypes>>> = OnceLock::new();
/// Broadcast channel for notifying about successfully imported block hashes
static IMPORTED_BLOCKS_TX: OnceLock<broadcast::Sender<B256>> = OnceLock::new();

/// Global MEV running status
static MEV_RUNNING: OnceLock<Arc<AtomicBool>> = OnceLock::new();
/// Global builder whitelist shared between miner and mev RPC namespaces
static BUILDER_WHITELIST: OnceLock<
    Arc<RwLock<std::collections::HashSet<alloy_primitives::Address>>>,
> = OnceLock::new();

// ============ Miner Dynamic Config ============
// These allow miner_* RPC methods to update mining parameters at runtime.
// Initialized from MiningConfig at startup; read by the miner workers.

use std::sync::atomic::AtomicU64;

/// Dynamic gas limit (set by miner_setGasLimit)
static MINER_GAS_LIMIT: OnceLock<AtomicU64> = OnceLock::new();
/// Dynamic min gas tip in wei (set by miner_setGasPrice), stored as u64
static MINER_GAS_TIP: OnceLock<AtomicU64> = OnceLock::new();
/// Dynamic etherbase / coinbase address (set by miner_setEtherbase)
static MINER_ETHERBASE: OnceLock<RwLock<alloy_primitives::Address>> = OnceLock::new();
/// Dynamic extra data bytes (set by miner_setExtra)
static MINER_EXTRA: OnceLock<RwLock<alloy_primitives::Bytes>> = OnceLock::new();
/// Dynamic recommit interval in milliseconds (set by miner_setRecommitInterval)
static MINER_RECOMMIT_INTERVAL_MS: OnceLock<AtomicU64> = OnceLock::new();
/// Mining enabled flag (set by miner_start / miner_stop)
static MINING_ENABLED: OnceLock<AtomicBool> = OnceLock::new();

/// Global proxyed peer IDs list
static PROXYED_PEER_IDS: OnceLock<Vec<PeerId>> = OnceLock::new();

/// Set global imported blocks broadcast sender.
pub fn set_imported_blocks_tx(tx: broadcast::Sender<B256>) -> Result<(), broadcast::Sender<B256>> {
    IMPORTED_BLOCKS_TX.set(tx)
}

/// Get global imported blocks broadcast sender if initialized.
pub fn get_imported_blocks_tx() -> Option<&'static broadcast::Sender<B256>> {
    IMPORTED_BLOCKS_TX.get()
}

/// Set global proxyed peer IDs.
pub fn set_proxyed_peer_ids(peer_ids: Vec<PeerId>) -> Result<(), Vec<PeerId>> {
    PROXYED_PEER_IDS.set(peer_ids)
}

/// Get global proxyed peer IDs if initialized.
pub fn get_proxyed_peer_ids() -> Option<&'static Vec<PeerId>> {
    PROXYED_PEER_IDS.get()
}

/// Trait for fork choice engine operations that can be stored globally
pub trait ForkChoiceEngineTrait: Send + Sync {
    fn update_forkchoice<'a>(
        &'a self,
        header: &'a Header,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<(), crate::consensus::ParliaConsensusErr>>
                + Send
                + 'a,
        >,
    >;
    fn is_need_reorg<'a>(
        &'a self,
        incoming_header: &'a Header,
        current_header: &'a Header,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<bool, crate::consensus::ParliaConsensusErr>>
                + Send
                + 'a,
        >,
    >;
}

impl<P> ForkChoiceEngineTrait for crate::node::consensus::BscForkChoiceEngine<P>
where
    P: HeaderProvider<Header = Header> + BlockNumReader + Clone + Send + Sync,
{
    fn update_forkchoice<'a>(
        &'a self,
        header: &'a Header,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<(), crate::consensus::ParliaConsensusErr>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(self.update_forkchoice(header))
    }

    fn is_need_reorg<'a>(
        &'a self,
        incoming_header: &'a Header,
        current_header: &'a Header,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<bool, crate::consensus::ParliaConsensusErr>>
                + Send
                + 'a,
        >,
    > {
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

/// Global blob store shared between the tx pool and the block body serving path.
static GLOBAL_BLOB_STORE: OnceLock<Arc<dyn reth_transaction_pool::blobstore::BlobStore>> =
    OnceLock::new();

/// Register the node-wide blob store so that block-body serving can read blob sidecars.
pub fn set_global_blob_store(
    store: Arc<dyn reth_transaction_pool::blobstore::BlobStore>,
) {
    let _ = GLOBAL_BLOB_STORE.set(store);
}

/// Return a reference to the global blob store, if one has been registered.
pub fn get_global_blob_store(
) -> Option<&'static Arc<dyn reth_transaction_pool::blobstore::BlobStore>> {
    GLOBAL_BLOB_STORE.get()
}

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
pub fn set_snapshot_provider(
    provider: Arc<dyn SnapshotProvider + Send + Sync>,
) -> Result<(), Arc<dyn SnapshotProvider + Send + Sync>> {
    SNAPSHOT_PROVIDER.set(provider)
}

/// Get the global snapshot provider
pub fn get_snapshot_provider() -> Option<&'static Arc<dyn SnapshotProvider + Send + Sync>> {
    SNAPSHOT_PROVIDER.get()
}

/// Register the sparse-trie state-root spawner.
///
/// Should be called once from the engine launch path. Subsequent calls return
/// an error and are ignored (mirrors the rest of the OnceLock setters in this
/// module). When the spawner is not registered, BSC miner falls back to the
/// synchronous `state_root_with_updates` path.
pub fn set_sparse_trie_spawn_fn(
    spawner: SparseTrieSpawnFn,
) -> Result<(), SparseTrieSpawnFn> {
    SPARSE_TRIE_SPAWN_FN.set(spawner)
}

/// Spawn a sparse-trie state-root task for the given parent block.
///
/// `parent_hash` is the parent block hash (used as the trie anchor for the overlay
/// state provider). `parent_state_root` is the parent's state-root commitment.
///
/// Returns `None` when:
///   * the spawner has not been registered (engine wiring incomplete), or
///   * the spawner itself decided not to spawn (e.g. resource pressure).
///
/// On `None`, callers must fall back to the synchronous state-root path.
pub fn spawn_sparse_trie_state_root(
    parent_hash: B256,
    parent_state_root: B256,
) -> Option<reth_engine_tree::tree::multiproof::StateRootHandle> {
    SPARSE_TRIE_SPAWN_FN
        .get()
        .and_then(|f| f(parent_hash, parent_state_root))
}

/// Store the header provider globally
/// Creates functions that directly call HeaderProvider::header() and HeaderProvider::header_by_number()
pub fn set_header_provider<T>(
    provider: Arc<T>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
where
    T: HeaderProvider<Header = Header> + BlockNumReader + Send + Sync + 'static,
{
    // Create function for header by hash
    let provider_clone = provider.clone();
    let header_by_hash_fn = Arc::new(move |block_hash: &B256| -> Option<Header> {
        match provider_clone.header(*block_hash) {
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
    HEADER_BY_NUMBER_PROVIDER
        .set(header_by_number_fn)
        .map_err(|_| "Failed to set number provider")?;

    // Create function for best block number
    let provider_clone3 = provider.clone();
    let best_block_number_fn =
        Arc::new(move || -> Option<u64> { provider_clone3.best_block_number().ok() });
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
pub fn set_block_import_mined_sender(
    sender: UnboundedSender<IncomingMinedBlock>,
) -> Result<(), UnboundedSender<IncomingMinedBlock>> {
    BLOCK_IMPORT_MINED_SENDER.set(sender)
}

/// Get a reference to the global block import sender, if initialized.
pub fn get_block_import_mined_sender() -> Option<&'static UnboundedSender<IncomingMinedBlock>> {
    BLOCK_IMPORT_MINED_SENDER.get()
}

/// Store the block import sender globally. Returns an error if it was set before.
pub fn set_block_import_sender(
    sender: UnboundedSender<IncomingBlock>,
) -> Result<(), UnboundedSender<IncomingBlock>> {
    BLOCK_IMPORT_SENDER.set(sender)
}

/// Get a reference to the global block import sender, if initialized.
pub fn get_block_import_sender() -> Option<&'static UnboundedSender<IncomingBlock>> {
    BLOCK_IMPORT_SENDER.get()
}

/// Store the BidBlock import sender globally. Returns an error if it was set before.
pub fn set_bid_block_import_sender(
    sender: UnboundedSender<IncomingBidBlock>,
) -> Result<(), UnboundedSender<IncomingBidBlock>> {
    BID_BLOCK_IMPORT_SENDER.set(sender)
}

/// Get a reference to the global BidBlock import sender, if initialized.
pub fn get_bid_block_import_sender() -> Option<&'static UnboundedSender<IncomingBidBlock>> {
    BID_BLOCK_IMPORT_SENDER.get()
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
pub fn push_bid_package(
    package: crate::node::miner::bid_simulator::Bid,
) -> Result<(), &'static str> {
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

/// Get the global BidBlock permission manager, initializing it lazily on first access.
pub fn get_bid_block_permission_manager(
) -> Arc<crate::node::miner::bid_block_permission::BidBlockPermissionManager> {
    BID_BLOCK_PERMISSION_MANAGER
        .get_or_init(|| {
            Arc::new(crate::node::miner::bid_block_permission::BidBlockPermissionManager::new())
        })
        .clone()
}

fn bid_block_queue() -> &'static Arc<Mutex<VecDeque<crate::node::miner::bid_block::DecodedBidBlock>>>
{
    BID_BLOCK_QUEUE.get_or_init(|| Arc::new(Mutex::new(VecDeque::new())))
}

fn recent_mined_blocks() -> &'static RecentMinedBlocksCache {
    RECENT_MINED_BLOCKS.get_or_init(|| {
        Arc::new(Mutex::new(lru::LruCache::new(
            std::num::NonZeroUsize::new(RECENT_MINED_BLOCKS_CACHE_SIZE).unwrap(),
        )))
    })
}

/// Double-sign guard shared by every block-submission path. Records `(block_number,
/// parent_hash)` and returns `false` if this exact pair was already recorded — i.e. the validator
/// already sealed a block for `block_number` on top of `parent_hash`, from either the
/// local/legacy path or a BEP-675 BidBlock, and must not sign a second one for the same height.
///
/// Mirrors go-bsc's `resultLoop`, which runs its double-sign check (`recordMinedBlock`) on every
/// sealed block *before* branching on whether it came from the BidBlock path — a validator that
/// signs a local block and a competing BidBlock at the same height on the same parent has
/// equivocated, regardless of which path produced either one.
pub fn check_and_record_mined_block(block_number: u64, parent_hash: B256) -> bool {
    let mut cache = recent_mined_blocks().lock();
    if let Some(prev_parents) = cache.get(&block_number) {
        if prev_parents.contains(&parent_hash) {
            return false;
        }
        let mut updated_parents = prev_parents.clone();
        updated_parents.push(parent_hash);
        cache.put(block_number, updated_parents);
    } else {
        cache.put(block_number, vec![parent_hash]);
    }
    true
}

/// Best-effort rollback for [`check_and_record_mined_block`]: removes `(block_number,
/// parent_hash)` if it was recorded. Only for use when the caller recorded a slot but then failed
/// to actually submit the block (e.g. the import channel was closed) — without this, a genuine
/// fallback submission for the same slot would be wrongly rejected as a double sign for a block
/// that was never actually broadcast.
pub fn forget_recorded_mined_block(block_number: u64, parent_hash: B256) {
    let mut cache = recent_mined_blocks().lock();
    if let Some(prev_parents) = cache.get(&block_number) {
        let retained: Vec<B256> =
            prev_parents.iter().copied().filter(|&h| h != parent_hash).collect();
        cache.put(block_number, retained);
    }
}

/// Push an admitted BidBlock onto the global intake queue for the miner to process.
pub fn push_bid_block_package(decoded: crate::node::miner::bid_block::DecodedBidBlock) {
    bid_block_queue().lock().push_back(decoded);
}

/// Pop the next admitted BidBlock from the global intake queue (FIFO).
pub fn pop_bid_block_package() -> Option<crate::node::miner::bid_block::DecodedBidBlock> {
    bid_block_queue().lock().pop_front()
}

/// Number of admitted BidBlocks waiting in the global intake queue.
pub fn bid_block_queue_len() -> usize {
    bid_block_queue().lock().len()
}

/// Store the reth `NetworkHandle` globally for dynamic peer actions.
pub fn set_network_handle(
    handle: NetworkHandle<BscNetworkPrimitives>,
) -> Result<(), NetworkHandle<BscNetworkPrimitives>> {
    NETWORK_HANDLE.set(handle)
}

/// Get a clone of the global network handle if available.
pub fn get_network_handle() -> Option<NetworkHandle<BscNetworkPrimitives>> {
    NETWORK_HANDLE.get().cloned()
}

/// Set global payload events broadcast sender.
pub fn set_payload_events_tx(
    tx: broadcast::Sender<Events<BscPayloadTypes>>,
) -> Result<(), broadcast::Sender<Events<BscPayloadTypes>>> {
    PAYLOAD_EVENTS_TX.set(tx)
}

/// Get global payload events broadcast sender if initialized.
pub fn get_payload_events_tx() -> Option<&'static broadcast::Sender<Events<BscPayloadTypes>>> {
    PAYLOAD_EVENTS_TX.get()
}

/// Store the fork choice engine globally.
///
/// This stores a `BscForkChoiceEngine` instance to provide global access for fork choice operations.
pub fn set_fork_choice_engine<P>(
    engine: crate::node::consensus::BscForkChoiceEngine<P>,
) -> Result<(), Box<dyn std::error::Error>>
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
pub fn set_full_block_provider(
    provider: Arc<dyn FullBlockProvider + Send + Sync>,
) -> Result<(), Arc<dyn FullBlockProvider + Send + Sync>> {
    FULL_BLOCK_PROVIDER.set(provider)
}

/// Get a clone of the installed [`FullBlockProvider`], if any.
pub fn get_full_block_provider() -> Option<Arc<dyn FullBlockProvider + Send + Sync>> {
    FULL_BLOCK_PROVIDER.get().cloned()
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
    pub fn new(by_hash: ByHash, by_number: ByNumber) -> Self {
        Self { by_hash, by_number }
    }
}

impl<ByHash, ByNumber> FullBlockProvider for ClosureFullBlockProvider<ByHash, ByNumber>
where
    ByHash: Fn(&B256) -> Option<BscBlock> + Send + Sync + 'static,
    ByNumber: Fn(u64) -> Option<BscBlock> + Send + Sync + 'static,
{
    fn block_by_hash(&self, hash: &B256) -> Option<BscBlock> {
        (self.by_hash)(hash)
    }
    fn block_by_number(&self, number: u64) -> Option<BscBlock> {
        (self.by_number)(number)
    }
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

// ============ MEV Running Status ============

/// Set global MEV running status (called by MevWorkWorker on startup)
pub fn set_mev_running(running: Arc<AtomicBool>) -> Result<(), Arc<AtomicBool>> {
    MEV_RUNNING.set(running)
}

/// Get global MEV running status
pub fn is_mev_running() -> bool {
    MEV_RUNNING.get().map(|status| status.load(Ordering::Relaxed)).unwrap_or(false)
}

/// Start MEV - set the global MEV running status to true
pub fn start_mev() {
    if let Some(status) = MEV_RUNNING.get() {
        status.store(true, Ordering::Relaxed);
    }
}

/// Stop MEV - set the global MEV running status to false
pub fn stop_mev() {
    if let Some(status) = MEV_RUNNING.get() {
        status.store(false, Ordering::Relaxed);
    }
}

// ============ Builder Whitelist ============

/// Initialize the global builder whitelist (called once during setup)
pub fn init_builder_whitelist(
    builders: std::collections::HashSet<alloy_primitives::Address>,
) -> Arc<RwLock<std::collections::HashSet<alloy_primitives::Address>>> {
    let whitelist = Arc::new(RwLock::new(builders));
    let _ = BUILDER_WHITELIST.set(whitelist.clone());
    whitelist
}

/// Get the global builder whitelist
pub fn get_builder_whitelist(
) -> Option<&'static Arc<RwLock<std::collections::HashSet<alloy_primitives::Address>>>> {
    BUILDER_WHITELIST.get()
}

/// Add a builder to the global whitelist
pub fn add_builder(builder: alloy_primitives::Address) -> bool {
    if let Some(whitelist) = BUILDER_WHITELIST.get() {
        if let Ok(mut set) = whitelist.write() {
            return set.insert(builder);
        }
    }
    false
}

/// Remove a builder from the global whitelist
pub fn remove_builder(builder: &alloy_primitives::Address) -> bool {
    if let Some(whitelist) = BUILDER_WHITELIST.get() {
        if let Ok(mut set) = whitelist.write() {
            return set.remove(builder);
        }
    }
    false
}

/// Check if a builder is in the global whitelist
pub fn is_builder_allowed(builder: &alloy_primitives::Address) -> bool {
    if let Some(whitelist) = BUILDER_WHITELIST.get() {
        if let Ok(set) = whitelist.read() {
            return set.contains(builder);
        }
    }
    false
}

// ============ Miner Dynamic Config Accessors ============

/// Initialize all miner dynamic config from MiningConfig (called once at miner startup).
/// If not called, getters return the fallback defaults.
pub fn init_miner_dynamic_config(
    gas_limit: u64,
    gas_tip: u64,
    validator_address: alloy_primitives::Address,
) {
    let _ = MINER_GAS_LIMIT.set(AtomicU64::new(gas_limit));
    let _ = MINER_GAS_TIP.set(AtomicU64::new(gas_tip));
    let _ = MINER_ETHERBASE.set(RwLock::new(validator_address));
    let _ = MINER_EXTRA.set(RwLock::new(alloy_primitives::Bytes::new()));
    let _ = MINER_RECOMMIT_INTERVAL_MS.set(AtomicU64::new(0));
    let _ = MINING_ENABLED.set(AtomicBool::new(true));
}

// --- gas limit ---

pub fn set_miner_gas_limit(val: u64) {
    if let Some(v) = MINER_GAS_LIMIT.get() {
        v.store(val, Ordering::Relaxed);
    }
}

pub fn get_miner_gas_limit() -> Option<u64> {
    MINER_GAS_LIMIT.get().map(|v| v.load(Ordering::Relaxed))
}

// --- gas tip (gas price) ---

pub fn set_miner_gas_tip(val: u64) {
    if let Some(v) = MINER_GAS_TIP.get() {
        v.store(val, Ordering::Relaxed);
    }
}

pub fn get_miner_gas_tip() -> Option<u64> {
    MINER_GAS_TIP.get().map(|v| v.load(Ordering::Relaxed))
}

// --- etherbase ---

pub fn set_miner_etherbase(addr: alloy_primitives::Address) {
    if let Some(lock) = MINER_ETHERBASE.get() {
        if let Ok(mut guard) = lock.write() {
            *guard = addr;
        }
    }
}

pub fn get_miner_etherbase() -> Option<alloy_primitives::Address> {
    MINER_ETHERBASE.get().and_then(|lock| lock.read().ok().map(|g| *g))
}

// --- extra data ---

pub fn set_miner_extra(data: alloy_primitives::Bytes) {
    if let Some(lock) = MINER_EXTRA.get() {
        if let Ok(mut guard) = lock.write() {
            *guard = data;
        }
    }
}

pub fn get_miner_extra() -> Option<alloy_primitives::Bytes> {
    MINER_EXTRA.get().and_then(|lock| lock.read().ok().map(|g| g.clone()))
}

// --- recommit interval ---

pub fn set_miner_recommit_interval_ms(val: u64) {
    if let Some(v) = MINER_RECOMMIT_INTERVAL_MS.get() {
        v.store(val, Ordering::Relaxed);
    }
}

pub fn get_miner_recommit_interval_ms() -> Option<u64> {
    MINER_RECOMMIT_INTERVAL_MS.get().map(|v| v.load(Ordering::Relaxed))
}

// --- mining enabled ---

pub fn set_mining_enabled(val: bool) {
    if let Some(v) = MINING_ENABLED.get() {
        v.store(val, Ordering::Relaxed);
    }
}

pub fn is_mining_enabled() -> bool {
    MINING_ENABLED.get().map(|v| v.load(Ordering::Relaxed)).unwrap_or(false)
}

// ============= IPC client ===============
pub static IPC_CLIENT: OnceLock<Arc<jsonrpsee::async_client::Client>> = OnceLock::new();

/// Set the IPC client
pub async fn set_ipc_client(path: String) -> Result<(), eyre::Error> {
    let client = reth_ipc::client::IpcClientBuilder::default()
        .build(&path)
        .await
        .map_err(|e| eyre::eyre!("Failed to build RPC client: {:?}", e))?;
    IPC_CLIENT
        .set(Arc::new(client))
        .map_err(|e| eyre::eyre!("Failed to set RPC client: {:?}", e))?;
    Ok(())
}

/// Get the IPC client
pub fn get_ipc_client() -> Option<Arc<jsonrpsee::async_client::Client>> {
    IPC_CLIENT.get().cloned()
}

/// Call the IPC client to get the result of an Ethereum call
/// This is a wrapper around the reth_rpc_eth_api::EthApiClient::call function
/// It takes a transaction request, a block ID, a state overrides, and a block overrides
/// It returns the result of the call as a Bytes object
pub async fn ipc_eth_call(
    req: RpcTransactionRequest,
    block_id: Option<BlockId>,
    state_overrides: Option<StateOverride>,
    block_overrides: Option<Box<BlockOverrides>>,
) -> Result<Bytes, eyre::Error> {
    let client = get_ipc_client().ok_or(eyre::eyre!("Failed to get RPC client"))?;
    reth_rpc_eth_api::EthApiClient::<
        RpcTransactionRequest,
        RpcTransaction,
        RpcBlock,
        RpcReceipt,
        RpcHeader,
        TransactionSigned,
    >::call(client.as_ref(), req, block_id, state_overrides, block_overrides)
    .await
    .map_err(|e| eyre::eyre!("failed to query chain id from healthy node: {e}"))
}

pub async fn ipc_estimate_gas(
    req: RpcTransactionRequest,
    block_id: Option<BlockId>,
    state_overrides: Option<StateOverride>,
) -> Result<U256, eyre::Error> {
    let client = get_ipc_client().ok_or(eyre::eyre!("Failed to get RPC client"))?;
    reth_rpc_eth_api::EthApiClient::<
        RpcTransactionRequest,
        RpcTransaction,
        RpcBlock,
        RpcReceipt,
        RpcHeader,
        TransactionSigned,
    >::estimate_gas(client.as_ref(), req, block_id, state_overrides)
    .await
    .map_err(|e| eyre::eyre!("failed to query chain id from healthy node: {e}"))
}

pub async fn ipc_send_transaction(req: RpcTransactionRequest) -> Result<B256, eyre::Error> {
    let client = get_ipc_client().ok_or(eyre::eyre!("Failed to get RPC client"))?;
    reth_rpc_eth_api::EthApiClient::<
        RpcTransactionRequest,
        RpcTransaction,
        RpcBlock,
        RpcReceipt,
        RpcHeader,
        TransactionSigned,
    >::send_transaction(client.as_ref(), req)
    .await
    .map_err(|e| eyre::eyre!("failed to query chain id from healthy node: {e}"))
}

/// Send a raw signed transaction via IPC (eth_sendRawTransaction)
pub async fn ipc_send_raw_transaction(tx: TransactionSigned) -> Result<B256, eyre::Error> {
    let client = get_ipc_client().ok_or(eyre::eyre!("Failed to get RPC client"))?;
    let mut buf = Vec::new();
    tx.encode(&mut buf);
    let bytes = Bytes::from(buf);
    reth_rpc_eth_api::EthApiClient::<
        RpcTransactionRequest,
        RpcTransaction,
        RpcBlock,
        RpcReceipt,
        RpcHeader,
        TransactionSigned,
    >::send_raw_transaction(client.as_ref(), bytes)
    .await
    .map_err(|e| eyre::eyre!("failed to query chain id from healthy node: {e}"))
}

/// Global access to the engine's `CanonicalInMemoryState`.
///
/// Published from `main.rs` right after `.launch()` returns (which is where the
/// concrete `BlockchainProvider` is available — generic builder code can't see
/// through `Node::Provider`).
///
/// Used by the sparse-trie state-root spawner to walk the in-memory canonical
/// chain back to the on-disk anchor when constructing the overlay factory,
/// without which proof workers fail with `BlockHashNotFound` whenever the parent
/// block hasn't been persisted to MDBX yet.
static CANONICAL_IN_MEMORY_STATE:
    OnceLock<reth_chain_state::CanonicalInMemoryState<crate::BscPrimitives>> = OnceLock::new();

/// Set the canonical in-memory state handle. Idempotent first-write-wins.
pub fn set_canonical_in_memory_state(
    state: reth_chain_state::CanonicalInMemoryState<crate::BscPrimitives>,
) -> Result<(), reth_chain_state::CanonicalInMemoryState<crate::BscPrimitives>> {
    CANONICAL_IN_MEMORY_STATE.set(state)
}

/// Get a cloned handle to the canonical in-memory state if published.
pub fn get_canonical_in_memory_state(
) -> Option<reth_chain_state::CanonicalInMemoryState<crate::BscPrimitives>> {
    CANONICAL_IN_MEMORY_STATE.get().cloned()
}

/// Global engine api tx (custom request sender)
static ENGINE_API_TX: OnceLock<BscEngineApiTx> = OnceLock::new();

/// Set global engine api tx if present.
pub fn set_engine_api_tx(
    tx: BscEngineApiTx,
) -> Result<(), BscEngineApiTx> {
    ENGINE_API_TX.set(tx)
}

/// Get global consensus engine handle if initialized.
pub fn get_engine_api_tx() -> Option<BscEngineApiTx> {
    ENGINE_API_TX.get().cloned()
}

// Note: difflayers are now returned directly from the BSC block builder (`finish_bsc`) and carried
// through `BscBuiltPayload` / `ExecutedBlockWithTrieUpdates`. We no longer use a global cache.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BscBlockBody;
    use alloy_consensus::Header;

    fn mk_block(num: u64, parent: B256) -> BscBlock {
        let header = Header { parent_hash: parent, number: num, ..Default::default() };
        BscBlock {
            header,
            body: BscBlockBody {
                inner: reth_ethereum_primitives::BlockBody::default(),
                sidecars: None,
            },
        }
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

    // `RECENT_MINED_BLOCKS` is a process-wide global shared across every test in this binary, so
    // each test below uses a block number reserved just for it to avoid cross-test interference.

    #[test]
    fn double_sign_guard_rejects_exact_repeat() {
        let block_number = 900_001;
        let parent = B256::repeat_byte(0xaa);

        // First claim of (block_number, parent) succeeds...
        assert!(check_and_record_mined_block(block_number, parent));
        // ...a second claim of the exact same pair is a double sign and is rejected. This is the
        // scenario fix #3 closes: previously only the local/legacy path recorded into this cache,
        // so a local block and a competing BidBlock at the same height/parent could both be signed.
        assert!(!check_and_record_mined_block(block_number, parent));
    }

    #[test]
    fn double_sign_guard_allows_different_parent_at_same_height() {
        let block_number = 900_002;
        let parent_a = B256::repeat_byte(0xbb);
        let parent_b = B256::repeat_byte(0xcc);

        // A reorg changing the parent at the same height is not equivocation — go-bsc's
        // `recordMinedBlock` tracks parent hashes per height for exactly this reason.
        assert!(check_and_record_mined_block(block_number, parent_a));
        assert!(check_and_record_mined_block(block_number, parent_b));
        // Both are now claimed; repeating either is rejected.
        assert!(!check_and_record_mined_block(block_number, parent_a));
        assert!(!check_and_record_mined_block(block_number, parent_b));
    }

    #[test]
    fn forget_recorded_mined_block_releases_the_slot() {
        let block_number = 900_003;
        let parent = B256::repeat_byte(0xdd);

        assert!(check_and_record_mined_block(block_number, parent));
        // Simulates a BidBlock that claimed the slot but then failed to actually send (e.g. the
        // import channel was closed) — the rollback must let the local-payload fallback claim it.
        forget_recorded_mined_block(block_number, parent);
        assert!(check_and_record_mined_block(block_number, parent));
    }

    #[test]
    fn forget_recorded_mined_block_is_a_noop_for_unknown_height() {
        // Rolling back a height that was never recorded must not panic or corrupt the cache.
        forget_recorded_mined_block(900_004, B256::repeat_byte(0xee));
        assert!(check_and_record_mined_block(900_004, B256::repeat_byte(0xee)));
    }
}
