use crate::hardforks::bsc::BscHardfork;
use alloy_primitives::{BlockHash, U256, B256};
use reth_chainspec::{
    make_genesis_header, BaseFeeParams, BaseFeeParamsKind, Chain, ChainSpec, Head,
};
use reth_primitives::SealedHeader;
use std::str::FromStr;

pub const RIALTO_CHAIN_ID: u64 = 714;

pub fn bsc_qanet() -> ChainSpec {
    let genesis = serde_json::from_str(include_str!("genesis_rialto.json"))
        .expect("Can't deserialize BSC Qanet genesis json");
    let hardforks = BscHardfork::bsc_qanet();
    ChainSpec {
        chain: Chain::from_id(RIALTO_CHAIN_ID),
        genesis: serde_json::from_str(include_str!("genesis_rialto.json"))
            .expect("Can't deserialize BSC Qanet genesis json"),
        paris_block_and_final_difficulty: Some((0, U256::from(0))),
        hardforks: BscHardfork::bsc_qanet(),
        deposit_contract: None,
        base_fee_params: BaseFeeParamsKind::Constant(BaseFeeParams::new(1, 1)),
        prune_delete_limit: 3500,
        genesis_header: SealedHeader::new(
            make_genesis_header(&genesis, &hardforks),
            BlockHash::from_str(
                "0x5b8930564626c76d8f30d4ec583166291a3d876122b48c84de3d33729ccb43ff",
            )
            .unwrap(),
        ),
        ..Default::default()
    }
}

// Dummy Head for BSC Qanet
pub fn head() -> Head {
    Head { 
        number: 1376256, 
        hash: B256::from_str("0xd4d2ad3ff55bb663c0fcde91b99e5da1dad4aeb03b1605693650b2f2b0f2d88b")
            .unwrap(),
        difficulty: U256::from(2),
        total_difficulty: U256::from(2746312),
        timestamp: 1756074108, 
    }
}