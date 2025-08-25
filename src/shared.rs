use crate::consensus::parlia::SnapshotProvider;
use std::sync::{Arc, OnceLock};
use alloy_consensus::Header;
use alloy_primitives::B256;
use reth_provider::HeaderProvider;

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
    T: HeaderProvider<Header = Header> + Send + Sync + 'static,
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
    
    Ok(())
}

/// Get the global header by hash provider function
pub fn get_header_by_hash_provider() -> Option<&'static HeaderByHashFn> {
    HEADER_BY_HASH_PROVIDER.get()
}

/// Get the global header by number provider function  
pub fn get_header_by_number_provider() -> Option<&'static HeaderByNumberFn> {
    HEADER_BY_NUMBER_PROVIDER.get()
}

/// Get header by hash from the global header provider
/// Directly calls the stored HeaderProvider::header() function
pub fn get_header_by_hash_from_provider(block_hash: &B256) -> Option<Header> {
    let provider_fn = HEADER_BY_HASH_PROVIDER.get()?;
    provider_fn(block_hash)
}

/// Get header by number from the global header provider
/// Directly calls the stored HeaderProvider::header_by_number() function
pub fn get_header_by_number_from_provider(block_number: u64) -> Option<Header> {
    let provider_fn = HEADER_BY_NUMBER_PROVIDER.get()?;
    provider_fn(block_number)
}

/// Get header by hash - simplified interface
pub fn get_header_by_hash(block_hash: &B256) -> Option<Header> {
    get_header_by_hash_from_provider(block_hash)
}

/// Get header by number - simplified interface
pub fn get_header_by_number(block_number: u64) -> Option<Header> {
    get_header_by_number_from_provider(block_number)
}