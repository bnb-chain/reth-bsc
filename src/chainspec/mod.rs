//! Chain specification for BSC, credits to: <https://github.com/bnb-chain/reth/blob/main/crates/bsc/chainspec/src/bsc.rs>
use crate::hardforks::{bsc::BscHardfork, BscHardforks};
use alloy_consensus::Header;
use alloy_eips::eip7840::BlobParams;
use alloy_genesis::Genesis;
use alloy_primitives::{Address, B256, U256};
use reth_chainspec::{
    BaseFeeParams, ChainKind, ChainSpec, DepositContract, EthChainSpec, EthereumHardfork, EthereumHardforks,
    ForkCondition, ForkFilter, ForkId, Hardforks, Head, NamedChain,
};
use reth_discv4::NodeRecord;
use reth_evm::eth::spec::EthExecutorSpec;
use std::{fmt::Display, sync::Arc};

pub mod bsc;
pub mod bsc_chapel;
pub mod bsc_rialto;
pub mod parser;
pub mod genesis_override;
pub mod bootnode_override;
mod local;

pub use bsc_chapel::bsc_testnet;

/// Default number of entries the live pruner may delete per run across all segments.
/// Overridable with `--prune.delete-limit`.
pub const BSC_PRUNE_DELETE_LIMIT: usize = 10_000;

/// Bsc chain spec type.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BscChainSpec {
    /// [`ChainSpec`].
    pub inner: ChainSpec,
}

impl EthChainSpec for BscChainSpec {
    type Header = Header;

    fn blob_params_at_timestamp(&self, timestamp: u64) -> Option<BlobParams> {
        // BSC doesn't modify blob params in Prague, while ETH does.
        // This is a key difference between BSC and ETH chain specifications.
        if self.inner.is_cancun_active_at_timestamp(timestamp) {
            Some(self.inner.blob_params.cancun)
        } else {
            None
        }
    }

    fn final_paris_total_difficulty(&self) -> Option<U256> {
        self.inner.final_paris_total_difficulty()
    }

    fn chain(&self) -> alloy_chains::Chain {
        self.inner.chain()
    }

    fn base_fee_params_at_timestamp(&self, timestamp: u64) -> BaseFeeParams {
        self.inner.base_fee_params_at_timestamp(timestamp)
    }

    fn deposit_contract(&self) -> Option<&DepositContract> {
        None
    }

    fn genesis_hash(&self) -> B256 {
        // Check if genesis hash override is active
        if let Some(override_hash) = genesis_override::get_genesis_hash_override() {
            return override_hash;
        }
        
        self.inner.genesis_hash()
    }

    fn prune_delete_limit(&self) -> usize {
        self.inner.prune_delete_limit()
    }

    fn display_hardforks(&self) -> Box<dyn Display> {
        Box::new(self.inner.display_hardforks())
    }

    fn genesis_header(&self) -> &Header {
        self.inner.genesis_header()
    }

    fn genesis(&self) -> &Genesis {
        self.inner.genesis()
    }

    fn bootnodes(&self) -> Option<Vec<NodeRecord>> {
        // Check if bootnode override is active
        if bootnode_override::has_bootnode_override() {
            return bootnode_override::get_bootnode_override().clone();
        }
        
        // Fall back to default bootnodes based on chain.
        //
        // NOTE: unknown (custom/dev) chains resolve to an explicit empty set instead of
        // `None`. Upstream reth falls back to Ethereum mainnet bootnodes when this returns
        // `None`, which makes a private/dev node dial and get banned by dozens of public
        // peers. An empty set keeps custom chains quiet unless the operator passes
        // `--bootnodes`/`--trusted-peers` explicitly.
        match self.inner.chain().kind() {
            ChainKind::Named(NamedChain::BinanceSmartChain) => {
                Some(crate::node::network::bootnodes::bsc_mainnet_nodes())
            }
            ChainKind::Named(NamedChain::BinanceSmartChainTestnet) => {
                Some(crate::node::network::bootnodes::bsc_testnet_nodes())
            }
            ChainKind::Id(bsc_rialto::RIALTO_CHAIN_ID) => {
                Some(crate::node::network::bootnodes::bsc_qanet_nodes())
            }
            _ => Some(Vec::new()),
        }
    }

    fn is_optimism(&self) -> bool {
        false
    }
}

impl Hardforks for BscChainSpec {
    fn fork<H: reth_chainspec::Hardfork>(&self, fork: H) -> reth_chainspec::ForkCondition {
        self.inner.fork(fork)
    }

    fn forks_iter(
        &self,
    ) -> impl Iterator<Item = (&dyn reth_chainspec::Hardfork, reth_chainspec::ForkCondition)> {
        self.inner.forks_iter()
    }

    fn fork_id(&self, head: &Head) -> ForkId {
        self.inner.fork_id(head)
    }

    fn latest_fork_id(&self) -> ForkId {
        let head = self.head();
        self.inner.fork_id(&head)
    }

    fn fork_filter(&self, head: Head) -> ForkFilter {
        self.inner.fork_filter(head)
    }
}

impl From<ChainSpec> for BscChainSpec {
    fn from(value: ChainSpec) -> Self {
        Self { inner: value }
    }
}

impl EthereumHardforks for BscChainSpec {
    fn ethereum_fork_activation(&self, fork: EthereumHardfork) -> ForkCondition {
        // BscHardfork::Osaka and EthereumHardfork::Osaka share the same name() = "Osaka",
        // so ChainHardforks would match BscHardfork::Osaka when queried for EthereumHardfork::Osaka.
        // BSC does not support EIP-7594 (PeerDAS), which is gated on EthereumHardfork::Osaka in
        // upstream reth (RPC sidecar conversion and pool validation). Returning Never here
        // prevents the upstream EIP-4844 → EIP-7594 sidecar auto-conversion.
        if matches!(fork, EthereumHardfork::Osaka) {
            return ForkCondition::Never;
        }
        self.inner.ethereum_fork_activation(fork)
    }
}

impl BscHardforks for BscChainSpec {
    fn bsc_fork_activation(&self, fork: BscHardfork) -> ForkCondition {
        self.fork(fork)
    }
}

impl EthExecutorSpec for BscChainSpec {
    fn deposit_contract_address(&self) -> Option<Address> {
        None
    }
}

impl BscChainSpec {
    /// Get the head information for this chain spec
    pub fn head(&self) -> Head {
        let mut head = match self.inner.chain().kind() {
            ChainKind::Named(NamedChain::BinanceSmartChain) => {
                bsc::head()
            }
            ChainKind::Named(NamedChain::BinanceSmartChainTestnet) => {
                bsc_chapel::head()
            }
            ChainKind::Id(bsc_rialto::RIALTO_CHAIN_ID) => {
                bsc_rialto::head()
            }
            // Custom/dev chains (e.g. `--chain <genesis.json>`): anchor the initial
            // head at genesis so the P2P Status message carries a non-zero blockhash.
            // A zero blockhash fails the eth/69+ handshake (`BlockhashZero`) on the
            // remote side, which deadlocks fresh private networks: no peer ever
            // connects, so the miner's peer gate never opens and no block is ever
            // produced to advance the head.
            _ => {
                let genesis_header = self.inner.genesis_header();
                Head {
                    number: 0,
                    hash: self.genesis_hash(),
                    difficulty: genesis_header.difficulty,
                    total_difficulty: genesis_header.difficulty,
                    timestamp: genesis_header.timestamp,
                }
            }
        };

        // Override head hash if genesis hash override is set
        if let Some(override_hash) = genesis_override::get_genesis_hash_override() {
            head.hash = override_hash;
        }

        head
    }
}

impl From<BscChainSpec> for ChainSpec {
    fn from(value: BscChainSpec) -> Self {
        value.inner
    }
}

impl BscHardforks for Arc<BscChainSpec> {
    fn bsc_fork_activation(&self, fork: BscHardfork) -> ForkCondition {
        self.as_ref().bsc_fork_activation(fork)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chainspec::bsc_chapel::bsc_testnet;

    #[test]
    fn test_blob_params_at_timestamp() {
        let chain_spec = BscChainSpec::from(bsc_testnet());

        // Test timestamp before Cancun (Cancun activates at 1713330442 on testnet)
        let before_cancun_timestamp = 1713330441;
        let result = chain_spec.blob_params_at_timestamp(before_cancun_timestamp);
        assert!(result.is_none(), "Should return None for timestamp before Cancun");

        // Test timestamp during Cancun (between Cancun and Prague)
        // Prague activates at 1740452880 on testnet
        let during_cancun_timestamp = 1713330442; // Cancun activation time
        let result = chain_spec.blob_params_at_timestamp(during_cancun_timestamp);
        assert!(result.is_some(), "Should return Some for timestamp during Cancun");
        if let Some(blob_params) = result {
            // Check the correct blob param values
            assert_eq!(blob_params.target_blob_count, 3);
            assert_eq!(blob_params.max_blob_count, 6);
        }

        // Test timestamp after Prague activation
        let after_prague_timestamp = 1740452880; // Prague activation time
        let result = chain_spec.blob_params_at_timestamp(after_prague_timestamp);
        // BSC doesn't modify blob params in Prague, so should still return Cancun params
        assert!(
            result.is_some(),
            "Should return Some for timestamp after Prague (BSC doesn't modify blob params)"
        );
        if let Some(blob_params) = result {
            // Check the correct blob param values (should be same as Cancun)
            assert_eq!(blob_params.target_blob_count, 3);
            assert_eq!(blob_params.max_blob_count, 6);
        }

        // Test timestamp well after Prague
        let well_after_prague_timestamp = 1740452881;
        let result = chain_spec.blob_params_at_timestamp(well_after_prague_timestamp);
        assert!(result.is_some(), "Should return Some for timestamp well after Prague");
        if let Some(blob_params) = result {
            // Check the correct blob param values (should be same as Cancun)
            assert_eq!(blob_params.target_blob_count, 3);
            assert_eq!(blob_params.max_blob_count, 6);
        }
    }

    #[test]
    fn test_genesis_hash_override_complete() {
        use alloy_primitives::B256;
        use std::str::FromStr;
        
        let chain_spec = BscChainSpec::from(bsc_testnet());
        
        // Check if override is already set from other tests
        let override_already_set = crate::chainspec::genesis_override::get_genesis_hash_override().is_some();
        
        if !override_already_set {
            // Test original behavior without override first
            let original_genesis_hash = chain_spec.genesis_hash();
            let original_head = chain_spec.head();
            
            // Set genesis hash override
            let custom_genesis_hash = B256::from_str("0xb4844167d735617495363867c84affa9f4069bcdae48411ae3badbe1d227d3e5").unwrap();
            crate::chainspec::genesis_override::set_genesis_hash_override(Some("0xb4844167d735617495363867c84affa9f4069bcdae48411ae3badbe1d227d3e5".to_string()))
                .expect("Should set genesis hash override");
            
            // Test that all methods now use the override
            let overridden_genesis_hash = chain_spec.genesis_hash();
            assert_eq!(overridden_genesis_hash, custom_genesis_hash, "genesis_hash() should return the override");
            
            let overridden_head = chain_spec.head();
            assert_eq!(overridden_head.hash, custom_genesis_hash, "head().hash should use the override");
            assert_eq!(overridden_head.number, original_head.number, "head().number should remain unchanged");
            assert_eq!(overridden_head.timestamp, original_head.timestamp, "head().timestamp should remain unchanged");
            
            // Test fork ID calculations with override
            let overridden_fork_id = chain_spec.fork_id(&overridden_head);
            let overridden_latest_fork_id = chain_spec.latest_fork_id();
            
            // NOTE: Fork ID calculation in reth may not directly use the head hash or our genesis_hash() override
            // because the inner chainspec doesn't know about our override. The key thing is that our 
            // genesis_hash() and head() methods correctly return the override.
            
            // Verify that our methods are consistent with each other
            assert_eq!(overridden_fork_id.hash, overridden_latest_fork_id.hash, "fork_id() and latest_fork_id() should have same hash");
            assert_eq!(overridden_fork_id.next, overridden_latest_fork_id.next, "fork_id() and latest_fork_id() should have same next");
            
            // Test validation function
            assert!(crate::chainspec::genesis_override::validate_genesis_hash(custom_genesis_hash), "Custom genesis hash should validate");
            assert!(!crate::chainspec::genesis_override::validate_genesis_hash(original_genesis_hash), "Original genesis hash should not validate with override set");
        } else {
            // Override is already set from another test, just verify it's working
            let current_override = crate::chainspec::genesis_override::get_genesis_hash_override().unwrap();
            
            let genesis_hash = chain_spec.genesis_hash();
            let head = chain_spec.head();
            let fork_id = chain_spec.fork_id(&head);
            let latest_fork_id = chain_spec.latest_fork_id();
            
            assert_eq!(genesis_hash, current_override, "genesis_hash() should match current override");
            assert_eq!(head.hash, current_override, "head().hash should match current override");
            assert_eq!(fork_id.hash, latest_fork_id.hash, "fork_id() and latest_fork_id() should have same hash");
            
        }
    }

    #[test]
    fn test_fork_id_consistency() {
        // Test that fork_id() and latest_fork_id() are consistent
        let chain_spec = BscChainSpec::from(bsc_testnet());
        let head = chain_spec.head();
        let fork_id = chain_spec.fork_id(&head);
        let latest_fork_id = chain_spec.latest_fork_id();

        // Test consistency
        assert_eq!(fork_id.hash, latest_fork_id.hash, "fork_id and latest_fork_id should have same hash");
        assert_eq!(fork_id.next, latest_fork_id.next, "fork_id and latest_fork_id should have same next");
    }

    #[test]
    fn test_known_chains_keep_their_bootnodes() {
        // Regression guard for the custom-chain bootnode fix: the bundled
        // networks must keep resolving to their own (non-empty) bootnode sets.
        for name in ["bsc", "bsc-testnet", "bsc-qanet"] {
            let spec = crate::chainspec::parser::chain_value_parser(name)
                .expect("bundled chain should parse");
            let bootnodes = spec.bootnodes();
            assert!(
                bootnodes.as_ref().is_some_and(|nodes| !nodes.is_empty()),
                "bundled chain {name} must keep non-empty bootnodes"
            );
        }
    }

    #[test]
    fn test_custom_chain_head_anchors_at_genesis() {
        // Regression test: custom/dev chains must advertise a non-zero genesis
        // blockhash as their initial head. A zero hash fails the eth/69+
        // handshake (`BlockhashZero`) and deadlocks fresh private networks.
        //
        // NOTE: the validator embedded below is a synthetic fixture address
        // (keccak256("reth-bsc-privnet-test-validator") truncated to 20 bytes).
        // It has no known private key and must never be used outside tests.
        let genesis_json = r#"{
            "config": {
                "chainId": 1337,
                "homesteadBlock": 0,
                "eip150Block": 0,
                "eip155Block": 0,
                "eip158Block": 0,
                "byzantiumBlock": 0,
                "constantinopleBlock": 0,
                "petersburgBlock": 0,
                "istanbulBlock": 0,
                "muirGlacierBlock": 0,
                "ramanujanBlock": 0,
                "nielsBlock": 0,
                "parlia": {"period": 3, "epoch": 200}
            },
            "nonce": "0x0",
            "timestamp": "0x5e9da7ce",
            "extraData": "0x000000000000000000000000000000000000000000000000000000000000000039876ae9543fd48e49661792c115a3984f73dbe50000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
            "gasLimit": "0x2625a00",
            "difficulty": "0x1",
            "mixHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
            "coinbase": "0x0000000000000000000000000000000000000000",
            "alloc": {}
        }"#;
        let chain_spec = crate::chainspec::parser::parse_genesis_json(genesis_json)
            .expect("custom genesis should parse");

        let head = chain_spec.head();
        assert_eq!(head.number, 0, "custom chain head must start at genesis number");
        assert_eq!(
            head.hash,
            chain_spec.genesis_hash(),
            "custom chain head hash must be the genesis hash"
        );
        assert_ne!(head.hash, B256::ZERO, "custom chain head hash must not be zero");

        // Custom chains must not inherit any public bootnodes (which would make a
        // private node dial the internet); an explicit empty set disables the
        // upstream Ethereum-mainnet fallback.
        assert_eq!(
            chain_spec.bootnodes(),
            Some(Vec::new()),
            "custom chains must resolve to an empty bootnode set"
        );
    }
}
