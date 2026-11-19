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
use alloy_primitives::{Address, Signature, B256};
use reth_chainspec::ForkCondition;
use reth_ethereum_primitives::TransactionSigned;
use reth_network_api::noop::NoopNetwork;
use reth_provider::test_utils::MockEthProvider;
use reth_rpc_eth_api::EthApiServer;
use reth_transaction_pool::test_utils::testing_pool;
use serde_json::{json, Value};
use std::sync::Arc;

#[test]
fn conversion_adds_hex_milliseconds_and_preserves_standard_header_fields() {
    for (seconds, remainder, expected) in [
        (0, 0, 0u64),
        (1_800_000_000, 0, 1_800_000_000_000),
        (1_800_000_000, 1, 1_800_000_000_001),
        (1_800_000_000, 750, 1_800_000_000_750),
        (1_800_000_000, 999, 1_800_000_000_999),
        (u64::MAX, 123, 18_446_744_073_709_550_739),
    ] {
        for size in [None, Some(1234)] {
            for td in [None, Some(U256::from(456))] {
                let header = SealedHeader::new(
                    Header {
                        timestamp: seconds,
                        mix_hash: B256::from(U256::from(remainder)),
                        ..Default::default()
                    },
                    B256::repeat_byte(0x42),
                );
                let mut expected_json =
                    serde_json::to_value(alloy_rpc_types_eth::Header::from_consensus(
                        header.clone().into(),
                        td,
                        size.map(U256::from),
                    ))
                    .unwrap();
                expected_json["milliTimestamp"] = json!(format!("0x{expected:x}"));
                let converted = BscHeaderConverter.convert_header(header, size, td).unwrap();
                assert_eq!(serde_json::to_value(&converted).unwrap(), expected_json);
                let decoded: <BscRpcTypes as RpcTypes>::Header =
                    serde_json::from_value(expected_json).unwrap();
                assert_eq!(decoded, converted);
            }
        }
    }
}

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
            header: Header {
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
    .map_converter(|r| {
        r.with_network::<BscRpcTypes>()
            .with_header_converter(BscHeaderConverter)
            .with_tx_env_converter(crate::rpc::transaction::BscTxEnvConverter(evm))
    })
    .build()
    .into_rpc();

    for (offset, (hash, block, tx)) in blocks.into_iter().enumerate() {
        let header = block.header;
        let number = format!("0x{:x}", header.number);
        let milliseconds = [1_799_999_999_000u64, 1_800_000_000_750, 1_800_000_001_999][offset];
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
