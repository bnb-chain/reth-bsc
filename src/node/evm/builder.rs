use crate::{BscPrimitives, hardforks::BscHardforks, node::evm::{assembler::{BscBlockAssembler, BscBlockAssemblerInput}, config::{BscBlockExecutionCtx, BscBlockExecutorFactory, BscExecutionSharedCtx}, executor::BscBlockExecutor, factory::BscEvmFactory, pre_execution::{TURN_LENGTH_CACHE, VALIDATOR_CACHE}}};
use reth_evm::execute::{BlockBuilder, BlockBuilderOutcome, BlockExecutionError, ExecutorTx};
use alloy_evm::eth::receipt_builder::ReceiptBuilder;
use reth_primitives_traits::{HeaderTy, NodePrimitives, Recovered, RecoveredBlock, SealedHeader, SignerRecoverable, TxTy};
use reth_provider::{
    providers::ConsistentDbView, BlockNumReader, BlockReader, DatabaseProviderFactory,
    HeaderProvider, StateProvider,
};
use revm::database::{State, states::bundle_state::BundleRetention};
use alloy_evm::{Evm, block::BlockExecutor};
use reth_chainspec::{EthChainSpec, EthereumHardforks, Hardforks};
use reth_trie::TrieInput;
use reth_trie_parallel::root::ParallelStateRoot;
use crate::node::trie_overlay::trie_overlay_cache;


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

        // calculate the state root (parallel)
        let state_root_start = std::time::Instant::now();
        let hashed_state = state.hashed_post_state(&db.bundle_state);

        let (state_root, trie_updates) = {
            // Try parallel state root with DB+overlay TrieInput.
            //
            // If inputs are insufficient (overlay missing, no trie updates, etc.) or parallel
            // computation fails for any reason, we fall back to serial `state_root_with_updates`.
            let attempt_parallel = (|| -> Result<(_, _, u64, usize), BlockExecutionError> {
                let provider_ro = self
                    .provider_factory
                    .database_provider_ro()
                    .map_err(BlockExecutionError::other)?;
                let db_last = provider_ro.last_block_number().map_err(BlockExecutionError::other)?;
                let db_tip = provider_ro
                    .sealed_header(db_last)
                    .map_err(BlockExecutionError::other)?
                    .ok_or_else(|| {
                        BlockExecutionError::other(std::io::Error::other("db tip missing"))
                    })?;

                let consistent_view =
                    ConsistentDbView::new(self.provider_factory.clone(), Some((db_tip.hash(), db_last)));

                let mut trie_input = TrieInput::default();
                let mut overlay_blocks = 0usize;

                if db_last < self.parent.number {
                    let cache = trie_overlay_cache().ok_or_else(|| {
                        BlockExecutionError::other(std::io::Error::other(
                            "trie overlay cache not initialized",
                        ))
                    })?;
                    let needed_range = (db_last + 1)..=self.parent.number;
                    let overlays = cache.read().get_range(needed_range.clone());
                    overlay_blocks = overlays.len();

                    // Require full coverage.
                    if overlays.len() != (self.parent.number - db_last) as usize {
                        return Err(BlockExecutionError::other(std::io::Error::other(format!(
                            "missing trie overlay blocks for range {:?} (have {})",
                            needed_range,
                            overlays.len()
                        ))));
                    }

                    for entry in overlays {
                        // Ensure hash matches expected canonical chain (best-effort safety).
                        if entry.number == self.parent.number && entry.hash != self.parent.hash() {
                            return Err(BlockExecutionError::other(std::io::Error::other(format!(
                                "parent hash mismatch in overlay cache: expected={:?} got={:?}",
                                self.parent.hash(),
                                entry.hash
                            ))));
                        }

                        // For correctness, require trie updates for overlay blocks. If they are not
                        // available, treat as insufficient input and fall back to serial root.
                        let Some(nodes) = entry.trie_updates.as_deref() else {
                            return Err(BlockExecutionError::other(std::io::Error::other(format!(
                                "missing trie_updates for overlay block {}",
                                entry.number
                            ))));
                        };
                        trie_input.append_cached_ref(nodes, &entry.hashed_state);
                    }
                }

                // Append current block's changes last.
                trie_input.append(hashed_state.clone());

                let (state_root, trie_updates) = ParallelStateRoot::new(consistent_view, trie_input)
                    .incremental_root_with_updates()
                    .map_err(BlockExecutionError::other)?;

                Ok((state_root, trie_updates, db_last, overlay_blocks))
            })();

            match attempt_parallel {
                Ok((state_root, trie_updates, db_last, overlay_blocks)) => {
                    tracing::debug!(
                        target: "bsc::builder",
                        parent_num = self.parent.number,
                        parent_hash = ?self.parent.hash(),
                        db_last,
                        overlay_blocks,
                        "Parallel state root succeeded"
                    );
                    (state_root, trie_updates)
                }
                Err(err) => {
                    tracing::debug!(
                        target: "bsc::builder",
                        parent_num = self.parent.number,
                        parent_hash = ?self.parent.hash(),
                        %err,
                        "Parallel state root unavailable, falling back to serial"
                    );
                    state
                        .state_root_with_updates(hashed_state.clone())
                        .map_err(BlockExecutionError::other)?
                }
            }
        };

        let state_root_duration = state_root_start.elapsed();

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
            "Succeed to seal block (parallel state root)"
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
