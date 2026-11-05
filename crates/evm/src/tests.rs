use super::*;
use alloy_consensus::{
    transaction::Recovered, TxEip1559, TxEip2930, TxEip4844, TxEip7702, TxLegacy,
};
use alloy_eips::eip7702::{Authorization, RecoveredAuthority, RecoveredAuthorization};
use alloy_primitives::{Bytes, TxKind, B256};
use evm2::{
    env::BlockEnv,
    evm::{AccountInfo, InMemoryDB},
    precompiles::Precompiles,
    BaseEvmTypes, ExecutionConfig, Version,
};

const CALLER: Address = address!("1111111111111111111111111111111111111111");
const RECIPIENT: Address = address!("2222222222222222222222222222222222222222");
const BENEFICIARY: Address = address!("3333333333333333333333333333333333333333");
const INITIAL_BALANCE: u64 = 1_000_000_000_000;

fn evm(spec: SpecId, db: InMemoryDB) -> Evm<'static, BaseEvmTypes> {
    let mut version = Version::new(spec);
    version.chain_id = 56;
    Evm::new_with_execution_config(
        ExecutionConfig::for_spec_and_version(spec, version),
        spec,
        BlockEnv::<BaseEvmTypes> {
            beneficiary: BENEFICIARY,
            gas_limit: U256::from(30_000_000),
            basefee: U256::from(2),
            blob_basefee: U256::from(3),
            ..Default::default()
        },
        bsc_user_tx_registry(spec),
        db,
        Precompiles::base(spec),
    )
}

fn database() -> InMemoryDB {
    let mut db = InMemoryDB::default();
    db.insert_account_info(
        &CALLER,
        AccountInfo::default().with_balance(U256::from(INITIAL_BALANCE)),
    );
    db
}

fn balance(evm: &mut Evm<'_, BaseEvmTypes>, address: Address) -> U256 {
    evm.state_mut().account_info_untracked(&address).unwrap().map(|a| a.balance).unwrap_or_default()
}

#[test]
fn every_transaction_type_collects_full_fee_and_blob_fee() {
    let txs: Vec<(SpecId, TxEnvelope, u64)> = vec![
        (
            SpecId::ISTANBUL,
            TxEnvelope::Legacy(TxLegacy {
                chain_id: Some(56),
                gas_limit: 30_000,
                gas_price: 5,
                to: TxKind::Call(RECIPIENT),
                value: U256::from(7),
                ..Default::default()
            }),
            0,
        ),
        (
            SpecId::BERLIN,
            TxEnvelope::Eip2930(TxEip2930 {
                chain_id: 56,
                gas_limit: 30_000,
                gas_price: 5,
                to: TxKind::Call(RECIPIENT),
                value: U256::from(7),
                ..Default::default()
            }),
            0,
        ),
        (
            SpecId::LONDON,
            TxEnvelope::Eip1559(TxEip1559 {
                chain_id: 56,
                gas_limit: 30_000,
                max_fee_per_gas: 9,
                max_priority_fee_per_gas: 3,
                to: TxKind::Call(RECIPIENT),
                value: U256::from(7),
                ..Default::default()
            }),
            0,
        ),
        (
            SpecId::CANCUN,
            TxEnvelope::Eip4844(
                TxEip4844 {
                    chain_id: 56,
                    gas_limit: 30_000,
                    max_fee_per_gas: 9,
                    max_priority_fee_per_gas: 3,
                    max_fee_per_blob_gas: 10,
                    blob_versioned_hashes: vec![B256::repeat_byte(1)],
                    to: RECIPIENT,
                    value: U256::from(7),
                    ..Default::default()
                }
                .into(),
            ),
            131_072,
        ),
        (SpecId::PRAGUE, authorization_tx(NANO_BLACKLIST[0], 0, false), 0),
    ];
    for (spec, tx, blob_gas) in txs {
        let mut evm = evm(spec, database());
        let result = evm.transact(&Recovered::new_unchecked(tx, CALLER)).unwrap().commit();
        assert!(result.status, "{spec:?}: {result:?}");
        let fee = U256::from(result.tx_gas_used() * 5 + blob_gas * 3);
        assert_eq!(balance(&mut evm, SYSTEM_ADDRESS), fee, "{spec:?}");
        assert_eq!(balance(&mut evm, BENEFICIARY), U256::ZERO, "{spec:?}");
        assert_eq!(balance(&mut evm, CALLER), U256::from(INITIAL_BALANCE - 7) - fee, "{spec:?}");
        assert_eq!(balance(&mut evm, RECIPIENT), U256::from(7));
        assert_eq!(evm.state_mut().account_info_untracked(&CALLER).unwrap().unwrap().nonce, 1);
    }
}

fn authorization_tx(authority: Address, nonce: u64, clears: bool) -> TxEnvelope {
    let auth = Authorization {
        chain_id: U256::from(56),
        address: if clears { Address::ZERO } else { RECIPIENT },
        nonce,
    };
    let recovered =
        RecoveredAuthorization::new_unchecked(auth, RecoveredAuthority::Valid(authority));
    let tx = TxEip7702 {
        chain_id: 56,
        gas_limit: 100_000,
        max_fee_per_gas: 9,
        max_priority_fee_per_gas: 3,
        to: RECIPIENT,
        value: U256::from(7),
        ..Default::default()
    };
    TxEnvelope::Eip7702(LazyTxEip7702::from_cached_recovered_authorizations(tx, vec![recovered]))
}

#[test]
fn blacklisted_authorizations_keep_intrinsic_gas_without_touching_state() {
    for authority in NANO_BLACKLIST {
        let mut evm = evm(SpecId::PRAGUE, database());
        let tx = Recovered::new_unchecked(authorization_tx(authority, 0, false), CALLER);
        let result = evm.transact(&tx).unwrap().commit();
        assert!(result.status);
        assert_eq!(result.tx_gas_used(), 46_000);
        assert_eq!(result.refunded, 0);
        assert_eq!(evm.state_mut().account_info_untracked(&authority).unwrap(), None);
    }
}

#[test]
fn accepted_authorization_changes_nonce_code_and_refund() {
    let authority = Address::with_last_byte(0xab);
    let mut db = database();
    db.insert_account_info(&authority, AccountInfo::default().with_balance(U256::from(1)));
    let mut evm = evm(SpecId::PRAGUE, db);
    let tx = Recovered::new_unchecked(authorization_tx(authority, 0, false), CALLER);
    let result = evm.transact(&tx).unwrap().commit();
    assert_eq!(result.tx_gas_used(), 36_800);
    assert_eq!(result.refunded, 9_200);
    let account = evm.state_mut().account_info_untracked(&authority).unwrap().unwrap();
    assert_eq!(account.nonce, 1);
    let mut expected = vec![0xef, 0x01, 0x00];
    expected.extend_from_slice(RECIPIENT.as_slice());
    assert_eq!(account.code_hash, alloy_primitives::keccak256(expected));
    assert_eq!(account.balance, U256::from(1));
}

#[test]
fn discarded_transaction_does_not_leak_fees_or_authorization_state() {
    let authority = Address::with_last_byte(0xab);
    let mut evm = evm(SpecId::PRAGUE, database());
    let tx = Recovered::new_unchecked(authorization_tx(authority, 0, false), CALLER);
    assert!(evm.transact(&tx).unwrap().discard().status);
    assert_eq!(balance(&mut evm, CALLER), U256::from(INITIAL_BALANCE));
    assert_eq!(balance(&mut evm, SYSTEM_ADDRESS), U256::ZERO);
    assert_eq!(evm.state_mut().account_info_untracked(&authority).unwrap(), None);
    assert!(evm.transact(&tx).unwrap().commit().status);
    assert_eq!(evm.state_mut().account_info_untracked(&CALLER).unwrap().unwrap().nonce, 1);
}

#[test]
fn revert_charges_gas_but_reverts_value_transfer() {
    let mut db = database();
    db.insert_account_info(
        &RECIPIENT,
        AccountInfo::default().with_code(evm2::bytecode::Bytecode::new_legacy(Bytes::from_static(
            &[0x60, 0, 0x60, 0, 0xfd],
        ))),
    );
    let mut evm = evm(SpecId::LONDON, db);
    let tx = Recovered::new_unchecked(
        TxEnvelope::Legacy(TxLegacy {
            chain_id: Some(56),
            gas_limit: 100_000,
            gas_price: 5,
            to: TxKind::Call(RECIPIENT),
            value: U256::from(7),
            ..Default::default()
        }),
        CALLER,
    );
    let result = evm.transact(&tx).unwrap().commit();
    assert!(!result.status);
    assert_eq!(balance(&mut evm, SYSTEM_ADDRESS), U256::from(result.tx_gas_used() * 5));
    assert_eq!(balance(&mut evm, RECIPIENT), U256::ZERO);
    assert_eq!(balance(&mut evm, CALLER), U256::from(INITIAL_BALANCE - result.tx_gas_used() * 5));
}

#[test]
fn blacklisted_authority_remains_cold_during_execution() {
    let authority = NANO_BLACKLIST[0];
    let mut code = vec![0x73]; // PUSH20 authority; BALANCE; POP; STOP
    code.extend_from_slice(authority.as_slice());
    code.extend_from_slice(&[0x31, 0x50, 0x00]);
    let mut db = database();
    db.insert_account_info(
        &RECIPIENT,
        AccountInfo::default().with_code(evm2::bytecode::Bytecode::new_legacy(code.into())),
    );
    let mut evm = evm(SpecId::PRAGUE, db);
    let tx = Recovered::new_unchecked(authorization_tx(authority, 0, false), CALLER);
    let result = evm.transact(&tx).unwrap().commit();
    assert!(result.status);
    assert_eq!(result.tx_gas_used(), 46_000 + 3 + 2_600 + 2);
}

#[test]
fn mixed_authorizations_preserve_order_and_charge_for_rejected_entries() {
    let authority = Address::with_last_byte(0xab);
    let mut tx = authorization_tx(authority, 0, false);
    let TxEnvelope::Eip7702(ref mut inner) = tx else { unreachable!() };
    let TxEnvelope::Eip7702(blocked) = authorization_tx(NANO_BLACKLIST[0], 0, false) else {
        unreachable!()
    };
    let TxEnvelope::Eip7702(clear) = authorization_tx(authority, 1, true) else { unreachable!() };
    inner.authorization_list.splice(0..0, blocked.authorization_list);
    inner.authorization_list.extend(clear.authorization_list);
    let original = tx.clone();
    let mut evm = evm(SpecId::PRAGUE, database());
    let tx = Recovered::new_unchecked(tx, CALLER);
    let result = evm.transact(&tx).unwrap().commit();
    assert!(result.status);
    assert_eq!(result.total_gas_spent, 21_000 + 3 * 25_000);
    assert_eq!(result.refunded, 12_500);
    let info = evm.state_mut().account_info_untracked(&authority).unwrap().unwrap();
    assert_eq!(info.nonce, 2);
    assert_eq!(info.code_hash, alloy_primitives::KECCAK256_EMPTY);
    assert_eq!(evm.state_mut().account_info_untracked(&NANO_BLACKLIST[0]).unwrap(), None);
    assert_eq!(tx.inner(), &original, "execution must not mutate the original envelope");
}

#[test]
fn revert_preserves_accepted_authorization_and_sender_nonce() {
    let authority = Address::with_last_byte(0xab);
    let mut db = database();
    db.insert_account_info(
        &RECIPIENT,
        AccountInfo::default().with_code(evm2::bytecode::Bytecode::new_legacy(Bytes::from_static(
            &[0x60, 0, 0x60, 0, 0xfd],
        ))),
    );
    let mut evm = evm(SpecId::PRAGUE, db);
    let tx = Recovered::new_unchecked(authorization_tx(authority, 0, false), CALLER);
    let result = evm.transact(&tx).unwrap().commit();
    assert!(!result.status);
    assert_eq!(balance(&mut evm, RECIPIENT), U256::ZERO);
    let info = evm.state_mut().account_info_untracked(&authority).unwrap().unwrap();
    assert_eq!(info.nonce, 1);
    assert_ne!(info.code_hash, alloy_primitives::KECCAK256_EMPTY);
    assert_eq!(evm.state_mut().account_info_untracked(&CALLER).unwrap().unwrap().nonce, 1);
    assert_eq!(balance(&mut evm, SYSTEM_ADDRESS), U256::from(result.tx_gas_used() * 5));
}

#[test]
fn ordinary_transactions_still_enforce_osaka_gas_cap() {
    let mut evm = evm(SpecId::OSAKA, database());
    let tx = Recovered::new_unchecked(
        TxEnvelope::Legacy(TxLegacy {
            chain_id: Some(56),
            gas_limit: (1 << 24) + 1,
            gas_price: 5,
            to: TxKind::Call(RECIPIENT),
            ..Default::default()
        }),
        CALLER,
    );
    assert!(matches!(
        evm.transact(&tx),
        Err(evm2::registry::HandlerError::TxGasLimitGreaterThanCap { .. })
    ));
    assert_eq!(balance(&mut evm, CALLER), U256::from(INITIAL_BALANCE));
    assert_eq!(balance(&mut evm, SYSTEM_ADDRESS), U256::ZERO);
    assert_eq!(evm.state_mut().account_info_untracked(&CALLER).unwrap().unwrap().nonce, 0);
}

#[test]
fn detached_state_is_invisible_until_committed() {
    let authority = Address::with_last_byte(0xab);
    let mut evm = evm(SpecId::PRAGUE, database());
    let tx = Recovered::new_unchecked(authorization_tx(authority, 0, false), CALLER);
    let detached = evm.transact(&tx).unwrap().detach();
    assert!(detached.result.status);
    assert_eq!(balance(&mut evm, CALLER), U256::from(INITIAL_BALANCE));
    assert_eq!(evm.state_mut().account_info_untracked(&authority).unwrap(), None);
    evm.overlay_db_mut().commit_pending(&detached.pending_state);
    assert_eq!(evm.state_mut().account_info_untracked(&authority).unwrap().unwrap().nonce, 1);
    assert_eq!(balance(&mut evm, SYSTEM_ADDRESS), U256::from(detached.result.tx_gas_used() * 5));
}

#[test]
fn streamed_state_and_final_bundle_produce_expected_root() {
    use alloy_primitives::keccak256;
    use alloy_trie::{
        root::{state_root_unsorted, storage_root_unhashed},
        TrieAccount,
    };
    use reth_execution_types::{state_update_to_hashed_post_state, BlockState, StateUpdate};
    use reth_trie_common::{HashedPostState, KeccakKeyHasher};
    use std::collections::BTreeMap;

    // The contract writes its calldata word to slot zero. The second transaction clears it,
    // exercising net-zero block storage changes as well as repeated sender/fee-account changes.
    let code = evm2::bytecode::Bytecode::new_legacy(Bytes::from_static(&[
        0x60, 0, 0x35, 0x60, 0, 0x55, 0x00,
    ]));
    let code_hash = keccak256(code.original_bytes());
    let mut db = database();
    db.insert_account_info(&RECIPIENT, AccountInfo::default().with_code(code));
    let mut evm = evm(SpecId::PRAGUE, db);
    let mut block = BlockState::new();
    let mut streamed = HashedPostState::default();
    let mut total_fee = 0u64;
    for (nonce, value) in [7u64, 0].into_iter().enumerate() {
        let tx = Recovered::new_unchecked(
            TxEnvelope::Legacy(TxLegacy {
                chain_id: Some(56),
                nonce: nonce as u64,
                gas_limit: 100_000,
                gas_price: 5,
                to: TxKind::Call(RECIPIENT),
                input: U256::from(value).to_be_bytes::<32>().to_vec().into(),
                ..Default::default()
            }),
            CALLER,
        );
        let detached = evm.transact(&tx).unwrap().detach();
        assert!(detached.result.status);
        total_fee += detached.result.tx_gas_used() * 5;
        let mut update = StateUpdate::default();
        for changes in detached.pending_state.account_changes() {
            block.commit_account_changes(changes, Some(&mut update));
        }
        streamed.extend(state_update_to_hashed_post_state(&update));
        evm.overlay_db_mut().commit_pending(&detached.pending_state);
    }
    let bundle = block.into_bundle();
    let bundled = HashedPostState::from_bundle_state::<KeccakKeyHasher>(&bundle.state);

    // Build complete tries from the same known pre-state, rather than comparing write-set
    // representation: an explicit zero slot and an omitted net-zero update are equivalent.
    let parent = BTreeMap::from([
        (
            keccak256(CALLER),
            TrieAccount { balance: U256::from(INITIAL_BALANCE), ..Default::default() },
        ),
        (keccak256(RECIPIENT), TrieAccount { code_hash, ..Default::default() }),
    ]);
    let root = |post: HashedPostState| {
        let mut accounts = parent.clone();
        for (address, info) in post.accounts {
            if let Some(info) = info {
                let storage = post
                    .storages
                    .get(&address)
                    .map(|storage| {
                        // Slots in HashedPostState are already hashed, so build their trie
                        // directly.
                        alloy_trie::root::storage_root_unsorted(
                            storage
                                .storage
                                .iter()
                                .filter(|(_, value)| !value.is_zero())
                                .map(|(key, value)| (*key, *value)),
                        )
                    })
                    .unwrap_or_else(|| storage_root_unhashed(std::iter::empty()));
                accounts.insert(
                    address,
                    TrieAccount {
                        nonce: info.nonce,
                        balance: info.balance,
                        code_hash: info.bytecode_hash.unwrap_or(alloy_primitives::KECCAK256_EMPTY),
                        storage_root: storage,
                    },
                );
            } else {
                accounts.remove(&address);
            }
        }
        state_root_unsorted(accounts)
    };
    let expected = state_root_unsorted([
        (
            keccak256(CALLER),
            TrieAccount {
                nonce: 2,
                balance: U256::from(INITIAL_BALANCE - total_fee),
                ..Default::default()
            },
        ),
        (keccak256(RECIPIENT), TrieAccount { code_hash, ..Default::default() }),
        (
            keccak256(SYSTEM_ADDRESS),
            TrieAccount { balance: U256::from(total_fee), ..Default::default() },
        ),
    ]);
    assert_eq!(
        root(streamed),
        expected,
        "streamed trie updates must include all BSC fees and final storage"
    );
    assert_eq!(root(bundled), expected, "persisted bundle must produce the same state root");
}
