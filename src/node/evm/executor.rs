use super::patch::{
    patch_chapel_after_tx, patch_chapel_before_tx, patch_mainnet_after_tx, patch_mainnet_before_tx,
};
use crate::{
    consensus::{SYSTEM_ADDRESS, parlia::{HertzPatchManager, VoteAddress, Snapshot}},
    evm::transaction::BscTxEnv,
    hardforks::BscHardforks,
    system_contracts::{
        feynman_fork::ValidatorElectionInfo,
        get_upgrade_system_contracts, is_system_transaction, SystemContract, STAKE_HUB_CONTRACT,
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
    Database as _, DatabaseCommit,
};
use tracing::debug;
use alloy_eips::eip2935::{HISTORY_STORAGE_ADDRESS, HISTORY_STORAGE_CODE};
use alloy_primitives::keccak256;
use std::{collections::HashMap, sync::Arc};
use crate::consensus::parlia::SnapshotProvider;

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
    /// Hertz patch manager for mainnet compatibility
    /// TODO: refine later.
    #[allow(dead_code)]
    hertz_patch_manager: HertzPatchManager,
    /// Context for block execution.
    pub(super) ctx: BscBlockExecutionCtx<'a>,
    /// Utility to call system caller.
    pub(super) system_caller: SystemCaller<Spec>,
    /// State hook.
    pub(super) hook: Option<Box<dyn OnStateHook>>,
    /// Snapshot provider for accessing Parlia validator snapshots.
    pub(super) snapshot_provider: Option<Arc<dyn SnapshotProvider + Send + Sync>>,
    /// Parlia consensus instance used (optional during execution).
    pub(super) parlia_consensus: Option<Arc<dyn crate::consensus::parlia::ParliaConsensusObject + Send + Sync>>,
    /// Inner execution context.
    pub(super) inner_ctx: InnerExecutionContext,
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
    Spec: EthereumHardforks + BscHardforks + EthChainSpec + Hardforks + Clone,
    R: ReceiptBuilder<Transaction = TransactionSigned, Receipt: TxReceipt>,
    <R as ReceiptBuilder>::Transaction: Unpin + From<TransactionSigned>,
    <EVM as alloy_evm::Evm>::Tx: FromTxWithEncoded<<R as ReceiptBuilder>::Transaction>,
    BscTxEnv: IntoTxEnv<<EVM as alloy_evm::Evm>::Tx>,
    R::Transaction: Into<TransactionSigned>,
{
    /// Creates a new BscBlockExecutor.
    pub fn new(
        evm: EVM,
        ctx: BscBlockExecutionCtx<'a>,
        spec: Spec,
        receipt_builder: R,
        system_contracts: SystemContract<Spec>,
    ) -> Self {
        // Determine if this is mainnet for Hertz patches
        let is_mainnet = spec.chain().id() == 56; // BSC mainnet chain ID
        let hertz_patch_manager = HertzPatchManager::new(is_mainnet);
        tracing::trace!("Succeed to new block executor, header: {:?}", ctx.header);

        if let Some(ref header) = ctx.header {
            crate::node::evm::util::HEADER_CACHE_READER.lock().unwrap().insert_header_to_cache(header.clone());
        } else {
            tracing::warn!("No header found in the context, block_number: {:?}", evm.block().number.to::<u64>());
        }

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
            hook: None,
            snapshot_provider: crate::shared::get_snapshot_provider().cloned(),
            parlia_consensus: crate::shared::get_parlia_consensus().cloned(),
            inner_ctx: InnerExecutionContext {
                current_validators: None,
                max_elected_validators: None,
                validators_election_info: None,
                snap: None,
                header: None,
                parent_header: None,
            },
        }
    }

    /// Applies system contract upgrades if the Feynman fork is not yet active.
    fn upgrade_contracts(&mut self) -> Result<(), BlockExecutionError> {
        let contracts = get_upgrade_system_contracts(
            &self.spec,
            self.evm.block().number.to(),
            self.evm.block().timestamp.to(),
            self.evm.block().timestamp.to::<u64>() - 3_000, /* TODO: how to get parent block
                                                             * timestamp? */
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
        if !self
            .evm
            .db_mut()
            .storage(STAKE_HUB_CONTRACT, U256::ZERO)
            .map_err(BlockExecutionError::other)?
            .is_zero()
        {
            return Ok(());
        }

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
        debug!(
            "Apply history storage account {:?} at height {:?}",
            HISTORY_STORAGE_ADDRESS, block_number
        );

        let account = self.evm.db_mut().load_cache_account(HISTORY_STORAGE_ADDRESS).map_err(|err| {
            BlockExecutionError::other(err)
        })?;

        let mut new_info = account.account_info().unwrap_or_default();
        new_info.code_hash = keccak256(HISTORY_STORAGE_CODE.clone());
        new_info.code = Some(Bytecode::new_raw(Bytes::from_static(&HISTORY_STORAGE_CODE)));
        new_info.nonce = 1_u64;
        new_info.balance = U256::ZERO;

        let transition = account.change(new_info, Default::default());
        self.evm.db_mut().apply_transition(vec![(HISTORY_STORAGE_ADDRESS, transition)]);
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
    Spec: EthereumHardforks + BscHardforks + EthChainSpec + Hardforks,
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
        // pre check and prepare some intermediate data for commit parlia snapshot in finish function.
        let block_env = self.evm.block().clone();
        self.check_new_block(&block_env)?;

        // set state clear flag if the block is after the Spurious Dragon hardfork.
        let state_clear_flag = self.spec.is_spurious_dragon_active_at_block(self.evm.block().number.to());
        self.evm.db_mut().set_state_clear_flag(state_clear_flag);

        if !self.spec.is_feynman_active_at_timestamp(self.evm.block().timestamp.to()) {
            self.upgrade_contracts()?;
        }

        // enable BEP-440/EIP-2935 for historical block hashes from state.
        if self.spec.is_prague_transition_at_timestamp(self.evm.block().timestamp.to(), self.evm.block().timestamp.to::<u64>() - 3) {
            self.apply_history_storage_account(self.evm.block().number.to::<u64>())?;
        }
        if self.spec.is_prague_active_at_timestamp(self.evm.block().timestamp.to()) {
            self.system_caller.apply_blockhashes_contract_call(self.ctx.base.parent_hash, &mut self.evm)?;
        }

        Ok(())
    }

    fn execute_transaction_with_commit_condition(
        &mut self,
        _tx: impl ExecutableTx<Self>,
        _f: impl FnOnce(&ExecutionResult<<Self::Evm as Evm>::HaltReason>) -> CommitChanges,
    ) -> Result<Option<u64>, BlockExecutionError> {
        unimplemented!();
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

        // TODO: refine it.
        // apply patches before (legacy - keeping for compatibility)
        patch_mainnet_before_tx(tx.tx(), self.evm.db_mut())?;
        patch_chapel_before_tx(tx.tx(), self.evm.db_mut())?;

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

        // Call state hook if it exists, passing the evmstate
        if let Some(hook) = &mut self.hook {
            let mut temp_state = state.clone();
            temp_state.remove(&SYSTEM_ADDRESS);
            hook.on_state(StateChangeSource::Transaction(self.receipts.len()), &temp_state);
        }

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

        // apply patches after
        patch_mainnet_after_tx(&tx_ref, self.evm.db_mut())?;
        patch_chapel_after_tx(&tx_ref, self.evm.db_mut())?;

        Ok(gas_used)
    }



    fn finish(
        mut self,
    ) -> Result<(Self::Evm, BlockExecutionResult<R::Receipt>), BlockExecutionError> {
        // If first block deploy genesis contracts
        if self.evm.block().number == uint!(1U256) {
            self.deploy_genesis_contracts(self.evm.block().beneficiary)?;
        }

        if self.spec.is_feynman_active_at_timestamp(self.evm.block().timestamp.to()) {
            self.upgrade_contracts()?;
        }
        if self.spec.is_feynman_active_at_timestamp(self.evm.block().timestamp.to()) &&
            !self.spec.is_feynman_active_at_timestamp(self.evm.block().timestamp.to::<u64>() - 100)
        {
            self.initialize_feynman_contracts(self.evm.block().beneficiary)?;
        }

        self.finalize_new_block(&self.evm.block().clone())?;

        Ok((
            self.evm,
            BlockExecutionResult {
                receipts: self.receipts,
                requests: Requests::default(),
                gas_used: self.gas_used,
            },
        ))
    }

    fn set_state_hook(&mut self, _hook: Option<Box<dyn OnStateHook>>) {
        self.hook = _hook;
    }

    fn evm_mut(&mut self) -> &mut Self::Evm {
        &mut self.evm
    }

    fn evm(&self) -> &Self::Evm {
        &self.evm
    }

}