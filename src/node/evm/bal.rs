//! BSC recording boundaries for BEP-732 block access lists.

use alloy_eips::eip7928::BlockAccessIndex;
use reth_evm::execute::BlockExecutionError;
use revm::state::{bal::Bal, EvmState};

/// Coalesces protocol writes with the next included transaction before recording its net changes.
///
/// Parlia defers system transactions until finalization. Only an executed, committed transaction
/// advances this recorder; merely encountering a system transaction in the block body does not.
pub(super) struct BscBalRecorder<EVM> {
    bal: Bal,
    pending: EvmState,
    index: u32,
    publish: fn(&mut EVM, Bal),
}

impl<EVM> BscBalRecorder<EVM> {
    pub(super) fn new(publish: fn(&mut EVM, Bal)) -> Self {
        Self { bal: Bal::new(), pending: EvmState::default(), index: 0, publish }
    }

    /// Records committed EVM state, excluding protocol queries that never commit state.
    pub(super) fn record(&mut self, changes: &EvmState) {
        for (address, account) in changes {
            self.pending
                .entry(*address)
                .and_modify(|pending| {
                    // Keep the first original info and slot values across direct Finalize writes
                    // and the following system transaction. An intermediate credit followed by a
                    // debit must disappear when its combined net change is zero.
                    pending.info.clone_from(&account.info);
                    pending.status = account.status;
                    for (key, slot) in &account.storage {
                        pending
                            .storage
                            .entry(*key)
                            .and_modify(|pending_slot| {
                                pending_slot.present_value = slot.present_value;
                            })
                            .or_insert_with(|| slot.clone());
                    }
                })
                .or_insert_with(|| account.clone());
        }
    }

    /// Closes pre-execution or one included transaction and opens the next index.
    pub(super) fn advance(&mut self) -> Result<(), BlockExecutionError> {
        let next = self
            .index
            .checked_add(1)
            .ok_or_else(|| BlockExecutionError::other(BscBalIndexOverflow))?;
        self.flush();
        self.index = next;
        Ok(())
    }

    /// Publishes the BAL after recording remaining post-execution writes at index `n + 1`.
    pub(super) fn finish(mut self, evm: &mut EVM) {
        self.flush();
        (self.publish)(evm, self.bal);
    }

    fn flush(&mut self) {
        let index = BlockAccessIndex::new(u64::from(self.index));
        for (address, account) in self.pending.drain() {
            self.bal.update_account(index, address, &account);
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("BEP-732 block access index exceeds uint32")]
struct BscBalIndexOverflow;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        chainspec::{parser::parse_genesis_json, BscChainSpec},
        evm::api::BscEvm,
        node::{
            evm::{config::BscEvmConfig, executor::BscBlockExecutor},
            primitives::BscBlock,
        },
        system_contracts::VALIDATOR_CONTRACT,
    };
    use alloy_consensus::{Header, TxLegacy};
    use alloy_eips::eip7928::{BalanceChange, BlockAccessList, StorageChange};
    use alloy_evm::block::{BlockExecutor, CommitChanges};
    use alloy_primitives::{address, Address, Bytes, Signature, TxKind, U256};
    use reth_ethereum_primitives::{Transaction, TransactionSigned};
    use reth_evm::{ConfigureEvm, Evm};
    use reth_evm_ethereum::RethReceiptBuilder;
    use reth_primitives_traits::{Recovered, SealedBlock};
    use revm::{
        database::{InMemoryDB, State},
        inspector::NoOpInspector,
        state::{Account, AccountInfo, Bytecode, EvmStorageSlot, TransactionId},
        Database,
    };
    use std::sync::Arc;

    fn recorder() -> BscBalRecorder<Option<BlockAccessList>> {
        BscBalRecorder::new(|output, bal| *output = Some(bal.into_alloy_bal()))
    }

    fn balance_change(before: u64, after: u64) -> EvmState {
        let mut account =
            Account::from(AccountInfo { balance: U256::from(before), ..Default::default() });
        account.info.balance = U256::from(after);
        account.mark_touch();
        [(Address::ZERO, account)].into_iter().collect()
    }

    #[test]
    fn pre_user_system_and_post_execution_indices() {
        let mut recorder = recorder();
        recorder.record(&balance_change(0, 1));
        recorder.advance().unwrap();
        recorder.record(&balance_change(1, 2));
        recorder.advance().unwrap();
        // Finalize's direct credit belongs to the following included system transaction.
        recorder.record(&balance_change(2, 10));
        recorder.record(&balance_change(10, 3));
        recorder.advance().unwrap();
        recorder.record(&balance_change(3, 4));
        let mut output = None;
        recorder.finish(&mut output);
        let bal = output.unwrap();
        assert_eq!(
            bal[0].balance_changes,
            (0..4)
                .map(|index| BalanceChange {
                    block_access_index: BlockAccessIndex::new(index),
                    post_balance: U256::from(index + 1),
                })
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn coalesces_net_zero_balance_and_storage_changes() {
        let mut recorder = recorder();
        recorder.advance().unwrap();
        let mut credit = balance_change(5, 10);
        credit.get_mut(&Address::ZERO).unwrap().storage.insert(
            U256::ZERO,
            EvmStorageSlot::new_changed(U256::from(7), U256::from(9), TransactionId::ZERO),
        );
        let mut debit = balance_change(10, 5);
        debit.get_mut(&Address::ZERO).unwrap().storage.insert(
            U256::ZERO,
            EvmStorageSlot::new_changed(U256::from(9), U256::from(7), TransactionId::ZERO),
        );
        recorder.record(&credit);
        recorder.record(&debit);
        recorder.advance().unwrap();
        let mut output = None;
        recorder.finish(&mut output);
        let bal = output.unwrap();
        assert!(bal[0].balance_changes.is_empty());
        assert!(bal[0].storage_changes.is_empty());
        assert_eq!(bal[0].storage_reads, vec![U256::ZERO]);
    }

    #[test]
    fn coalesces_multiple_writes_but_preserves_reads() {
        let mut recorder = recorder();
        recorder.advance().unwrap();
        let mut first = balance_change(0, 1);
        first
            .get_mut(&Address::ZERO)
            .unwrap()
            .storage
            .insert(U256::from(1), EvmStorageSlot::new(U256::from(4), TransactionId::ZERO));
        let mut second = balance_change(1, 2);
        second.get_mut(&Address::ZERO).unwrap().storage.insert(
            U256::from(2),
            EvmStorageSlot::new_changed(U256::ZERO, U256::from(9), TransactionId::ZERO),
        );
        recorder.record(&first);
        recorder.record(&second);
        let mut output = None;
        recorder.finish(&mut output);
        let bal = output.unwrap();
        assert_eq!(bal[0].storage_reads, vec![U256::from(1)]);
        assert_eq!(
            bal[0].storage_changes[0].changes,
            vec![StorageChange {
                block_access_index: BlockAccessIndex::new(1),
                new_value: U256::from(9),
            }]
        );
        assert_eq!(bal[0].balance_changes.len(), 1);
        assert_eq!(bal[0].balance_changes[0].post_balance, U256::from(2));
    }

    #[test]
    fn rejects_index_overflow() {
        let mut recorder = recorder();
        recorder.index = u32::MAX;
        assert_eq!(
            recorder.advance().unwrap_err().to_string(),
            "BEP-732 block access index exceeds uint32"
        );
    }

    const SENDER: Address = address!("1000000000000000000000000000000000000001");
    const VALIDATOR: Address = address!("2000000000000000000000000000000000000002");
    const RECIPIENT: Address = address!("3000000000000000000000000000000000000003");
    const SKIPPED: Address = address!("4000000000000000000000000000000000000004");

    type TestExecutor<'a> = BscBlockExecutor<
        'a,
        BscEvm<&'a mut State<InMemoryDB>, NoOpInspector>,
        Arc<BscChainSpec>,
        RethReceiptBuilder,
    >;

    fn execute_recorded(run: impl FnOnce(&mut TestExecutor<'_>)) -> BlockAccessList {
        let spec = parse_genesis_json(
            r#"{
            "config": {
                "chainId":714, "ramanujanBlock":0, "nielsBlock":0,
                "berlinBlock":0, "londonBlock":0, "shanghaiTime":0, "keplerTime":0,
                "cancunTime":0, "pragueTime":0, "pascalTime":0
            },
            "difficulty":"0x1", "gasLimit":"0x2625a00", "alloc":{}
        }"#,
        )
        .unwrap();
        let config = BscEvmConfig::bsc(spec);
        let block = SealedBlock::seal_slow(BscBlock {
            header: Header {
                number: 10,
                timestamp: 10,
                beneficiary: VALIDATOR,
                gas_limit: 40_000_000,
                blob_gas_used: Some(0),
                excess_blob_gas: Some(0),
                ..Default::default()
            },
            body: Default::default(),
        });
        let mut inner = InMemoryDB::default();
        inner.insert_account_info(
            SENDER,
            AccountInfo { balance: U256::from(1_000_000), ..Default::default() },
        );
        inner.insert_account_info(
            VALIDATOR,
            AccountInfo { balance: U256::from(50), ..Default::default() },
        );
        let mut db = State::builder().with_database(inner).with_bal_builder().build();
        let mut executor = config.executor_for_block(&mut db, &block).unwrap();
        // The factory recognizes CacheDB as RPC tracing. This fixture uses it as an importer;
        // real importers use StateProviderDatabase and must not receive replay-only funding.
        executor.evm.trace = false;
        run(&mut executor);
        executor.bal_recorder.take().unwrap().finish(&mut executor.evm);
        drop(executor);
        db.take_built_alloy_bal().unwrap()
    }

    fn transaction(to: Address, value: u64, nonce: u64) -> Transaction {
        Transaction::Legacy(TxLegacy {
            chain_id: Some(714),
            to: TxKind::Call(to),
            value: U256::from(value),
            nonce,
            gas_limit: 100_000,
            ..Default::default()
        })
    }

    fn recovered(tx: Transaction, sender: Address) -> Recovered<TransactionSigned> {
        Recovered::new_unchecked(
            TransactionSigned::new_unhashed(tx, Signature::test_signature()),
            sender,
        )
    }

    #[test]
    fn deferred_system_execution_and_omitted_slash_preserve_indices_and_net_changes() {
        let run = |omit_slash| {
            execute_recorded(|executor| {
                executor.advance_bal_index().unwrap();
                executor
                    .execute_transaction(recovered(transaction(RECIPIENT, 1, 0), SENDER))
                    .unwrap();
                let system_tx = transaction(VALIDATOR_CONTRACT, 100, 0);
                executor.execute_transaction(recovered(system_tx.clone(), VALIDATOR)).unwrap();
                assert_eq!(executor.receipts().len(), 1, "system transaction is deferred");

                let before = executor.evm.db_mut().basic(VALIDATOR).unwrap().unwrap();
                let mut credit = Account::from(before);
                credit.info.balance += U256::from(100);
                credit.mark_touch();
                executor.commit_state([(VALIDATOR, credit)].into_iter().collect());
                if omit_slash {
                    executor
                        .transact_system_tx(executor.system_contracts.slash(RECIPIENT), SKIPPED)
                        .unwrap();
                }
                executor.transact_system_tx(system_tx, VALIDATOR).unwrap();
                assert_eq!(executor.receipts().len(), 2);
                assert_eq!(
                    executor.evm.db_mut().basic(VALIDATOR).unwrap().unwrap().balance,
                    U256::from(50)
                );
            })
        };
        let bal = run(false);
        assert_eq!(bal, run(true), "omitted slash must not add reads or advance the index");
        let validator = bal.iter().find(|account| account.address == VALIDATOR).unwrap();
        assert_eq!(validator.balance_changes, vec![], "combined credit and debit cancel");
        assert_eq!(validator.nonce_changes[0].block_access_index, BlockAccessIndex::new(2));
        let recipient = bal.iter().find(|account| account.address == RECIPIENT).unwrap();
        assert_eq!(recipient.balance_changes[0].block_access_index, BlockAccessIndex::new(1));
        let contract = bal.iter().find(|account| account.address == VALIDATOR_CONTRACT).unwrap();
        assert_eq!(contract.balance_changes[0].block_access_index, BlockAccessIndex::new(2));
        assert_eq!(contract.balance_changes[0].post_balance, U256::from(100));
        assert!(!bal.iter().any(|account| account.address == SKIPPED));
    }

    #[test]
    fn rejected_candidate_does_not_leak_reads_or_consume_an_index() {
        let bal = execute_recorded(|executor| {
            executor.advance_bal_index().unwrap();
            let skipped = executor
                .execute_transaction_with_commit_condition(
                    recovered(transaction(SKIPPED, 1, 0), SENDER),
                    |_| CommitChanges::No,
                )
                .unwrap();
            assert!(skipped.is_none());
            executor.execute_transaction(recovered(transaction(RECIPIENT, 2, 0), SENDER)).unwrap();
        });
        assert!(!bal.iter().any(|account| account.address == SKIPPED));
        let recipient = bal.iter().find(|account| account.address == RECIPIENT).unwrap();
        assert_eq!(recipient.balance_changes[0].block_access_index, BlockAccessIndex::new(1));
        assert_eq!(recipient.balance_changes[0].post_balance, U256::from(2));
    }

    #[test]
    fn reverted_transaction_keeps_storage_reads_but_not_writes() {
        let bal = execute_recorded(|executor| {
            // SSTORE(0, 9), then REVERT(0, 0).
            let code = Bytecode::new_raw(Bytes::from_static(&[
                0x60, 0x09, 0x60, 0x00, 0x55, 0x60, 0x00, 0x60, 0x00, 0xfd,
            ]));
            executor
                .evm
                .db_mut()
                .database
                .insert_account_info(RECIPIENT, AccountInfo::default().with_code(code));
            executor.advance_bal_index().unwrap();
            executor.execute_transaction(recovered(transaction(RECIPIENT, 0, 0), SENDER)).unwrap();
            assert!(!executor.receipts()[0].success);
        });
        let recipient = bal.iter().find(|account| account.address == RECIPIENT).unwrap();
        assert!(recipient.storage_changes.is_empty());
        assert_eq!(recipient.storage_reads, vec![U256::ZERO]);
    }
}
