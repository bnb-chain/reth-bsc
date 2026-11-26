use super::*;
use crate::{
    chainspec::{bsc::bsc_mainnet, BscChainSpec},
    node::{
        evm::config::BscEvmConfig,
        primitives::{BscBlock, BscBlockBody, BscPrimitives},
    },
    rpc::transaction::BscTxEnvConverter,
};
use alloy_eips::eip2930::{AccessList, AccessListItem};
use alloy_primitives::{hex, Address, Bytes, B256};
use alloy_rpc_types_eth::state::AccountOverride;
use reth_network_api::noop::NoopNetwork;
use reth_provider::test_utils::MockEthProvider;
use reth_transaction_pool::test_utils::testing_pool;
use std::sync::Arc;

const SENDER: Address = Address::repeat_byte(0x11);
const CONTRACT: Address = Address::repeat_byte(0xaa);

macro_rules! rpc {
    ($gas_cap:expr, $block_limit:expr, $memory_limit:expr) => {{
        let spec = Arc::new(BscChainSpec::from(bsc_mainnet()));
        let provider = MockEthProvider::<BscPrimitives, _>::new().with_chain_spec((*spec).clone());
        let header = alloy_consensus::Header {
            number: 40_000_000,
            timestamp: 1_800_000_000,
            gas_limit: $block_limit,
            base_fee_per_gas: Some(0),
            excess_blob_gas: Some(0),
            blob_gas_used: Some(0),
            ..Default::default()
        };
        provider.add_block(header.hash_slow(), BscBlock { header, body: BscBlockBody::default() });
        let evm_config = BscEvmConfig::new(spec);
        BscAccessListApiImpl(
            reth::rpc::eth::core::EthApi::builder(
                provider,
                testing_pool(),
                NoopNetwork::default(),
                evm_config.clone(),
            )
            .map_converter(|r| r.with_tx_env_converter(BscTxEnvConverter(evm_config)))
            .gas_cap($gas_cap.into())
            .evm_memory_limit($memory_limit)
            .build(),
        )
    }};
}

#[tokio::test]
async fn access_list_respects_rpc_gas_cap() {
    // GAS; SLOAD; POP; STOP. The returned storage key exposes the first execution's
    // actual gas budget without consuming that budget or allocating large memory.
    let code = Bytes::from_static(&hex!("5a545000"));

    // RPC cap, supplied gas, price, balance after value, block limit, expected budget.
    for (gas_cap, gas, price, balance, block_limit, budget) in [
        (50_000u64, Some(u64::MAX), 0u128, 100_000u64, 100_000u64, 50_000u64),
        (50_000, Some(70_000), 1, 100_000, 100_000, 50_000),
        (50_000, Some(30_000), 1, 100_000, 100_000, 30_000),
        (50_000, None, 0, 100_000, 100_000, 50_000),
        (50_000, None, 1, 100_000, 100_000, 50_000),
        (50_000, None, 1, 40_000, 100_000, 40_000),
        (50_000, None, 1, 100_000, 40_000, 40_000),
        // Explicit gas may exceed the block limit, but must respect the RPC cap.
        (50_000, Some(70_000), 1, 100_000, 40_000, 50_000),
        // Preserve the API builder's explicitly disabled cap.
        (0, None, 1, 100_000, 100_000, 100_000),
        (0, Some(70_000), 1, 100_000, 40_000, 70_000),
    ] {
        let api = rpc!(gas_cap, block_limit, (1 << 32) - 1);
        let state = StateOverride::from_iter([
            (
                SENDER,
                AccountOverride {
                    balance: Some(U256::from(balance + 1_000)),
                    ..Default::default()
                },
            ),
            (CONTRACT, AccountOverride { code: Some(code.clone()), ..Default::default() }),
        ]);
        for dynamic_fee in [false, true] {
            let request = TransactionRequest {
                from: Some(SENDER),
                to: Some(CONTRACT.into()),
                gas,
                gas_price: (!dynamic_fee).then_some(price),
                max_fee_per_gas: dynamic_fee.then_some(price),
                max_priority_fee_per_gas: dynamic_fee.then_some(price),
                value: Some(U256::from(1_000)),
                ..Default::default()
            };
            let result = api.create_access_list(request, None, Some(state.clone())).await.unwrap();
            assert!(result.error.is_none(), "{result:?}");
            assert_eq!(
                result.access_list,
                AccessList(vec![AccessListItem {
                    address: CONTRACT,
                    storage_keys: vec![B256::from(U256::from(budget - 21_002))],
                }]),
                "cap={gas_cap}, gas={gas:?}, price={price}, balance={balance}, \
                 block={block_limit}, dynamic_fee={dynamic_fee}"
            );
            // gasUsed comes from the second execution, with the generated list applied.
            assert!(result.gas_used > U256::from(21_000));
            assert!(result.gas_used <= U256::from(budget));
        }
    }
}

#[tokio::test]
async fn access_list_respects_rpc_memory_limit() {
    // MSTORE at offset 1024 needs 1056 bytes. Both cases have ample gas; only
    // the configured EVM memory limit should determine whether this succeeds.
    let state = StateOverride::from_iter([(
        CONTRACT,
        AccountOverride {
            code: Some(Bytes::from_static(&hex!("60006104005200"))),
            ..Default::default()
        },
    )]);
    for (memory_limit, succeeds) in [(512, false), (2048, true)] {
        let api = rpc!(50_000u64, 100_000, memory_limit);
        let request = TransactionRequest {
            from: Some(SENDER),
            to: Some(CONTRACT.into()),
            gas: Some(50_000),
            ..Default::default()
        };
        let result = api.create_access_list(request, None, Some(state.clone())).await.unwrap();
        if succeeds {
            assert!(result.error.is_none(), "{result:?}");
            assert!(result.gas_used < U256::from(50_000));
        } else {
            assert!(
                result
                    .error
                    .as_deref()
                    .is_some_and(|error| error.contains("memory limit exceeded")),
                "{result:?}"
            );
        }
    }
}
