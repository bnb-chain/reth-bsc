use super::config::{revm_spec_by_timestamp_and_block_number, BscBlockExecutionCtx};
use super::patch::HertzPatchManager;
use crate::consensus::parlia::SnapshotProvider;
use crate::{
    consensus::{
        parlia::{Parlia, Snapshot, VoteAddress},
        SYSTEM_ADDRESS,
    },
    evm::{precompiles, transaction::BscTxEnv},
    hardforks::BscHardforks,
    metrics::{
        BscBlockchainMetrics, BscConsensusMetrics, BscExecutorMetrics, BscRewardsMetrics,
        BscVoteMetrics,
    },
    node::evm::config::BscExecutionSharedCtx,
    system_contracts::{
        feynman_fork::ValidatorElectionInfo, get_upgrade_system_contracts, is_system_transaction,
        SystemContract,
    },
};
use alloy_consensus::{Header, TxReceipt, TxType};
use alloy_eips::eip2935::{HISTORY_STORAGE_ADDRESS, HISTORY_STORAGE_CODE};
use alloy_eips::{eip7685::Requests, Encodable2718};
use alloy_evm::{
    block::{ExecutableTx, GasOutput, StateChangeSource, TxResult},
    eth::receipt_builder::ReceiptBuilderCtx,
};
use alloy_primitives::keccak256;
use alloy_primitives::{hex, uint, Address, BlockNumber, Bytes, U256};
use reth_chainspec::{EthChainSpec, EthereumHardforks, Hardforks};
use reth_ethereum_primitives::TransactionSigned;
use reth_evm::{
    block::BlockValidationError,
    eth::receipt_builder::ReceiptBuilder,
    execute::{BlockExecutionError, BlockExecutor},
    system_calls::SystemCaller,
    Evm, FromRecoveredTx, FromTxWithEncoded, IntoTxEnv, OnStateHook,
};
use reth_provider::BlockExecutionResult;
use revm::Database as _;
use revm::{
    context::result::{ExecutionResult, Output, ResultAndState, ResultGas, SuccessReason},
    context_interface::block::Block,
    state::{Account as RevmAccount, Bytecode, EvmState},
    DatabaseCommit,
};
use std::{collections::HashMap, sync::Arc};
use tracing::{debug, error, info, trace, warn};

/// Result of executing a single BSC transaction.
pub struct BscTxResult<H> {
    pub inner: ResultAndState<H>,
    pub blob_gas_used: u64,
    pub tx_type: TxType,
    pub tx: TransactionSigned,
    pub is_system: bool,
}

impl<H: Send + 'static> TxResult for BscTxResult<H> {
    type HaltReason = H;

    fn result(&self) -> &ResultAndState<H> {
        &self.inner
    }

    fn into_result(self) -> ResultAndState<H> {
        self.inner
    }
}
/// Helper type for the input of post execution.
#[allow(clippy::type_complexity)]
#[derive(Debug, Clone)]
pub(crate) struct InnerExecutionContext {
    pub(crate) current_validators: Option<(Vec<Address>, HashMap<Address, VoteAddress>)>,
    pub(crate) expected_turn_length: Option<u8>,
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
    /// Total blob gas used in the block.
    pub(super) blob_gas_used: u64,
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
    /// Shared context for block execution.
    pub(super) shared_ctx: BscExecutionSharedCtx,
    /// Consensus metrics for tracking block height and other consensus stats.
    pub(super) consensus_metrics: BscConsensusMetrics,
    /// Blockchain metrics for tracking receipts and block processing.
    pub(super) blockchain_metrics: BscBlockchainMetrics,
    /// Vote metrics for tracking attestation errors.
    pub(super) vote_metrics: BscVoteMetrics,
    /// Executor metrics for tracking block execution.
    pub(super) executor_metrics: BscExecutorMetrics,
    /// Rewards metrics for tracking reward distributions.
    pub(super) rewards_metrics: BscRewardsMetrics,
    /// Deferred error from commit_transaction (e.g. hertz patch), returned from finish().
    pub(super) deferred_error: Option<BlockExecutionError>,
}

impl<'a, EVM, Spec, R: ReceiptBuilder> BscBlockExecutor<'a, EVM, Spec, R>
where
    EVM: Evm<
        DB: alloy_evm::block::StateDB,
        Tx: FromRecoveredTx<R::Transaction>
                + FromRecoveredTx<TransactionSigned>
                + FromTxWithEncoded<TransactionSigned>,
        BlockEnv = revm::context::BlockEnv,
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
        shared_ctx: BscExecutionSharedCtx,
        spec: Spec,
        receipt_builder: R,
        system_contracts: SystemContract<Spec>,
    ) -> Self {
        let is_mainnet = spec.chain().id() == 56; // BSC mainnet chain ID
        let hertz_patch_manager = HertzPatchManager::new(is_mainnet);

        trace!("Succeed to new block executor, header: {:?}", ctx.header);
        if let Some(ref header) = ctx.header {
            crate::node::evm::util::HEADER_CACHE_READER
                .lock()
                .unwrap()
                .insert_header_to_cache_with_hash(header.clone(), ctx.header_hash);
        } else if !ctx.is_miner {
            // miner has no current header.
            warn!(
                "No header found in the context, block_number: {:?}",
                evm.block().number().to::<u64>()
            );
        }

        let parlia = Arc::new(Parlia::new(Arc::new(spec.clone()), 200));
        let spec_clone = spec.clone();
        Self {
            spec,
            evm,
            gas_used: 0,
            blob_gas_used: 0,
            receipts: vec![],
            system_txs: vec![],
            receipt_builder,
            system_contracts,
            hertz_patch_manager,
            ctx,
            shared_ctx,
            system_caller: SystemCaller::new(spec_clone),
            snapshot_provider: crate::shared::get_snapshot_provider().cloned(),
            parlia,
            inner_ctx: InnerExecutionContext {
                current_validators: None,
                expected_turn_length: None,
                max_elected_validators: None,
                validators_election_info: None,
                snap: None,
                header: None,
                parent_header: None,
            },
            consensus_metrics: BscConsensusMetrics::default(),
            blockchain_metrics: BscBlockchainMetrics::default(),
            vote_metrics: BscVoteMetrics::default(),
            executor_metrics: BscExecutorMetrics::default(),
            rewards_metrics: BscRewardsMetrics::default(),
            deferred_error: None,
        }
    }

    /// Applies system contract upgrades if the Feynman fork is not yet active.
    fn upgrade_contracts(
        &mut self,
        block_number: BlockNumber,
        block_timestamp: u64,
        parent_timestamp: u64,
    ) -> Result<(), BlockExecutionError> {
        trace!(
            target: "bsc::executor::upgrade",
            block_number,
            block_timestamp,
            parent_timestamp,
            "Calling get_upgrade_system_contracts"
        );

        let contracts = get_upgrade_system_contracts(
            &self.spec,
            block_number,
            block_timestamp,
            parent_timestamp,
        )
        .map_err(|_| BlockExecutionError::msg("Failed to get upgrade system contracts"))?;

        for (address, maybe_code) in contracts {
            if let Some(code) = maybe_code {
                debug!(
                    target: "bsc::executor::upgrade",
                    block_number,
                    address = ?address,
                    code_len = code.len(),
                    "Upgrading system contract"
                );
                self.upgrade_system_contract(address, code)?;
            }
        }

        Ok(())
    }

    /// Mimics Geth-BSC's TryUpdateBuildInSystemContract function
    fn try_update_build_in_system_contract(
        &mut self,
        block_number: BlockNumber,
        block_timestamp: u64,
        parent_timestamp: u64,
        at_block_begin: bool,
    ) -> Result<(), BlockExecutionError> {
        if at_block_begin {
            // Upgrade system contracts before Feynman at block begin
            if !self.spec.is_feynman_active_at_timestamp(block_number, parent_timestamp) {
                trace!(
                    target: "bsc::executor::upgrade",
                    block_number,
                    parent_timestamp,
                    "Upgrading system contracts at block begin (before Feynman)"
                );
                self.upgrade_contracts(block_number, block_timestamp, parent_timestamp)?;
            }

            // HistoryStorageAddress is a special system contract in BSC, which can't be upgraded
            // This must be done at block begin when Prague activates
            if self.spec.is_prague_transition_at_block_and_timestamp(
                block_number,
                block_timestamp,
                parent_timestamp,
            ) {
                info!(
                    target: "bsc::executor::prague",
                    block_number,
                    block_timestamp,
                    "Deploying HistoryStorageAddress contract (Prague transition at block begin)"
                );
                self.apply_history_storage_account(block_number)?;
            }
        } else {
            // Upgrade system contracts after Feynman at block end
            if self.spec.is_feynman_active_at_timestamp(block_number, parent_timestamp) {
                trace!(
                    target: "bsc::executor::upgrade",
                    block_number,
                    parent_timestamp,
                    "Upgrading system contracts at block end (Feynman active)"
                );
                self.upgrade_contracts(block_number, block_timestamp, parent_timestamp)?;
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
        for tx in txs {
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
        let db = self.evm.db_mut();
        let mut info = db.basic(address).map_err(BlockExecutionError::other)?.unwrap_or_default();
        info.code_hash = code.hash_slow();
        info.code = Some(code);
        let mut account = RevmAccount::from(info);
        account.mark_touch();
        let mut changes: EvmState = Default::default();
        changes.insert(address, account);
        db.commit(changes);
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

        let db = self.evm.db_mut();
        let old_info = db.basic(HISTORY_STORAGE_ADDRESS).map_err(|err| {
            error!(
                target: "bsc::executor::prague",
                block_number,
                error = ?err,
                "Failed to load HistoryStorageAddress account",
            );
            BlockExecutionError::other(err)
        })?;
        debug!(
            target: "bsc::executor::prague",
            block_number,
            old_nonce = ?old_info.as_ref().map(|i| i.nonce),
            old_code_hash = ?old_info.as_ref().map(|i| i.code_hash),
            "HistoryStorageAddress account before deployment"
        );

        let mut new_info = old_info.unwrap_or_default();
        new_info.code_hash = keccak256(HISTORY_STORAGE_CODE.clone());
        new_info.code = Some(Bytecode::new_raw(Bytes::from_static(&HISTORY_STORAGE_CODE)));
        new_info.nonce = 1_u64;
        new_info.balance = U256::ZERO;
        let mut account = RevmAccount::from(new_info);
        account.mark_touch();
        let mut changes: EvmState = Default::default();
        changes.insert(HISTORY_STORAGE_ADDRESS, account);
        db.commit(changes);

        info!(
            target: "bsc::executor::prague",
            block_number,
            "Successfully deployed HistoryStorageAddress contract"
        );
        Ok(true)
    }
}

impl<'a, E, Spec, R> BlockExecutor for BscBlockExecutor<'a, E, Spec, R>
where
    E: Evm<
        DB: alloy_evm::block::StateDB,
        Tx: FromRecoveredTx<R::Transaction>
                + FromRecoveredTx<TransactionSigned>
                + FromTxWithEncoded<TransactionSigned>,
        BlockEnv = revm::context::BlockEnv,
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
    type Result = BscTxResult<E::HaltReason>;

    fn apply_pre_execution_changes(&mut self) -> Result<(), BlockExecutionError> {
        let block_env = self.evm.block().clone();
        trace!(
            target: "bsc::executor",
            block_id = %block_env.number(),
            is_miner = self.ctx.is_miner,
            "Start to apply_pre_execution_changes"
        );

        // Update current block height and header height metrics
        let block_number = block_env.number().to::<u64>();
        self.consensus_metrics.current_block_height.set(block_number as f64);

        // pre check and prepare some intermediate data for commit parlia snapshot in finish function.
        if self.ctx.is_miner {
            self.prepare_new_block(&block_env)?;
        } else {
            self.check_new_block(&block_env)?;
        }

        let parent_timestamp = self.inner_ctx.parent_header.as_ref().unwrap().timestamp;
        self.try_update_build_in_system_contract(
            self.evm.block().number().to::<u64>(),
            self.evm.block().timestamp().to::<u64>(),
            parent_timestamp,
            true,
        )?;

        // Apply historical block hashes if Prague is active
        if self.spec.is_prague_active_at_block_and_timestamp(
            self.evm.block().number().to::<u64>(),
            self.evm.block().timestamp().to::<u64>(),
        ) {
            trace!(
                target: "bsc::executor::prague",
                block_number = self.evm.block().number().to::<u64>(),
                parent_hash = ?self.ctx.base.parent_hash,
                "Calling apply_blockhashes_contract_call (Prague active)"
            );
            self.system_caller
                .apply_blockhashes_contract_call(self.ctx.base.parent_hash, &mut self.evm)?;
        }

        Ok(())
    }

    fn execute_transaction_without_commit(
        &mut self,
        tx: impl ExecutableTx<Self>,
    ) -> Result<BscTxResult<E::HaltReason>, BlockExecutionError> {
        use alloy_evm::RecoveredTx as _;

        let (tx_env, recovered) = tx.into_parts();
        let signer = *recovered.signer();
        let tx_signed: TransactionSigned = recovered.tx().clone();
        let tx_type = tx_signed.tx_type();

        // Detect system transactions: skip EVM execution, accumulate for later.
        let is_system = is_system_transaction(&tx_signed, signer, self.evm.block().beneficiary());
        if is_system {
            self.system_txs.push(tx_signed.clone());
            let dummy = ResultAndState {
                result: ExecutionResult::Success {
                    reason: SuccessReason::Stop,
                    gas: ResultGas::default(),
                    logs: vec![],
                    output: Output::Call(Bytes::new()),
                },
                state: Default::default(),
            };
            return Ok(BscTxResult {
                inner: dummy,
                blob_gas_used: 0,
                tx_type,
                tx: tx_signed,
                is_system: true,
            });
        }

        // Apply hertz patch before tx (validation only, not mining).
        if !self.ctx.is_miner {
            self.hertz_patch_manager.patch_before_tx(&tx_signed, self.evm.db_mut())?;
        }

        let block_available_gas = self.evm.block().gas_limit() - self.gas_used;
        let tx_gas_limit = {
            use alloy_consensus::Transaction as _;
            tx_signed.gas_limit()
        };
        if tx_gas_limit > block_available_gas {
            return Err(BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas {
                transaction_gas_limit: tx_gas_limit,
                block_available_gas,
            }
            .into());
        }

        let tx_hash = tx_signed.trie_hash();
        let block_number = self.evm.block().number().to::<u64>();
        let timestamp = self.evm.block().timestamp().to::<u64>();
        let spec =
            revm_spec_by_timestamp_and_block_number(self.spec.clone(), timestamp, block_number);
        let (to, selector, input_len) = {
            use alloy_consensus::Transaction as _;
            let to = tx_signed.to();
            let input = tx_signed.input();
            let selector = if input.len() >= 4 { Some(hex::encode(&input[..4])) } else { None };
            (to, selector, input.len())
        };

        precompiles::push_precompile_trace_context(
            precompiles::PrecompileTraceContext::from_parts(
                block_number,
                spec,
                false,
                Some(tx_hash),
                to,
                selector,
                input_len,
            ),
        );
        struct PrecompileTracePopGuard;
        impl Drop for PrecompileTracePopGuard {
            fn drop(&mut self) {
                precompiles::pop_precompile_trace_context();
            }
        }
        let _precompile_trace_pop_guard = PrecompileTracePopGuard;

        let blob_gas_used =
            if BscHardforks::is_cancun_active_at_timestamp(&self.spec, block_number, timestamp) {
                use alloy_consensus::Transaction as _;
                tx_signed.blob_gas_used().unwrap_or_default()
            } else {
                0
            };

        let inner =
            self.evm.transact(tx_env).map_err(|err| BlockExecutionError::evm(err, tx_hash))?;

        Ok(BscTxResult { inner, blob_gas_used, tx_type, tx: tx_signed, is_system: false })
    }

    fn commit_transaction(
        &mut self,
        output: BscTxResult<E::HaltReason>,
    ) -> GasOutput {
        if output.is_system {
            return GasOutput::new(0);
        }

        let ResultAndState { result, state } = output.inner;

        let mut temp_state = state.clone();
        temp_state.remove(&SYSTEM_ADDRESS);
        self.system_caller
            .on_state(StateChangeSource::Transaction(self.receipts.len()), &temp_state);

        let gas_used = result.tx_gas_used();
        self.gas_used += gas_used;
        self.blob_gas_used = self.blob_gas_used.saturating_add(output.blob_gas_used);

        self.receipts.push(self.receipt_builder.build_receipt(ReceiptBuilderCtx {
            tx_type: output.tx_type,
            evm: &self.evm,
            result,
            state: &state,
            cumulative_gas_used: self.gas_used,
        }));

        self.evm.db_mut().commit(state);

        // Apply hertz patch after tx (validation only, not mining).
        // commit_transaction cannot return errors in the new API, so defer any error to finish().
        if !self.ctx.is_miner {
            if let Err(e) = self.hertz_patch_manager.patch_after_tx(&output.tx, self.evm.db_mut()) {
                self.deferred_error = Some(e);
            }
        }

        GasOutput::new(gas_used)
    }

    fn finish(
        mut self,
    ) -> Result<(Self::Evm, BlockExecutionResult<R::Receipt>), BlockExecutionError> {
        if let Some(err) = self.deferred_error.take() {
            return Err(err);
        }
        let block_env = self.evm.block().clone();
        debug!(
            target: "bsc::executor",
            block_id = %block_env.number(),
            is_miner = self.ctx.is_miner,
            "Start to finish"
        );

        let parent_timestamp = self.inner_ctx.parent_header.as_ref().unwrap().timestamp;
        self.try_update_build_in_system_contract(
            self.evm.block().number().to::<u64>(),
            self.evm.block().timestamp().to::<u64>(),
            parent_timestamp,
            false,
        )?;

        // Initialize Feynman contracts on transition block
        if self.spec.is_feynman_transition_at_timestamp(
            self.evm.block().number().to::<u64>(),
            self.evm.block().timestamp().to::<u64>(),
            parent_timestamp,
        ) {
            info!(
                target: "bsc::executor::feynman",
                block_number = self.evm.block().number().to::<u64>(),
                "Initializing Feynman contracts"
            );
            self.initialize_feynman_contracts(self.evm.block().beneficiary())?;
        }

        // Deploy genesis contracts on Block 1
        if self.evm.block().number() == uint!(1U256) {
            info!(
                target: "bsc::executor::genesis",
                "Deploying genesis contracts on Block 1"
            );
            self.deploy_genesis_contracts(self.evm.block().beneficiary())?;
        }

        if self.ctx.is_miner {
            self.finalize_new_block(&self.evm.block().clone())?;
        } else {
            self.post_check_new_block(&self.evm.block().clone())?;
        }

        // Update receipt height metric
        let block_number = self.evm.block().number().to::<u64>();
        self.blockchain_metrics.current_receipt_height.set(block_number as f64);

        // Update block execution metrics
        self.executor_metrics.executed_blocks_total.increment(1);

        // Update block insert metrics
        // Calculate total transaction size in bytes (simplified estimation)
        // Each receipt contributes approximately:
        // - Base tx overhead: ~100 bytes
        // - Per log: ~100 bytes (address + topics + data average)
        let tx_size_bytes: usize = self
            .receipts
            .iter()
            .map(|r| {
                let logs_count = r.logs().len();
                100 + logs_count * 100 // Base + logs estimation
            })
            .sum();
        self.blockchain_metrics.block_tx_size_bytes.set(tx_size_bytes as f64);

        // Calculate block receive time difference
        // This is the difference between current block timestamp and parent block timestamp
        let current_timestamp = self.evm.block().timestamp().to::<u64>();
        if let Some(parent_header) = &self.inner_ctx.parent_header {
            let parent_timestamp = parent_header.timestamp;
            let time_diff = (current_timestamp as i64) - (parent_timestamp as i64);
            self.blockchain_metrics.block_receive_time_diff_seconds.set(time_diff as f64);
        }

        // Note: For gas-related metrics, use reth's ExecutorMetrics:
        // - sync.execution.gas_used_histogram
        // - sync.execution.gas_per_second (can be converted to MGas/s)
        // - sync.execution.execution_duration

        Ok((
            self.evm,
            BlockExecutionResult {
                receipts: self.receipts,
                requests: Requests::default(),
                gas_used: self.gas_used,
                blob_gas_used: self.blob_gas_used,
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

    fn receipts(&self) -> &[Self::Receipt] {
        &self.receipts
    }
}
