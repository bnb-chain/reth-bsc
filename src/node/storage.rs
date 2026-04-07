use crate::{BscBlock, BscBlockBody, BscPrimitives};
use crate::node::primitives::BscBlobTransactionSidecar;
use alloy_consensus::BlockHeader;
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
                if let Err(e) = blob_store.insert_all(to_insert) {
                    tracing::warn!(
                        target: "bsc::storage",
                        "Failed to insert blob sidecars into blob store: {e}"
                    );
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
        // Pre-extract block metadata and tx hashes before `inputs` is consumed.
        let block_info: Vec<(u64, B256, Vec<B256>)> = inputs
            .iter()
            .map(|(header, txs)| {
                (
                    header.number(),
                    header.hash_slow(),
                    txs.iter().map(|tx| *tx.hash()).collect(),
                )
            })
            .collect();

        let eth_bodies = self.0.read_block_bodies(provider, inputs)?;

        let blob_store = crate::shared::get_global_blob_store().cloned();
        let bodies = eth_bodies
            .into_iter()
            .zip(block_info.into_iter())
            .map(|(inner, (block_number, block_hash, tx_hashes))| {
                let sidecars = blob_store.as_ref().and_then(|store| {
                    read_sidecars_from_blob_store(store, block_number, block_hash, &tx_hashes)
                });
                BscBlockBody { inner, sidecars }
            })
            .collect();
        Ok(bodies)
    }
}

/// Look up blob sidecars for all transactions in a block from the blob store.
fn read_sidecars_from_blob_store(
    blob_store: &Arc<dyn BlobStore>,
    block_number: u64,
    block_hash: B256,
    tx_hashes: &[B256],
) -> Option<Vec<BscBlobTransactionSidecar>> {
    let blobs = blob_store.get_all(tx_hashes.to_vec()).ok()?;
    if blobs.is_empty() {
        return None;
    }
    let hash_to_idx: HashMap<B256, u64> = tx_hashes
        .iter()
        .enumerate()
        .map(|(i, h)| (*h, i as u64))
        .collect();

    let sidecars: Vec<_> = blobs
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
