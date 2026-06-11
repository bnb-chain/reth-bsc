//! Chain specification for BSC, credits to: <https://github.com/bnb-chain/reth/blob/main/crates/bsc/chainspec/src/bsc_chapel.rs>
use crate::hardforks::bsc::BscHardfork;
use alloy_primitives::{BlockHash, B256, U256};
use reth_chainspec::{
    make_genesis_header, BaseFeeParams, BaseFeeParamsKind, Chain, ChainSpec, Head, NamedChain,
};
use reth_primitives_traits::SealedHeader;
use std::str::FromStr;

pub fn bsc_testnet() -> ChainSpec {
    let genesis = serde_json::from_str(include_str!("genesis_chapel.json"))
        .expect("Can't deserialize BSC Testnet genesis json");
    let hardforks = BscHardfork::bsc_testnet();
    ChainSpec {
        chain: Chain::from_named(NamedChain::BinanceSmartChainTestnet),
        genesis: serde_json::from_str(include_str!("genesis_chapel.json"))
            .expect("Can't deserialize BSC Testnet genesis json"),
        hardforks: BscHardfork::bsc_testnet(),
        deposit_contract: None,
        base_fee_params: BaseFeeParamsKind::Constant(BaseFeeParams::new(1, 1)),
        prune_delete_limit: 3500,
        genesis_header: SealedHeader::new(
            make_genesis_header(&genesis, &hardforks),
            BlockHash::from_str(
                "0x6d3c66c5357ec91d5c43af47e234a939b22557cbb552dc45bebbceeed90fbe34",
            )
            .unwrap(),
        ),
        ..Default::default()
    }
}

// Dummy Head for BSC Testnet
pub fn head() -> Head {
    Head {
        number: 57_638_970,
        hash: B256::from_str("0x74e802362fb536395ef7d9d82a87631d5fffaa584a891999d5e77b91bda33754")
            .unwrap(),
        difficulty: U256::from(2),
        total_difficulty: U256::from(115_030_996),
        timestamp: 1752059605,
    }
}
