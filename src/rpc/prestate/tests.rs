use super::*;
use crate::evm::precompiles::cas20::FACTORY_ADDRESS;
use alloy_consensus::{
    crypto::secp256k1::sign_message, transaction::SignerRecoverable, SignableTransaction, TxLegacy,
};
use alloy_primitives::{Address, U256};
use alloy_rpc_types::trace::geth::PreStateMode;
use reth_ethereum_primitives::{EthPrimitives, TransactionSigned};
use reth_evm_ethereum::EthEvmConfig;
use reth_network_api::noop::NoopNetwork;
use reth_provider::test_utils::{ExtendedAccount, MockEthProvider};
use reth_transaction_pool::test_utils::testing_pool;
use serde_json::json;

#[test]
fn empty_filter_preserves_existing_code_and_non_cas20_accounts() {
    let existing =
        Address::from_slice(&alloy_primitives::hex!("ca52000000000000000000000000000000000001"));
    let empty =
        Address::from_slice(&alloy_primitives::hex!("ca52000000000000000000000000000000000002"));
    let other = Address::repeat_byte(0x11);
    let mut trace = GethTrace::PreStateTracer(PreStateFrame::Default(PreStateMode(
        [
            (
                existing,
                AccountState { code: Some(Bytes::from_static(&[0xef])), ..Default::default() },
            ),
            (empty, AccountState::default()),
            (other, AccountState::default()),
        ]
        .into_iter()
        .collect(),
    )));
    Plan::Prestate { hide_code: true, include_empty: false }.apply(&mut trace);
    let value = serde_json::to_value(trace).unwrap();
    assert!(value.get(format!("{existing:#x}")).is_some());
    assert!(value[format!("{existing:#x}")].get("code").is_none());
    assert!(value.get(format!("{empty:#x}")).is_none());
    assert!(value.get(format!("{other:#x}")).is_some());
}

#[test]
fn prestate_defaults_aliases_and_validation_use_shared_parser() {
    for config in [json!(null), json!({})] {
        let mut opts: GethDebugTracingOptions =
            serde_json::from_value(json!({"tracer":"prestateTracer","tracerConfig":config}))
                .unwrap();
        assert!(matches!(
            Plan::prepare(&mut opts).unwrap(),
            Plan::Prestate { hide_code: false, include_empty: false }
        ));
    }
    for (empty_key, code_key) in [
        ("includeEmpty", "disableCode"),
        ("IncludeEmpty", "DisableCode"),
        ("INCLUDEEMPTY", "DISABLECODE"),
    ] {
        let mut opts: GethDebugTracingOptions = serde_json::from_value(
            json!({"tracer":"prestateTracer","tracerConfig":{(code_key):true,(empty_key):true}}),
        )
        .unwrap();
        let plan = Plan::prepare(&mut opts).unwrap();
        assert_eq!(opts.tracer_config.0["disableCode"], false);
        let mut trace = GethTrace::PreStateTracer(PreStateFrame::Default(PreStateMode(
            [(FACTORY_ADDRESS, AccountState::default())].into_iter().collect(),
        )));
        plan.apply(&mut trace);
        assert!(serde_json::to_value(trace)
            .unwrap()
            .get(format!("{FACTORY_ADDRESS:#x}"))
            .is_some());
        opts.tracer_config.0 = json!({(code_key):"invalid"});
        let error = Plan::prepare(&mut opts).err().unwrap();
        assert_eq!(error.code(), -32000);
        assert_eq!(error.message(), "json: cannot unmarshal string into Go struct field PrestateTracerConfig.disableCode of type bool");
    }
}

#[tokio::test]
async fn transaction_and_block_traces_include_empty_prestate() {
    let recipient = Address::repeat_byte(0x91);
    // Exercise the generic DebugApi replay and the BSC response wrapper without Parlia validation.
    // BSC EVM execution is covered by the native-transfer call tests.
    let spec = reth_chainspec::MAINNET.clone();
    let provider = MockEthProvider::<EthPrimitives, _>::new().with_chain_spec((*spec).clone());
    let transactions = (0..2)
        .map(|nonce| {
            let tx = TxLegacy {
                chain_id: Some(1),
                nonce,
                gas_limit: 21_000,
                to: recipient.into(),
                value: U256::from(1),
                ..Default::default()
            };
            let signature = sign_message(B256::repeat_byte(1), tx.signature_hash()).unwrap();
            TransactionSigned::from(tx.into_signed(signature))
        })
        .collect::<Vec<_>>();
    let sender = transactions[0].recover_signer().unwrap();
    provider.add_account(sender, ExtendedAccount::new(0, U256::from(100)));
    let parent = alloy_consensus::Block {
        header: alloy_consensus::Header {
            number: 40_000_000,
            timestamp: 1_800_000_000,
            gas_limit: 140_000_000,
            base_fee_per_gas: Some(0),
            excess_blob_gas: Some(0),
            blob_gas_used: Some(0),
            parent_beacon_block_root: Some(B256::ZERO),
            withdrawals_root: Some(alloy_consensus::proofs::calculate_withdrawals_root(&[])),
            ..Default::default()
        },
        body: alloy_consensus::BlockBody {
            withdrawals: Some(Default::default()),
            ..Default::default()
        },
    };
    let parent_hash = parent.header.hash_slow();
    provider.add_block(parent_hash, parent.clone());
    let block = alloy_consensus::Block {
        header: alloy_consensus::Header {
            parent_hash,
            number: parent.header.number + 1,
            timestamp: parent.header.timestamp + 1,
            transactions_root: alloy_consensus::proofs::calculate_transaction_root(&transactions),
            gas_used: 42_000,
            ..parent.header
        },
        body: alloy_consensus::BlockBody {
            transactions: transactions.clone(),
            withdrawals: Some(Default::default()),
            ..Default::default()
        },
    };
    let block_hash = block.header.hash_slow();
    provider.add_block(block_hash, block.clone());
    let evm = EthEvmConfig::new(spec);
    let eth = reth::rpc::eth::core::EthApi::builder(
        provider,
        testing_pool(),
        NoopNetwork::default(),
        evm,
    )
    .build();
    let module = BscPrestateApiImpl(reth_rpc::DebugApi::new(
        eth,
        reth_tasks::pool::BlockingTaskGuard::new(4),
        &reth_tasks::Runtime::test(),
        futures::stream::empty(),
    ))
    .into_rpc();
    for mux in [false, true] {
        for include_empty in [None, Some(false), Some(true)] {
            let mut config = json!({"disableStorage":true});
            if let Some(include_empty) = include_empty {
                config["includeEmpty"] = include_empty.into();
            }
            let opts = if mux {
                json!({"tracer":"muxTracer","tracerConfig":{"prestateTracer":config,"callTracer":{}}})
            } else {
                json!({"tracer":"prestateTracer","tracerConfig":config})
            };
            let mut indexed_opts = opts.clone();
            indexed_opts["txIndex"] = "0x0".into();
            let call = json!({"from":sender,"to":recipient,"value":"0x1","gas":"0x5208"});
            let unwrap_prestate = |value: serde_json::Value| {
                if mux {
                    value["prestateTracer"].clone()
                } else {
                    value
                }
            };
            for (method, params, block_response) in [
                ("debug_traceCall", json!([call, block_hash, indexed_opts]), false),
                ("debug_traceTransaction", json!([transactions[0].tx_hash(), opts]), false),
                ("debug_traceBlockByHash", json!([block_hash, opts]), true),
                (
                    "debug_traceBlockByNumber",
                    json!([format!("0x{:x}", block.header.number), opts]),
                    true,
                ),
                ("debug_traceBlock", json!([Bytes::from(alloy_rlp::encode(&block)), opts]), true),
            ] {
                let request = json!({"jsonrpc":"2.0","id":1,"method":method,"params":params});
                let (response, _) = module.raw_json_request(&request.to_string(), 1).await.unwrap();
                let response: serde_json::Value = serde_json::from_str(response.get()).unwrap();
                assert!(response.get("error").is_none(), "{method}: {response}");
                let result = &response["result"];
                let first = unwrap_prestate(if block_response {
                    result[0]["result"].clone()
                } else {
                    result.clone()
                });
                let account = first.get(format!("{recipient:#x}"));
                assert_eq!(account.is_some(), include_empty == Some(true), "{method}: {response}");
                if let Some(account) = account {
                    assert_eq!(account, &json!({"balance":"0x0"}));
                }
                if block_response {
                    let second = unwrap_prestate(result[1]["result"].clone());
                    assert_eq!(
                        second[format!("{recipient:#x}")]["balance"],
                        "0x1",
                        "{method}: {response}"
                    );
                }
            }
            let request = json!({"jsonrpc":"2.0","id":1,"method":"debug_traceTransaction","params":[transactions[1].tx_hash(),opts]});
            let (response, _) = module.raw_json_request(&request.to_string(), 1).await.unwrap();
            let response: serde_json::Value = serde_json::from_str(response.get()).unwrap();
            assert!(response.get("error").is_none(), "{response}");
            let second = unwrap_prestate(response["result"].clone());
            assert_eq!(second[format!("{recipient:#x}")]["balance"], "0x1");
        }
    }
}
