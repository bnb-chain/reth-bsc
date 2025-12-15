use crate::{BscPrimitives, hardforks::BscHardforks, node::evm::{assembler::{BscBlockAssembler, BscBlockAssemblerInput}, config::{BscBlockExecutionCtx, BscBlockExecutorFactory, BscExecutionSharedCtx}, executor::BscBlockExecutor, factory::BscEvmFactory, pre_execution::{TURN_LENGTH_CACHE, VALIDATOR_CACHE}}};
use reth_evm::execute::{BlockBuilder, BlockBuilderOutcome, BlockExecutionError, ExecutorTx};
use alloy_evm::eth::receipt_builder::ReceiptBuilder;
use reth_primitives_traits::{HeaderTy, NodePrimitives, Recovered, RecoveredBlock, SealedHeader, SignerRecoverable, TxTy};
use reth_provider::{
    BlockNumReader, BlockReader, DatabaseProviderFactory, HeaderProvider, StateProvider,
};
use revm::database::{State, states::bundle_state::BundleRetention};
use alloy_evm::{Evm, block::BlockExecutor};
use reth_chainspec::{EthChainSpec, EthereumHardforks, Hardforks};
use crate::node::trie_root::{RootSpeeder, RootSpeederMode, StateRootCompareState};
use parking_lot::Mutex;
use std::sync::Arc;


/// rewrite BasicBlockBuilder, mainly about the finish() trait.
/// add system txs to sealed block.
pub struct BscBlockBuilder<'a, EVM, Spec, R>
where
    R: ReceiptBuilder,
    Spec: EthChainSpec + EthereumHardforks + BscHardforks + Hardforks + Clone,
{
    /// The block executor used to execute transactions.
    pub executor: BscBlockExecutor<'a, EVM, Spec, R>,
    /// The transactions executed in this block.
    pub transactions: Vec<Recovered<TxTy<BscPrimitives>>>,
    /// The parent block execution context.
    pub ctx: BscBlockExecutionCtx<'a>,
    /// The shared context for block execution.
    pub shared_ctx: BscExecutionSharedCtx,
    /// The sealed parent block header.
    pub parent: &'a SealedHeader<HeaderTy<BscPrimitives>>,
    /// The assembler used to build the block.
    pub assembler: &'a BscBlockAssembler<crate::chainspec::BscChainSpec>,
}

impl<'a, EVM, Spec, R> BscBlockBuilder<'a, EVM, Spec, R>
where
    R: ReceiptBuilder,
    Spec: EthChainSpec + EthereumHardforks + BscHardforks + Hardforks + Clone,
{
    pub fn new(
        executor: BscBlockExecutor<'a, EVM, Spec, R>,
        ctx: BscBlockExecutionCtx<'a>,
        shared_ctx: BscExecutionSharedCtx,
        assembler: &'a BscBlockAssembler<crate::chainspec::BscChainSpec>,
        parent: &'a SealedHeader<HeaderTy<BscPrimitives>>,
    ) -> Self {
        Self {
            executor,
            transactions: Vec::new(),
            ctx,
            shared_ctx,
            parent,
            assembler,
        }
    }
}

/// A [`BscBlockBuilder`] variant that can compute state root using `ParallelStateRoot`.
///
/// This is used in the validator/miner payload build path where we have access to a
/// `DatabaseProviderFactory` (the node provider).
pub struct BscBlockBuilderWithFactory<'a, EVM, Spec, R, Factory>
where
    R: ReceiptBuilder,
    Spec: EthChainSpec + EthereumHardforks + BscHardforks + Hardforks + Clone,
{
    /// The block executor used to execute transactions.
    pub executor: BscBlockExecutor<'a, EVM, Spec, R>,
    /// The transactions executed in this block.
    pub transactions: Vec<Recovered<TxTy<BscPrimitives>>>,
    /// The parent block execution context.
    pub ctx: BscBlockExecutionCtx<'a>,
    /// The shared context for block execution.
    pub shared_ctx: BscExecutionSharedCtx,
    /// The sealed parent block header.
    pub parent: &'a SealedHeader<HeaderTy<BscPrimitives>>,
    /// The assembler used to build the block.
    pub assembler: &'a BscBlockAssembler<crate::chainspec::BscChainSpec>,
    /// Provider factory for creating a consistent DB view in parallel state root computation.
    pub provider_factory: Factory,
}

impl<'a, EVM, Spec, R, Factory> BscBlockBuilderWithFactory<'a, EVM, Spec, R, Factory>
where
    R: ReceiptBuilder,
    Spec: EthChainSpec + EthereumHardforks + BscHardforks + Hardforks + Clone,
{
    pub fn new(
        executor: BscBlockExecutor<'a, EVM, Spec, R>,
        ctx: BscBlockExecutionCtx<'a>,
        shared_ctx: BscExecutionSharedCtx,
        assembler: &'a BscBlockAssembler<crate::chainspec::BscChainSpec>,
        parent: &'a SealedHeader<HeaderTy<BscPrimitives>>,
        provider_factory: Factory,
    ) -> Self {
        Self {
            executor,
            transactions: Vec::new(),
            ctx,
            shared_ctx,
            parent,
            assembler,
            provider_factory,
        }
    }
}

impl<'a, DB, EVM, Spec, R> BlockBuilder for BscBlockBuilder<'a, EVM, Spec, R>
where
    BscBlockExecutor<'a, EVM, Spec, R>: alloy_evm::block::BlockExecutor<
        Evm: alloy_evm::Evm<
            Spec = <BscEvmFactory as reth_evm::EvmFactory>::Spec,
            HaltReason = <BscEvmFactory as reth_evm::EvmFactory>::HaltReason,
            DB = &'a mut State<DB>,
        >,
        Transaction = <BscPrimitives as NodePrimitives>::SignedTx,
        Receipt = <BscPrimitives as NodePrimitives>::Receipt,
    >,
    DB: reth_evm::Database + 'a,
    R: ReceiptBuilder<Transaction = <BscPrimitives as NodePrimitives>::SignedTx>,
    Spec: EthChainSpec + EthereumHardforks + BscHardforks + Hardforks + Clone,
    R::Transaction: Clone + SignerRecoverable,
    EVM: alloy_evm::Evm,
{
    type Primitives = BscPrimitives;
    type Executor = BscBlockExecutor<'a, EVM, Spec, R>;

    fn apply_pre_execution_changes(&mut self) -> Result<(), BlockExecutionError> {
        self.executor.apply_pre_execution_changes()
    }

    fn execute_transaction_with_commit_condition(
        &mut self,
        tx: impl ExecutorTx<Self::Executor>,
        f: impl FnOnce(
            &revm::context::result::ExecutionResult<<<Self::Executor as alloy_evm::block::BlockExecutor>::Evm as alloy_evm::Evm>::HaltReason>,
        ) -> alloy_evm::block::CommitChanges,
    ) -> Result<Option<u64>, BlockExecutionError> {
        if let Some(gas_used) =
            self.executor.execute_transaction_with_commit_condition(tx.as_executable(), f)?
        {
            self.transactions.push(tx.into_recovered());
            Ok(Some(gas_used))
        } else {
            Ok(None)
        }
    }

    // fetch assembled_system_txs and add into sealed block.
    fn finish(
        mut self,
        state: impl StateProvider,
    ) -> Result<BlockBuilderOutcome<BscPrimitives>, BlockExecutionError> {
        let finish_start = std::time::Instant::now();
        let (evm, result) = self.executor.finish()?;
        let (db, evm_env) = evm.finish();

        let assembled_system_txs = self.shared_ctx.inner.borrow().assembled_system_txs.clone();
        // merge all transitions into bundle state
        db.merge_transitions(BundleRetention::Reverts);

        // calculate the state root
        let state_root_start = std::time::Instant::now();
        let hashed_state = state.hashed_post_state(&db.bundle_state);
        let (state_root, trie_updates) = state
            .state_root_with_updates(hashed_state.clone())
            .map_err(BlockExecutionError::other)?;
        let state_root_duration = state_root_start.elapsed();

        let user_tx_len = self.transactions.len();
        let system_tx_len = assembled_system_txs.len();
        self.transactions.extend(assembled_system_txs);
        let total_tx_len = self.transactions.len();

        let (transactions, senders): (Vec<_>, Vec<_>) =
            self.transactions.into_iter().map(|tx| tx.into_parts()).unzip();

        // BlockAssemblerInput is non_exhaustive. 
        // So define a new struct BscBlockAssemblerInput and a new interface assemble_block_bsc.
        let bsc_input: BscBlockAssemblerInput<'_, '_, BscBlockExecutorFactory> = BscBlockAssemblerInput {
            evm_env,
            execution_ctx: self.ctx,
            parent: self.parent,
            transactions: transactions.clone(),
            output: &result,
            bundle_state: &db.bundle_state,
            state_provider: &state,
            state_root,
        };
        let assemble_start = std::time::Instant::now();
        let block = self.assembler.assemble_block_bsc(bsc_input)?;

        // cache current validators and turn length
        let current_validators = self.shared_ctx.inner.borrow().current_validators.clone();
        if let Some((validators, vote_addresses)) = current_validators {
            VALIDATOR_CACHE.lock().unwrap().insert(block.header.hash_slow(), (validators, vote_addresses));
            tracing::debug!("Succeed to update validator cache in builder, block_number: {}, block_hash: {}", block.header.number, block.header.hash_slow());
        }
        if let Some(turn_length) = self.shared_ctx.inner.borrow().turn_length {
            TURN_LENGTH_CACHE.lock().unwrap().insert(block.header.hash_slow(), turn_length);
            tracing::debug!("Succeed to update turn length cache in builder, block_number: {}, block_hash: {}", block.header.number, block.header.hash_slow());
        }
        let assemble_duration = assemble_start.elapsed();
        
        let finish_duration = finish_start.elapsed();
        tracing::debug!(
            target: "bsc::builder",
            block_number = %block.header.number,
            block_hash = %block.header.hash_slow(),
            user_tx_len = user_tx_len,
            system_tx_len = system_tx_len,
            total_tx_len = total_tx_len,
            finish_duration_ms = finish_duration.as_millis(),
            state_root_duration_ms = state_root_duration.as_millis(),
            assemble_duration_ms = assemble_duration.as_millis(),
            "Succeed to seal block"
        );

        let block = RecoveredBlock::new_unhashed(block, senders);
        Ok(BlockBuilderOutcome { execution_result: result, hashed_state, trie_updates, block })
    }

    fn executor_mut(&mut self) -> &mut Self::Executor {
        &mut self.executor
    }

    fn executor(&self) -> &Self::Executor {
        &self.executor
    }

    fn into_executor(self) -> Self::Executor {
        self.executor
    }
}

impl<'a, DB, EVM, Spec, R, Factory> BlockBuilder for BscBlockBuilderWithFactory<'a, EVM, Spec, R, Factory>
where
    BscBlockExecutor<'a, EVM, Spec, R>: alloy_evm::block::BlockExecutor<
        Evm: alloy_evm::Evm<
            Spec = <BscEvmFactory as reth_evm::EvmFactory>::Spec,
            HaltReason = <BscEvmFactory as reth_evm::EvmFactory>::HaltReason,
            DB = &'a mut State<DB>,
        >,
        Transaction = <BscPrimitives as NodePrimitives>::SignedTx,
        Receipt = <BscPrimitives as NodePrimitives>::Receipt,
    >,
    DB: reth_evm::Database + 'a,
    R: ReceiptBuilder<Transaction = <BscPrimitives as NodePrimitives>::SignedTx>,
    Spec: EthChainSpec + EthereumHardforks + BscHardforks + Hardforks + Clone,
    R::Transaction: Clone + SignerRecoverable,
    EVM: alloy_evm::Evm,
    Factory: DatabaseProviderFactory<Provider: BlockNumReader + HeaderProvider + BlockReader>
        + Clone
        + Send
        + Sync
        + 'static,
{
    type Primitives = BscPrimitives;
    type Executor = BscBlockExecutor<'a, EVM, Spec, R>;

    fn apply_pre_execution_changes(&mut self) -> Result<(), BlockExecutionError> {
        self.executor.apply_pre_execution_changes()
    }

    fn execute_transaction_with_commit_condition(
        &mut self,
        tx: impl ExecutorTx<Self::Executor>,
        f: impl FnOnce(
            &revm::context::result::ExecutionResult<<<Self::Executor as alloy_evm::block::BlockExecutor>::Evm as alloy_evm::Evm>::HaltReason>,
        ) -> alloy_evm::block::CommitChanges,
    ) -> Result<Option<u64>, BlockExecutionError> {
        if let Some(gas_used) =
            self.executor.execute_transaction_with_commit_condition(tx.as_executable(), f)?
        {
            self.transactions.push(tx.into_recovered());
            Ok(Some(gas_used))
        } else {
            Ok(None)
        }
    }

    fn finish(
        mut self,
        state: impl StateProvider,
    ) -> Result<BlockBuilderOutcome<BscPrimitives>, BlockExecutionError> {
        let finish_start = std::time::Instant::now();
        let (evm, result) = self.executor.finish()?;
        let (db, evm_env) = evm.finish();

        let assembled_system_txs = self.shared_ctx.inner.borrow().assembled_system_txs.clone();
        // merge all transitions into bundle state
        db.merge_transitions(BundleRetention::Reverts);

        // calculate the state root (serial, authoritative)
        let state_root_start = std::time::Instant::now();
        let hashed_state = state.hashed_post_state(&db.bundle_state);

        // Start accelerated state-root computation in the background BEFORE we run the serial
        // authoritative state-root, so we can compare overlapped timings.
        let compare_state = Arc::new(Mutex::new(StateRootCompareState::default()));
        RootSpeeder::spawn_accelerated_compare(
            self.provider_factory.clone(),
            self.parent.number,
            self.parent.hash(),
            hashed_state.clone(),
            compare_state.clone(),
        );

        let (state_root, trie_updates) = state
            .state_root_with_updates(hashed_state.clone())
            .map_err(BlockExecutionError::other)?;
        let state_root_mode = RootSpeederMode::Serial;

        let state_root_duration = state_root_start.elapsed();
        {
            let mut w = compare_state.lock();
            w.serial_root = Some(state_root);
            w.serial_duration_ms = Some(state_root_duration.as_millis());
        }

        let user_tx_len = self.transactions.len();
        let system_tx_len = assembled_system_txs.len();
        self.transactions.extend(assembled_system_txs);
        let total_tx_len = self.transactions.len();

        let (transactions, senders): (Vec<_>, Vec<_>) =
            self.transactions.into_iter().map(|tx| tx.into_parts()).unzip();

        let bsc_input: BscBlockAssemblerInput<'_, '_, BscBlockExecutorFactory> = BscBlockAssemblerInput {
            evm_env,
            execution_ctx: self.ctx,
            parent: self.parent,
            transactions: transactions.clone(),
            output: &result,
            bundle_state: &db.bundle_state,
            state_provider: &state,
            state_root,
        };
        let assemble_start = std::time::Instant::now();
        let block = self.assembler.assemble_block_bsc(bsc_input)?;
        let block_hash = block.header.hash_slow();

        // Provide the final block hash to the background compare task for easy grep.
        compare_state.lock().block_hash = Some(block_hash);

        // cache current validators and turn length
        let current_validators = self.shared_ctx.inner.borrow().current_validators.clone();
        if let Some((validators, vote_addresses)) = current_validators {
            VALIDATOR_CACHE.lock().unwrap().insert(block.header.hash_slow(), (validators, vote_addresses));
            tracing::debug!("Succeed to update validator cache in builder, block_number: {}, block_hash: {}", block.header.number, block.header.hash_slow());
        }
        if let Some(turn_length) = self.shared_ctx.inner.borrow().turn_length {
            TURN_LENGTH_CACHE.lock().unwrap().insert(block.header.hash_slow(), turn_length);
            tracing::debug!("Succeed to update turn length cache in builder, block_number: {}, block_hash: {}", block.header.number, block.header.hash_slow());
        }
        let assemble_duration = assemble_start.elapsed();

        let finish_duration = finish_start.elapsed();
        tracing::debug!(
            target: "bsc::builder",
            block_number = %block.header.number,
            block_hash = %block_hash,
            user_tx_len = user_tx_len,
            system_tx_len = system_tx_len,
            total_tx_len = total_tx_len,
            finish_duration_ms = finish_duration.as_millis(),
            state_root_duration_ms = state_root_duration.as_millis(),
            assemble_duration_ms = assemble_duration.as_millis(),
            state_root_mode = ?state_root_mode,
            serial_mode_used = matches!(state_root_mode, crate::node::trie_root::RootSpeederMode::Serial),
            "Succeed to seal block (state root)"
        );

        let block = RecoveredBlock::new_unhashed(block, senders);
        Ok(BlockBuilderOutcome { execution_result: result, hashed_state, trie_updates, block })
    }

    fn executor_mut(&mut self) -> &mut Self::Executor {
        &mut self.executor
    }

    fn executor(&self) -> &Self::Executor {
        &self.executor
    }

    fn into_executor(self) -> Self::Executor {
        self.executor
    }
}
