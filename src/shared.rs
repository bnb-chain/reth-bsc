use crate::consensus::parlia::SnapshotProvider;
use std::sync::{Arc, OnceLock};
use alloy_consensus::Header;
use alloy_primitives::B256;
use reth_provider::{HeaderProvider, BlockNumReader};
use crate::node::network::BscNetworkPrimitives;
use reth_network::NetworkHandle;
use crate::node::network::block_import::service::IncomingBlock;
use tokio::sync::mpsc::UnboundedSender;
use reth_network_api::PeerId;

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
static BLOCK_IMPORT_SENDER: OnceLock<UnboundedSender<IncomingBlock>> = OnceLock::new();

/// Global local peer ID for network identification
static LOCAL_PEER_ID: OnceLock<PeerId> = OnceLock::new();

/// Global network handle to interact with P2P (reth).
static NETWORK_HANDLE: OnceLock<NetworkHandle<BscNetworkPrimitives>> = OnceLock::new();

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

/// Store the reth `NetworkHandle` globally for dynamic peer actions.
pub fn set_network_handle(handle: NetworkHandle<BscNetworkPrimitives>) -> Result<(), NetworkHandle<BscNetworkPrimitives>> {
    NETWORK_HANDLE.set(handle)
}

/// Get a clone of the global network handle if available.
pub fn get_network_handle() -> Option<NetworkHandle<BscNetworkPrimitives>> {
    NETWORK_HANDLE.get().cloned()
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