use alloy_consensus::Header;
use alloy_primitives::U256;
use reth_bsc::{
    chainspec::{bsc::bsc_mainnet, BscChainSpec},
    node::{network::BscNetworkBuilder, BscNode},
};
use reth_ethereum_forks::Head;
use reth_node_builder::{common::WithConfigs, BuilderContext, FullNodeTypesAdapter, NodeConfig};
use reth_primitives_traits::SealedHeader;
use reth_provider::{
    providers::BlockchainProvider,
    test_utils::{create_test_provider_factory_with_node_types, insert_headers},
};
use reth_tasks::Runtime;
use std::sync::Arc;
use tokio::sync::{oneshot, Mutex};

#[tokio::test]
async fn startup_status_advertises_the_canonical_head_and_its_td() {
    let chain = Arc::new(BscChainSpec::from(bsc_mainnet()));
    let factory = create_test_provider_factory_with_node_types::<BscNode>(chain.clone());
    let genesis = chain.inner.genesis_header.clone();
    let tip = SealedHeader::seal_slow(Header {
        number: 1,
        parent_hash: genesis.hash(),
        difficulty: U256::from(2),
        ..Default::default()
    });
    let td = genesis.difficulty + tip.difficulty;
    insert_headers(&factory, &[genesis.clone(), tip.clone()]);

    let mut config = NodeConfig::new(chain.clone());
    config.datadir.datadir = factory.db_ref().path().to_path_buf().into();
    let provider = BlockchainProvider::with_latest(factory, tip.clone()).unwrap();
    let ctx = BuilderContext::<FullNodeTypesAdapter<BscNode, _, _>>::new(
        Head { number: tip.number, hash: tip.hash(), ..Default::default() },
        provider,
        Runtime::test(),
        WithConfigs { config, toml_config: Default::default() },
    );
    let (_engine_tx, engine_rx) = oneshot::channel();
    let network =
        BscNetworkBuilder::new(Arc::new(Mutex::new(Some(engine_rx)))).network_config(&ctx).unwrap();

    assert_eq!(network.status.blockhash, tip.hash());
    assert_eq!(network.status.total_difficulty, Some(td));
    assert_eq!(network.status.genesis, genesis.hash());
    assert_eq!(network.status.forkid, chain.inner.fork_filter(chain.head()).current());
}
