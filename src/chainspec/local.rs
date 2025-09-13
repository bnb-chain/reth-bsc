//! Chain specification for BSC, credits to: <https://github.com/bnb-chain/reth/blob/main/crates/bsc/chainspec/src/bsc.rs>
use crate::hardforks::bsc::BscHardfork;
use alloy_primitives::U256;
use reth_chainspec::{
    make_genesis_header, BaseFeeParams, BaseFeeParamsKind, Chain, ChainSpec, Head, NamedChain,
};
use reth_primitives::SealedHeader;

pub fn bsc_local() -> ChainSpec {
    let genesis = serde_json::from_str(include_str!("genesis_local.json"))
        .expect("Can't deserialize BSC Local genesis json");
    let hardforks = BscHardfork::bsc_local();
    ChainSpec {
        chain: Chain::from_named(NamedChain::BinanceSmartChain),
        genesis: serde_json::from_str(include_str!("genesis_local.json"))
            .expect("Can't deserialize BSC Local genesis json"),
        paris_block_and_final_difficulty: Some((0, U256::from(0))),
        hardforks: hardforks.clone(),
        deposit_contract: None,
        base_fee_params: BaseFeeParamsKind::Constant(BaseFeeParams::new(1, 1)),
        prune_delete_limit: 3500,
        genesis_header: {
            let header = make_genesis_header(&genesis, &hardforks);
            let hash = header.hash_slow();
            SealedHeader::new(header, hash)
        },
        ..Default::default()
    }
}

pub fn head() -> Head {
    Head { number: 0, timestamp: 1751250600, ..Default::default() }
}

#[cfg(test)]
mod tests {
    use super::head;
    use alloy_primitives::hex;
    use reth_chainspec::{ForkHash, ForkId};
    use crate::chainspec::local::bsc_local;

    #[test]
    fn can_create_forkid() {
        let b = hex::decode("e4d5334c").unwrap();
        let expected = [b[0], b[1], b[2], b[3]];
        let expected_f_id = ForkId { hash: ForkHash(expected), next: 0 };

        let fork_id = bsc_local().fork_id(&head());
        assert_eq!(fork_id, expected_f_id);
    }
}
