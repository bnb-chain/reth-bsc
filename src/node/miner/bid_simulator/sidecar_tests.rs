use super::*;
use crate::rpc::mev::legacy_sidecar_tests::blob_fixture;
use alloy_consensus::{Header, TxLegacy};
use alloy_eips::eip7594::BlobTransactionSidecarVariant;
use alloy_evm::block::BlockExecutor;
use alloy_primitives::{Signature, TxKind};
use reth_chainspec::ChainSpecBuilder;
use reth_primitives_traits::Recovered;
use reth_rpc_eth_api::helpers::pending_block::BuildPendingEnv;
use reth_transaction_pool::{
    blobstore::InMemoryBlobStore, noop::MockTransactionValidator, CoinbaseTipOrdering,
    EthPooledTransaction, Pool, PoolConfig, TransactionPool,
};
use revm::{database::InMemoryDB, state::AccountInfo};

#[test]
fn legacy_bid_sidecars_are_checked_before_execution_and_storage() {
    let _guard = crate::node::pool::tests::TipGuard::new(0);
    for from_pool in [false, true] {
        for case in ["missing", "invalid", "valid"] {
            let (blob_tx, mut sidecar) = blob_fixture();
            let sender = blob_tx.recover_signer().unwrap();
            if case == "invalid" {
                sidecar.blobs[0][31] ^= 1;
            }
            let pool = Pool::new(
                MockTransactionValidator::<EthPooledTransaction>::default(),
                CoinbaseTipOrdering::default(),
                InMemoryBlobStore::default(),
                PoolConfig::default(),
            );
            let mut bid_sidecars = HashMap::new();
            if case != "missing" {
                if from_pool {
                    pool.insert_blob(
                        *blob_tx.hash(),
                        BlobTransactionSidecarVariant::Eip4844(sidecar.clone()),
                    )
                    .unwrap();
                } else {
                    bid_sidecars.insert(*blob_tx.hash(), sidecar.clone());
                }
            }
            let spec = Arc::new(BscChainSpec::from(
                ChainSpecBuilder::mainnet().chain(56.into()).cancun_activated().build(),
            ));
            let config = BscEvmConfig::new(spec.clone());
            let parent = SealedHeader::seal_slow(Header {
                number: 9,
                timestamp: 1,
                gas_limit: 1_000_000,
                base_fee_per_gas: Some(1),
                excess_blob_gas: Some(0),
                blob_gas_used: Some(0),
                ..Default::default()
            });
            let mut attrs = BscNextBlockEnvAttributes::build_pending_env(&parent);
            attrs.mode = BscExecutionMode::BidSimulation;
            let mining_ctx = MiningContext {
                header: Some(Header {
                    number: 10,
                    timestamp: attrs.inner.timestamp,
                    ..Default::default()
                }),
                parent_header: parent.clone(),
                parent_snapshot: Arc::default(),
                is_inturn: true,
                cached_reads: None,
                block_timestamp_ms: attrs.inner.timestamp * 1000,
                end_mining_timestamp_ms: 0,
            };
            let bid = Bid {
                builder: sender,
                block_number: 10,
                parent_hash: parent.hash(),
                txs: vec![],
                blob_sidecars: bid_sidecars,
                un_revertible: vec![],
                gas_used: 0,
                gas_fee: U256::ZERO,
                builder_fee: U256::ZERO,
                committed: false,
                bid_hash: B256::ZERO,
                interrupt_flag: Arc::new(AtomicBool::new(false)),
            };
            let mut runtime = BidRuntime::new(
                bid,
                pool.clone(),
                config.clone(),
                EthPayloadAttributes { timestamp: attrs.inner.timestamp, ..Default::default() },
                spec,
                mining_ctx,
            );
            let prefix_sender = Address::repeat_byte(0x24);
            let prefix = Recovered::new_unchecked(
                TransactionSigned::new_unhashed(
                    TxLegacy {
                        chain_id: Some(56),
                        gas_limit: TX_GAS,
                        gas_price: 1_000_000_000,
                        to: TxKind::Call(Address::repeat_byte(0x42)),
                        ..Default::default()
                    }
                    .into(),
                    Signature::new(U256::from(1), U256::from(1), false),
                ),
                prefix_sender,
            );
            let mut db = InMemoryDB::default();
            for address in [sender, prefix_sender] {
                db.insert_account_info(
                    address,
                    AccountInfo {
                        balance: U256::from(1_000_000_000_000_000_000u64),
                        ..Default::default()
                    },
                );
            }
            let mut state = State::builder().with_database(db).with_bundle_update().build();
            let mut builder = config.builder_for_next_block(&mut state, &parent, attrs).unwrap();
            let mut transactions = vec![blob_tx.clone().try_into_recovered().unwrap()];
            if from_pool {
                // Pool additions start a new batch after the private prefix;
                // their sidecar index must still refer to the whole block.
                runtime
                    .commit_transaction_recovered(vec![prefix], &mut builder, 1_000_000, false, 0)
                    .unwrap();
            } else {
                transactions.insert(0, prefix);
            }
            let result = runtime.commit_transaction_recovered(
                transactions,
                &mut builder,
                1_000_000,
                from_pool,
                10_000,
            );
            let valid = case == "valid";
            assert_eq!(result.is_ok(), from_pool || valid, "{from_pool}/{case}: {result:?}");
            let expected_receipts = if valid {
                2
            } else if from_pool {
                1
            } else {
                0
            };
            assert_eq!(
                builder.executor().receipts().len(),
                expected_receipts,
                "{from_pool}/{case}"
            );
            assert_eq!(runtime.block_blob_count, u64::from(valid));
            assert_eq!(runtime.blob_sidecars.len(), usize::from(valid));
            assert_eq!(
                builder.evm_mut().db_mut().basic(sender).unwrap().unwrap().nonce,
                u64::from(valid)
            );
            if valid {
                assert_eq!(runtime.blob_sidecars[0].tx_index, 1);
                assert_eq!(runtime.blob_sidecars[0].tx_hash, *blob_tx.hash());
                assert_eq!(runtime.blob_sidecars[0].inner, sidecar);
            }
            if !from_pool {
                assert_eq!(
                    pool.get_blob(*blob_tx.hash()).unwrap().is_some(),
                    valid,
                    "invalid private sidecar must never enter the blob store"
                );
            }
        }
    }
}
