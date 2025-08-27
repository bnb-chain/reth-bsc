use crate::hardforks::bsc::BscHardfork;
use alloy_primitives::{BlockHash, U256};
use reth_chainspec::{
    make_genesis_header, BaseFeeParams, BaseFeeParamsKind, Chain, ChainSpec,
};
use reth_primitives::SealedHeader;
use std::str::FromStr;

pub fn bsc_qanet() -> ChainSpec {
    let genesis = serde_json::from_str(include_str!("genesis_rialto.json"))
        .expect("Can't deserialize BSC Qanet genesis json");
    let hardforks = BscHardfork::bsc_qanet();
    ChainSpec {
        chain: Chain::from_id(714),
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