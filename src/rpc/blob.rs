use jsonrpsee::core::RpcResult;
use jsonrpsee::proc_macros::rpc;
use jsonrpsee::types::ErrorObject;
use alloy_primitives::B256;
use alloy_consensus::transaction::TxHashRef;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use reth_transaction_pool::{BlobStoreError, TransactionPool};
use alloy_eips::eip7594::BlobTransactionSidecarVariant;
use reth_provider::{BlockNumReader, BlockReader, TransactionsProvider};

/// Inner blob sidecar data
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BlobSidecarData {
    /// Blobs (full or truncated to 32 bytes)
    pub blobs: Vec<String>,
    /// Commitments
    pub commitments: Vec<String>,
    /// Proofs
    pub proofs: Vec<String>,
}

/// Blob sidecar response with optional truncation
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BlobSidecarResponse {
    /// Blob sidecar data
    pub blob_sidecar: BlobSidecarData,
    /// Block hash
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block_hash: Option<String>,
    /// Block number
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block_number: Option<String>,
    /// Transaction hash
    pub tx_hash: String,
    /// Transaction index in block
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tx_index: Option<String>,
}

/// Blob RPC API for BSC
#[rpc(server, namespace = "eth")]
pub trait BlobApi {
    /// Get blob sidecar by transaction hash
    /// 
    /// # Parameters
    /// - `tx_hash`: Transaction hash
    /// - `full_blob_flag`: If true returns full blob, if false returns first 32 bytes (default: true)
    #[method(name = "getBlobSidecarByTxHash")]
    async fn get_blob_sidecar_by_tx_hash(
        &self,
        tx_hash: B256,
        full_blob_flag: Option<bool>,
    ) -> RpcResult<Option<BlobSidecarResponse>>;

    /// Get blob sidecars for a block
    /// 
    /// # Parameters
    /// - `block_number`: Block number (hex), hash, or tag ("latest", "earliest", "safe", "finalized")
    /// - `full_blob_flag`: If true returns full blob, if false returns first 32 bytes (default: true)
    #[method(name = "getBlobSidecars")]
    async fn get_blob_sidecars(
        &self,
        block_number: String,
        full_blob_flag: Option<bool>,
    ) -> RpcResult<Vec<BlobSidecarResponse>>;
}

/// Implementation of BlobApi
pub struct BlobApiImpl<Pool, Provider> {
    /// Transaction pool with blob store
    pool: Pool,
    /// Provider for blockchain data
    provider: Provider,
}

impl<Pool, Provider> BlobApiImpl<Pool, Provider> {
    /// Create a new BlobApi instance
    pub fn new(pool: Pool, provider: Provider) -> Self {
        Self { pool, provider }
    }
}

impl<Pool, Provider> BlobApiImpl<Pool, Provider>
where
    Pool: TransactionPool + Clone + 'static,
    Provider: TransactionsProvider + BlockNumReader + Clone + 'static,
{
    /// Convert BlobTransactionSidecarVariant to BlobSidecarResponse
    fn sidecar_to_response(
        tx_hash: B256,
        sidecar: Arc<BlobTransactionSidecarVariant>,
        full_blob: bool,
        block_number: Option<u64>,
        block_hash: Option<B256>,
        index: Option<u64>,
    ) -> BlobSidecarResponse {
        let (blobs, commitments, proofs) = match sidecar.as_ref() {
            BlobTransactionSidecarVariant::Eip4844(eip4844) => {
                let blobs: Vec<String> = eip4844
                    .blobs
                    .iter()
                    .map(|blob| {
                        if full_blob {
                            format!("0x{}", hex::encode(blob.as_slice()))
                        } else {
                            // Return first 32 bytes only
                            format!("0x{}", hex::encode(&blob.as_slice()[..32.min(blob.as_slice().len())]))
                        }
                    })
                    .collect();

                let commitments: Vec<String> = eip4844
                    .commitments
                    .iter()
                    .map(|c| format!("0x{}", hex::encode(c.as_slice())))
                    .collect();

                let proofs: Vec<String> = eip4844
                    .proofs
                    .iter()
                    .map(|p| format!("0x{}", hex::encode(p.as_slice())))
                    .collect();

                (blobs, commitments, proofs)
            }
            BlobTransactionSidecarVariant::Eip7594(eip7594) => {
                let blobs: Vec<String> = eip7594
                    .blobs
                    .iter()
                    .map(|blob| {
                        if full_blob {
                            format!("0x{}", hex::encode(blob.as_slice()))
                        } else {
                            // Return first 32 bytes only
                            format!("0x{}", hex::encode(&blob.as_slice()[..32.min(blob.as_slice().len())]))
                        }
                    })
                    .collect();

                let commitments: Vec<String> = eip7594
                    .commitments
                    .iter()
                    .map(|c| format!("0x{}", hex::encode(c.as_slice())))
                    .collect();

                // EIP7594 uses cell_proofs instead of proofs
                let proofs: Vec<String> = eip7594
                    .cell_proofs
                    .iter()
                    .map(|p| format!("0x{}", hex::encode(p.as_slice())))
                    .collect();

                (blobs, commitments, proofs)
            }
        };

        BlobSidecarResponse {
            blob_sidecar: BlobSidecarData {
                blobs,
                commitments,
                proofs,
            },
            block_hash: block_hash.map(|h| format!("0x{:x}", h)),
            block_number: block_number.map(|n| format!("0x{:x}", n)),
            tx_hash: format!("0x{:x}", tx_hash),
            tx_index: index.map(|i| format!("0x{:x}", i)),
        }
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
impl<Pool, Provider> BlobApiServer for BlobApiImpl<Pool, Provider>
where
    Pool: TransactionPool + Clone + Send + Sync + 'static,
    Provider: TransactionsProvider + BlockNumReader + BlockReader + Clone + Send + Sync + 'static,
{
    /// Get blob sidecar by transaction hash
    async fn get_blob_sidecar_by_tx_hash(
        &self,
        tx_hash: B256,
        full_blob_flag: Option<bool>,
    ) -> RpcResult<Option<BlobSidecarResponse>> {
        let full_blob = full_blob_flag.unwrap_or(true);

        tracing::debug!(
            "eth_getBlobSidecarByTxHash called for tx {:?}, full_blob: {}",
            tx_hash,
            full_blob
        );

        // Try to get blob from pool's blob store
        let sidecar = match self.pool.get_blob(tx_hash) {
            Ok(Some(sidecar)) => sidecar,
            Ok(None) => {
                tracing::debug!("No blob sidecar found for tx {:?}", tx_hash);
                return Ok(None);
            }
            Err(BlobStoreError::MissingSidecar(_)) => {
                tracing::debug!("No blob sidecar found for tx {:?}", tx_hash);
                return Ok(None);
            }
            Err(e) => {
                tracing::error!("Failed to get blob from store: {}", e);
                return Err(ErrorObject::owned(
                    -32603,
                    format!("Failed to get blob from store: {}", e),
                    None::<()>,
                ));
            }
        };

        // Try to get transaction metadata (block number, hash, index)
        let (block_number, block_hash, index) = if let Ok(Some(tx_id)) = self.provider.transaction_id(tx_hash) {
            if let Ok(Some(block_num)) = self.provider.block_by_transaction_id(tx_id) {
                // Get block hash
                let block_hash = self.provider.block_hash(block_num).ok().flatten();
                
                // Try to get transaction index in block
                let index = if let Ok(Some(txs)) = self.provider.transactions_by_block(block_num.into()) {
                    txs.iter()
                        .position(|tx| *tx.tx_hash() == tx_hash)
                        .map(|pos| pos as u64)
                } else {
                    None
                };

                (Some(block_num), block_hash, index)
            } else {
                (None, None, None)
            }
        } else {
            (None, None, None)
        };

        let response = Self::sidecar_to_response(
            tx_hash,
            sidecar,
            full_blob,
            block_number,
            block_hash,
            index,
        );

        Ok(Some(response))
    }

    /// Get blob sidecars for a block
    async fn get_blob_sidecars(
        &self,
        block_number: String,
        full_blob_flag: Option<bool>,
    ) -> RpcResult<Vec<BlobSidecarResponse>> {
        let full_blob = full_blob_flag.unwrap_or(true);

        tracing::debug!(
            "eth_getBlobSidecars called for block {}, full_blob: {}",
            block_number,
            full_blob
        );

        // Parse block number
        let block_num = self.parse_block_number(&block_number)?;

        // Get all transactions in the block
        let transactions = self
            .provider
            .transactions_by_block(block_num.into())
            .map_err(|e| {
                ErrorObject::owned(
                    -32603,
                    format!("Failed to get transactions for block {}: {}", block_num, e),
                    None::<()>,
                )
            })?;

        let Some(txs) = transactions else {
            tracing::debug!("Block {} not found", block_num);
            return Err(ErrorObject::owned(
                -32602,
                format!("Block {} not found", block_num),
                None::<()>,
            ));
        };

        // Get block hash
        let block_hash = self.provider.block_hash(block_num).ok().flatten();

        // Collect transaction hashes
        let tx_hashes: Vec<B256> = txs.iter().map(|tx| *tx.tx_hash()).collect();

        // Get all blobs for these transactions
        let blob_results = self.pool.get_all_blobs(tx_hashes).map_err(|e| {
            ErrorObject::owned(
                -32603,
                format!("Failed to get blobs for block {}: {}", block_num, e),
                None::<()>,
            )
        })?;

        // Convert to responses
        let mut responses = Vec::new();
        for (index, (tx_hash, sidecar)) in blob_results.into_iter().enumerate() {
            let response = Self::sidecar_to_response(
                tx_hash,
                sidecar,
                full_blob,
                Some(block_num),
                block_hash,
                Some(index as u64),
            );
            responses.push(response);
        }

        tracing::debug!(
            "Found {} blob sidecars for block {}",
            responses.len(),
            block_num
        );

        Ok(responses)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_blob_sidecar_response_serialization() {
        let response = BlobSidecarResponse {
            blob_sidecar: BlobSidecarData {
                blobs: vec!["0x00".to_string()],
                commitments: vec!["0x01".to_string()],
                proofs: vec!["0x02".to_string()],
            },
            block_hash: Some("0xabcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890".to_string()),
            block_number: Some("0x100".to_string()),
            tx_hash: "0x1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef".to_string(),
            tx_index: Some("0x0".to_string()),
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("blobSidecar"));
        assert!(json.contains("txHash"));
        assert!(json.contains("blockNumber"));
        assert!(json.contains("blockHash"));
        assert!(json.contains("txIndex"));
    }
}
