use crate::{BscBlock, BscBlockBody, BscPrimitives};
use crate::node::network::data_availability::validate_sidecar_for_storage;
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
    DBProvider, DatabaseProvider, EthStorage, ProviderError, ProviderResult, ReadBodyInput,
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
            .map(|(number, body)| ((number, body.map(|b| &b.inner)), (number, body)))
            .unzip();
        self.0.write_block_bodies(provider, eth_bodies)?;
        let first_block = sidecar_entries.first().map(|(number, _)| *number);
        let last_block = sidecar_entries.last().map(|(number, _)| *number);

        // Write blob sidecars to the blob store keyed by tx hash.
        if let Some(blob_store) = crate::shared::get_global_blob_store() {
            let to_insert = verified_sidecars(&sidecar_entries);
            if !to_insert.is_empty() {
                let tx_hashes: Vec<_> = to_insert.iter().map(|(h, _)| *h).collect();
                if let Err(error) = blob_store.insert_all(to_insert) {
                    tracing::warn!(
                        target: "bsc::storage",
                        first_block,
                        last_block,
                        sidecar_count = tx_hashes.len(),
                        ?tx_hashes,
                        %error,
                        "blob_store_write: failed to insert sidecars; aborting block body write"
                    );
                    return Err(ProviderError::other(error));
                }
                tracing::debug!(
                    target: "bsc::storage",
                    first_block,
                    last_block,
                    sidecar_count = tx_hashes.len(),
                    ?tx_hashes,
                    "blob_store_write: inserted sidecars"
                );
            }
        } else if sidecar_entries
            .iter()
            .any(|(_, body)| body.and_then(|b| b.sidecars.as_ref()).is_some_and(|s| !s.is_empty()))
        {
            tracing::warn!(
                target: "bsc::storage",
                first_block,
                last_block,
                "blob_store_write: blob store unavailable; sidecars not written"
            );
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
        #[allow(clippy::type_complexity)]
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

        let blob_store = crate::shared::get_global_blob_store();
        let bodies = eth_bodies
            .into_iter()
            .zip(block_info)
            .map(|(inner, (block_number, block_hash, tx_info))| {
                // Collect blob tx hashes and their position in the full block tx list.
                let blob_txs: Vec<(B256, u64)> = tx_info.iter()
                    .enumerate()
                    .filter(|(_, (_, ty))| *ty == EIP4844_TX_TYPE_ID)
                    .map(|(idx, (hash, _))| (*hash, idx as u64))
                    .collect();
                let sidecars = if blob_txs.is_empty() {
                    None
                } else {
                    blob_store.and_then(|store| {
                        read_sidecars_from_blob_store(store, block_number, block_hash, &blob_txs)
                    })
                };
                if !blob_txs.is_empty() {
                    tracing::debug!(
                        target: "bsc::storage",
                        block_number,
                        ?block_hash,
                        ?blob_txs,
                        expected = blob_txs.len(),
                        found = sidecars.as_ref().map_or(0, Vec::len),
                        found_tx_hashes = ?sidecars.iter().flatten().map(|s| s.tx_hash).collect::<Vec<_>>(),
                        blob_store_available = blob_store.is_some(),
                        "blob_store_read: queried sidecars for blob txs"
                    );
                }
                BscBlockBody { inner, sidecars }
            })
            .collect();
        Ok(bodies)
    }
}

/// Look up blob sidecars from the blob store.
/// `blob_txs` contains `(tx_hash, tx_index_in_block)` pairs — only type-3 txs.
/// Sidecars from `bodies` that may be stored, dropping any that do not match their
/// body.
///
/// The blob store overwrites by tx hash and its contents are served as canonical,
/// so an unchecked sidecar here would replace genuine data with a peer's bytes.
/// A mismatch drops the sidecar rather than failing the write: the body itself is
/// valid, and failing would stall the stage on data the header does not commit to.
fn verified_sidecars(
    bodies: &[(u64, Option<&BscBlockBody>)],
) -> Vec<(B256, BlobTransactionSidecarVariant)> {
    let mut verified = Vec::new();
    for (block_number, body) in bodies {
        let Some(body) = body else { continue };
        for sidecar in body.sidecars.iter().flatten() {
            match validate_sidecar_for_storage(body, *block_number, sidecar) {
                Ok(()) => verified.push((
                    sidecar.tx_hash,
                    BlobTransactionSidecarVariant::Eip4844(sidecar.inner.clone()),
                )),
                Err(error) => tracing::warn!(
                    target: "bsc::storage",
                    block_number,
                    tx_hash = %sidecar.tx_hash,
                    error = %format_args!("{error:#}"),
                    "blob_store_write: dropping sidecar that does not match its block"
                ),
            }
        }
    }
    verified
}

fn read_sidecars_from_blob_store(
    blob_store: &Arc<dyn BlobStore>,
    block_number: u64,
    block_hash: B256,
    blob_txs: &[(B256, u64)],
) -> Option<Vec<BscBlobTransactionSidecar>> {
    let tx_hashes: Vec<B256> = blob_txs.iter().map(|(h, _)| *h).collect();
    let blobs = blob_store
        .get_all(tx_hashes)
        .inspect_err(|error| {
            tracing::warn!(
                target: "bsc::storage",
                block_number,
                ?block_hash,
                error = %error,
                "blob_store_read: failed to query sidecars"
            );
        })
        .ok()?;
    if blobs.is_empty() {
        return None;
    }
    let hash_to_idx: HashMap<B256, u64> = blob_txs.iter().copied().collect();

    let mut sidecars: Vec<_> = blobs
        .into_iter()
        .filter_map(|(tx_hash, variant)| {
            let inner = match variant.as_ref() {
                BlobTransactionSidecarVariant::Eip4844(s) => s.clone(),
                _ => {
                    tracing::warn!(
                        target: "bsc::storage",
                        block_number,
                        ?block_hash,
                        ?tx_hash,
                        "blob_store_read: sidecar is NOT Eip4844 variant, skipping"
                    );
                    return None;
                }
            };
            let tx_index = *hash_to_idx.get(&tx_hash)?;
            if inner.blobs.is_empty() || inner.commitments.is_empty() || inner.proofs.is_empty() {
                tracing::error!(
                    target: "bsc::storage",
                    block_number,
                    ?block_hash,
                    ?tx_hash,
                    blobs = inner.blobs.len(),
                    commitments = inner.commitments.len(),
                    proofs = inner.proofs.len(),
                    "blob_store_read: sidecar has EMPTY fields!"
                );
            }
            Some(BscBlobTransactionSidecar {
                inner,
                block_number,
                block_hash,
                tx_index,
                tx_hash,
                version: 0,
            })
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::network::data_availability::tests::blob_block;
    use alloy_eips::eip4844::builder::{SidecarBuilder, SimpleCoder};

    fn stored_tx_hashes(block: &BscBlock) -> Vec<B256> {
        verified_sidecars(&[(block.header.number, Some(&block.body))])
            .into_iter()
            .map(|(tx_hash, _)| tx_hash)
            .collect()
    }

    /// SRC-2715: the staged body downloader writes sidecars that no data-availability
    /// check has seen. A peer can return the genuine transactions, which satisfy
    /// the transaction root, beside forged sidecars that keep each real tx hash.
    /// Those must never reach the blob store, which serves them as canonical.
    #[test]
    fn only_sidecars_matching_their_body_are_stored() {
        let block = blob_block(100);
        let sidecars = block.body.sidecars.clone().unwrap();
        assert_eq!(
            stored_tx_hashes(&block),
            sidecars.iter().map(|s| s.tx_hash).collect::<Vec<_>>(),
            "genuine sidecars are stored",
        );

        // KZG-valid, but for other data: the commitment cannot match the
        // transaction's versioned hash.
        let other =
            SidecarBuilder::<SimpleCoder>::from_slice(b"forged blob").build_4844().unwrap();
        let legacy_tx_hash = *block.body.inner.transactions[0].hash();
        type Forge = Box<dyn Fn(&mut BscBlobTransactionSidecar)>;
        let cases: Vec<(&str, Forge)> = vec![
            ("blob byte", Box::new(|s| s.inner.blobs[0][31] ^= 1)),
            ("other valid blob", Box::new(move |s| s.inner = other.clone())),
            ("tx index", Box::new(|s| s.tx_index = 0)),
            (
                "legacy tx",
                Box::new(move |s| {
                    s.tx_index = 0;
                    s.tx_hash = legacy_tx_hash;
                }),
            ),
            ("out of range index", Box::new(|s| s.tx_index = u64::MAX)),
            ("block number", Box::new(|s| s.block_number += 1)),
            ("version", Box::new(|s| s.version = 1)),
        ];
        for (name, forge) in cases {
            let mut forged = block.clone();
            forge(&mut forged.body.sidecars.as_mut().unwrap()[0]);
            assert_eq!(
                stored_tx_hashes(&forged),
                vec![sidecars[1].tx_hash],
                "{name}: the forged sidecar is dropped and its genuine sibling kept",
            );
        }
    }
}
