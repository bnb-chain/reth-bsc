//! Chain specification for BSC, credits to: <https://github.com/bnb-chain/reth/blob/main/crates/bsc/chainspec/src/bsc.rs>
use crate::hardforks::bsc::BscHardfork;
use alloy_genesis::Genesis;
use reth_chainspec::{make_genesis_header, BaseFeeParams, BaseFeeParamsKind, Chain, ChainSpec};
use reth_primitives_traits::SealedHeader;

pub fn bsc_local() -> ChainSpec {
    let genesis: Genesis = serde_json::from_str(include_str!("genesis_local.json"))
        .expect("Can't deserialize BSC Local genesis json");
    let hardforks = BscHardfork::bsc_local();
    // NOTE: the chain id must track the genesis file (`chainId: 12345`), not BSC
    // mainnet. Advertising mainnet's id (56) with a foreign genesis hash fails the
    // Status handshake (`networkID`/`genesis` mismatch) against any peer started
    // from the same genesis file (e.g. a Go node on `--chain local` material).
    let chain_id = genesis.config.chain_id;
    ChainSpec {
        chain: Chain::from_id(chain_id),
        genesis: serde_json::from_str(include_str!("genesis_local.json"))
            .expect("Can't deserialize BSC Local genesis json"),
        hardforks: hardforks.clone(),
        deposit_contract: None,
        base_fee_params: BaseFeeParamsKind::Constant(BaseFeeParams::new(1, 1)),
        prune_delete_limit: super::BSC_PRUNE_DELETE_LIMIT,
        genesis_header: {
            let header = make_genesis_header(&genesis, &hardforks);
            let hash = header.hash_slow();
            SealedHeader::new(header, hash)
        },
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::bsc_local;
    use crate::chainspec::BscChainSpec;
    use alloy_primitives::hex;
    use reth_chainspec::{ForkHash, ForkId, Hardforks};

    #[test]
    fn can_create_forkid() {
        let b = hex::decode("e4d5334c").unwrap();
        let expected = [b[0], b[1], b[2], b[3]];
        let expected_f_id = ForkId { hash: ForkHash(expected), next: 0 };

        // Exercise the real fallback path in `BscChainSpec::head()`: the local
        // chain kind is custom, so the head must anchor at genesis (non-zero
        // hash) while the advertised fork id stays unchanged.
        let spec = BscChainSpec::from(bsc_local());
        let head = spec.head();
        assert_ne!(head.hash, alloy_primitives::B256::ZERO);
        let fork_id = spec.fork_id(&head);
        assert_eq!(fork_id, expected_f_id);
    }

    #[test]
    fn local_chain_tracks_genesis_chain_id() {
        // The local dev chain must advertise the genesis file's chain id so the
        // P2P Status `networkID` matches peers started from the same genesis.
        let spec = bsc_local();
        assert_eq!(spec.chain().id(), 12345);

        // And its head must anchor at genesis (non-zero hash for eth/69+).
        let h = BscChainSpec::from(bsc_local()).head();
        assert_eq!(h.number, 0);
        assert_ne!(h.hash, alloy_primitives::B256::ZERO);
    }
}
