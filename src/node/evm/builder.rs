use crate::{
    hardforks::BscHardforks,
    node::evm::{
        assembler::{BscBlockAssembler, BscBlockAssemblerInput},
        config::{BscBlockExecutionCtx, BscBlockExecutorFactory, BscExecutionSharedCtx},
        executor::BscBlockExecutor,
        factory::BscEvmFactory,
    },
    BscPrimitives,
};
use alloy_evm::{
    block::{BlockExecutor, GasOutput},
    eth::receipt_builder::ReceiptBuilder,
};
use reth_chainspec::{EthChainSpec, EthereumHardforks, Hardforks};
use reth_evm::execute::{BlockBuilder, BlockBuilderOutcome, BlockExecutionError, ExecutorTx};
use reth_primitives_traits::{
    HeaderTy, NodePrimitives, Recovered, RecoveredBlock, SealedHeader, SignerRecoverable, TxTy,
};
use reth_provider::StateProvider;
use reth_trie_common::updates::TrieUpdates;
use revm::{
    context::BlockEnv,
    database::{states::bundle_state::BundleRetention, State},
};

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
        Self { executor, transactions: Vec::new(), ctx, shared_ctx, parent, assembler }
    }
}

impl<'a, DB, EVM, Spec, R> BlockBuilder for BscBlockBuilder<'a, EVM, Spec, R>
where
    BscBlockExecutor<'a, EVM, Spec, R>: alloy_evm::block::BlockExecutor<
        Evm = EVM,
        Transaction = <BscPrimitives as NodePrimitives>::SignedTx,
        Receipt = <BscPrimitives as NodePrimitives>::Receipt,
    >,
    EVM: alloy_evm::Evm<
        Spec = <BscEvmFactory as reth_evm::EvmFactory>::Spec,
        HaltReason = <BscEvmFactory as reth_evm::EvmFactory>::HaltReason,
        DB = &'a mut State<DB>,
        BlockEnv = BlockEnv,
    >,
    DB: reth_evm::Database + 'a,
    R: ReceiptBuilder<Transaction = <BscPrimitives as NodePrimitives>::SignedTx>,
    Spec: EthChainSpec + EthereumHardforks + BscHardforks + Hardforks + Clone,
    R::Transaction: Clone + SignerRecoverable,
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
            &<Self::Executor as alloy_evm::block::BlockExecutor>::Result,
        ) -> alloy_evm::block::CommitChanges,
    ) -> Result<Option<GasOutput>, BlockExecutionError> {
        let (tx_env, recovered) = tx.into_parts();
        if let Some(gas_output) =
            self.executor.execute_transaction_with_commit_condition((tx_env, &recovered), f)?
        {
            self.transactions.push(recovered);
            Ok(Some(gas_output))
        } else {
            Ok(None)
        }
    }

    /// Finalize the block.
    ///
    /// Trie removal: this node maintains no Merkle
    /// trie, so no state root is computed. Locally-built blocks carry
    /// `B256::ZERO` in the `state_root` header field — valid only under the
    /// trie-less protocol assumption that peers do not verify state roots
    /// (fastnode mode). BEP-675 BidBlocks are sealed with the builder-supplied
    /// header and never pass through here.
    ///
    /// The `state_root_precomputed` parameter is part of the upstream
    /// `BlockBuilder` trait signature and is ignored.
    fn finish(
        mut self,
        state: impl StateProvider,
        _state_root_precomputed: Option<(alloy_primitives::B256, TrieUpdates)>,
    ) -> Result<BlockBuilderOutcome<BscPrimitives>, BlockExecutionError> {
        let finish_start = std::time::Instant::now();
        // `executor.finish()` runs BSC's post-execution system txs (slash spoiled
        // validator, distribute fees / finality rewards, breathe-block validator-set
        // updates).
        let (evm, result) = self.executor.finish()?;
        let (db, evm_env) = evm.finish();

        let assembled_system_txs = {
            let mut inner = self.shared_ctx.inner.borrow_mut();
            std::mem::take(&mut inner.assembled_system_txs)
        };
        // merge all transitions into bundle state
        db.merge_transitions(BundleRetention::Reverts);

        // Hashed post-state is still produced (keccak over changed accounts, no trie
        // walk) — under storage v2 the hashed tables are the canonical state
        // representation, and downstream payload consumers expect it.
        let hashed_state = state.hashed_post_state(&db.bundle_state);
        let (state_root, trie_updates) = (alloy_primitives::B256::ZERO, TrieUpdates::default());

        let user_tx_len = self.transactions.len();
        let system_tx_len = assembled_system_txs.len();
        self.transactions.extend(assembled_system_txs);
        let total_tx_len = self.transactions.len();

        let (transactions, senders): (Vec<_>, Vec<_>) =
            self.transactions.into_iter().map(|tx| tx.into_parts()).unzip();

        // Extract sinks from ctx before it is moved into BscBlockAssemblerInput.
        let validator_cache_sink = self.ctx.validator_cache_sink.take();
        let turn_length_sink = self.ctx.turn_length_sink.take();

        // BlockAssemblerInput is non_exhaustive, so we use BscBlockAssemblerInput with
        // assemble_block_body_only() which skips finalize_new_header() at build time.
        let bsc_input: BscBlockAssemblerInput<'_, '_, BscBlockExecutorFactory> =
            BscBlockAssemblerInput {
                evm_env,
                execution_ctx: self.ctx,
                parent: self.parent,
                transactions,
                output: &result,
                bundle_state: &db.bundle_state,
                state_provider: &state,
                state_root,
            };
        let assemble_start = std::time::Instant::now();
        // Assemble block body only — finalize_new_header() is deferred to pick_best_payload()
        // so that FF votes can be collected right up to the moment the best payload is chosen.
        let block = self.assembler.assemble_block_body_only(bsc_input)?;

        // Transport validator and turn-length data to the payload layer via sinks.
        // The final block hash is not yet known here (finalize_new_header hasn't run),
        // so we cannot write to VALIDATOR_CACHE / TURN_LENGTH_CACHE yet.
        let current_validators = self.shared_ctx.inner.borrow().current_validators.clone();
        if let Some((validators, vote_addresses)) = current_validators {
            if let Some(sink) = &validator_cache_sink {
                *sink.lock().unwrap() = Some((validators, vote_addresses));
            }
        }
        if let Some(turn_length) = self.shared_ctx.inner.borrow().turn_length {
            if let Some(sink) = &turn_length_sink {
                *sink.lock().unwrap() = Some(turn_length);
            }
        }
        let assemble_duration = assemble_start.elapsed();

        let finish_duration = finish_start.elapsed();
        tracing::debug!(
            target: "bsc::builder",
            block_number = %block.header.number,
            user_tx_len = user_tx_len,
            system_tx_len = system_tx_len,
            total_tx_len = total_tx_len,
            finish_duration_ms = finish_duration.as_millis(),
            assemble_duration_ms = assemble_duration.as_millis(),
            "Assembled block body (pre-finalize)"
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
