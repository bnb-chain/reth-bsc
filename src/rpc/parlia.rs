use alloy_consensus::Sealable;
use alloy_primitives::{B256, BlockHash};
use jsonrpsee::{core::RpcResult, proc_macros::rpc, types::ErrorObject};
use reth_provider::{BlockNumReader, HeaderProvider};
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
            .map(|(block_num, addr)| (block_num.to_string(), format!("0x{addr:040x}")))
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
            epoch_length: 200,    // BSC epoch length
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

    /// Build call data for StakeHub.removeNodeIDs(bytes32[] nodeIDs). Returns { to, data } as hex.
    #[method(name = "getJustifiedNumber")]
    async fn get_justified_number(&self, block_number: String) -> RpcResult<u64>;
}

/// Implementation of the Parlia snapshot RPC API
pub struct ParliaApiImpl<P: SnapshotProvider, B: HeaderProvider + BlockNumReader + Send + Sync> {
    /// Snapshot provider for accessing validator snapshots
    snapshot_provider: Arc<P>,
    provider: B,
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

impl<P, B> ParliaApiImpl<P, B> 
    where
        P: SnapshotProvider + Send + Sync + 'static,
        B: HeaderProvider + BlockNumReader + Send + Sync + 'static,
{
    /// Create a new Parlia API instance
    pub fn new(snapshot_provider: Arc<P>, provider: B) -> Self {
        Self { snapshot_provider, provider }
    }

    /// Parse block number string (hex, tag, or hash)
    fn parse_block_number(&self, block_str: &str) -> RpcResult<u64> {
        // Handle tags
        match block_str {
            "latest" => {
                return self.provider.best_block_number()
                    .map_err(|e| ErrorObject::owned(
                        -32603,
                        format!("Failed to get latest block: {}", e),
                        None::<()>,
                    ))
            }
            "earliest" => return Ok(0),
            "safe" | "finalized" => {
                // For BSC, treat safe/finalized as latest
                return self.provider.best_block_number()
                    .map_err(|e| ErrorObject::owned(
                        -32603,
                        format!("Failed to get latest block: {}", e),
                        None::<()>,
                    ))
            }
            _ => {}
        }

        // Try hex number
        if let Some(hex_str) = block_str.strip_prefix("0x") {
            if let Ok(num) = u64::from_str_radix(hex_str, 16) {
                return Ok(num);
            }
        }

        // Try as decimal
        if let Ok(num) = block_str.parse::<u64>() {
            return Ok(num);
        }

        // Try as block hash (32 bytes hex)
        if block_str.len() == 66 || (block_str.len() == 64 && !block_str.starts_with("0x")) {
            let hash_str = block_str.strip_prefix("0x").unwrap_or(block_str);
            if let Ok(hash_bytes) = hex::decode(hash_str) {
                if hash_bytes.len() == 32 {
                    let block_hash = B256::from_slice(&hash_bytes);
                    // Try to get block by hash
                    if let Ok(Some(block_num)) = self.provider.block_number(block_hash) {
                        return Ok(block_num);
                    }
                }
            }
        }

        Err(ErrorObject::owned(
            -32602,
            format!("Invalid block number format: {}", block_str),
            None::<()>,
        ))
    }
}

#[async_trait::async_trait]
impl<P, B> ParliaApiServer for ParliaApiImpl<P, B>
    where
        P: SnapshotProvider + Send + Sync + 'static,
        B: HeaderProvider + BlockNumReader + Send + Sync + 'static,
{
    /// Get snapshot at a specific block (matches BSC official API.GetSnapshot)
    /// Accepts block number as hex string like "0x123132"
    async fn get_snapshot_by_hash(&self, block_hash: String) -> RpcResult<Option<SnapshotResult>> {
        // parlia_getSnapshot called
        let block_hash = BlockHash::from_str(&block_hash)
            .map_err(|_| ErrorObject::owned(-32602, "Invalid block hash format", None::<()>))?;

        // Get snapshot from provider (equivalent to api.parlia.snapshot call in BSC)
        match self.snapshot_provider.snapshot_by_hash(&block_hash) {
            Some(snapshot) => {
                tracing::info!(
                    "Found snapshot for block {}: validators={}, epoch_num={}, block_hash=0x{:x}",
                    block_hash,
                    snapshot.validators.len(),
                    snapshot.epoch_num,
                    snapshot.block_hash
                );
                let result: SnapshotResult = snapshot.into();
                // Snapshot result prepared
                Ok(Some(result))
            }
            None => {
                tracing::warn!("No snapshot found for block hash {}", block_hash);
                Ok(None)
            }
        }
    }

    async fn build_add_node_ids_call(&self, node_ids: Vec<String>) -> RpcResult<ContractCall> {
        let ids = parse_node_ids(node_ids)?;
        let (to, data) = crate::system_contracts::encode_add_node_ids_call(ids);
        Ok(ContractCall {
            to: format!("0x{to:040x}"),
            data: format!("0x{}", alloy_primitives::hex::encode(data)),
        })
    }

    async fn build_remove_node_ids_call(&self, node_ids: Vec<String>) -> RpcResult<ContractCall> {
        let ids = parse_node_ids(node_ids)?;
        let (to, data) = crate::system_contracts::encode_remove_node_ids_call(ids);
        Ok(ContractCall {
            to: format!("0x{to:040x}"),
            data: format!("0x{}", alloy_primitives::hex::encode(data)),
        })
    }

    async fn get_justified_number(&self, block_number: String) -> RpcResult<u64> {
        let block_number = self.parse_block_number(&block_number)?;
        let header = self.provider.header_by_number(block_number)
            .map_err(|e| ErrorObject::owned(
                -32603,
                format!("Failed to get header by number: {}", e),
                None::<()>,
            ))?.ok_or(ErrorObject::owned(-32602, "Header not found", None::<()>))?;
        let snapshot = self.snapshot_provider.snapshot_by_hash(&header.hash_slow());
        if let Some(snapshot) = snapshot {
            Ok(snapshot.vote_data.target_number)
        } else {
            Err(ErrorObject::owned(-32602, "No snapshot found for block hash", None::<()>))
        }
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
    use crate::consensus::parlia::VoteData;
    use crate::consensus::parlia::provider::EnhancedDbSnapshotProvider;
    use alloy_consensus::Header;
    use reth_db::test_utils::create_test_rw_db;
    use reth_provider::ProviderResult;
    use core::ops::RangeBounds;
    use reth_primitives_traits::SealedHeader;
    use alloy_primitives::b256;
    use std::sync::LazyLock;

    static TEST_GENSIS_HEADER: LazyLock<Header> = LazyLock::new(|| Header {
        parent_hash: b256!("0x043851a1a96c9912fcdcf4ed27ac73b1d2c1a790c3ee0e3acc1b55cc75685a11"),
        number: 0,
        ..Header::default()
    });
    static TEST_HEADER: LazyLock<Header> = LazyLock::new(|| Header {
        parent_hash: b256!("0x7b781c398681c3735b40a60956e9b89e0ec2158f0850b6449e999de4c5889e87"),
        number: 100,
        ..Header::default()
    });
    /// Minimal test provider that satisfies the required traits with stubbed methods.
    #[derive(Clone, Debug, Default)]
    struct TestProvider();

    impl reth_provider::BlockHashReader for TestProvider {
        fn block_hash(&self, _number: alloy_primitives::BlockNumber) -> ProviderResult<Option<alloy_primitives::B256>> {
            Ok(None)
        }

        fn canonical_hashes_range(
            &self,
            _start: alloy_primitives::BlockNumber,
            _end: alloy_primitives::BlockNumber,
        ) -> ProviderResult<Vec<alloy_primitives::B256>> {
            Ok(Vec::new())
        }
    }

    impl reth_provider::BlockNumReader for TestProvider {
        fn chain_info(&self) -> ProviderResult<reth_chainspec::ChainInfo> {
            Ok(Default::default())
        }

        fn best_block_number(&self) -> ProviderResult<alloy_primitives::BlockNumber> {
            Ok(100)
        }

        fn last_block_number(&self) -> ProviderResult<alloy_primitives::BlockNumber> {
            Ok(100)
        }

        fn block_number(&self, _hash: alloy_primitives::B256) -> ProviderResult<Option<alloy_primitives::BlockNumber>> {
            Ok(None)
        }
    }

    impl reth_provider::HeaderProvider for TestProvider {
        type Header = alloy_consensus::Header;

        fn header(&self, _block_hash: &alloy_primitives::BlockHash) -> ProviderResult<Option<Self::Header>> {
            Ok(None)
        }

        fn header_by_number(&self, _num: u64) -> ProviderResult<Option<Self::Header>> {
            match _num {
                0 => Ok(Some((*TEST_GENSIS_HEADER).clone())),
                100 => Ok(Some((*TEST_HEADER).clone())),
                _ => Ok(None),
            }
        }

        fn header_td(&self, _hash: &alloy_primitives::BlockHash) -> ProviderResult<Option<alloy_primitives::U256>> {
            Ok(None)
        }

        fn header_td_by_number(
            &self,
            _number: alloy_primitives::BlockNumber,
        ) -> ProviderResult<Option<alloy_primitives::U256>> {
            Ok(None)
        }

        fn headers_range(
            &self,
            _range: impl RangeBounds<alloy_primitives::BlockNumber>,
        ) -> ProviderResult<Vec<Self::Header>> {
            Ok(Vec::new())
        }

        fn sealed_header(&self, _number: alloy_primitives::BlockNumber) -> ProviderResult<Option<SealedHeader<Self::Header>>> {
            Ok(None)
        }

        fn sealed_headers_while(
            &self,
            _range: impl RangeBounds<alloy_primitives::BlockNumber>,
            _predicate: impl FnMut(&SealedHeader<Self::Header>) -> bool,
        ) -> ProviderResult<Vec<SealedHeader<Self::Header>>> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn test_snapshot_api() {
        // Build an EnhancedDbSnapshotProvider backed by a temp DB and noop header provider
        let db = create_test_rw_db();
        let chain_spec = Arc::new(BscChainSpec::from(bsc_testnet()));
        let snapshot_provider =
            Arc::new(EnhancedDbSnapshotProvider::new(db.clone(), 2048, chain_spec));

        // Insert a test snapshot
        let test_snapshot = Snapshot {
            block_number: TEST_HEADER.number,
            block_hash: TEST_HEADER.hash_slow(),
            validators: vec![
                alloy_primitives::Address::random(),
                alloy_primitives::Address::random(),
            ],
            epoch_num: 200,
            turn_length: Some(1),
            vote_data: VoteData {
                target_number: 99,
                source_number: 98,
                ..Default::default()
            },
            ..Default::default()
        };
        snapshot_provider.insert(test_snapshot.clone());
        snapshot_provider.insert(Snapshot { 
            block_number: 0,
            block_hash: TEST_GENSIS_HEADER.hash_slow(),
            ..Default::default()
         });

        let provider = TestProvider();
        let api = ParliaApiImpl::new(snapshot_provider, provider);

        // Test snapshot retrieval with hex block number (BSC official format)
        let result = api
            .get_snapshot_by_hash(
                TEST_HEADER.hash_slow().to_string(),
            )
            .await
            .unwrap(); // 0x64 = 100
        assert!(result.is_some());

        let snapshot_result = result.unwrap();
        assert_eq!(snapshot_result.number, 100);
        assert_eq!(snapshot_result.validators.len(), 2);
        assert_eq!(snapshot_result.epoch_length, 200);
        assert_eq!(snapshot_result.turn_length, 1);

        // Test with decimal format too
        let result = api
            .get_snapshot_by_hash(
                TEST_HEADER.hash_slow().to_string().strip_prefix("0x").unwrap().to_string(),
            )
            .await
            .unwrap();
        assert!(result.is_some());
        let snapshot_result = result.unwrap();
        assert_eq!(snapshot_result.number, 100);
        assert_eq!(99, api.get_justified_number("latest".to_string()).await.unwrap());
        assert_eq!(0, api.get_justified_number("earliest".to_string()).await.unwrap());
    }
}
