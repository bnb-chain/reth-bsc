use crate::{
    hardforks::BscHardforks,
    node::evm::{
        assembler::{BscBlockAssembler, BscBlockAssemblerInput},
        config::{BscBlockExecutionCtx, BscBlockExecutorFactory, BscExecutionSharedCtx},
        executor::BscBlockExecutor,
        factory::BscEvmFactory,
    },
    shared::BscEngineApiTx,
    BscPrimitives,
};
use alloy_consensus::BlockHeader as _;
use alloy_evm::block::BlockExecutor;
use alloy_evm::eth::receipt_builder::ReceiptBuilder;
use alloy_primitives::BlockHash;
use reth_chainspec::{EthChainSpec, EthereumHardforks, Hardforks};
use reth_engine_primitives::BSCEngineMessageError;
use reth_engine_tree::engine::EngineApiRequest;
use reth_engine_tree::tree::CustomRequestMessage;
use reth_evm::execute::{BlockBuilder, BlockBuilderOutcome, BlockBuilderOutcomeWithDiffLayer, BlockExecutionError, ExecutorTx};
use reth_primitives_traits::{
    HeaderTy, NodePrimitives, Recovered, RecoveredBlock, SealedHeader, SignerRecoverable, TxTy,
};
use reth_provider::StateProvider;
use reth_trie_common::updates::TrieUpdates;
use rust_eth_triedb::get_global_triedb;
use rust_eth_triedb_common::DiffLayers;
use revm::context::BlockEnv;
use revm::database::{states::bundle_state::BundleRetention, State};
use tokio::sync::oneshot;

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
        self,
        state: impl StateProvider,
    ) -> Result<BlockBuilderOutcome<BscPrimitives>, BlockExecutionError> {
        Ok(self.finish_with_difflayer(state)?.inner)
    }

    /// Finalize the block and compute the state root, supporting both MDBX-only and
    /// TrieDB modes. When TrieDB is active the returned `difflayer` is `Some`, carrying
    /// the precomputed diff layer that can be committed directly to the TrieDB backend;
    /// otherwise `difflayer` is `None` and the caller falls back to the standard
    /// hashed-state + trie-updates path.
    fn finish_with_difflayer(
        mut self,
        state: impl StateProvider,
    ) -> Result<BlockBuilderOutcomeWithDiffLayer<BscPrimitives>, BlockExecutionError> {
        let finish_start = std::time::Instant::now();
        let executor_finish_started = std::time::Instant::now();
        let (evm, result) = self.executor.finish()?;
        let (db, evm_env) = evm.finish();
        let executor_finish_us = executor_finish_started.elapsed().as_micros();

        let assembled_system_txs = {
            let mut inner = self.shared_ctx.inner.borrow_mut();
            std::mem::take(&mut inner.assembled_system_txs)
        };
        // merge all transitions into bundle state
        let merge_transitions_started = std::time::Instant::now();
        db.merge_transitions(BundleRetention::Reverts);
        let merge_transitions_us = merge_transitions_started.elapsed().as_micros();

        let state_root_start = std::time::Instant::now();
        let hashed_state = state.hashed_post_state(&db.bundle_state);

        // Use triedb to calculate state root
        let (state_root, trie_updates, produced_difflayer) = if rust_eth_triedb::triedb_manager::is_triedb_active() {
            let mut triedb = get_global_triedb();
            // Miner-side: try to use triedb prefetcher + parent difflayers from execution ctx.
            let prefetch_state = self.ctx.triedb_prefetcher.take().and_then(|p| p.finish());
            let parent_state_root = (**self.parent).state_root();
            let trie_hashed_state = hashed_state.to_triedb_hashed_post_state();
            let difflayers_opt = self.ctx.parent_difflayers.as_ref();

            let triedb_calc_started = std::time::Instant::now();
            let (new_root, new_difflayer) = triedb
                .intermediate_and_commit_hashed_post_state(
                    parent_state_root,
                    difflayers_opt,
                    &trie_hashed_state,
                    prefetch_state,
                )
                .map_err(BlockExecutionError::other)?;
            let triedb_calc_with_prefetch_ms = triedb_calc_started.elapsed().as_millis();

            tracing::debug!(
                target: "bsc::builder",
                parent_hash = %self.parent.hash(),
                block_number = %(self.parent.number + 1),
                parent_state_root = %parent_state_root,
                new_state_root = %new_root,
                has_parent_difflayers = difflayers_opt.is_some(),
                user_tx_count = self.transactions.len(),
                hashed_accounts = hashed_state.accounts.len(),
                hashed_storages = hashed_state.storages.len(),
                hashed_storage_slots = hashed_state
                    .storages
                    .values()
                    .map(|s| s.storage.len())
                    .sum::<usize>(),
                triedb_calc_ms = triedb_calc_with_prefetch_ms,
                triedb_calc_us = triedb_calc_started.elapsed().as_micros(),
                "Calculated state root using triedb"
            );

            // Snapshot the per-layer bloom filter stats accumulated during this
            // state-root call (resets to 0 atomically for the next block).
            // Only meaningful when the chain actually had parent diff layers
            // — first few blocks after restart will see queries=0.
            if let Some(difflayers) = difflayers_opt {
                let s = difflayers.filter_stats.snapshot_and_reset();
                tracing::debug!(
                    target: "bsc::builder::difflayer",
                    block_number = self.parent.number + 1,
                    chain_depth = difflayers.diff_layers.len(),
                    queries = s.queries,
                    bloom_checks = s.bloom_checks,
                    bloom_rejects = s.bloom_rejects,
                    bloom_passes = s.bloom_passes,
                    hashmap_hits = s.hashmap_hits,
                    hashmap_misses = s.hashmap_misses,
                    resolve_total_us = s.total_lookup_ns / 1_000,
                    "difflayer filter stats"
                );
            }
            (new_root, TrieUpdates::default(), Some(new_difflayer))
        } else {
            let (root, updates) =
                state.state_root_with_updates(hashed_state.clone()).map_err(BlockExecutionError::other)?;
            (root, updates, None)
        };
        let state_root_duration = state_root_start.elapsed();

        let user_tx_len = self.transactions.len();
        let system_tx_len = assembled_system_txs.len();
        self.transactions.extend(assembled_system_txs);
        let total_tx_len = self.transactions.len();

        let tx_into_parts_started = std::time::Instant::now();
        let (transactions, senders): (Vec<_>, Vec<_>) =
            self.transactions.into_iter().map(|tx| tx.into_parts()).unzip();
        let tx_into_parts_us = tx_into_parts_started.elapsed().as_micros();

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
            state_root_duration_ms = state_root_duration.as_millis(),
            assemble_duration_ms = assemble_duration.as_millis(),
            "Assembled block body (pre-finalize)"
        );
        tracing::debug!(
            target: "bsc::builder::finish",
            block_number = block.header.number,
            user_tx_count = user_tx_len,
            system_tx_count = system_tx_len,
            total_tx_count = total_tx_len,
            executor_finish_us,
            merge_transitions_us,
            state_root_us = state_root_duration.as_micros(),
            tx_into_parts_us,
            assemble_us = assemble_duration.as_micros(),
            finish_total_us = finish_duration.as_micros(),
            "finish breakdown"
        );

        let block = RecoveredBlock::new_unhashed(block, senders);
        Ok(BlockBuilderOutcomeWithDiffLayer {
            inner: BlockBuilderOutcome { execution_result: result, hashed_state, trie_updates, block },
            difflayer: produced_difflayer,
        })
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

/// Request the parent block's diff layers from the engine (TrieDB mode only).
///
/// Diff layers capture the trie-node changes produced by prior blocks and serve as
/// incremental input for computing the next state root via TrieDB, avoiding a full
/// trie traversal from disk.
pub async fn request_difflayer(
    engine_api_tx: &BscEngineApiTx,
    parent_hash: BlockHash,
) -> Result<DiffLayers, BSCEngineMessageError> {
    let (tx, rx) = oneshot::channel();
    let _ = engine_api_tx.send(EngineApiRequest::Custom(CustomRequestMessage::RequestDiffLayer {
        parent_hash,
        tx,
        _phantom: std::marker::PhantomData,
    }));
    rx.await.map_err(BSCEngineMessageError::internal)?.map_err(BSCEngineMessageError::internal)
}
