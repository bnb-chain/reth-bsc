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

use alloy_primitives::{address, b256, hex, B256};
use reth_bsc::chainspec::{bsc::bsc_mainnet, BscChainSpec};
use reth_bsc::consensus::parlia::{Parlia, Snapshot, SnapshotProvider};
use reth_bsc::node::miner::signer::init_global_signer;
use std::collections::HashMap;
use std::sync::RwLock;
use reth_chainspec::{
    make_genesis_header, BaseFeeParams, BaseFeeParamsKind, Chain, ChainHardforks, ChainSpec,
    EthChainSpec, EthereumHardfork, ForkCondition, Hardfork, NamedChain,
};
use reth_db_common::init::init_genesis;
use reth_primitives_traits::SealedHeader;
use reth_provider::test_utils::create_test_provider_factory_with_chain_spec;
use std::sync::Arc;

/// Address of Anvil dev key 0 (`ac0974…ff80`) — the validator whose key the harness controls, so it
/// can build/seal blocks. The same key the miner tests initialize the global signer with.
const TEST_VALIDATOR: alloy_primitives::Address =
    address!("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");

/// The private key matching [`TEST_VALIDATOR`] (Anvil dev key 0), used to seal/sign as the validator.
const TEST_VALIDATOR_KEY: B256 =
    b256!("0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80");

/// In-memory [`SnapshotProvider`] for the harness. The BSC executor reads the parent snapshot from
/// the global provider during pre/post-execution, so the harness publishes this one.
#[derive(Default)]
struct MockSnapshotProvider {
    snaps: RwLock<HashMap<B256, Snapshot>>,
}

impl SnapshotProvider for MockSnapshotProvider {
    fn snapshot_by_hash(&self, hash: &B256) -> Option<Snapshot> {
        self.snaps.read().unwrap().get(hash).cloned()
    }
    fn insert(&self, snapshot: Snapshot) {
        self.snaps.write().unwrap().insert(snapshot.block_hash, snapshot);
    }
}

/// Build the genesis snapshot (validator set) for a signable chain spec.
fn genesis_snapshot(chain_spec: Arc<BscChainSpec>) -> Snapshot {
    let parlia = Parlia::new(chain_spec.clone(), 200);
    let header = chain_spec.genesis_header();
    let info =
        parlia.parse_validators_from_header(header, 200).expect("parse genesis validators");
    Snapshot::new(info.consensus_addrs, 0, header.hash_slow(), 200, info.vote_addrs)
}

/// A **signable** BSC test chain spec: a genesis whose sole validator is [`TEST_VALIDATOR`], so the
/// harness can build and seal blocks as that validator (unlike `bsc_mainnet`, whose validator keys
/// we don't hold). Minimal hardforks (Frontier only) keep the validator encoding pre-Luban — a
/// plain 32-byte vanity + 20-byte address + 65-byte seal — so genesis parsing is trivial.
fn signable_test_chain_spec() -> Arc<BscChainSpec> {
    let validator = hex::encode(TEST_VALIDATOR);
    // extraData = vanity(32) ++ validator(20) ++ seal(65), all but the address zeroed.
    let extra = format!("0x{}{}{}", "00".repeat(32), validator, "00".repeat(65));
    let genesis_json = format!(
        r#"{{
            "config": {{ "chainId": 714 }},
            "gasLimit": "0x2625a00",
            "timestamp": "0x0",
            "extraData": "{extra}",
            "alloc": {{ "0x{validator}": {{ "balance": "0x21e19e0c9bab2400000" }} }}
        }}"#
    );
    let genesis: alloy_genesis::Genesis =
        serde_json::from_str(&genesis_json).expect("deserialize test genesis");

    let hardforks =
        ChainHardforks::new(vec![(EthereumHardfork::Frontier.boxed(), ForkCondition::Block(0))]);
    let genesis_header = {
        let header = make_genesis_header(&genesis, &hardforks);
        let hash = header.hash_slow();
        SealedHeader::new(header, hash)
    };
    let spec = ChainSpec {
        chain: Chain::from_named(NamedChain::BinanceSmartChain),
        genesis,
        hardforks,
        base_fee_params: BaseFeeParamsKind::Constant(BaseFeeParams::new(1, 1)),
        genesis_header,
        ..Default::default()
    };
    Arc::new(BscChainSpec::from(spec))
}

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

/// The signable test genesis parses to a snapshot whose validator is the key we control — the
/// prerequisite for building/sealing blocks in the harness.
#[test]
fn signable_genesis_snapshot_has_known_validator() {
    let chain_spec = signable_test_chain_spec();
    let parlia = Parlia::new(chain_spec.clone(), 200);

    // Genesis (block 0) is an epoch boundary, so its extra-data carries the validator set.
    let validators = parlia
        .parse_validators_from_header(chain_spec.genesis_header(), 200)
        .expect("parse genesis validators");
    assert_eq!(validators.consensus_addrs, vec![TEST_VALIDATOR]);
}

/// Publish the snapshot provider + validator signer the BSC executor reads from globals, and
/// confirm the parent (genesis) snapshot is retrievable — the environment a block build needs.
#[test]
fn execution_environment_is_wired() {
    let chain_spec = signable_test_chain_spec();
    let snap = genesis_snapshot(chain_spec.clone());
    let genesis_hash = chain_spec.genesis_hash();

    let provider = Arc::new(MockSnapshotProvider::default());
    provider.insert(snap);
    // OnceLock setters: ignore "already initialized" so the harness is order-independent.
    let _ = reth_bsc::shared::set_snapshot_provider(provider);
    let _ = init_global_signer(TEST_VALIDATOR_KEY);

    let published = reth_bsc::shared::get_snapshot_provider().expect("snapshot provider published");
    assert_eq!(
        published.snapshot_by_hash(&genesis_hash).map(|s| s.validators),
        Some(vec![TEST_VALIDATOR])
    );
}
