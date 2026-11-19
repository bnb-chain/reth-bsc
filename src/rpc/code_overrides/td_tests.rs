use super::*;
use crate::node::BscNode;
use alloy_consensus::Header;
use reth_chain_state::{ExecutedBlock, NewCanonicalChain};
use reth_primitives_traits::{Block as _, SealedBlock};
use reth_provider::{
    providers::BlockchainProvider, test_utils::create_test_provider_factory_with_node_types,
    BlockReader, BlockWriter, CanonChainTracker, HeaderProvider,
};
use serde_json::{json, Value};

fn child(parent: &SealedBlock<BscBlock>, difficulty: u64) -> SealedBlock<BscBlock> {
    BscBlock {
        header: Header {
            number: parent.number + 1,
            parent_hash: parent.hash(),
            difficulty: U256::from(difficulty),
            timestamp: NOW + parent.number + 1,
            gas_limit: 140_000_000,
            base_fee_per_gas: Some(0),
            excess_blob_gas: Some(0),
            blob_gas_used: Some(0),
            ..Default::default()
        },
        body: BscBlockBody::default(),
    }
    .seal_slow()
}

#[tokio::test]
async fn simulation_td_follows_memory_branch_before_durable_reorg() {
    let spec = Arc::new(BscChainSpec::from(forks_at(&[BscHardfork::Jenner], NOW - 1)));
    let factory = create_test_provider_factory_with_node_types::<BscNode>(spec.clone());
    reth_db_common::init::init_genesis(&factory).unwrap();
    let genesis = BscBlock { header: spec.genesis_header().clone(), body: BscBlockBody::default() }
        .seal_slow();
    let common = child(&genesis, 2);
    let old_one = child(&common, 1);
    let old_tip = child(&old_one, 1);
    let new_one = child(&common, 2);
    let new_tip = child(&new_one, 2);
    let writer = factory.provider_rw().unwrap();
    for block in [&common, &old_one, &old_tip] {
        writer.insert_block(&block.clone().try_recover().unwrap()).unwrap();
    }
    writer.commit().unwrap();
    let provider = BlockchainProvider::new(factory.clone()).unwrap();
    provider.canonical_in_memory_state().update_chain(NewCanonicalChain::Commit {
        new: [&new_one, &new_tip]
            .into_iter()
            .map(|block| ExecutedBlock {
                recovered_block: Arc::new(block.clone().try_recover().unwrap()),
                ..Default::default()
            })
            .collect(),
    });
    provider.set_canonical_head(new_tip.sealed_header().clone());
    for block in [&common, &new_one, &new_tip] {
        crate::node::evm::util::insert_header_to_cache(block.header().clone());
    }

    let anchor_td =
        factory.provider().unwrap().header_td_by_number(common.number).unwrap().unwrap();
    let old_td = anchor_td + U256::from(2);
    let new_td = anchor_td + U256::from(4);
    // The height lookup still sees the old disk branch, despite receiving the new hash.
    assert_eq!(provider.header_td(&new_tip.hash()).unwrap(), Some(old_td));
    assert_eq!(provider.block_with_td_by_hash(new_tip.hash()).unwrap().unwrap().1, new_td);
    assert!(provider.block_with_td_by_hash(old_tip.hash()).unwrap().is_none());

    let evm = BscEvmConfig::new(spec);
    let api = reth::rpc::eth::core::EthApi::builder(
        provider,
        testing_pool(),
        NoopNetwork::default(),
        evm.clone(),
    )
    .map_converter(|r| r.with_tx_env_converter(crate::rpc::transaction::BscTxEnvConverter(evm)))
    .build();
    let module = BscCodeOverridesApiImpl(api).into_rpc();
    for base in [json!(new_tip.hash()), json!(format!("0x{:x}", new_tip.number)), json!("latest")] {
        for full in [false, true] {
            let request = json!({"jsonrpc":"2.0", "id":1, "method":"eth_simulateV1", "params":[{
                "blockStateCalls":[
                    {"blockOverrides":{"number":format!("0x{:x}",new_tip.number + 2)},"calls":[]},
                    {"calls":[]}
                ], "validation":false, "returnFullTransactions":full
            }, base]});
            let (response, _) = module.raw_json_request(&request.to_string(), 1).await.unwrap();
            let response: Value = serde_json::from_str(response.get()).unwrap();
            assert!(response.get("error").is_none(), "{request}: {response}");
            let blocks = response["result"].as_array().unwrap();
            assert_eq!(blocks.len(), 3, "the generated gap block also contributes difficulty");
            for (index, block) in blocks.iter().enumerate() {
                let expected_td = new_td + U256::from(2 * (index + 1));
                assert_eq!(
                    block["totalDifficulty"],
                    format!("0x{expected_td:x}"),
                    "{request}: {response}"
                );
                assert_eq!(block["difficulty"], "0x2");
                assert_eq!(block["number"], format!("0x{:x}", new_tip.number + index as u64 + 1));
                assert!(block.get("milliTimestamp").is_none());
            }
            assert_eq!(blocks[0]["parentHash"], json!(new_tip.hash()));
        }
    }
}
