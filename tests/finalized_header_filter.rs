//! BSC RPC wiring for poll-based finalized header notifications.

use alloy_consensus::Header;
use alloy_primitives::B256;
use alloy_rpc_types_eth::FilterId;
use reth_bsc::{
    chainspec::{bsc::bsc_mainnet, BscChainSpec},
    node::{evm::config::BscEvmConfig, primitives::BscPrimitives},
};
use reth_network_api::noop::NoopNetwork;
use reth_provider::test_utils::MockEthProvider;
use reth_rpc::EthFilter;
use reth_rpc_eth_api::EthFilterApiServer;
use reth_tasks::Runtime;
use reth_transaction_pool::test_utils::testing_pool;
use std::sync::Arc;

#[tokio::test]
async fn finalized_header_filter_is_available_for_bsc() {
    let spec = Arc::new(BscChainSpec::from(bsc_mainnet()));
    let provider = MockEthProvider::<BscPrimitives, _>::new().with_chain_spec((*spec).clone());
    let initial =
        reth::primitives::SealedHeader::seal_slow(Header { number: 10, ..Default::default() });
    provider.chain_info_tracker.set_finalized(initial);
    let api = reth::rpc::eth::core::EthApi::builder(
        provider.clone(),
        testing_pool(),
        NoopNetwork::default(),
        BscEvmConfig::new(spec),
    )
    .build();
    // The node's RPC builder merges this same filter module into the eth namespace.
    let module = EthFilter::new(api, Default::default(), Runtime::test()).into_rpc();
    let id: FilterId =
        module.call("eth_newFinalizedHeaderFilter", jsonrpsee::rpc_params![]).await.unwrap();
    let empty: Vec<B256> =
        module.call("eth_getFilterChanges", jsonrpsee::rpc_params![id.clone()]).await.unwrap();
    assert!(empty.is_empty());

    let first =
        reth::primitives::SealedHeader::seal_slow(Header { number: 11, ..Default::default() });
    let second =
        reth::primitives::SealedHeader::seal_slow(Header { number: 15, ..Default::default() });
    provider.chain_info_tracker.set_finalized(first.clone());
    provider.chain_info_tracker.set_finalized(first.clone());
    provider.chain_info_tracker.set_finalized(second.clone());
    let hashes: Vec<B256> =
        module.call("eth_getFilterChanges", jsonrpsee::rpc_params![id.clone()]).await.unwrap();
    assert_eq!(hashes, vec![first.hash(), second.hash()]);
    let empty: Vec<B256> =
        module.call("eth_getFilterChanges", jsonrpsee::rpc_params![id.clone()]).await.unwrap();
    assert!(empty.is_empty());
    let removed: bool =
        module.call("eth_uninstallFilter", jsonrpsee::rpc_params![id.clone()]).await.unwrap();
    assert!(removed);
    assert!(module
        .call::<_, Vec<B256>>("eth_getFilterChanges", jsonrpsee::rpc_params![id])
        .await
        .is_err());
}
