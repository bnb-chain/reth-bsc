//! Chain-level Jenner (BEP-706) fork-transition test — the reth-bsc counterpart of
//! go-bsc's `TestJennerForkTransition` (C6): a chain whose genesis config schedules
//! `jennerTime` mid-chain, executed block by block across the activation boundary
//! with the BEP-706 §4.5 caller contract deployed.
//!
//! Like go's test (which runs `ethash.NewFullFaker()` to skip seal checks), this
//! exercises the execution layer — chainspec-driven activation, per-header env
//! derivation, spec-gated precompile registration — not Parlia sealing.

use crate::{
    chainspec::parser::parse_genesis_json,
    consensus::parlia::util::{calculate_millisecond_timestamp, set_millisecond_part_of_timestamp},
    evm::{api::BscEvm, transaction::BscTxEnv},
    node::evm::config::evm_env_for_header,
};
use alloy_consensus::Header;
use alloy_primitives::{Address, Bytes, U256};
use revm::{
    context::result::ExecutionResult,
    context::TxEnv,
    database::InMemoryDB,
    inspector::NoOpInspector,
    primitives::TxKind,
    state::{AccountInfo, Bytecode},
    ExecuteEvm,
};

/// Jenner activates mid-chain at this timestamp.
const JENNER_TIME: u64 = 1_790_000_000;

/// The BEP-706 §4.5 caller pattern as raw bytecode: STATICCALL into `0x70` with no
/// calldata and return the first 32 bytes of memory. Before activation the call
/// succeeds against an empty account and leaves memory untouched (returns 0); after
/// activation it returns the block's millisecond timestamp.
fn bep706_caller_code() -> Vec<u8> {
    // PUSH1 0x20 (retSize) PUSH1 0x00 (retOffset) PUSH1 0x00 (argsSize)
    // PUSH1 0x00 (argsOffset) PUSH1 0x70 (addr) GAS STATICCALL POP
    // PUSH1 0x20 PUSH1 0x00 RETURN
    vec![
        0x60, 0x20, 0x60, 0x00, 0x60, 0x00, 0x60, 0x00, 0x60, 0x70, 0x5A, 0xFA, 0x50, 0x60,
        0x20, 0x60, 0x00, 0xF3,
    ]
}

fn genesis_with_jenner() -> std::sync::Arc<crate::chainspec::BscChainSpec> {
    parse_genesis_json(&format!(
        r#"{{
            "config": {{
                "chainId": 714,
                "ramanujanBlock": 0,
                "nielsBlock": 0,
                "berlinBlock": 0,
                "londonBlock": 0,
                "shanghaiTime": 0,
                "keplerTime": 0,
                "cancunTime": 0,
                "pragueTime": 0,
                "pascalTime": 0,
                "lorentzTime": 0,
                "maxwellTime": 0,
                "jennerTime": {JENNER_TIME}
            }},
            "difficulty": "0x1",
            "gasLimit": "0x2625a00",
            "alloc": {{}}
        }}"#
    ))
    .expect("genesis with jennerTime should parse")
}

/// Executes the caller contract in the EVM environment `header` presents, returning
/// the 32-byte word the contract reads back from `0x70`.
fn call_bep706_contract_at(
    spec: &crate::chainspec::BscChainSpec,
    header: &Header,
) -> (bool, U256) {
    let caller = Address::from([0x11; 20]);
    let contract = Address::from([0x22; 20]);

    let env = evm_env_for_header(spec, header);

    let mut db = InMemoryDB::default();
    db.insert_account_info(
        caller,
        AccountInfo { balance: U256::from(1_000_000u64), ..AccountInfo::default() },
    );
    db.insert_account_info(
        contract,
        AccountInfo::default().with_code(Bytecode::new_raw(Bytes::from(bep706_caller_code()))),
    );

    let mut evm = BscEvm::new(env, db, NoOpInspector, false, false);
    let tx = BscTxEnv::new(
        TxEnv::builder()
            .caller(caller)
            .chain_id(Some(714))
            .gas_limit(200_000)
            .gas_price(1)
            .kind(TxKind::Call(contract))
            .build()
            .expect("tx env should build"),
    );

    match evm.transact_one(tx).expect("execution should not error") {
        ExecutionResult::Success { output, .. } => (true, U256::from_be_slice(&output.into_data())),
        _ => (false, U256::ZERO),
    }
}

/// go-bsc `TestJennerForkTransition`: five consecutive blocks crossing the
/// activation boundary — before it the §4.5 contract reads an empty account
/// (ok == true, value 0); from it on, each block reads its own header's
/// millisecond timestamp, increasing block over block (never a cached value).
#[test]
fn jenner_fork_transition_across_blocks() {
    let spec = genesis_with_jenner();

    // Five blocks: two before activation, one exactly at it, two after.
    // Millisecond remainders exercise the mix_hash decoding on every block.
    let schedule: [(u64, u64, u64, bool); 5] = [
        // (number, timestamp, ms_remainder, jenner_active)
        (1, JENNER_TIME - 2, 100, false),
        (2, JENNER_TIME - 1, 200, false),
        (3, JENNER_TIME, 300, true),
        (4, JENNER_TIME + 1, 400, true),
        (5, JENNER_TIME + 2, 500, true),
    ];

    let mut last_active_value = U256::ZERO;
    for (number, timestamp, remainder, active) in schedule {
        let mut header = Header {
            number,
            timestamp,
            gas_limit: 40_000_000,
            // Cancun is active from genesis in this spec.
            excess_blob_gas: Some(0),
            blob_gas_used: Some(0),
            ..Default::default()
        };
        set_millisecond_part_of_timestamp(timestamp * 1000 + remainder, &mut header);

        let (ok, value) = call_bep706_contract_at(&spec, &header);
        assert!(ok, "block {number}: the caller contract must succeed on both sides of the fork");

        if active {
            let expected = calculate_millisecond_timestamp(&header);
            assert_eq!(
                value,
                U256::from(expected),
                "block {number}: 0x70 must report this block's own millisecond timestamp"
            );
            assert!(
                value > last_active_value,
                "block {number}: the value must increase with every block, not stay cached"
            );
            last_active_value = value;
        } else {
            assert_eq!(
                value,
                U256::ZERO,
                "block {number}: before Jenner, 0x70 is an empty account and the \
                 contract reads back zero"
            );
        }
    }
}
