//! From Nano, a block whose normal transaction has a blacklisted sender or recipient is
//! invalid, mirroring go-bsc's `stateTransition.execute`.

use crate::{
    chainspec::parser::parse_genesis_json,
    evm::api::BscEvm,
    node::evm::{
        config::{
            evm_env_for_header, BscBlockExecutionCtx, BscExecutionMode, BscExecutionSharedCtx,
        },
        executor::BscBlockExecutor,
    },
    system_contracts::SystemContract,
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

const NANO_BLOCK: u64 = 100;
const BLACKLISTED: Address = address!("0x489A8756C18C0b8B24EC2a2b9FF3D4d447F79BEc");
const NORMAL: Address = address!("0x1000000000000000000000000000000000000001");

fn execute(number: u64, from: Address, to: Address) -> Result<(), BlockExecutionError> {
    let spec = parse_genesis_json(&format!(
        r#"{{
            "config": {{ "chainId": 714, "ramanujanBlock": 0, "nielsBlock": 0, "nanoBlock": {NANO_BLOCK} }},
            "difficulty": "0x1", "gasLimit": "0x2625a00", "alloc": {{}}
        }}"#
    ))
    .expect("genesis with nanoBlock should parse");
    let header =
        Header { number, gas_limit: 40_000_000, parent_hash: B256::random(), ..Default::default() };
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
    let mut executor = BscBlockExecutor::new(
        evm,
        ctx,
        BscExecutionSharedCtx::default(),
        spec.clone(),
        RethReceiptBuilder::default(),
        SystemContract::new(spec),
    );
    let tx = TransactionSigned::new_unhashed(
        Transaction::Legacy(TxLegacy {
            to: TxKind::Call(to),
            gas_limit: 21_000,
            ..Default::default()
        }),
        Signature::test_signature(),
    );
    executor.execute_transaction_without_commit(Recovered::new_unchecked(tx, from)).map(|_| ())
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
