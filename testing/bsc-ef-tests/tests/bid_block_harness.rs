//! Integration harness for BEP-675 BidBlock execution.
//!
//! A BidBlock is a complete builder-proposed block that the validator must re-execute (in
//! verify-mode, with the trailing system txs supplied to the executor), blind-sign, and seal while
//! preserving the builder's exact block context. None of that is unit-testable — correctness is
//! byte-exact (the re-executed state root must match the builder's), so it needs a real execution
//! environment.
//!
//! This harness builds that environment on top of the same primitives the EF blockchain-test runner
//! uses (`create_test_provider_factory_with_chain_spec` + genesis init + a state provider), but with
//! the BSC chain spec / genesis so the system-contract execution path is exercised.
//!
//! Step 1 (this file) is the scaffold: stand up the provider, initialize the BSC genesis, and
//! confirm a state provider opens at the expected genesis. The trusted local build,
//! `simulate_bid_block`, and the round-trip assertion (build a block → repackage as a DecodedBidBlock
//! → simulate → assert identical hash/state root) build on this foundation.

use reth_bsc::chainspec::bsc::bsc_mainnet;
use reth_chainspec::{ChainSpec, EthChainSpec};
use reth_db_common::init::init_genesis;
use reth_provider::test_utils::create_test_provider_factory_with_chain_spec;
use std::sync::Arc;

/// Stand up a fresh in-memory provider seeded with the BSC mainnet genesis.
#[test]
fn harness_initializes_bsc_genesis() {
    let chain_spec: Arc<ChainSpec> = Arc::new(bsc_mainnet());
    let factory = create_test_provider_factory_with_chain_spec(chain_spec.clone());

    // init_genesis writes the genesis header + full alloc (incl. BSC system contracts) and returns
    // the genesis hash; it must match the chain spec's own genesis hash.
    let genesis_hash = init_genesis(&factory).expect("init BSC genesis");
    assert_eq!(genesis_hash, chain_spec.genesis_hash());
}
