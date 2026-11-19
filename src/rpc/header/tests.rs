use super::*;
use crate::{
    chainspec::{bsc::bsc_mainnet, BscChainSpec},
    hardforks::bsc::BscHardfork,
    node::{
        evm::config::BscEvmConfig,
        primitives::{BscBlock, BscBlockBody, BscPrimitives},
    },
};
use alloy_consensus::{SignableTransaction, TxLegacy};
use alloy_primitives::{Address, Signature, B256, U256};
use reth_chainspec::ForkCondition;
use reth_ethereum_primitives::TransactionSigned;
use reth_network_api::noop::NoopNetwork;
use reth_primitives_traits::SealedHeader;
use reth_provider::test_utils::MockEthProvider;
use reth_rpc_convert::RpcConvert;
use reth_rpc_eth_api::{EthApiServer, EthApiTypes};

use reth_transaction_pool::test_utils::testing_pool;
use serde_json::{json, Value};
use std::sync::Arc;

#[tokio::test]
async fn block_and_header_rpc_responses_include_milliseconds_across_jenner() {
    const JENNER: u64 = 1_800_000_000;
    let mut chain = bsc_mainnet();
    chain.hardforks.insert(BscHardfork::Jenner, ForkCondition::Timestamp(JENNER));
    let spec = Arc::new(BscChainSpec::from(chain));
    let provider = MockEthProvider::<BscPrimitives, _>::new().with_chain_spec((*spec).clone());
    let mut blocks = Vec::new();
    for (offset, (timestamp, remainder)) in
        [(JENNER - 1, 0), (JENNER, 750), (JENNER + 1, 999)].into_iter().enumerate()
    {
        let tx: TransactionSigned = TxLegacy {
            chain_id: Some(56),
            nonce: offset as u64,
            gas_limit: 21_000,
            to: Address::repeat_byte(0x11).into(),
            ..Default::default()
        }
        .into_signed(Signature::new(U256::from(1), U256::from(1), false))
        .into();
        let block = BscBlock {
            header: alloy_consensus::Header {
                number: 40_000_000 + offset as u64,
                timestamp,
                mix_hash: B256::from(U256::from(remainder)),
                difficulty: U256::from(2),
                gas_limit: 140_000_000,
                base_fee_per_gas: Some(0),
                ..Default::default()
            },
            body: BscBlockBody {
                inner: alloy_consensus::BlockBody {
                    transactions: vec![tx.clone()],
                    ommers: vec![alloy_consensus::Header {
                        timestamp: JENNER,
                        mix_hash: B256::from(U256::from(750)),
                        ..Default::default()
                    }],
                    ..Default::default()
                },
                ..Default::default()
            },
        };
        let hash = block.header.hash_slow();
        provider.add_block(hash, block.clone());
        blocks.push((hash, block, tx));
    }
    let evm = BscEvmConfig::new(spec);
    let api = reth::rpc::eth::core::EthApi::builder(
        provider,
        testing_pool(),
        NoopNetwork::default(),
        evm.clone(),
    )
    .map_converter(|r| r.with_tx_env_converter(crate::rpc::transaction::BscTxEnvConverter(evm)))
    .build();
    // newHeads uses this converter directly, without the explicit-query wrapper.
    let notification = api
        .converter()
        .convert_header(SealedHeader::new_unhashed(blocks[1].1.header.clone()), Some(1234), None)
        .unwrap();
    assert!(serde_json::to_value(notification).unwrap().get("milliTimestamp").is_none());
    let mut module = api.clone().into_rpc();
    for method in METHODS {
        module.remove_method(method);
    }
    module.merge(BscBlockApiImpl(api).into_rpc()).unwrap();
    let api = module;

    for (offset, (hash, block, tx)) in blocks.into_iter().enumerate() {
        let header = block.header;
        let number = format!("0x{:x}", header.number);
        let milliseconds = [1_799_999_999_000u64, 1_800_000_000_750, 1_800_000_001_999][offset];
        for (method, params) in [
            ("eth_getUncleByBlockHashAndIndex", json!([hash, "0x0"])),
            ("eth_getUncleByBlockNumberAndIndex", json!([number, "0x0"])),
        ] {
            let request = json!({"jsonrpc":"2.0", "id":1, "method":method, "params":params});
            let (response, _) = api.raw_json_request(&request.to_string(), 1).await.unwrap();
            let response: Value = serde_json::from_str(response.get()).unwrap();
            assert!(response.get("error").is_none(), "{response}");
            let result = response["result"].as_object().expect("uncle header");
            assert!(!result.contains_key("milliTimestamp"));
        }
        for (method, params, full) in [
            ("eth_getBlockByNumber", json!([number, false]), Some(false)),
            ("eth_getBlockByNumber", json!([number, true]), Some(true)),
            ("eth_getBlockByHash", json!([hash, false]), Some(false)),
            ("eth_getBlockByHash", json!([hash, true]), Some(true)),
            ("eth_getHeaderByNumber", json!([number]), None),
            ("eth_getHeaderByHash", json!([hash]), None),
        ] {
            let request = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
            let (response, _) = api.raw_json_request(&request.to_string(), 1).await.unwrap();
            let response: Value = serde_json::from_str(response.get()).unwrap();
            assert!(response.get("error").is_none(), "{request}: {response}");
            let result = &response["result"];
            assert!(result.is_object(), "{request}: {response}");
            assert_eq!(result["milliTimestamp"], json!(format!("0x{milliseconds:x}")), "{request}");
            assert_eq!(result["timestamp"], json!(format!("0x{:x}", header.timestamp)));
            assert_eq!(result["mixHash"], json!(header.mix_hash));
            assert_eq!(result["number"], json!(number));
            assert_eq!(result["hash"], json!(hash));
            match full {
                Some(true) => assert_eq!(result["transactions"][0]["hash"], json!(tx.tx_hash())),
                Some(false) => assert_eq!(result["transactions"], json!([tx.tx_hash()])),
                None => assert!(result.get("transactions").is_none()),
            }
        }
    }

    let request =
        r#"{"jsonrpc":"2.0","id":1,"method":"eth_getBlockByNumber","params":["0xfffffff",false]}"#;
    let (response, _) = api.raw_json_request(request, 1).await.unwrap();
    let response: Value = serde_json::from_str(response.get()).unwrap();
    assert!(response.get("error").is_none(), "{response}");
    assert_eq!(response["result"], Value::Null);
}

#[test]
fn explicit_queries_add_milliseconds_without_changing_header_metadata() {
    for (seconds, remainder, expected) in [
        (0, 0, 0u64),
        (1_800_000_000, 999, 1_800_000_000_999),
        (u64::MAX, 123, 18_446_744_073_709_550_739),
    ] {
        for td in [None, Some(U256::from(456))] {
            let header = Header {
                inner: alloy_consensus::Header {
                    timestamp: seconds,
                    mix_hash: B256::from(U256::from(remainder)),
                    ..Default::default()
                },
                hash: B256::repeat_byte(0x42),
                total_difficulty: td,
                size: Some(U256::from(1234)),
            };
            let mut expected_json = serde_json::to_value(&header).unwrap();
            // The standard header is also used by newHeads and ommer responses.
            assert!(expected_json.get("milliTimestamp").is_none());
            expected_json["milliTimestamp"] = json!(format!("0x{expected:x}"));
            assert_eq!(
                serde_json::to_value(with_milliseconds(header.clone(), &header)).unwrap(),
                expected_json
            );
        }
    }
}
