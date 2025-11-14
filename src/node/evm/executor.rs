use super::patch::HertzPatchManager;
use crate::{
    consensus::{SYSTEM_ADDRESS, parlia::{VoteAddress, Snapshot, Parlia}},
    evm::transaction::BscTxEnv,
    hardforks::BscHardforks,
    system_contracts::{
        get_upgrade_system_contracts, is_system_transaction, SystemContract,
        feynman_fork::ValidatorElectionInfo,
    },
};
use alloy_consensus::{Header, Transaction, TxReceipt};
use alloy_eips::{eip7685::Requests, Encodable2718};
use alloy_evm::{block::{ExecutableTx, StateChangeSource}, eth::receipt_builder::ReceiptBuilderCtx};
use alloy_primitives::{uint, Address, U256, BlockNumber, Bytes};
use reth_chainspec::{EthChainSpec, EthereumHardforks, Hardforks};
use super::config::BscBlockExecutionCtx;
use reth_evm::{
    block::{BlockValidationError, CommitChanges},
    eth::receipt_builder::ReceiptBuilder,
    execute::{BlockExecutionError, BlockExecutor},
    system_calls::SystemCaller,
    Database, Evm, FromRecoveredTx, FromTxWithEncoded, IntoTxEnv, OnStateHook, RecoveredTx,
};
use reth_primitives::TransactionSigned;
use reth_provider::BlockExecutionResult;
use reth_revm::State;
use revm::{
    context::{
        result::{ExecutionResult, ResultAndState},

    },
    state::Bytecode,
    DatabaseCommit,
};
use tracing::{error, warn, info, debug, trace};
use alloy_eips::eip2935::{HISTORY_STORAGE_ADDRESS, HISTORY_STORAGE_CODE};
use alloy_primitives::keccak256;
use std::{collections::HashMap, sync::Arc, cell::RefCell};
use crate::consensus::parlia::SnapshotProvider;

/// Type alias for system transactions to reduce complexity
type SystemTxs = Vec<reth_primitives_traits::Recovered<reth_primitives_traits::TxTy<crate::BscPrimitives>>>;

thread_local! {
    pub static ASSEMBLED_SYSTEM_TXS: RefCell<SystemTxs> = const { RefCell::new(Vec::new()) };
}

/// Helper type for the input of post execution.
#[allow(clippy::type_complexity)]
#[derive(Debug, Clone)]
pub(crate) struct InnerExecutionContext {
    pub(crate) current_validators: Option<(Vec<Address>, HashMap<Address, VoteAddress>)>,
    pub(crate) max_elected_validators: Option<U256>,
    pub(crate) validators_election_info: Option<Vec<ValidatorElectionInfo>>,
    pub(crate) snap: Option<Snapshot>,
    pub(crate) header: Option<Header>,
    pub(crate) parent_header: Option<Header>,
}

pub struct BscBlockExecutor<'a, EVM, Spec, R: ReceiptBuilder>
where
    Spec: EthChainSpec,
{
    /// Reference to the specification object.
    pub(super) spec: Spec,
    /// Inner EVM.
    pub(super) evm: EVM,
    /// Gas used in the block.
    pub(super) gas_used: u64,
    /// Receipts of executed transactions.
    pub(super) receipts: Vec<R::Receipt>,
    /// System txs
    pub(super) system_txs: Vec<R::Transaction>,
    /// Receipt builder.
    pub(super) receipt_builder: R,
    /// System contracts used to trigger fork specific logic.
    pub(super) system_contracts: SystemContract<Spec>,
    /// Hertz patch manager for compatibility.
    hertz_patch_manager: HertzPatchManager,
    /// Context for block execution.
    pub(super) ctx: BscBlockExecutionCtx<'a>,
    /// Utility to call system caller.
    pub(super) system_caller: SystemCaller<Spec>,
    /// Snapshot provider for accessing Parlia validator snapshots.
    pub(super) snapshot_provider: Option<Arc<dyn SnapshotProvider + Send + Sync>>,
    /// Parlia consensus instance.
    pub(crate) parlia: Arc<Parlia<Spec>>,
    /// Inner execution context.
    pub(super) inner_ctx: InnerExecutionContext,
    /// assembled system txs.
    pub(crate) assembled_system_txs: SystemTxs,
}

impl<'a, DB, EVM, Spec, R: ReceiptBuilder> BscBlockExecutor<'a, EVM, Spec, R>
where
    DB: Database + 'a,
    EVM: Evm<
        DB = &'a mut State<DB>,
        Tx: FromRecoveredTx<R::Transaction>
                + FromRecoveredTx<TransactionSigned>
                + FromTxWithEncoded<TransactionSigned>,
    >,
    Spec: EthereumHardforks + BscHardforks + EthChainSpec + Hardforks + Clone + 'static,
    R: ReceiptBuilder<Transaction = TransactionSigned, Receipt: TxReceipt>,
    <R as ReceiptBuilder>::Transaction: Unpin + From<TransactionSigned>,
    <EVM as alloy_evm::Evm>::Tx: FromTxWithEncoded<<R as ReceiptBuilder>::Transaction>,
    BscTxEnv: IntoTxEnv<<EVM as alloy_evm::Evm>::Tx>,
    R::Transaction: Into<TransactionSigned>,
{
    /// Creates a new BscBlockExecutor.
    pub(crate) fn new(
        evm: EVM,
        ctx: BscBlockExecutionCtx<'a>,
        spec: Spec,
        receipt_builder: R,
        system_contracts: SystemContract<Spec>,
    ) -> Self {
        let is_mainnet = spec.chain().id() == 56; // BSC mainnet chain ID
        let hertz_patch_manager = HertzPatchManager::new(is_mainnet);
        
        trace!("Succeed to new block executor, header: {:?}", ctx.header);
        if let Some(ref header) = ctx.header {
            crate::node::evm::util::HEADER_CACHE_READER.lock().unwrap().insert_header_to_cache(header.clone());
        } else if !ctx.is_miner { // miner has no current header.
            warn!("No header found in the context, block_number: {:?}", evm.block().number.to::<u64>());
        }

        let parlia = Arc::new(Parlia::new(Arc::new(spec.clone()), 200));
        let spec_clone = spec.clone();
        Self {
            spec,
            evm,
            gas_used: 0,
            receipts: vec![],
            system_txs: vec![],
            receipt_builder,
            system_contracts,
            hertz_patch_manager,
            ctx,
            system_caller: SystemCaller::new(spec_clone),
            snapshot_provider: crate::shared::get_snapshot_provider().cloned(),
            parlia,
            inner_ctx: InnerExecutionContext {
                current_validators: None,
                max_elected_validators: None,
                validators_election_info: None,
                snap: None,
                header: None,
                parent_header: None,
            },
            assembled_system_txs: vec![],
        }
    }

    /// Applies system contract upgrades if the Feynman fork is not yet active.
    fn upgrade_contracts(&mut self) -> Result<(), BlockExecutionError> {
        let contracts = get_upgrade_system_contracts(
            &self.spec,
            self.evm.block().number.to(),
            self.evm.block().timestamp.to(),
            self.inner_ctx.parent_header.as_ref().unwrap().timestamp,
        )
        .map_err(|_| BlockExecutionError::msg("Failed to get upgrade system contracts"))?;

        for (address, maybe_code) in contracts {
            if let Some(code) = maybe_code {
                self.upgrade_system_contract(address, code)?;
            }
        }

        Ok(())
    }

    /// Initializes the feynman contracts
    fn initialize_feynman_contracts(
        &mut self,
        beneficiary: Address,
    ) -> Result<(), BlockExecutionError> {
        let txs = self.system_contracts.feynman_contracts_txs();
        for tx in txs {
            self.transact_system_tx(tx.into(), beneficiary)?;
        }
        Ok(())
    }

    /// Initializes the genesis contracts
    fn deploy_genesis_contracts(
        &mut self,
        beneficiary: Address,
    ) -> Result<(), BlockExecutionError> {
        let txs = self.system_contracts.genesis_contracts_txs();
        for  tx in txs {
            self.transact_system_tx(tx.into(), beneficiary)?;
        }
        Ok(())
    }

    /// Replaces the code of a system contract in state.
    fn upgrade_system_contract(
        &mut self,
        address: Address,
        code: Bytecode,
    ) -> Result<(), BlockExecutionError> {
        let account =
            self.evm.db_mut().load_cache_account(address).map_err(BlockExecutionError::other)?;

        let mut info = account.account_info().unwrap_or_default();
        info.code_hash = code.hash_slow();
        info.code = Some(code);

        let transition = account.change(info, Default::default());
        self.evm.db_mut().apply_transition(vec![(address, transition)]);
        Ok(())
    }

    pub(crate) fn apply_history_storage_account(
        &mut self,
        block_number: BlockNumber,
    ) -> Result<bool, BlockExecutionError> {
        info!(
            target: "bsc::executor::prague",
            block_number,
            address = ?HISTORY_STORAGE_ADDRESS,
            "Deploying HistoryStorageAddress contract (Prague transition)"
        );

        let account = self.evm.db_mut().load_cache_account(HISTORY_STORAGE_ADDRESS).map_err(|err| {
            error!(
                target: "bsc::executor::prague",
                block_number,
                error = ?err,
                "Failed to load HistoryStorageAddress account",
            );
            BlockExecutionError::other(err)
        })?;

        let old_info = account.account_info();
        debug!(
            target: "bsc::executor::prague",
            block_number,
            old_nonce = ?old_info.as_ref().map(|i| i.nonce),
            old_code_hash = ?old_info.as_ref().map(|i| i.code_hash),
            "HistoryStorageAddress account before deployment"
        );

        let mut new_info = account.account_info().unwrap_or_default();
        new_info.code_hash = keccak256(HISTORY_STORAGE_CODE.clone());
        new_info.code = Some(Bytecode::new_raw(Bytes::from_static(&HISTORY_STORAGE_CODE)));
        new_info.nonce = 1_u64;
        new_info.balance = U256::ZERO;

        let transition = account.change(new_info, Default::default());
        self.evm.db_mut().apply_transition(vec![(HISTORY_STORAGE_ADDRESS, transition)]);
        
        info!(
            target: "bsc::executor::prague",
            block_number,
            "Successfully deployed HistoryStorageAddress contract"
        );
        Ok(true)
    }
}

impl<'a, DB, E, Spec, R> BlockExecutor for BscBlockExecutor<'a, E, Spec, R>
where
    DB: Database + 'a,
    E: Evm<
        DB = &'a mut State<DB>,
        Tx: FromRecoveredTx<R::Transaction>
                + FromRecoveredTx<TransactionSigned>
                + FromTxWithEncoded<TransactionSigned>,
    >,
    Spec: EthereumHardforks + BscHardforks + EthChainSpec + Hardforks + 'static,
    R: ReceiptBuilder<Transaction = TransactionSigned, Receipt: TxReceipt>,
    <R as ReceiptBuilder>::Transaction: Unpin + From<TransactionSigned>,
    <E as alloy_evm::Evm>::Tx: FromTxWithEncoded<<R as ReceiptBuilder>::Transaction>,
    BscTxEnv: IntoTxEnv<<E as alloy_evm::Evm>::Tx>,
    R::Transaction: Into<TransactionSigned>,
{
    type Transaction = TransactionSigned;
    type Receipt = R::Receipt;
    type Evm = E;

    fn apply_pre_execution_changes(&mut self) -> Result<(), BlockExecutionError> {
        let block_env = self.evm.block().clone();
        debug!(
            target: "bsc::executor", 
            block_id = %block_env.number,
            is_miner = self.ctx.is_miner,
            "Start to apply_pre_execution_changes"
        );
        
        // pre check and prepare some intermediate data for commit parlia snapshot in finish function.
        if self.ctx.is_miner {
            self.prepare_new_block(&block_env)?;
        } else {
            self.check_new_block(&block_env)?;
        }
        
        // set state clear flag if the block is after the Spurious Dragon hardfork.
        let state_clear_flag = self.spec.is_spurious_dragon_active_at_block(self.evm.block().number.to());
        self.evm.db_mut().set_state_clear_flag(state_clear_flag);
        let parent_timestamp = self.inner_ctx.parent_header.as_ref().unwrap().timestamp;
        debug!(
            target: "bsc::executor::prague",
            block_number = self.evm.block().number.to::<u64>(),
            block_timestamp = self.evm.block().timestamp.to::<u64>(),
            parent_timestamp,
            "Prague hardfork check parameters"
        );
        // Note: here use a parent timestamp to check if the feynman fork is active. 
        if !self.spec.is_feynman_active_at_timestamp(self.evm.block().number.to::<u64>(), parent_timestamp) {
            self.upgrade_contracts()?;
        }
     
        // enable BEP-440/EIP-2935 for historical block hashes from state
        let is_prague_transition = self.spec.is_prague_transition_at_block_and_timestamp(self.evm.block().number.to::<u64>(), self.evm.block().timestamp.to::<u64>(), parent_timestamp);
        let is_prague_active = self.spec.is_prague_active_at_block_and_timestamp(self.evm.block().number.to::<u64>(), self.evm.block().timestamp.to::<u64>());
        
        // Check if HistoryStorageAddress contract exists in state
        let history_contract_exists = self.evm.db_mut().load_cache_account(HISTORY_STORAGE_ADDRESS)
            .ok()
            .and_then(|acc| acc.account_info())
            .map(|info| info.nonce > 0 || !info.code_hash.is_zero())
            .unwrap_or(false);
        
        debug!(
            target: "bsc::executor::prague",
            block_number = self.evm.block().number.to::<u64>(),
            block_hash = ?self.evm.block().blob_excess_gas_and_price,
            is_prague_transition,
            is_prague_active,
            is_miner = self.ctx.is_miner,
            history_contract_exists,
            "Prague hardfork check result and contract state"
        );
        if is_prague_transition {
                debug!(
                    target: "bsc::executor::prague",
                    block_number = self.evm.block().number.to::<u64>(),
                    "Calling apply_history_storage_account (Prague transition)"
                );
                self.apply_history_storage_account(self.evm.block().number.to::<u64>())?;
        }
        if is_prague_active {
            debug!(
                target: "bsc::executor::prague",
                block_number = self.evm.block().number.to::<u64>(),
                parent_hash = ?self.ctx.base.parent_hash,
                "Calling apply_blockhashes_contract_call (Prague active)"
            );
            self.system_caller
                .apply_blockhashes_contract_call(self.ctx.base.parent_hash, &mut self.evm)?;
        } else {
            trace!(
                target: "bsc::executor::prague",
                block_number = self.evm.block().number.to::<u64>(),
                "Prague not active, skipping apply_blockhashes_contract_call"
            );
        }

        Ok(())
    }

    fn execute_transaction_with_commit_condition(
        &mut self,
        tx: impl ExecutableTx<Self>,
        f: impl FnOnce(&ExecutionResult<<Self::Evm as Evm>::HaltReason>) -> CommitChanges,
    ) -> Result<Option<u64>, BlockExecutionError> {
        // The sum of the transaction's gas limit, Tg, and the gas utilized in this block prior,
        // must be no greater than the block's gasLimit.
        let block_available_gas = self.evm.block().gas_limit - self.gas_used;

        if tx.tx().gas_limit() > block_available_gas {
            return Err(BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas {
                transaction_gas_limit: tx.tx().gas_limit(),
                block_available_gas,
            }
            .into());
        }

        // Execute transaction.
        let ResultAndState { result, state } = self
            .evm
            .transact(&tx)
            .map_err(|err| BlockExecutionError::evm(err, tx.tx().trie_hash()))?;

        if !f(&result).should_commit() {
            return Ok(None);
        }

        let mut temp_state = state.clone();
        temp_state.remove(&SYSTEM_ADDRESS);
        self.system_caller
            .on_state(StateChangeSource::Transaction(self.receipts.len()), &temp_state);

        let gas_used = result.gas_used();

        // append gas used
        self.gas_used += gas_used;

        // Push transaction changeset and calculate header bloom filter for receipt.
        self.receipts.push(self.receipt_builder.build_receipt(ReceiptBuilderCtx {
            tx: tx.tx(),
            evm: &self.evm,
            result,
            state: &state,
            cumulative_gas_used: self.gas_used,
        }));

        // Commit the state changes.
        self.evm.db_mut().commit(state);

        Ok(Some(gas_used))
    }

    fn execute_transaction_with_result_closure(
        &mut self,
        tx: impl ExecutableTx<Self>
            + IntoTxEnv<<E as alloy_evm::Evm>::Tx>
            + RecoveredTx<TransactionSigned>,
        f: impl for<'b> FnOnce(&'b ExecutionResult<<E as alloy_evm::Evm>::HaltReason>),
    ) -> Result<u64, BlockExecutionError> {
        let signer = tx.signer();
        let is_system = is_system_transaction(tx.tx(), *signer, self.evm.block().beneficiary);
        if is_system {
            self.system_txs.push(tx.tx().clone());
            return Ok(0);
        }

        self.hertz_patch_manager.patch_before_tx(tx.tx(), self.evm.db_mut())?;

        let block_available_gas = self.evm.block().gas_limit - self.gas_used;
        if tx.tx().gas_limit() > block_available_gas {
            return Err(BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas {
                transaction_gas_limit: tx.tx().gas_limit(),
                block_available_gas,
            }
            .into());
        }
        let tx_hash = tx.tx().trie_hash();
        let tx_ref = tx.tx().clone();
        let result_and_state =
            self.evm.transact(tx).map_err(|err| BlockExecutionError::evm(err, tx_hash))?;
        let ResultAndState { result, state } = result_and_state;

        f(&result);

        let mut temp_state = state.clone();
        temp_state.remove(&SYSTEM_ADDRESS);
        self.system_caller.on_state(StateChangeSource::Transaction(self.receipts.len()), &temp_state);

        let gas_used = result.gas_used();
        self.gas_used += gas_used;
        self.receipts.push(self.receipt_builder.build_receipt(ReceiptBuilderCtx {
            tx: &tx_ref,
            evm: &self.evm,
            result,
            state: &state,
            cumulative_gas_used: self.gas_used,
        }));
        self.evm.db_mut().commit(state);

        self.hertz_patch_manager.patch_after_tx(&tx_ref, self.evm.db_mut())?;

        Ok(gas_used)
    }


    fn finish(
        mut self,
    ) -> Result<(Self::Evm, BlockExecutionResult<R::Receipt>), BlockExecutionError> {
        let block_env = self.evm.block().clone();
        debug!(
            target: "bsc::executor", 
            block_id = %block_env.number,
            is_miner = self.ctx.is_miner,
            "Start to finish"
        );

        // If first block deploy genesis contracts
        if self.evm.block().number == uint!(1U256) {
            self.deploy_genesis_contracts(self.evm.block().beneficiary)?;
        }
        let parent_timestamp = self.inner_ctx.parent_header.as_ref().unwrap().timestamp;
        // Note: here use a parent timestamp to check if the feynman fork is active. 
        if self.spec.is_feynman_active_at_timestamp(self.evm.block().number.to::<u64>(), parent_timestamp) {
            self.upgrade_contracts()?;
        }

        if self.spec.is_feynman_transition_at_timestamp(self.evm.block().number.to::<u64>(), self.evm.block().timestamp.to::<u64>(), parent_timestamp) {
            self.initialize_feynman_contracts(self.evm.block().beneficiary)?;
        }

        if self.ctx.is_miner {
            self.finalize_new_block(&self.evm.block().clone())?;
        } else {
            self.post_check_new_block(&self.evm.block().clone())?;
        }

        ASSEMBLED_SYSTEM_TXS.with(|txs| {
            *txs.borrow_mut() = self.assembled_system_txs.clone();
        });

        Ok((
            self.evm,
            BlockExecutionResult {
                receipts: self.receipts,
                requests: Requests::default(),
                gas_used: self.gas_used,
            },
        ))
    }

    fn set_state_hook(&mut self, hook: Option<Box<dyn OnStateHook>>) {
        self.system_caller.with_state_hook(hook);
    }

    fn evm_mut(&mut self) -> &mut Self::Evm {
        &mut self.evm
    }

    fn evm(&self) -> &Self::Evm {
        &self.evm
    }

}

impl<'a, EVM, Spec, R: ReceiptBuilder> BscBlockExecutor<'a, EVM, Spec, R>
where
    Spec: EthChainSpec + EthereumHardforks + BscHardforks + Hardforks + Clone,
    EVM: alloy_evm::Evm,
{
    // miner BscBlockBuilder use this method to fetch system txs.
    pub(crate) fn finish_with_system_txs<F, T>(self, finish_fn: F) -> Result<(T, SystemTxs), BlockExecutionError>
    where
        F: FnOnce(Self) -> Result<T, BlockExecutionError>,
    {
        let result = finish_fn(self)?;
        let system_txs = ASSEMBLED_SYSTEM_TXS.with(|txs| txs.borrow().clone());
        Ok((result, system_txs))
    }
}
