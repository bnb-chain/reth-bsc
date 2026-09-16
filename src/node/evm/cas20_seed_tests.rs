//! The CAS20 registries get their account sentinels at the Jenner transition
//! (BEP-702 3.16), the reth-bsc counterpart of go-bsc's `SeedCAS20Activation`
//! running from `TryUpdateBuildInSystemContract` at block begin.

use crate::{
    chainspec::{parser::parse_genesis_json, BscChainSpec},
    evm::{api::BscEvm, precompiles::cas20},
    hardforks::BscHardforks,
    node::evm::{
        config::{
            evm_env_for_header, BscBlockExecutionCtx, BscExecutionMode, BscExecutionSharedCtx,
        },
        executor::BscBlockExecutor,
    },
    system_contracts::SystemContract,
};
use alloy_consensus::Header;
use alloy_evm::{block::BlockExecutor, eth::EthBlockExecutionCtx};
use alloy_primitives::{Bytes, B256};
use reth_evm::Evm;
use reth_evm_ethereum::RethReceiptBuilder;
use revm::{
    bytecode::Bytecode, database::InMemoryDB, inspector::NoOpInspector, primitives::KECCAK_EMPTY,
    state::AccountInfo, Database,
};
use std::sync::Arc;

const JENNER_TIME: u64 = 1_790_000_000;

fn spec() -> Arc<BscChainSpec> {
    parse_genesis_json(&format!(
        r#"{{
            "config": {{
                "chainId": 714, "ramanujanBlock": 0, "nielsBlock": 0, "berlinBlock": 0,
                "londonBlock": 0, "shanghaiTime": 0, "keplerTime": 0, "cancunTime": 0,
                "pragueTime": 0, "pascalTime": 0, "lorentzTime": 0, "maxwellTime": 0,
                "jennerTime": {JENNER_TIME}
            }},
            "difficulty": "0x1", "gasLimit": "0x2625a00", "alloc": {{}}
        }}"#
    ))
    .expect("genesis with jennerTime should parse")
}

fn header_at(number: u64, timestamp: u64) -> Header {
    Header {
        number,
        timestamp,
        gas_limit: 40_000_000,
        parent_hash: B256::random(),
        ..Default::default()
    }
}

type TestExecutor = BscBlockExecutor<
    'static,
    BscEvm<InMemoryDB, NoOpInspector>,
    Arc<BscChainSpec>,
    RethReceiptBuilder,
>;

fn executor(db: InMemoryDB, header: Header) -> TestExecutor {
    let spec = spec();
    let evm = BscEvm::new(evm_env_for_header(&spec, &header), db, NoOpInspector {}, false, false);
    let ctx = BscBlockExecutionCtx {
        base: EthBlockExecutionCtx {
            parent_hash: header.parent_hash,
            parent_beacon_block_root: None,
            ommers: &[],
            withdrawals: None,
            extra_data: Bytes::new(),
            tx_count_hint: None,
            slot_number: None,
        },
        header: Some(header),
        header_hash: None,
        mode: BscExecutionMode::Import,
        validator_cache_sink: None,
        turn_length_sink: None,
        state_root_precomputed_sink: None,
        trie_handle: None,
        state_root_deadline_ms: None,
    };
    BscBlockExecutor::new(
        evm,
        ctx,
        BscExecutionSharedCtx::default(),
        spec.clone(),
        RethReceiptBuilder::default(),
        SystemContract::new(spec),
    )
}

#[test]
fn the_transition_is_the_first_block_at_or_past_jenner_time() {
    let spec = spec();
    assert!(spec.is_jenner_transition_at_timestamp(10, JENNER_TIME, JENNER_TIME - 3));
    assert!(spec.is_jenner_transition_at_timestamp(10, JENNER_TIME + 2, JENNER_TIME - 1));
    assert!(!spec.is_jenner_transition_at_timestamp(11, JENNER_TIME + 3, JENNER_TIME));
    assert!(!spec.is_jenner_transition_at_timestamp(9, JENNER_TIME - 3, JENNER_TIME - 6));
}

#[test]
fn seeding_plants_the_sentinel_on_both_registries_and_leaves_foreign_code_alone() {
    let foreign = Bytecode::new_raw(Bytes::from_static(&[0x60, 0x00]));
    let mut db = InMemoryDB::default();
    // One registry already carries code that is not the marker: it must survive.
    db.insert_account_info(
        cas20::POLICY_REGISTRY_ADDRESS,
        AccountInfo::default().with_code(foreign.clone()),
    );
    let mut executor = executor(db, header_at(10, JENNER_TIME));

    executor.seed_cas20_registries(10).expect("seeding succeeds");

    let db = executor.evm_mut().db_mut();
    let activation = db.basic(cas20::ACTIVATION_REGISTRY_ADDRESS).unwrap().expect("account exists");
    assert!(cas20::is_marker_code_hash(activation.code_hash));
    assert_eq!(
        activation.code.as_ref().map(|c: &Bytecode| c.original_bytes()),
        Some(Bytes::from_static(&cas20::MARKER_CODE))
    );
    let policy = db.basic(cas20::POLICY_REGISTRY_ADDRESS).unwrap().expect("account exists");
    assert_eq!(policy.code_hash, foreign.hash_slow(), "foreign code is not overwritten");

    // Seeding again is a no-op: the marker is left as it is.
    executor.seed_cas20_registries(10).expect("second seeding succeeds");
    let db = executor.evm_mut().db_mut();
    let activation = db.basic(cas20::ACTIVATION_REGISTRY_ADDRESS).unwrap().unwrap();
    assert!(cas20::is_marker_code_hash(activation.code_hash));
    assert_ne!(activation.code_hash, KECCAK_EMPTY);
}
