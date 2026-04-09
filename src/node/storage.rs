use crate::{BscBlock, BscBlockBody, BscPrimitives};
use crate::node::primitives::BscBlobTransactionSidecar;
use alloy_consensus::BlockHeader;
use alloy_eips::eip2718::{EIP4844_TX_TYPE_ID, Typed2718};
use alloy_eips::eip7594::BlobTransactionSidecarVariant;
use alloy_primitives::B256;
use reth_chainspec::EthereumHardforks;
use reth_db::transaction::{DbTx, DbTxMut};
use reth_provider::{
    providers::{ChainStorage, NodeTypesForProvider},
    BlockBodyReader, BlockBodyWriter, ChainSpecProvider, ChainStorageReader, ChainStorageWriter,
    DBProvider, DatabaseProvider, EthStorage, ProviderResult, ReadBodyInput,
};
use reth_transaction_pool::blobstore::BlobStore;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct BscStorage(EthStorage);

impl<Provider> BlockBodyWriter<Provider, BscBlockBody> for BscStorage
where
    Provider: DBProvider<Tx: DbTxMut>,
{
    fn write_block_bodies(
        &self,
        provider: &Provider,
        bodies: Vec<(u64, Option<&BscBlockBody>)>,
    ) -> ProviderResult<()> {
        let (eth_bodies, sidecar_entries): (Vec<_>, Vec<_>) = bodies
            .into_iter()
            .map(|(block_number, body)| {
                let inner = body.map(|b| &b.inner);
                let sidecars = body.and_then(|b| b.sidecars.as_ref());
                ((block_number, inner), (block_number, sidecars))
            })
            .unzip();
        self.0.write_block_bodies(provider, eth_bodies)?;

        // Write blob sidecars to the blob store keyed by tx hash.
        if let Some(blob_store) = crate::shared::get_global_blob_store() {
            let mut to_insert: Vec<(B256, BlobTransactionSidecarVariant)> = Vec::new();
            for (_, sidecars) in &sidecar_entries {
                if let Some(sidecars) = sidecars {
                    for sidecar in sidecars.iter() {
                        to_insert.push((
                            sidecar.tx_hash,
                            BlobTransactionSidecarVariant::Eip4844(sidecar.inner.clone()),
                        ));
                    }
                }
            }
            if !to_insert.is_empty() {
                let tx_hashes: Vec<_> = to_insert.iter().map(|(h, _)| *h).collect();
                match blob_store.insert_all(to_insert) {
                    Ok(()) => {
                        tracing::debug!(
                            target: "bsc::storage",
                            ?tx_hashes,
                            "blob_store_write: inserted sidecars"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            target: "bsc::storage",
                            ?tx_hashes,
                            "blob_store_write: failed to insert sidecars: {e}"
                        );
                    }
                }
            }
        }

        Ok(())
    }

    fn remove_block_bodies_above(
        &self,
        provider: &Provider,
        block: u64,
    ) -> ProviderResult<()> {
        self.0.remove_block_bodies_above(provider, block)?;
        // Blob store cleanup is handled by the pool maintenance task (finality-based eviction).
        Ok(())
    }
}

impl<Provider> BlockBodyReader<Provider> for BscStorage
where
    Provider: DBProvider + ChainSpecProvider<ChainSpec: EthereumHardforks>,
{
    type Block = BscBlock;

    fn read_block_bodies(
        &self,
        provider: &Provider,
        inputs: Vec<ReadBodyInput<'_, Self::Block>>,
    ) -> ProviderResult<Vec<BscBlockBody>> {
        // Pre-extract block metadata and per-tx (hash, type) before `inputs` is consumed.
        let block_info: Vec<(u64, B256, Vec<(B256, u8)>)> = inputs
            .iter()
            .map(|(header, txs)| {
                (
                    header.number(),
                    header.hash_slow(),
                    txs.iter().map(|tx| (*tx.hash(), tx.ty())).collect(),
                )
            })
            .collect();

        let eth_bodies = self.0.read_block_bodies(provider, inputs)?;

        let blob_store = crate::shared::get_global_blob_store().cloned();
        let bodies = eth_bodies
            .into_iter()
            .zip(block_info.into_iter())
            .map(|(inner, (block_number, block_hash, tx_info))| {
                // Collect blob tx hashes and their position in the full block tx list.
                let blob_txs: Vec<(B256, u64)> = tx_info.iter()
                    .enumerate()
                    .filter(|(_, (_, ty))| *ty == EIP4844_TX_TYPE_ID)
                    .map(|(idx, (hash, _))| (*hash, idx as u64))
                    .collect();
                let blob_tx_hashes: Vec<B256> = blob_txs.iter().map(|(h, _)| *h).collect();
                let sidecars = if blob_txs.is_empty() {
                    None
                } else {
                    blob_store.as_ref().and_then(|store| {
                        read_sidecars_from_blob_store(store, block_number, block_hash, &blob_txs)
                    })
                };
                if !blob_tx_hashes.is_empty() {
                    let found = sidecars.as_ref().map_or(0, |s| s.len());
                    tracing::debug!(
                        target: "bsc::storage",
                        block_number,
                        ?block_hash,
                        ?blob_tx_hashes,
                        found,
                        blob_store_available = blob_store.is_some(),
                        "blob_store_read: queried sidecars for blob txs"
                    );
                    if found == 0 {
                        tracing::warn!(
                            target: "bsc::storage",
                            block_number,
                            ?block_hash,
                            ?blob_tx_hashes,
                            "blob_store_read: block has blob txs but no sidecars found!"
                        );
                    }
                }
                BscBlockBody { inner, sidecars }
            })
            .collect();
        Ok(bodies)
    }
}

/// Look up blob sidecars from the blob store.
/// `blob_txs` contains `(tx_hash, tx_index_in_block)` pairs — only type-3 txs.
fn read_sidecars_from_blob_store(
    blob_store: &Arc<dyn BlobStore>,
    block_number: u64,
    block_hash: B256,
    blob_txs: &[(B256, u64)],
) -> Option<Vec<BscBlobTransactionSidecar>> {
    let tx_hashes: Vec<B256> = blob_txs.iter().map(|(h, _)| *h).collect();
    let blobs = blob_store.get_all(tx_hashes).ok()?;
    if blobs.is_empty() {
        return None;
    }
    let hash_to_idx: HashMap<B256, u64> = blob_txs.iter().copied().collect();

    let mut sidecars: Vec<_> = blobs
        .into_iter()
        .filter_map(|(tx_hash, variant)| {
            let inner = match variant.as_ref() {
                BlobTransactionSidecarVariant::Eip4844(s) => s.clone(),
                _ => return None,
            };
            let tx_index = *hash_to_idx.get(&tx_hash)?;
            Some(BscBlobTransactionSidecar { inner, block_number, block_hash, tx_index, tx_hash })
        })
        .collect();

    // get_all() does not guarantee input order (cache-hits precede disk-reads).
    // go-bsc validates sidecars[i].TxHash == blobTxs[i].Hash(), so order must
    // match the tx position in the block.
    sidecars.sort_unstable_by_key(|s| s.tx_index);

    if sidecars.is_empty() { None } else { Some(sidecars) }
}

impl ChainStorage<BscPrimitives> for BscStorage {
    fn reader<TX, Types>(
        &self,
    ) -> impl ChainStorageReader<DatabaseProvider<TX, Types>, BscPrimitives>
    where
        TX: DbTx + 'static,
        Types: NodeTypesForProvider<Primitives = BscPrimitives>,
    {
        self
    }

    fn writer<TX, Types>(
        &self,
    ) -> impl ChainStorageWriter<DatabaseProvider<TX, Types>, BscPrimitives>
    where
        TX: DbTxMut + DbTx + 'static,
        Types: NodeTypesForProvider<Primitives = BscPrimitives>,
    {
        self
    }
}
