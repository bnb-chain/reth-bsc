use super::*;
use crate::{
    chainspec::{bsc::bsc_mainnet, BscChainSpec},
    node::{
        evm::config::BscEvmConfig,
        primitives::{BscBlock, BscBlockBody, BscPrimitives},
    },
    rpc::access_list::{BscAccessListApiImpl, BscAccessListApiServer},
};
use alloy_eips::eip2930::{AccessList, AccessListItem};
use alloy_primitives::Bytes;
use alloy_rpc_types_eth::{
    state::{AccountOverride, StateOverride},
    TransactionRequest,
};
use reth_chainspec::ForkCondition;
use reth_network_api::noop::NoopNetwork;
use reth_provider::test_utils::{ExtendedAccount, MockEthProvider};
use reth_rpc_eth_api::{helpers::EthCall, EthApiServer};
use reth_transaction_pool::test_utils::testing_pool;
use std::sync::Arc;

// Keep the concrete EthApi type inferred, as in rpc/block_overrides_tests.rs.
macro_rules! rpc {
    ($host:expr) => {{
        let mut chain = bsc_mainnet();
        chain.hardforks.insert(BscHardfork::Jenner, ForkCondition::Timestamp(NOW - 1));
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
        provider.add_block(header.hash_slow(), BscBlock { header, body: BscBlockBody::default() });
        let host = &$host;
        for addr in host.coded_accounts() {
            provider.add_account(
                addr,
                ExtendedAccount::new(0, U256::ZERO)
                    .with_bytecode(Bytes::from_static(&MARKER_CODE))
                    .extend_storage(
                        host.storage()
                            .filter(|(at, _, _)| *at == addr)
                            .map(|(_, key, value)| (B256::from(key), value)),
                    ),
            );
        }
        BscAccessListApiImpl(
            reth::rpc::eth::core::EthApi::builder(
                provider,
                testing_pool(),
                NoopNetwork::default(),
                BscEvmConfig::new(spec),
            )
            .build(),
        )
    }};
}

fn request(to: Address, data: Vec<u8>) -> TransactionRequest {
    TransactionRequest {
        from: Some(ALICE),
        to: Some(to.into()),
        gas: Some(1_000_000),
        input: Bytes::from(data).into(),
        ..Default::default()
    }
}

fn contains(list: &AccessList, address: Address, slot: U256) -> bool {
    list.0
        .iter()
        .any(|item| item.address == address && item.storage_keys.contains(&B256::from(slot)))
}

fn token(h: &mut Harness) -> Address {
    h.create(ALICE, VARIANT_ASSET, 500, ALICE, &[call_data(SEL_MINT, &[a(ALICE), w(123)])])
}

#[tokio::test]
async fn rpc_collects_native_balance_slot_and_replays_with_the_list() {
    let mut h = Harness::new();
    let token = token(&mut h);
    let api = rpc!(h.st);
    let mut req = request(token, call_data(SEL_BALANCE_OF, &[a(ALICE)]));
    let out = EthApiServer::call(&api.0, req.clone(), None, None, None).await.unwrap();
    assert_eq!(U256::from_be_slice(&out), U256::from(123));

    let result = api.create_access_list(req.clone(), None, None).await.unwrap();
    assert!(result.error.is_none(), "{:?}", result.error);
    let slot = mapping_slot(slot_at(SLOT_BALANCES), a(ALICE));
    assert_eq!(
        result.access_list.0,
        vec![AccessListItem { address: token, storage_keys: vec![B256::from(slot)] }]
    );

    req.access_list = Some(result.access_list.clone());
    let replay = api.0.create_access_list_at(req, None, None).await.unwrap();
    assert_eq!(replay.gas_used, result.gas_used);
    assert_eq!(replay.access_list, result.access_list);
}

#[tokio::test]
async fn rpc_collects_registry_reads_writes_and_preserves_initial_entries() {
    let mut h = Harness::new();
    let policy = h
        .call(
            ADMIN,
            POLICY_REGISTRY_ADDRESS,
            &call_data(SEL_CREATE_POLICY, &[a(ADMIN), w(TYPE_BLOCKLIST as u64)]),
        )
        .u256()
        .to::<u64>();
    let token = h.create(
        ALICE,
        VARIANT_ASSET,
        501,
        ALICE,
        &[
            call_data(SEL_MINT, &[a(ALICE), w(123)]),
            call_data(SEL_UPDATE_POLICY, &[SCOPE_TRANSFER_RECEIVER, w(policy)]),
        ],
    );
    let api = rpc!(h.st);
    let mut req = request(token, call_data(SEL_TRANSFER, &[a(BOB), w(1)]));
    req.access_list =
        Some(AccessList(vec![AccessListItem { address: ADMIN, storage_keys: vec![w(99)] }]));
    let result = api.create_access_list(req, None, None).await.unwrap();
    assert!(result.error.is_none(), "{:?}", result.error);
    let member =
        mapping_slot(mapping_slot(policy::pol_slot(policy::SLOT_MEMBERS), w(policy)), a(BOB));
    assert!(contains(&result.access_list, POLICY_REGISTRY_ADDRESS, member));
    assert!(contains(&result.access_list, token, mapping_slot(slot_at(SLOT_BALANCES), a(BOB))));
    assert!(contains(&result.access_list, ADMIN, U256::from(99)));

    let req = request(token, call_data(SEL_APPROVE, &[a(BOB), w(17)]));
    let result = api.create_access_list(req, None, None).await.unwrap();
    assert!(result.error.is_none());
    let allowance = mapping_slot(mapping_slot(slot_at(storage::SLOT_ALLOWANCES), a(ALICE)), a(BOB));
    assert!(contains(&result.access_list, token, allowance), "write-only slot must be recorded");
}

/// Forward calldata by CALL, then either revert or read the caller's slot zero and stop.
fn forward(target: Address, revert: bool) -> Bytes {
    let mut code =
        vec![0x36, 0x60, 0, 0x60, 0, 0x37, 0x60, 0, 0x60, 0, 0x36, 0x60, 0, 0x60, 0, 0x73];
    code.extend_from_slice(target.as_slice());
    code.extend([0x5a, 0xf1, 0x50]);
    if revert {
        code.extend([0x60, 0, 0x60, 0, 0xfd]);
    } else {
        code.extend([0x60, 0, 0x54, 0x50, 0x00]);
    }
    code.into()
}

#[tokio::test]
async fn rpc_retains_native_writes_from_reverted_children_and_opcode_accesses() {
    let mut h = Harness::new();
    let token = token(&mut h);
    let api = rpc!(h.st);
    let inner = Address::repeat_byte(0xc1);
    let outer = Address::repeat_byte(0xc2);
    let overrides: StateOverride = [
        (inner, AccountOverride { code: Some(forward(token, true)), ..Default::default() }),
        (outer, AccountOverride { code: Some(forward(inner, false)), ..Default::default() }),
    ]
    .into_iter()
    .collect();
    let req = request(outer, call_data(SEL_APPROVE, &[a(BOB), w(17)]));
    let result = api.create_access_list(req, None, Some(overrides.clone())).await.unwrap();
    assert!(result.error.is_none(), "outer call catches the child's revert");
    let allowance = mapping_slot(mapping_slot(slot_at(storage::SLOT_ALLOWANCES), a(inner)), a(BOB));
    assert!(contains(&result.access_list, token, allowance));
    assert!(contains(&result.access_list, outer, U256::ZERO));

    // A top-level revert must also return the accesses collected before it failed.
    let result = api
        .create_access_list(
            request(inner, call_data(SEL_APPROVE, &[a(BOB), w(17)])),
            None,
            Some(overrides),
        )
        .await
        .unwrap();
    assert!(result.error.is_some());
    assert!(contains(&result.access_list, token, allowance));
}

mod prestate;

#[tokio::test]
async fn rpc_collects_native_account_reads_and_creation() {
    let mut h = Harness::new();
    let token = token(&mut h);
    let api = rpc!(h.st);
    let result = api
        .create_access_list(
            request(FACTORY_ADDRESS, call_data(SEL_IS_CAS20_INITIALIZED, &[a(token)])),
            None,
            None,
        )
        .await
        .unwrap();
    assert!(result.error.is_none());
    assert!(result
        .access_list
        .0
        .contains(&AccessListItem { address: token, storage_keys: vec![] }));
    let created = derive_address(VARIANT_ASSET, ALICE, w(901));
    let result = api
        .create_access_list(
            request(FACTORY_ADDRESS, encode_create(VARIANT_ASSET, w(901), ALICE, &[])),
            None,
            None,
        )
        .await
        .unwrap();
    assert!(result.error.is_none(), "{:?}", result.error);
    assert!(contains(&result.access_list, created, slot_at(storage::SLOT_NAME)));
    let feature = mapping_slot(activation::act_slot(activation::SLOT_FEATURES), FEATURE_ASSET);
    assert!(contains(&result.access_list, ACTIVATION_REGISTRY_ADDRESS, feature));
}
