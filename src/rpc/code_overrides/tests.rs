use super::*;
use crate::{
    chainspec::{bsc::bsc_mainnet, BscChainSpec},
    evm::precompiles::cas20::{
        ACTIVATION_REGISTRY_ADDRESS, FACTORY_ADDRESS, MARKER_CODE, POLICY_REGISTRY_ADDRESS,
    },
    hardforks::bsc::BscHardfork,
    node::{
        evm::config::BscEvmConfig,
        primitives::{BscBlock, BscBlockBody, BscPrimitives},
    },
    rpc::access_list::{BscAccessListApiImpl, BscAccessListApiServer},
};
use alloy_primitives::{address, hex, keccak256};
use alloy_rpc_types_eth::state::AccountOverride;
use reth_chainspec::ForkCondition;
use reth_network_api::noop::NoopNetwork;
use reth_provider::test_utils::{ExtendedAccount, MockEthProvider};
use reth_transaction_pool::test_utils::testing_pool;
use std::sync::Arc;

const TOKEN: Address = address!("ca52000000000000000000000000000000000001");
const RESERVED: Address = address!("ca52000000000000000000000000000000000002");
const STABLE: Address = address!("ca52000000000000000001000000000000000001");
const PROXY: Address = Address::repeat_byte(0x11);
const RETURNS_42: &[u8] = &hex!("602a60005260206000f3");
const TARGETS: [Address; 6] = [
    TOKEN,
    RESERVED,
    STABLE,
    FACTORY_ADDRESS,
    ACTIVATION_REGISTRY_ADDRESS,
    POLICY_REGISTRY_ADDRESS,
];
const NOW: u64 = 1_800_000_000;

macro_rules! rpc {
    ($jenner:expr) => {{
        let mut chain = bsc_mainnet();
        chain.hardforks.insert(BscHardfork::Jenner, ForkCondition::Timestamp($jenner));
        let spec = Arc::new(BscChainSpec::from(chain));
        let provider = MockEthProvider::<BscPrimitives, _>::new().with_chain_spec((*spec).clone());
        let header = alloy_consensus::Header {
            number: 40_000_000,
            timestamp: NOW,
            gas_limit: 140_000_000,
            base_fee_per_gas: Some(0),
            excess_blob_gas: Some(0),
            blob_gas_used: Some(0),
            ..Default::default()
        };
        let hash = header.hash_slow();
        crate::node::evm::util::insert_header_to_cache(header.clone());
        provider.add_header(hash, header.clone());
        provider.add_block(hash, BscBlock { header, body: BscBlockBody::default() });
        for target in TARGETS.into_iter().filter(|target| *target != RESERVED) {
            provider.add_account(
                target,
                ExtendedAccount::new(0, U256::ZERO).with_bytecode(Bytes::from_static(&MARKER_CODE)),
            );
        }
        (
            BscCodeOverridesApiImpl(
                reth::rpc::eth::core::EthApi::builder(
                    provider,
                    testing_pool(),
                    NoopNetwork::default(),
                    BscEvmConfig::new(spec),
                )
                .build(),
            ),
            hash,
        )
    }};
}

fn request(target: Address, input: &[u8]) -> TransactionRequest {
    TransactionRequest {
        to: Some(target.into()),
        gas: Some(100_000),
        input: Bytes::copy_from_slice(input).into(),
        ..Default::default()
    }
}
fn overrides(target: Address, code: &[u8]) -> StateOverride {
    [(target, AccountOverride { code: Some(Bytes::copy_from_slice(code)), ..Default::default() })]
        .into_iter()
        .collect()
}
fn value(out: &Bytes) -> U256 {
    assert_eq!(out.len(), 32);
    U256::from_be_slice(out)
}

// Port of go-bsc TestCAS20CodeOverrideRunsTheOverride and TestCAS20CodeOverrideThroughRPC.
#[tokio::test]
async fn code_override_runs_on_every_cas20_route_through_rpc() {
    let (api, hash) = rpc!(NOW - 1);
    for target in TARGETS {
        let state = overrides(target, RETURNS_42);
        let req = request(target, &[]);
        assert_eq!(
            value(
                &api.call(req.clone(), Some(hash.into()), Some(state.clone()), None).await.unwrap()
            ),
            U256::from(42),
            "{target}"
        );
        let gas =
            api.estimate_gas(req.clone(), Some(hash.into()), Some(state.clone())).await.unwrap();
        assert!(gas >= U256::from(21_000));
        let list = BscAccessListApiImpl(api.0.clone())
            .create_access_list(req, Some(hash.into()), Some(state))
            .await
            .unwrap();
        assert!(list.error.is_none(), "{target}: {:?}", list.error);
        assert!(list.access_list.0.is_empty());
    }
}

#[tokio::test]
async fn empty_code_and_marker_code_are_explicit_overrides() {
    let (api, _) = rpc!(NOW - 1);
    for target in TARGETS {
        let req = request(target, &hex!("18160ddd"));
        let out = api.call(req.clone(), None, Some(overrides(target, &[])), None).await.unwrap();
        assert!(out.is_empty(), "{target}");
        let state = overrides(target, &[]);
        let gas = api.estimate_gas(request(target, &[]), None, Some(state.clone())).await.unwrap();
        // Estimation may include a margin; the returned limit must execute the
        // overridden empty account successfully instead of entering CAS20.
        let mut estimated = request(target, &[]);
        estimated.gas = Some(gas.to());
        assert!(api.call(estimated, None, Some(state.clone()), None).await.unwrap().is_empty());
        let list = BscAccessListApiImpl(api.0.clone())
            .create_access_list(request(target, &[]), None, Some(state))
            .await
            .unwrap();
        assert!(list.error.is_none());
        assert!(list.access_list.0.is_empty());
        // Explicitly overriding with the sentinel must execute 0xEF, not native totalSupply.
        let err =
            api.call(req, None, Some(overrides(target, &MARKER_CODE)), None).await.unwrap_err();
        assert!(!err.to_string().contains("execution reverted"), "{err}");
    }
}

#[tokio::test]
async fn storage_nonce_and_balance_overrides_keep_native_routing() {
    let (api, _) = rpc!(NOW - 1);
    let inner = U256::from_be_bytes(keccak256("bsc.cas20").0) - U256::from(1);
    let mut root = keccak256(inner.to_be_bytes::<32>());
    root.0[31] = 0;
    let supply = B256::from(U256::from_be_bytes(root.0) + U256::from(3));
    let state = [(
        TOKEN,
        AccountOverride {
            nonce: Some(9),
            balance: Some(U256::from(100)),
            state_diff: Some([(supply, B256::from(U256::from(77)))].into_iter().collect()),
            ..Default::default()
        },
    )]
    .into_iter()
    .collect();
    assert_eq!(
        value(&api.call(request(TOKEN, &hex!("18160ddd")), None, Some(state), None).await.unwrap()),
        U256::from(77)
    );
    // No override in the next request: canonical storage and native routing both survive.
    assert_eq!(
        value(&api.call(request(TOKEN, &hex!("18160ddd")), None, None, None).await.unwrap()),
        U256::ZERO
    );
    assert!(api
        .call(request(TOKEN, &[]), None, None, None)
        .await
        .unwrap_err()
        .to_string()
        .contains("execution reverted"));
}

#[tokio::test]
async fn nested_calls_use_the_overridden_target() {
    let (api, _) = rpc!(NOW - 1);
    for target in TARGETS {
        let mut code = hex!("36600060003760206000366000").to_vec();
        code.push(0x73);
        code.extend_from_slice(target.as_slice());
        code.extend_from_slice(&hex!("5afa5060206000f3"));
        let mut state = overrides(target, RETURNS_42);
        state.extend(overrides(PROXY, &code));
        let out = api.call(request(PROXY, &[]), None, Some(state), None).await.unwrap();
        assert_eq!(value(&out), U256::from(42), "{target}");
    }
}

#[tokio::test]
async fn call_many_preserves_code_override_across_transactions_and_bundles() {
    let (api, _) = rpc!(NOW - 1);
    let bundle = Bundle {
        transactions: vec![request(TOKEN, &[]), request(TOKEN, &[])],
        block_override: None,
    };
    let out = api
        .call_many(vec![bundle.clone(), bundle], None, Some(overrides(TOKEN, RETURNS_42)))
        .await
        .unwrap();
    assert_eq!(out.len(), 2);
    for result in out.into_iter().flatten() {
        assert!(result.error.is_none());
        assert_eq!(value(&result.value.unwrap()), U256::from(42));
    }
}

#[tokio::test]
async fn code_overrides_do_not_leak_between_concurrent_requests() {
    let (api, _) = rpc!(NOW - 1);
    for _ in 0..4 {
        let (overridden, native) = tokio::join!(
            api.call(
                request(TOKEN, &hex!("18160ddd")),
                None,
                Some(overrides(TOKEN, RETURNS_42)),
                None
            ),
            api.call(request(TOKEN, &hex!("18160ddd")), None, None, None)
        );
        assert_eq!(value(&overridden.unwrap()), U256::from(42));
        assert_eq!(value(&native.unwrap()), U256::ZERO);
    }
}

#[tokio::test]
async fn pre_jenner_code_override_still_executes_bytecode() {
    let (api, _) = rpc!(NOW + 100);
    for target in TARGETS {
        assert_eq!(
            value(
                &api.call(request(target, &[]), None, Some(overrides(target, RETURNS_42)), None)
                    .await
                    .unwrap()
            ),
            U256::from(42)
        );
    }
}

#[tokio::test]
async fn simulation_code_override_scope_matches_geth_per_block() {
    let (api, _) = rpc!(NOW - 1);
    for trace_transfers in [false, true] {
        let native = SimBlock {
            block_overrides: None,
            state_overrides: None,
            calls: vec![request(TOKEN, &hex!("18160ddd"))],
        };
        let replaced =
            SimBlock { state_overrides: Some(overrides(TOKEN, RETURNS_42)), ..native.clone() };
        let storage_only = SimBlock {
            state_overrides: Some(
                [(TOKEN, AccountOverride { balance: Some(U256::from(1)), ..Default::default() })]
                    .into_iter()
                    .collect(),
            ),
            ..native.clone()
        };
        let blocks = [native, replaced.clone(), storage_only, replaced];
        // BSC's existing simulation executor resolves synthetic parents through its
        // header cache. Seed each simulated parent, without changing production state.
        for count in 1..=4 {
            let payload = SimulatePayload {
                block_state_calls: blocks[..count].to_vec(),
                trace_transfers,
                validation: false,
                return_full_transactions: false,
            };
            let result = api.simulate_v1(payload, None).await.unwrap();
            assert_eq!(result.len(), count);
            for (i, block) in result.iter().enumerate() {
                if i == 2 {
                    // Geth rebuilds native routing for each block. The earlier code
                    // remains in state, so this token no longer has its sentinel and
                    // the native existence check reverts. A storage-only override
                    // must not be mistaken for another code override.
                    assert!(!block.calls[0].status);
                    assert!(block.calls[0].return_data.is_empty());
                } else {
                    assert!(block.calls[0].status, "{:?}", block.calls[0].error);
                    assert_eq!(
                        value(&block.calls[0].return_data),
                        U256::from(if i == 0 { 0 } else { 42 })
                    );
                }
                crate::node::evm::util::insert_header_to_cache(block.inner.header.inner.clone());
            }
        }
    }
}

#[tokio::test]
async fn access_list_records_the_override_bytecodes_storage() {
    let (api, _) = rpc!(NOW - 1);
    let code = hex!("60095460005260206000f3");
    let mut state = overrides(TOKEN, &code);
    state.get_mut(&TOKEN).unwrap().state_diff =
        Some([(B256::from(U256::from(9)), B256::from(U256::from(99)))].into_iter().collect());
    let out = api.call(request(TOKEN, &[]), None, Some(state.clone()), None).await.unwrap();
    assert_eq!(value(&out), U256::from(99));
    let list = BscAccessListApiImpl(api.0)
        .create_access_list(request(TOKEN, &[]), None, Some(state))
        .await
        .unwrap();
    assert!(list.error.is_none());
    assert_eq!(list.access_list.0.len(), 1);
    assert_eq!(list.access_list.0[0].address, TOKEN);
    assert_eq!(list.access_list.0[0].storage_keys, vec![B256::from(U256::from(9))]);
}

#[tokio::test]
async fn rpc_registration_preserves_named_call_parameters_and_block_overrides() {
    let (api, _) = rpc!(NOW - 1);
    let module = api.into_rpc();
    let message = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "eth_call",
        "params": {
            "request": request(TOKEN, &[]),
            "block_number": "latest",
            "state_overrides": overrides(TOKEN, &hex!("4260005260206000f3")),
            "block_overrides": { "time": NOW + 17 }
        }
    });
    let (response, _) = module.raw_json_request(&message.to_string(), 1).await.unwrap();
    let response: serde_json::Value = serde_json::from_str(response.get()).unwrap();
    let bytes: Bytes = serde_json::from_value(response["result"].clone()).unwrap();
    assert_eq!(value(&bytes), U256::from(NOW + 17));
}
