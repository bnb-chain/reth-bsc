//! BSC block RPCs interpret `earliest` as genesis, independently of retained history.

use alloy_eips::BlockNumberOrTag;
use jsonrpsee::{core::RpcResult, proc_macros::rpc};
use reth_rpc_convert::RpcHeader;
use reth_rpc_eth_api::{
    helpers::{EthBlocks, EthTransactions},
    RpcBlock, RpcTransaction,
};

#[rpc(server, namespace = "eth")]
pub trait BscBlockApi<B, H, T> {
    #[method(name = "getBlockByNumber")]
    async fn block_by_number(&self, number: BlockNumberOrTag, full: bool) -> RpcResult<Option<B>>;

    #[method(name = "getHeaderByNumber")]
    async fn header_by_number(&self, hash: BlockNumberOrTag) -> RpcResult<Option<H>>;

    #[method(name = "getTransactionsByBlockNumber")]
    async fn transactions_by_block_number(
        &self,
        number: BlockNumberOrTag,
    ) -> RpcResult<Option<Vec<T>>>;
}

pub struct BscBlockApiImpl<Eth>(pub Eth);

/// Methods replaced by the BSC block API on transports with the eth namespace enabled.
pub const METHODS: [&str; 3] =
    ["eth_getBlockByNumber", "eth_getHeaderByNumber", "eth_getTransactionsByBlockNumber"];

fn genesis_for_earliest(number: BlockNumberOrTag) -> BlockNumberOrTag {
    match number {
        // Do not change the provider's history boundary: it also guards expired data reads.
        BlockNumberOrTag::Earliest => BlockNumberOrTag::Number(0),
        number => number,
    }
}

#[async_trait::async_trait]
impl<Eth>
    BscBlockApiServer<
        RpcBlock<Eth::NetworkTypes>,
        RpcHeader<Eth::NetworkTypes>,
        RpcTransaction<Eth::NetworkTypes>,
    > for BscBlockApiImpl<Eth>
where
    Eth: EthBlocks + EthTransactions,
    reth_evm::BlockEnvFor<Eth::Evm>: reth_rpc_eth_types::BlockOverridesExt,
{
    async fn block_by_number(
        &self,
        number: BlockNumberOrTag,
        full: bool,
    ) -> RpcResult<Option<RpcBlock<Eth::NetworkTypes>>> {
        self.0.rpc_block(genesis_for_earliest(number).into(), full).await.map_err(Into::into)
    }

    async fn header_by_number(
        &self,
        block_number: BlockNumberOrTag,
    ) -> RpcResult<Option<RpcHeader<Eth::NetworkTypes>>> {
        self.0.rpc_block_header(genesis_for_earliest(block_number).into()).await.map_err(Into::into)
    }

    async fn transactions_by_block_number(
        &self,
        number: BlockNumberOrTag,
    ) -> RpcResult<Option<Vec<RpcTransaction<Eth::NetworkTypes>>>> {
        self.0
            .transactions_by_block_id(genesis_for_earliest(number).into())
            .await
            .map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        chainspec::{bsc::bsc_mainnet, BscChainSpec},
        node::{evm::config::BscEvmConfig, BscNode},
        BscBlock, BscBlockBody,
    };
    use alloy_consensus::TxLegacy;
    use alloy_primitives::{Signature, B256};
    use jsonrpsee::core::server::Methods;
    use reth_chainspec::EthChainSpec;
    use reth_ethereum_primitives::TransactionSigned;
    use reth_network_api::noop::NoopNetwork;
    use reth_primitives_traits::{RecoveredBlock, SealedHeader, SignerRecoverable};
    use reth_provider::{
        providers::BlockchainProvider, test_utils::create_test_provider_factory_with_node_types,
        BlockIdReader, BlockNumReader, BlockWriter, ProviderFactory, RocksDBProviderFactory,
        StaticFileProviderBuilder, StaticFileProviderFactory, StaticFileSegment,
    };
    use reth_rpc_eth_api::EthApiServer;
    use reth_transaction_pool::test_utils::testing_pool;
    use serde_json::{json, Value};
    use std::sync::Arc;

    async fn request(module: &Methods, method: &str, params: Value) -> Value {
        let request = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        let (response, _) = module.raw_json_request(&request.to_string(), 1).await.unwrap();
        serde_json::from_str(response.get()).unwrap()
    }

    async fn assert_genesis(module: &Methods, genesis_hash: B256) {
        for (method, suffix) in [
            (METHODS[0], vec![json!(false)]),
            (METHODS[0], vec![json!(true)]),
            (METHODS[1], vec![]),
            (METHODS[2], vec![]),
        ] {
            let params = |tag| {
                let mut params = vec![json!(tag)];
                params.extend(suffix.clone());
                Value::Array(params)
            };
            let earliest = request(module, method, params("earliest")).await;
            let zero = request(module, method, params("0x0")).await;
            assert_eq!(earliest, zero, "{method}: earliest must behave exactly like 0x0");
            assert!(zero.get("error").is_none(), "{method}: {zero}");
            if method == METHODS[2] {
                assert_eq!(zero["result"], json!([]));
            } else {
                assert_eq!(zero["result"]["number"], "0x0");
                assert_eq!(zero["result"]["hash"], json!(genesis_hash));
            }
        }
    }

    #[tokio::test]
    async fn earliest_is_genesis_after_transaction_history_pruning_and_reopen() {
        let spec = Arc::new(BscChainSpec::from(bsc_mainnet()));
        let original = create_test_provider_factory_with_node_types::<BscNode>(spec.clone());
        let static_path = original.static_file_provider().directory().to_path_buf();
        let db = original.db_ref().clone();
        let rocks = original.rocksdb_provider();
        drop(original);
        let open = || {
            ProviderFactory::<reth::api::NodeTypesWithDBAdapter<BscNode, _>>::new(
                db.clone(),
                spec.clone(),
                StaticFileProviderBuilder::read_write(&static_path)
                    .with_blocks_per_file(10)
                    .build()
                    .unwrap(),
                rocks.clone(),
                reth_tasks::Runtime::test(),
            )
            .unwrap()
        };
        let factory = open();
        let mut header = spec.genesis_header().clone();
        let genesis_hash = header.hash_slow();
        let mut parent_hash = header.parent_hash;
        for number in 0..50 {
            header.number = number;
            header.parent_hash = parent_hash;
            let transactions = if number == 40 {
                vec![TransactionSigned::new_unhashed(
                    TxLegacy { nonce: number, gas_limit: 21_000, ..Default::default() }.into(),
                    Signature::test_signature(),
                )]
            } else {
                vec![]
            };
            let senders = transactions.iter().map(|tx| tx.recover_signer().unwrap()).collect();
            let block = RecoveredBlock::new_unhashed(
                BscBlock {
                    header: header.clone(),
                    body: BscBlockBody {
                        inner: alloy_consensus::BlockBody { transactions, ..Default::default() },
                        ..Default::default()
                    },
                },
                senders,
            );
            let writer = factory.provider_rw().unwrap();
            writer.insert_block(&block).unwrap();
            writer.commit().unwrap();
            parent_hash = block.hash();
        }

        // Before pruning, after pruning, and after reconstructing providers from disk.
        for phase in 0..3 {
            if phase == 1 {
                let removed = factory
                    .static_file_provider()
                    .delete_segment_below_block(StaticFileSegment::Transactions, 40)
                    .unwrap();
                assert_eq!(removed.len(), 4);
            }
            let storage = if phase == 2 { open() } else { factory.clone() };
            let provider =
                BlockchainProvider::with_latest(storage, SealedHeader::seal_slow(header.clone()))
                    .unwrap();
            let boundary = if phase == 0 { 0 } else { 40 };
            assert_eq!(provider.earliest_block_number().unwrap(), boundary);
            assert_eq!(
                provider.convert_block_number(BlockNumberOrTag::Earliest).unwrap(),
                Some(boundary),
            );
            let api = reth::rpc::eth::core::EthApi::builder(
                provider.clone(),
                testing_pool(),
                NoopNetwork::default(),
                BscEvmConfig::new(spec.clone()),
            )
            .build();
            let original: Methods = api.clone().into_rpc().into();
            if phase > 0 {
                // Prove the unmodified RPC follows the nonzero boundary in this fixture.
                let response = request(&original, METHODS[0], json!(["earliest", false])).await;
                assert_eq!(response["result"]["number"], "0x28", "{response}");
                let response = request(&original, METHODS[2], json!(["earliest"])).await;
                assert_eq!(response["result"].as_array().unwrap().len(), 1);
            }
            let module: Methods = BscBlockApiImpl(api).into_rpc().into();
            assert_genesis(&module, genesis_hash).await;

            // Other tags, explicit numbers, missing blocks, and malformed parameters retain
            // their normal behavior. Block 40 has a transaction to distinguish full/hash modes.
            for (tag, expected) in [("latest", "0x31"), ("0x28", "0x28")] {
                let response = request(&module, METHODS[0], json!([tag, false])).await;
                assert_eq!(response["result"]["number"], expected, "{response}");
            }
            let hashes = request(&module, METHODS[0], json!(["0x28", false])).await;
            let full = request(&module, METHODS[0], json!(["0x28", true])).await;
            assert!(hashes["result"]["transactions"][0].is_string());
            assert!(full["result"]["transactions"][0].is_object());
            for method in METHODS {
                let params =
                    if method == METHODS[0] { json!(["0xffff", false]) } else { json!(["0xffff"]) };
                assert_eq!(request(&module, method, params).await["result"], Value::Null);
                assert_eq!(
                    request(&module, method, json!(["invalid"])).await["error"]["code"],
                    -32602
                );
                for tag in ["latest", "pending", "safe", "finalized", "0x28", "0xffff"] {
                    let params =
                        if method == METHODS[0] { json!([tag, false]) } else { json!([tag]) };
                    assert_eq!(
                        request(&module, method, params.clone()).await,
                        request(&original, method, params).await,
                        "{method}: {tag} must retain the underlying RPC behavior",
                    );
                }
                // Retain upstream names for JSON-RPC named parameters (including `hash` on
                // getHeaderByNumber), while still interpreting earliest as block zero.
                let named = match method {
                    "eth_getBlockByNumber" => json!({"number": "earliest", "full": false}),
                    "eth_getHeaderByNumber" => json!({"hash": "earliest"}),
                    _ => json!({"number": "earliest"}),
                };
                let positional =
                    if method == METHODS[0] { json!(["0x0", false]) } else { json!(["0x0"]) };
                assert_eq!(
                    request(&module, method, named).await,
                    request(&original, method, positional).await,
                );
            }
            // Neither RPC normalization nor reads alter the storage pruning boundary.
            assert_eq!(provider.earliest_block_number().unwrap(), boundary);
        }
    }
}
