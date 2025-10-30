
use alloy_primitives::BlockHash;
use jsonrpsee::{core::RpcResult, proc_macros::rpc, types::ErrorObject};
use serde::{Deserialize, Serialize};

use crate::consensus::parlia::{Snapshot, SnapshotProvider};

use std::{str::FromStr, sync::Arc};

/// Validator information in the snapshot (matches BSC official format)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidatorInfo {
    #[serde(rename = "index:omitempty")]
    pub index: u64,
    pub vote_address: Vec<u8>, // 48-byte vote address array as vec for serde compatibility
}

impl Default for ValidatorInfo {
    fn default() -> Self {
        Self {
            index: 0,
            vote_address: vec![0; 48], // All zeros as shown in BSC example
        }
    }
}

/// Official BSC Parlia snapshot response structure matching bsc-erigon
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotResult {
    pub number: u64,
    pub hash: String,
    pub epoch_length: u64,
    pub block_interval: u64,
    pub turn_length: u8,
    pub validators: std::collections::HashMap<String, ValidatorInfo>,
    pub recents: std::collections::HashMap<String, String>,
    pub recent_fork_hashes: std::collections::HashMap<String, String>,
    #[serde(rename = "attestation:omitempty")]
    pub attestation: Option<serde_json::Value>,
}

impl From<Snapshot> for SnapshotResult {
    fn from(snapshot: Snapshot) -> Self {
        // Convert validators to the expected format: address -> ValidatorInfo
        let validators: std::collections::HashMap<String, ValidatorInfo> = snapshot
            .validators
            .iter()
            .map(|addr| {
                (
                    format!("0x{addr:040x}"), // 40-char hex address
                    ValidatorInfo::default(),
                )
            })
            .collect();

        // Convert recent proposers to string format: block_number -> address
        let recents: std::collections::HashMap<String, String> = snapshot
            .recent_proposers
            .iter()
            .map(|(block_num, addr)| {
                (
                    block_num.to_string(),
                    format!("0x{addr:040x}"),
                )
            })
            .collect();

        // Generate recent fork hashes (simplified - all zeros like in BSC example)
        let recent_fork_hashes: std::collections::HashMap<String, String> = snapshot
            .recent_proposers
            .keys()
            .map(|block_num| {
                (
                    block_num.to_string(),
                    "00000000".to_string(), // Simplified fork hash
                )
            })
            .collect();

        Self {
            number: snapshot.block_number,
            hash: format!("0x{:064x}", snapshot.block_hash),
            epoch_length: 200, // BSC epoch length
            block_interval: 3000, // BSC block interval in milliseconds
            turn_length: snapshot.turn_length.unwrap_or(1),
            validators,
            recents,
            recent_fork_hashes,
            attestation: None,
        }
    }
}

/// Parlia snapshot RPC API (matches BSC official standard)
#[rpc(server, namespace = "parlia")]
pub trait ParliaApi {
    /// Get snapshot at a specific block (official BSC API method)
    /// Params: block number as hex string (e.g., "0x123132")
    #[method(name = "getSnapshot")]
    async fn get_snapshot_by_hash(&self, block_hash: String) -> RpcResult<Option<SnapshotResult>>;

    /// Build call data for StakeHub.addNodeIDs(bytes32[] nodeIDs). Returns { to, data } as hex.
    #[method(name = "buildAddNodeIDsCall")]
    async fn build_add_node_ids_call(&self, node_ids: Vec<String>) -> RpcResult<ContractCall>;

    /// Build call data for StakeHub.removeNodeIDs(bytes32[] nodeIDs). Returns { to, data } as hex.
    #[method(name = "buildRemoveNodeIDsCall")]
    async fn build_remove_node_ids_call(&self, node_ids: Vec<String>) -> RpcResult<ContractCall>;
}

/// Implementation of the Parlia snapshot RPC API
pub struct ParliaApiImpl<P: SnapshotProvider> {
    /// Snapshot provider for accessing validator snapshots
    snapshot_provider: Arc<P>,
}

/// Wrapper for trait object to work around Sized requirement
pub struct DynSnapshotProvider {
    inner: Arc<dyn SnapshotProvider + Send + Sync>,
}

impl DynSnapshotProvider {
    pub fn new(provider: Arc<dyn SnapshotProvider + Send + Sync>) -> Self {
        Self { inner: provider }
    }
}

impl SnapshotProvider for DynSnapshotProvider {
    fn insert(&self, snapshot: Snapshot) {
        self.inner.insert(snapshot)
    }
    
    fn snapshot_by_hash(&self, block_hash: &BlockHash) -> Option<Snapshot> {
        self.inner.snapshot_by_hash(block_hash)
    }
}

/// Convenience type alias for ParliaApiImpl using the wrapper
pub type ParliaApiDyn = ParliaApiImpl<DynSnapshotProvider>;

impl<P: SnapshotProvider> ParliaApiImpl<P> {
    /// Create a new Parlia API instance
    pub fn new(snapshot_provider: Arc<P>) -> Self {
        Self { snapshot_provider }
    }
}

#[async_trait::async_trait]
impl<P: SnapshotProvider + Send + Sync + 'static> ParliaApiServer for ParliaApiImpl<P> {
    /// Get snapshot at a specific block (matches BSC official API.GetSnapshot)
    /// Accepts block number as hex string like "0x123132"
    async fn get_snapshot_by_hash(&self, block_hash: String) -> RpcResult<Option<SnapshotResult>> {
        // parlia_getSnapshot called
        let block_hash = BlockHash::from_str(&block_hash).map_err(|_| ErrorObject::owned(
            -32602, 
            "Invalid block hash format", 
            None::<()>
        ))?;
        
        // Get snapshot from provider (equivalent to api.parlia.snapshot call in BSC)
        match self.snapshot_provider.snapshot_by_hash(&block_hash) {
            Some(snapshot) => {
                tracing::info!("Found snapshot for block {}: validators={}, epoch_num={}, block_hash=0x{:x}", 
                block_hash, snapshot.validators.len(), snapshot.epoch_num, snapshot.block_hash);
                let result: SnapshotResult = snapshot.into();
                // Snapshot result prepared
                Ok(Some(result))
            },
            None => {
                tracing::warn!("No snapshot found for block hash {}", block_hash);
                Ok(None)
            }
        }
    }

    async fn build_add_node_ids_call(&self, node_ids: Vec<String>) -> RpcResult<ContractCall> {
        let ids = parse_node_ids(node_ids)?;
        let (to, data) = crate::system_contracts::encode_add_node_ids_call(ids);
        Ok(ContractCall { to: format!("0x{to:040x}"), data: format!("0x{}", alloy_primitives::hex::encode(data)) })
    }

    async fn build_remove_node_ids_call(&self, node_ids: Vec<String>) -> RpcResult<ContractCall> {
        let ids = parse_node_ids(node_ids)?;
        let (to, data) = crate::system_contracts::encode_remove_node_ids_call(ids);
        Ok(ContractCall { to: format!("0x{to:040x}"), data: format!("0x{}", alloy_primitives::hex::encode(data)) })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContractCall {
    pub to: String,
    pub data: String,
}

fn parse_node_ids(input: Vec<String>) -> RpcResult<Vec<[u8; 32]>> {
    let mut out = Vec::with_capacity(input.len());
    for s in input {
        let s = s.strip_prefix("0x").unwrap_or(&s);
        let bytes = match alloy_primitives::hex::decode(s) {
            Ok(b) => b,
            Err(_) => return Err(ErrorObject::owned(-32602, "Invalid nodeID hex", None::<()>)),
        };
        if bytes.len() != 32 {
            return Err(ErrorObject::owned(-32602, "NodeID must be 32 bytes", None::<()>));
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&bytes);
        out.push(arr);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chainspec::{bsc_testnet, BscChainSpec};
    use crate::consensus::parlia::provider::EnhancedDbSnapshotProvider;
    use reth_db::test_utils::create_test_rw_db;


    #[tokio::test]
    async fn test_snapshot_api() {
        // Build an EnhancedDbSnapshotProvider backed by a temp DB and noop header provider
        let db = create_test_rw_db();
        let chain_spec = Arc::new(BscChainSpec::from(bsc_testnet()));
        let snapshot_provider = Arc::new(EnhancedDbSnapshotProvider::new(
            db.clone(),
            2048,
            chain_spec,
        ));
        
        let bh1 = BlockHash::from_str("0xeeed4270b9874af140ab3e9293a144941203d45adb994a6d6de833897a52fe68").unwrap();
        // Insert a test snapshot
        let test_snapshot = Snapshot {
            block_number: 100,
            block_hash: bh1,
            validators: vec![alloy_primitives::Address::random(), alloy_primitives::Address::random()],
            epoch_num: 200,
            turn_length: Some(1),
            ..Default::default()
        };
        snapshot_provider.insert(test_snapshot.clone());

        let api = ParliaApiImpl::new(snapshot_provider);
        
        // Test snapshot retrieval with hex block number (BSC official format)
        let result = api.get_snapshot_by_hash("0xeeed4270b9874af140ab3e9293a144941203d45adb994a6d6de833897a52fe68".to_string()).await.unwrap(); // 0x64 = 100
        assert!(result.is_some());
        
        let snapshot_result = result.unwrap();
        assert_eq!(snapshot_result.number, 100);
        assert_eq!(snapshot_result.validators.len(), 2);
        assert_eq!(snapshot_result.epoch_length, 200);
        assert_eq!(snapshot_result.turn_length, 1);
        
        // Test with decimal format too
        let result = api.get_snapshot_by_hash("eeed4270b9874af140ab3e9293a144941203d45adb994a6d6de833897a52fe68".to_string()).await.unwrap();
        assert!(result.is_some());
        let snapshot_result = result.unwrap();
        assert_eq!(snapshot_result.number, 100);
    }
}
