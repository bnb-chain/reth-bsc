//! From Nano, a block whose normal transaction has a blacklisted sender or recipient is
//! invalid, mirroring go-bsc's `stateTransition.execute`. From Cancun, a normal transaction
//! after a system transaction makes the block invalid, mirroring go-bsc's `StateProcessor.Process`.

use crate::{
    chainspec::{parser::parse_genesis_json, BscChainSpec},
    evm::api::BscEvm,
    node::evm::{
        config::{
            evm_env_for_header, BscBlockExecutionCtx, BscExecutionMode, BscExecutionSharedCtx,
        },
        error::BscBlockValidationError,
        executor::BscBlockExecutor,
    },
    system_contracts::{SystemContract, VALIDATOR_CONTRACT},
};
use alloy_consensus::{Header, TxLegacy};
use alloy_evm::{
    block::{BlockExecutionError, BlockExecutor, BlockValidationError},
    eth::EthBlockExecutionCtx,
};
use alloy_primitives::{address, Address, Bytes, Signature, TxKind, B256};
use reth_ethereum_primitives::{Transaction, TransactionSigned};
use reth_evm_ethereum::RethReceiptBuilder;
use reth_primitives_traits::Recovered;
use revm::{database::InMemoryDB, inspector::NoOpInspector};
use std::sync::Arc;

const NANO_BLOCK: u64 = 100;
const CANCUN_TIME: u64 = 1_000;
const BLACKLISTED: Address = address!("0x489A8756C18C0b8B24EC2a2b9FF3D4d447F79BEc");
const NORMAL: Address = address!("0x1000000000000000000000000000000000000001");
const VALIDATOR: Address = address!("0x2000000000000000000000000000000000000002");

type TestExecutor = BscBlockExecutor<
    'static,
    BscEvm<InMemoryDB, NoOpInspector>,
    Arc<BscChainSpec>,
    RethReceiptBuilder,
>;

fn executor(number: u64, timestamp: u64) -> TestExecutor {
    let spec = parse_genesis_json(&format!(
        r#"{{
            "config": {{
                "chainId": 714, "ramanujanBlock": 0, "nielsBlock": 0, "nanoBlock": {NANO_BLOCK},
                "berlinBlock": 0, "londonBlock": 0, "shanghaiTime": 0, "keplerTime": 0,
                "cancunTime": {CANCUN_TIME}
            }},
            "difficulty": "0x1", "gasLimit": "0x2625a00", "alloc": {{}}
        }}"#
    ))
    .expect("genesis with nanoBlock and cancunTime should parse");
    let header = Header {
        number,
        timestamp,
        beneficiary: VALIDATOR,
        gas_limit: 40_000_000,
        parent_hash: B256::random(),
        blob_gas_used: Some(0),
        excess_blob_gas: Some(0),
        ..Default::default()
    };
    let evm = BscEvm::new(
        evm_env_for_header(&spec, &header),
        InMemoryDB::default(),
        NoOpInspector {},
        false,
        false,
    );
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

/// A zero-gas-price call; it is a system transaction when sent by the beneficiary to a system
/// contract.
fn tx(from: Address, to: Address) -> Recovered<TransactionSigned> {
    let tx = TransactionSigned::new_unhashed(
        Transaction::Legacy(TxLegacy {
            to: TxKind::Call(to),
            gas_limit: 21_000,
            ..Default::default()
        }),
        Signature::test_signature(),
    );
    Recovered::new_unchecked(tx, from)
}

fn execute(number: u64, from: Address, to: Address) -> Result<(), BlockExecutionError> {
    executor(number, 0).execute_transaction_without_commit(tx(from, to)).map(|_| ())
}

fn is_invalid_tx(result: Result<(), BlockExecutionError>) -> bool {
    matches!(result, Err(BlockExecutionError::Validation(BlockValidationError::InvalidTx { .. })))
}

#[test]
fn blacklisted_sender_or_recipient_is_rejected_from_nano() {
    assert!(is_invalid_tx(execute(NANO_BLOCK, BLACKLISTED, NORMAL)));
    assert!(is_invalid_tx(execute(NANO_BLOCK, NORMAL, BLACKLISTED)));
    execute(NANO_BLOCK, NORMAL, NORMAL).expect("clean tx executes");
}

#[test]
fn blacklist_is_not_enforced_before_nano() {
    execute(NANO_BLOCK - 1, BLACKLISTED, NORMAL).expect("pre-Nano sender is not checked");
    execute(NANO_BLOCK - 1, NORMAL, BLACKLISTED).expect("pre-Nano recipient is not checked");
}

#[test]
fn normal_tx_after_system_tx_is_rejected_from_cancun() {
    let mut executor = executor(NANO_BLOCK, CANCUN_TIME);
    executor.execute_transaction_without_commit(tx(VALIDATOR, VALIDATOR_CONTRACT)).unwrap();
    let expected = BscBlockValidationError::UnexpectedNormalTx.to_string();
    assert!(matches!(
        executor.execute_transaction_without_commit(tx(NORMAL, NORMAL)),
        Err(BlockExecutionError::Validation(BlockValidationError::DepositRequestDecode(msg)))
            if msg.ends_with(&expected)
    ));
}

#[test]
fn system_tx_order_is_not_enforced_before_cancun() {
    let mut executor = executor(NANO_BLOCK, CANCUN_TIME - 1);
    executor.execute_transaction_without_commit(tx(VALIDATOR, VALIDATOR_CONTRACT)).unwrap();
    executor.execute_transaction_without_commit(tx(NORMAL, NORMAL)).expect("order is not checked");
}

#[test]
fn system_txs_at_the_end_are_allowed() {
    let mut executor = executor(NANO_BLOCK, CANCUN_TIME);
    executor.execute_transaction_without_commit(tx(NORMAL, NORMAL)).expect("normal tx executes");
    executor.execute_transaction_without_commit(tx(VALIDATOR, VALIDATOR_CONTRACT)).unwrap();
}
