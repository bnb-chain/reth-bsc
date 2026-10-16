use crate::{chainspec::BscChainSpec, BscBlock};
use alloy_consensus::Transaction;
use alloy_eips::eip4844::env_settings::EnvKzgSettings;
use eyre::{ensure, WrapErr};
use reth_chainspec::{EthChainSpec, EthereumHardforks};
use std::time::{SystemTime, UNIX_EPOCH};

// Geth-BSC's MinTimeDurationForBlobRequests: 18.2 days, independent of block time.
const BLOB_AVAILABILITY_WINDOW: u64 = 1_572_480;

/// Validate received data before forwarding or submitting it to the engine. A missing
/// sidecar invalidates this response, not the block hash; another peer may supply it.
pub(crate) fn validate_data_availability(
    block: &mut BscBlock,
    chain_spec: &BscChainSpec,
) -> eyre::Result<()> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    validate_at(block, chain_spec, now)
}

fn validate_at(block: &mut BscBlock, chain_spec: &BscChainSpec, now: u64) -> eyre::Result<()> {
    if !chain_spec.is_cancun_active_at_timestamp(block.header.timestamp) {
        ensure!(
            block.body.sidecars.as_ref().is_none_or(Vec::is_empty),
            "sidecars present before Cancun"
        );
        return Ok(());
    }
    if now.saturating_sub(block.header.timestamp) > BLOB_AVAILABILITY_WINDOW {
        // Historical sync must not require expired data or persist unchecked sidecars.
        block.body.sidecars = None;
        return Ok(());
    }

    let sidecars = block.body.sidecars.as_deref().unwrap_or_default();
    let blob_txs =
        block.body.inner.transactions.iter().enumerate().filter(|(_, tx)| tx.is_eip4844());
    let expected = blob_txs.clone().count();
    ensure!(
        sidecars.len() == expected,
        "blob sidecar count mismatch: expected {expected}, got {}",
        sidecars.len()
    );
    let blob_count: usize = sidecars.iter().map(|s| s.inner.blobs.len()).sum();
    let max_blobs = chain_spec
        .blob_params_at_timestamp(block.header.timestamp)
        .map_or(0, |params| params.max_blob_count);
    ensure!(
        blob_count as u64 <= max_blobs,
        "too many blobs: have {blob_count}, permitted {max_blobs}"
    );
    if sidecars.is_empty() {
        return Ok(());
    }

    let block_hash = block.header.hash_slow();
    for ((tx_index, tx), sidecar) in blob_txs.zip(sidecars) {
        ensure!(sidecar.version == 0, "unsupported sidecar version at tx {tx_index}");
        ensure!(
            sidecar.block_number == block.header.number && sidecar.block_hash == block_hash,
            "sidecar block metadata mismatch at tx {tx_index}"
        );
        ensure!(
            sidecar.tx_index == tx_index as u64 && sidecar.tx_hash == *tx.hash(),
            "sidecar transaction metadata mismatch at tx {tx_index}"
        );
        sidecar
            .inner
            .validate(tx.blob_versioned_hashes().unwrap_or_default(), EnvKzgSettings::Default.get())
            .wrap_err_with(|| format!("invalid blob sidecar at tx {tx_index}"))?;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::node::primitives::BscBlobTransactionSidecar;
    use alloy_consensus::{Header, TxEip4844, TxLegacy};
    use alloy_eips::eip4844::{
        builder::{SidecarBuilder, SimpleCoder},
        Bytes48,
    };
    use alloy_primitives::{Signature, B256, U256};
    use reth_chainspec::ChainSpecBuilder;
    use reth_ethereum_primitives::TransactionSigned;

    pub(crate) fn blob_block(timestamp: u64) -> BscBlock {
        let inner =
            SidecarBuilder::<SimpleCoder>::from_slice(b"bsc DA regression").build_4844().unwrap();
        let signature = Signature::new(U256::from(1), U256::from(1), false);
        let mut block = BscBlock {
            header: Header { number: 10, timestamp, ..Default::default() },
            ..Default::default()
        };
        block
            .body
            .inner
            .transactions
            .push(TransactionSigned::new_unhashed(TxLegacy::default().into(), signature));
        let mut sidecars = Vec::new();
        for nonce in 0..2 {
            let tx = TransactionSigned::new_unhashed(
                TxEip4844 {
                    nonce,
                    blob_versioned_hashes: inner.versioned_hashes().collect(),
                    ..Default::default()
                }
                .into(),
                signature,
            );
            sidecars.push(BscBlobTransactionSidecar {
                inner: inner.clone(),
                block_number: block.header.number,
                block_hash: block.header.hash_slow(),
                tx_index: block.body.inner.transactions.len() as u64,
                tx_hash: *tx.hash(),
                version: 0,
            });
            block.body.inner.transactions.push(tx);
        }
        block.body.sidecars = Some(sidecars);
        block
    }

    #[test]
    fn checks_sidecars_against_block_and_transactions() {
        let spec = BscChainSpec::from(ChainSpecBuilder::mainnet().cancun_activated().build());
        let block = blob_block(100);
        validate_at(&mut block.clone(), &spec, 100).unwrap();
        type MutateBlock = fn(&mut BscBlock);
        let cases: &[(&str, MutateBlock)] = &[
            ("missing", |b| b.body.sidecars = None),
            ("extra", |b| {
                let sc = b.body.sidecars.as_mut().unwrap();
                sc.push(sc[0].clone());
            }),
            ("order", |b| b.body.sidecars.as_mut().unwrap().swap(0, 1)),
            ("block number", |b| b.body.sidecars.as_mut().unwrap()[0].block_number += 1),
            ("block hash", |b| b.body.sidecars.as_mut().unwrap()[0].block_hash = B256::ZERO),
            ("tx hash", |b| b.body.sidecars.as_mut().unwrap()[0].tx_hash = B256::ZERO),
            ("tx index", |b| b.body.sidecars.as_mut().unwrap()[0].tx_index = 0),
            ("version", |b| b.body.sidecars.as_mut().unwrap()[0].version = 1),
            ("blob count", |b| b.body.sidecars.as_mut().unwrap()[0].inner.blobs.clear()),
            ("commitment count", |b| {
                b.body.sidecars.as_mut().unwrap()[0].inner.commitments.clear()
            }),
            ("proof count", |b| b.body.sidecars.as_mut().unwrap()[0].inner.proofs.clear()),
            ("commitment", |b| {
                b.body.sidecars.as_mut().unwrap()[0].inner.commitments[0] = Bytes48::ZERO
            }),
            ("proof", |b| b.body.sidecars.as_mut().unwrap()[0].inner.proofs[0] = Bytes48::ZERO),
            ("blob", |b| b.body.sidecars.as_mut().unwrap()[0].inner.blobs[0][31] ^= 1),
            ("max blobs", |b| {
                let sc = &mut b.body.sidecars.as_mut().unwrap()[0];
                sc.inner.blobs.resize(7, sc.inner.blobs[0]);
            }),
        ];
        for (name, mutate) in cases {
            let mut bad = block.clone();
            mutate(&mut bad);
            assert!(validate_at(&mut bad, &spec, 100).is_err(), "{name}");
        }
    }

    #[test]
    fn respects_cancun_and_retention_boundary() {
        let spec = BscChainSpec::from(ChainSpecBuilder::mainnet().cancun_activated().build());
        let mut block = blob_block(100);
        let mut missing = block.clone();
        missing.body.sidecars = None;
        for age in [0, BLOB_AVAILABILITY_WINDOW - 1, BLOB_AVAILABILITY_WINDOW] {
            assert!(validate_at(&mut missing, &spec, 100 + age).is_err());
        }
        validate_at(&mut missing, &spec, 101 + BLOB_AVAILABILITY_WINDOW).unwrap();
        validate_at(&mut block, &spec, 101 + BLOB_AVAILABILITY_WINDOW).unwrap();
        assert!(block.body.sidecars.is_none());
        let pre_cancun =
            BscChainSpec::from(ChainSpecBuilder::mainnet().shanghai_activated().build());
        assert!(validate_at(&mut blob_block(100), &pre_cancun, u64::MAX).is_err());
        validate_at(&mut BscBlock::default(), &pre_cancun, 0).unwrap();
        validate_at(&mut BscBlock::default(), &spec, 0).unwrap();
    }
}
