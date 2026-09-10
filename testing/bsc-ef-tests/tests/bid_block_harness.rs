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

use alloy_primitives::{hex, B256};
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

/// A random signing key for the harness, generated once per test process, and the validator
/// address it derives.
///
/// Random rather than a hard-coded development key, so no key literal exists in the tree. Generated
/// *once* and shared because `init_global_signer` is first-wins and this binary's tests share a
/// process; a per-test key would make the validator depend on test order.
///
/// Nothing here is pinned to a particular key: the harness *generates* its genesis from
/// [`TEST_VALIDATOR`] (sole validator in `extraData`) and compares computed roots against each
/// other, so the whole chain moves with the key rather than against a fixed expectation.
static TEST_VALIDATOR_KEY: std::sync::LazyLock<B256> = std::sync::LazyLock::new(|| {
    use rand::Rng;
    // Rejection-sample so the result is always a valid secp256k1 scalar.
    loop {
        let candidate: [u8; 32] = rand::rng().random();
        let hex = format!("0x{}", alloy_primitives::hex::encode(candidate));
        if reth_bsc::node::miner::config::keystore::load_private_key_from_hex(&hex).is_ok() {
            return B256::from(candidate);
        }
    }
});

/// The validator whose key the harness controls, so it can build and seal blocks.
static TEST_VALIDATOR: std::sync::LazyLock<alloy_primitives::Address> =
    std::sync::LazyLock::new(|| {
        let hex = format!("0x{}", alloy_primitives::hex::encode(TEST_VALIDATOR_KEY.as_slice()));
        let sk = reth_bsc::node::miner::config::keystore::load_private_key_from_hex(&hex)
            .expect("TEST_VALIDATOR_KEY is a valid scalar by construction");
        reth_bsc::node::miner::config::keystore::get_validator_address(&sk)
    });

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
    build_signable_chain_spec(vec![])
}

/// Signable test spec with Kepler active (timestamp 0). Post-Kepler skips the `distribute_to_system`
/// system-reward tx, so a normal block's trailing system-tx region is exactly `[deposit]` — what the
/// BidBlock validation expects. Kepler activates independently; Luban/Lorentz/London stay off, so
/// the validator encoding and header format stay simple.
fn kepler_signable_chain_spec() -> Arc<BscChainSpec> {
    use reth_bsc::hardforks::bsc::BscHardfork;
    // Kepler requires London active (is_kepler_active_at_timestamp gates on is_london_active_at_block).
    // genesis baseFeePerGas=0 keeps the base fee 0 for all blocks (0 * adjustment = 0), so a
    // gas_price=1 user tx still pays a fee — which is what funds the validator deposit.
    let validator = hex::encode(*TEST_VALIDATOR);
    let extra_data = format!("0x{}{}{}", "00".repeat(32), validator, "00".repeat(65));
    let genesis_json = format!(
        r#"{{
            "config": {{ "chainId": 714 }},
            "gasLimit": "0x2625a00",
            "timestamp": "0x0",
            "baseFeePerGas": "0x0",
            "extraData": "{extra_data}",
            "alloc": {{ "0x{validator}": {{ "balance": "0x21e19e0c9bab2400000" }} }}
        }}"#
    );
    let genesis: alloy_genesis::Genesis =
        serde_json::from_str(&genesis_json).expect("deserialize kepler genesis");
    let hardforks = ChainHardforks::new(vec![
        (EthereumHardfork::Frontier.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::London.boxed(), ForkCondition::Block(0)),
        (BscHardfork::Kepler.boxed(), ForkCondition::Timestamp(0)),
    ]);
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

/// Build a signable test chain spec (sole genesis validator = [`TEST_VALIDATOR`], pre-Luban 20-byte
/// encoding) with `extra` hardforks beyond Frontier@0.
fn build_signable_chain_spec(extra: Vec<(Box<dyn Hardfork>, ForkCondition)>) -> Arc<BscChainSpec> {
    let validator = hex::encode(*TEST_VALIDATOR);
    // extraData = vanity(32) ++ validator(20) ++ seal(65), all but the address zeroed.
    let extra_data = format!("0x{}{}{}", "00".repeat(32), validator, "00".repeat(65));
    let genesis_json = format!(
        r#"{{
            "config": {{ "chainId": 714 }},
            "gasLimit": "0x2625a00",
            "timestamp": "0x0",
            "extraData": "{extra_data}",
            "alloc": {{ "0x{validator}": {{ "balance": "0x21e19e0c9bab2400000" }} }}
        }}"#
    );
    let genesis: alloy_genesis::Genesis =
        serde_json::from_str(&genesis_json).expect("deserialize test genesis");

    let mut forks = vec![(EthereumHardfork::Frontier.boxed(), ForkCondition::Block(0))];
    forks.extend(extra);
    let hardforks = ChainHardforks::new(forks);
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
    assert_eq!(validators.consensus_addrs, vec![*TEST_VALIDATOR]);
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
    let _ = init_global_signer(*TEST_VALIDATOR_KEY);

    let published = reth_bsc::shared::get_snapshot_provider().expect("snapshot provider published");
    assert_eq!(
        published.snapshot_by_hash(&genesis_hash).map(|s| s.validators),
        Some(vec![*TEST_VALIDATOR])
    );
}

/// Shared, accumulating mock header provider, published once to the global header reader. Tests
/// register their headers (genesis + built blocks) into it; the global reader is `OnceLock`-once
/// (can't be per-test), so a single accumulating provider lets multiple chain specs / factories
/// coexist across tests in one process.
fn shared_header_provider() -> Arc<reth_provider::test_utils::MockEthProvider> {
    static P: std::sync::OnceLock<Arc<reth_provider::test_utils::MockEthProvider>> =
        std::sync::OnceLock::new();
    P.get_or_init(|| {
        let m = Arc::new(reth_provider::test_utils::MockEthProvider::default());
        let _ = reth_bsc::shared::set_header_provider(m.clone());
        m
    })
    .clone()
}

/// Publish env globals — accumulate the genesis snapshot + header (so multiple chain specs coexist
/// across tests sharing the process-global providers) and init the validator signer.
fn publish_env(chain_spec: Arc<BscChainSpec>) {
    let snap = genesis_snapshot(chain_spec.clone());
    match reth_bsc::shared::get_snapshot_provider() {
        Some(p) => p.insert(snap),
        None => {
            let p = Arc::new(MockSnapshotProvider::default());
            p.insert(snap);
            let _ = reth_bsc::shared::set_snapshot_provider(p);
        }
    }
    let _ = init_global_signer(*TEST_VALIDATOR_KEY);
    shared_header_provider()
        .add_header(chain_spec.genesis_hash(), chain_spec.genesis_header().clone());
}

/// Trusted local build: drive `BscEvmConfig`'s builder to produce block 1 on the signable genesis.
/// This is the reference the round-trip will compare `simulate_bid_block` against.
#[test]
fn trusted_local_build_produces_block() {
    use reth_bsc::node::evm::config::{
        BscEvmConfig, BscExecutionMode, BscNextBlockEnvAttributes,
    };
    use reth_evm::execute::{BlockBuilder, BlockBuilderOutcome};
    use reth_evm::{ConfigureEvm, NextBlockEnvAttributes};
    use reth_provider::DatabaseProviderFactory;
    use reth_revm::{database::StateProviderDatabase, db::State};

    let chain_spec = signable_test_chain_spec();
    let factory = create_test_provider_factory_with_chain_spec(Arc::new(chain_spec.inner.clone()));
    init_genesis(&factory).expect("init genesis");
    publish_env(chain_spec.clone());
    // BSC pre/post-execution looks the parent header up via the global header reader.

    let parent = SealedHeader::new(chain_spec.genesis_header().clone(), chain_spec.genesis_hash());
    let provider = factory.database_provider_rw().expect("rw provider");
    let state_provider = provider.latest();
    let sp_db = StateProviderDatabase::new(&state_provider);
    let mut db = State::builder().with_database(sp_db).with_bundle_update().build();

    let evm_config = BscEvmConfig::new(chain_spec.clone());
    let mut builder = evm_config
        .builder_for_next_block(
            &mut db,
            &parent,
            BscNextBlockEnvAttributes {
                inner: NextBlockEnvAttributes {
                    timestamp: parent.timestamp + 3,
                    suggested_fee_recipient: *TEST_VALIDATOR,
                    prev_randao: B256::ZERO,
                    gas_limit: parent.gas_limit,
                    parent_beacon_block_root: None,
                    withdrawals: None,
                    extra_data: Default::default(),
                    slot_number: None,
                },
                mode: BscExecutionMode::Mining,
                milli_remainder: 0,
                validator_cache_sink: None,
                turn_length_sink: None,
                state_root_precomputed_sink: None,
                trie_handle: None,
                state_root_deadline_ms: None,
            },
        )
        .expect("builder for next block");

    builder.apply_pre_execution_changes().expect("apply pre-execution changes");
    let out = builder.finish(&state_provider, None).expect("finish builder");
    let BlockBuilderOutcome { block, .. } = out;

    assert_eq!(block.header().number, 1);
    assert_ne!(block.header().state_root, B256::ZERO);
}

/// Round-trip: build block 1 via the builder path, finalize/seal it, then re-execute the sealed
/// block via the executor path and assert both paths agree on the post-state root.
///
/// This is the core verify-mode invariant a BidBlock relies on: a builder builds (builder path) and
/// the validator re-executes (executor path, consuming the block's system txs). Agreement on the
/// state root means the validator faithfully reproduces the builder's block. Finalization
/// (difficulty + ECDSA seal) is required because the executor validates a *sealed* header; the
/// build uses `prev_randao = calculate_difficulty(...)` so it matches the finalized difficulty.
#[test]
fn round_trip_build_finalize_reexecute_agree() {
    use reth_bsc::consensus::parlia::util::calculate_difficulty;
    use reth_bsc::node::evm::config::{
        BscEvmConfig, BscExecutionMode, BscNextBlockEnvAttributes,
    };
    use reth_bsc::node::miner::util::finalize_new_header;
    use reth_evm::execute::{BlockBuilder, BlockBuilderOutcome, Executor};
    use reth_evm::{ConfigureEvm, NextBlockEnvAttributes};
    use reth_primitives_traits::RecoveredBlock;
    use reth_provider::DatabaseProviderFactory;
    use reth_revm::{database::StateProviderDatabase, db::State};
    use reth_trie::{HashedPostState, KeccakKeyHasher, StateRoot};
    use reth_trie_db::{
        DatabaseHashedCursorFactory, DatabaseStateRoot, DatabaseTrieCursorFactory, LegacyKeyAdapter,
    };

    let chain_spec = signable_test_chain_spec();
    let factory = create_test_provider_factory_with_chain_spec(Arc::new(chain_spec.inner.clone()));
    init_genesis(&factory).expect("init genesis");
    publish_env(chain_spec.clone());

    let parent = SealedHeader::new(chain_spec.genesis_header().clone(), chain_spec.genesis_hash());
    let snap = genesis_snapshot(chain_spec.clone());
    let parlia = Arc::new(Parlia::new(chain_spec.clone(), 200));
    let difficulty = calculate_difficulty(&snap, *TEST_VALIDATOR);
    let provider = factory.database_provider_rw().expect("rw provider");

    // Build block 1 via the builder path. Use a 32-byte vanity (so finalize's seal append reaches
    // the vanity+seal minimum) and prev_randao = the finalized difficulty (BSC PREVRANDAO).
    let block = {
        let state_provider = provider.latest();
        let sp_db = StateProviderDatabase::new(&state_provider);
        let mut db = State::builder().with_database(sp_db).with_bundle_update().build();
        let evm_config = BscEvmConfig::new(chain_spec.clone());
        let mut builder = evm_config
            .builder_for_next_block(
                &mut db,
                &parent,
                BscNextBlockEnvAttributes {
                    inner: NextBlockEnvAttributes {
                        timestamp: parent.timestamp + 3,
                        suggested_fee_recipient: *TEST_VALIDATOR,
                        prev_randao: difficulty.into(),
                        gas_limit: parent.gas_limit,
                        parent_beacon_block_root: None,
                        withdrawals: None,
                        extra_data: alloy_primitives::Bytes::from(vec![0u8; 32]),
                        slot_number: None,
                    },
                    mode: BscExecutionMode::Mining,
                    milli_remainder: 0,
                    validator_cache_sink: None,
                    turn_length_sink: None,
                    state_root_precomputed_sink: None,
                    trie_handle: None,
                    state_root_deadline_ms: None,
                },
            )
            .expect("builder for next block");
        builder.apply_pre_execution_changes().expect("apply pre-execution changes");
        let out = builder.finish(&state_provider, None).expect("finish builder");
        let BlockBuilderOutcome { block, .. } = out;
        block
    };
    let reference_root = block.header().state_root;

    // Finalize/seal the built block (difficulty + ECDSA seal); state root is unchanged.
    let snapshot_provider =
        reth_bsc::shared::get_snapshot_provider().cloned().expect("snapshot provider");
    let senders = block.senders().to_vec();
    let mut plain = block.sealed_block().clone_block();
    finalize_new_header(
        parlia,
        &snap,
        &parent,
        &mut plain.header,
        &snapshot_provider,
        (parent.timestamp + 3) * 1000,
        None, // not an epoch block
    )
    .expect("finalize header");

    // The executor also looks up the *current* block's snapshot (post-apply). For this non-epoch
    // block the validator set is unchanged, so publish a block-1 snapshot keyed by the sealed hash.
    let block1_hash = plain.header.hash_slow();
    let mut snap1 = snap.clone();
    snap1.block_number = 1;
    snap1.block_hash = block1_hash;
    snapshot_provider.insert(snap1);

    let finalized = RecoveredBlock::new_unhashed(plain, senders);

    // Re-execute the sealed block via the executor path and recompute the state root.
    let state_provider2 = provider.latest();
    let evm_config = BscEvmConfig::new(chain_spec.clone());
    let executor = evm_config.batch_executor(StateProviderDatabase::new(&state_provider2));
    let output = executor.execute(&finalized).expect("re-execute sealed block");
    let hashed = HashedPostState::from_bundle_state::<KeccakKeyHasher>(output.state.state());
    let (computed_root, _) = <StateRoot<
        DatabaseTrieCursorFactory<_, LegacyKeyAdapter>,
        DatabaseHashedCursorFactory<_>,
    > as DatabaseStateRoot<_>>::overlay_root_with_updates(
        provider.tx_ref(),
        &hashed.clone_into_sorted(),
    )
    .expect("compute state root");

    // Builder path and executor path must agree on the post-state root.
    assert_eq!(computed_root, reference_root);
}

/// Full execution gate: with a real block-1→block-2 chain (Kepler active), build block 2 with a
/// fee-paying user tx (validator generates the deposit), repackage it as a BidBlock with the
/// deposit *unsigned*, run `simulate_bid_block` (which re-signs it), execute the sealed result, and
/// assert the state root equals the reference build. This is the byte-exact verify-mode proof.
#[test]
fn execution_gate_round_trip() {
    use alloy_consensus::TxLegacy;
    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::{Signature, TxKind};
    use reth_bsc::consensus::parlia::util::calculate_difficulty;
    use reth_bsc::node::evm::config::{
        BscEvmConfig, BscExecutionMode, BscNextBlockEnvAttributes,
    };
    use reth_bsc::node::miner::bid_block::{
        simulate_bid_block, submitted_tx_root, BidBlock, BidBlockArgs,
    };
    use reth_bsc::node::miner::signer::sign_system_transaction;
    use reth_bsc::node::miner::util::finalize_new_header;
    use reth_bsc::node::BscNode;
    use reth_ethereum_primitives::TransactionSigned;
    use reth_evm::execute::{BlockBuilder, BlockBuilderOutcome, BlockExecutionOutput, Executor};
    use reth_evm::{ConfigureEvm, NextBlockEnvAttributes};
    use reth_primitives_traits::{RecoveredBlock, SignerRecoverable};
    use reth_provider::test_utils::create_test_provider_factory_with_node_types;
    use reth_provider::{
        BlockWriter, DBProvider, DatabaseProviderFactory, ExecutionOutcome, OriginalValuesKnown,
        StateWriteConfig, StateWriter, StaticFileProviderFactory, StaticFileWriter,
    };
    use reth_revm::{database::StateProviderDatabase, db::State};
    use reth_trie::{HashedPostState, KeccakKeyHasher, StateRoot};
    use reth_trie_db::{
        DatabaseHashedCursorFactory, DatabaseStateRoot, DatabaseTrieCursorFactory, LegacyKeyAdapter,
    };

    let chain_spec = kepler_signable_chain_spec();
    let factory = create_test_provider_factory_with_node_types::<BscNode>(chain_spec.clone());
    init_genesis(&factory).expect("init genesis");
    publish_env(chain_spec.clone());

    let parlia = Arc::new(Parlia::new(chain_spec.clone(), 200));
    let genesis = SealedHeader::new(chain_spec.genesis_header().clone(), chain_spec.genesis_hash());
    let genesis_snap = genesis_snapshot(chain_spec.clone());
    let snapshot_provider =
        reth_bsc::shared::get_snapshot_provider().cloned().expect("snapshot provider");

    let attrs = |ts: u64, diff: alloy_primitives::U256, gas_limit: u64| BscNextBlockEnvAttributes {
        inner: NextBlockEnvAttributes {
            timestamp: ts,
            suggested_fee_recipient: *TEST_VALIDATOR,
            prev_randao: diff.into(),
            gas_limit,
            parent_beacon_block_root: None,
            withdrawals: None,
            extra_data: alloy_primitives::Bytes::from(vec![0u8; 32]),
            slot_number: None,
        },
        mode: BscExecutionMode::Mining,
        milli_remainder: 0,
        validator_cache_sink: None,
        turn_length_sink: None,
        state_root_precomputed_sink: None,
        trie_handle: None,
        state_root_deadline_ms: None,
    };

    // --- Build + finalize + insert block 1 (absorbs the one-time system-contract init txs). ---
    let (block1, out1): (RecoveredBlock<reth_bsc::BscBlock>, BlockExecutionOutput<_>) = {
        let provider = factory.database_provider_rw().unwrap();
        let state_provider = provider.latest();
        let sp_db = StateProviderDatabase::new(&state_provider);
        let mut db = State::builder().with_database(sp_db).with_bundle_update().build();
        let evm_config = BscEvmConfig::new(chain_spec.clone());
        let diff = calculate_difficulty(&genesis_snap, *TEST_VALIDATOR);
        let mut builder = evm_config
            .builder_for_next_block(&mut db, &genesis, attrs(genesis.timestamp + 3, diff, genesis.gas_limit))
            .expect("builder b1");
        builder.apply_pre_execution_changes().expect("pre-exec b1");
        let out = builder.finish(&state_provider, None).expect("finish b1");
        let BlockBuilderOutcome { execution_result, block, .. } = out;
        let senders = block.senders().to_vec();
        let mut plain = block.sealed_block().clone_block();
        finalize_new_header(parlia.clone(), &genesis_snap, &genesis, &mut plain.header, &snapshot_provider, (genesis.timestamp + 3) * 1000, None) // not an epoch block
            .expect("finalize b1");
        let output = BlockExecutionOutput { state: db.take_bundle(), result: execution_result };
        (RecoveredBlock::new_unhashed(plain, senders), output)
    };
    let block1_sealed = SealedHeader::new(block1.header().clone(), block1.header().hash_slow());
    // Register block 1's header so the global reader resolves it when block 2 builds on it.
    shared_header_provider().add_header(block1_sealed.hash(), block1.header().clone());
    {
        let mut snap1 = genesis_snap.clone();
        snap1.block_number = 1;
        snap1.block_hash = block1_sealed.hash();
        snapshot_provider.insert(snap1);
        let provider = factory.database_provider_rw().unwrap();
        provider.insert_block(&block1).expect("insert b1");
        provider.write_state(&ExecutionOutcome::single(1, out1), OriginalValuesKnown::Yes, StateWriteConfig::default())
            .expect("write b1 state");
        provider.static_file_provider().commit().expect("sf commit");
        provider.commit().expect("commit b1");
    }
    let block1_snap = {
        let mut s = genesis_snap.clone();
        s.block_number = 1;
        s.block_hash = block1_sealed.hash();
        s
    };

    // --- Build block 2 (reference) with a fee-paying user tx -> [user, deposit]. ---
    let user = sign_system_transaction(
        TxLegacy { chain_id: None, nonce: 0, gas_price: 1, gas_limit: 21_000, to: TxKind::Call(alloy_primitives::Address::ZERO), value: alloy_primitives::U256::from(1u64), input: Default::default() }.into(),
    )
    .expect("sign user");
    let (block2_ref, _out2): (RecoveredBlock<reth_bsc::BscBlock>, BlockExecutionOutput<_>) = {
        let provider = factory.database_provider_rw().unwrap();
        let state_provider = provider.latest();
        let sp_db = StateProviderDatabase::new(&state_provider);
        let mut db = State::builder().with_database(sp_db).with_bundle_update().build();
        let evm_config = BscEvmConfig::new(chain_spec.clone());
        let diff = calculate_difficulty(&block1_snap, *TEST_VALIDATOR);
        let mut builder = evm_config
            .builder_for_next_block(&mut db, &block1_sealed, attrs(block1_sealed.timestamp + 3, diff, block1_sealed.gas_limit))
            .expect("builder b2");
        builder.apply_pre_execution_changes().expect("pre-exec b2");
        builder.execute_transaction(user.clone().try_into_recovered().expect("recover user")).expect("exec user");
        let out = builder.finish(&state_provider, None).expect("finish b2");
        let BlockBuilderOutcome { execution_result, block, .. } = out;
        let output = BlockExecutionOutput { state: db.take_bundle(), result: execution_result };
        (block, output)
    };
    let reference_root = block2_ref.header().state_root;

    // --- Repackage block 2 as a BidBlock: [user (signed), deposit (UNSIGNED placeholder)]. ---
    let ref_txs: Vec<TransactionSigned> = block2_ref.body().transactions().cloned().collect();
    // Post-Kepler, past the block-1 init: the trailing system-tx region is exactly the deposit.
    assert_eq!(ref_txs.len(), 2, "block 2 should be [user, deposit]");
    let unsigned_deposit = TransactionSigned::new_unhashed(
        ref_txs[1].clone().into_typed_transaction(),
        Signature::new(alloy_primitives::U256::ZERO, alloy_primitives::U256::ZERO, false),
    );
    let submitted_txs = vec![
        alloy_primitives::Bytes::from(ref_txs[0].encoded_2718()),
        alloy_primitives::Bytes::from(unsigned_deposit.encoded_2718()),
    ];
    // The reference header's `transactions_root` commits to the *signed* deposit, but the body
    // submitted below carries the unsigned placeholder — so the root has to be recomputed over what
    // is actually sent. That is what a real builder does: since bsc #3742 the signature covers only
    // the header, so this root is the commitment binding the body, and `verify_bid_block_payload`
    // rejects a mismatch. Reusing the reference root here built a BidBlock no builder could produce.
    // `simulate_bid_block` still overwrites the root with the re-signed set, so the sealed header —
    // and the state root this test compares — is unaffected.
    let mut bid_header = block2_ref.header().clone();
    bid_header.transactions_root = submitted_tx_root(&submitted_txs);
    let bid = BidBlock { header: bid_header, transactions: submitted_txs, sidecars: vec![] };
    let args = BidBlockArgs { bid_block: bid, signature: alloy_primitives::Bytes::from(vec![0u8; 65]) };
    let decoded = args.to_decoded_bid_block(alloy_primitives::Address::repeat_byte(0xbb)).expect("decode bid");

    // --- Simulate (re-signs the deposit, finalizes) and execute the sealed block. ---
    let sim = simulate_bid_block(
        parlia,
        &chain_spec,
        &decoded,
        &block1_sealed,
        &block1_snap,
        &snapshot_provider,
        *TEST_VALIDATOR,
        block1_sealed.gas_limit,
        alloy_primitives::Bytes::from(vec![0u8; 32]),
        (block1_sealed.timestamp + 3) * 1000,
        None, // not an epoch block
    )
    .expect("simulate bid block");

    {
        let mut snap2 = block1_snap.clone();
        snap2.block_number = 2;
        snap2.block_hash = sim.block.header().hash_slow();
        snapshot_provider.insert(snap2);
    }

    let provider = factory.database_provider_rw().unwrap();
    let state_provider = provider.latest();
    let evm_config = BscEvmConfig::new(chain_spec.clone());
    let executor = evm_config.batch_executor(StateProviderDatabase::new(&state_provider));
    let output = executor.execute(&sim.block).expect("execute simulated block 2");
    let hashed = HashedPostState::from_bundle_state::<KeccakKeyHasher>(output.state.state());
    let (computed_root, _) = <StateRoot<
        DatabaseTrieCursorFactory<_, LegacyKeyAdapter>,
        DatabaseHashedCursorFactory<_>,
    > as DatabaseStateRoot<_>>::overlay_root_with_updates(provider.tx_ref(), &hashed.clone_into_sorted())
        .expect("state root");

    assert_eq!(computed_root, reference_root);
}
