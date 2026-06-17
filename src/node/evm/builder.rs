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
use alloy_evm::block::{BlockExecutor, GasOutput};
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
    /// Optional precomputed `(state_root, trie_updates)` from a sparse-trie background
    /// task.
    ///
    /// When `Some`, `finish_with_difflayer`'s MDBX branch uses these values directly
    /// and skips the blocking `state_root_with_updates` call. Set via either:
    ///   * [`BscBlockBuilder::with_precomputed_state_root`] (fluent), or
    ///   * the `state_root_precomputed` parameter on the [`BlockBuilder::finish`]
    ///     trait method (which forwards into this field before delegating to
    ///     `finish_with_difflayer`).
    ///
    /// `None` in the TrieDB branch and when no sparse-trie task is in flight.
    pub precomputed_state_root: Option<(alloy_primitives::B256, TrieUpdates)>,
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
            precomputed_state_root: None,
        }
    }

    /// Install a precomputed `(state_root, trie_updates)` to be consumed by
    /// `finish_with_difflayer` (MDBX branch only). Returns `self` for fluent usage.
    ///
    /// Pass `None` to clear an existing value.
    ///
    /// Currently has no callers — `BscPayloadJob` obtains its builder via
    /// `evm_config.builder_for_next_block(...)` which returns `impl BlockBuilder`, so
    /// this concrete setter is unreachable through the trait. Kept as the documented
    /// integration point for the follow-up sparse-trie wiring; see TODOs in
    /// `payload.rs` for the two ways to bridge the gap.
    #[allow(dead_code)]
    pub fn with_precomputed_state_root(
        mut self,
        precomputed: Option<(alloy_primitives::B256, TrieUpdates)>,
    ) -> Self {
        self.precomputed_state_root = precomputed;
        self
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
        f: impl FnOnce(&<Self::Executor as alloy_evm::block::BlockExecutor>::Result) -> alloy_evm::block::CommitChanges,
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

    // fetch assembled_system_txs and add into sealed block.
    fn finish(
        mut self,
        state: impl StateProvider,
        state_root_precomputed: Option<(alloy_primitives::B256, TrieUpdates)>,
    ) -> Result<BlockBuilderOutcome<BscPrimitives>, BlockExecutionError> {
        // Forward into the field so `finish_with_difflayer` (whose trait signature
        // takes only `state`) can pick it up.
        if state_root_precomputed.is_some() {
            self.precomputed_state_root = state_root_precomputed;
        }
        Ok(self.finish_with_difflayer(state)?.inner)
    }

    /// Finalize the block and compute the state root, supporting both MDBX-only and
    /// TrieDB modes. When TrieDB is active the returned `difflayer` is `Some`, carrying
    /// the precomputed diff layer that can be committed directly to the TrieDB backend;
    /// otherwise `difflayer` is `None` and the caller falls back to the standard
    /// hashed-state + trie-updates path.
    ///
    /// MDBX branch additionally honors `self.precomputed_state_root` (set via
    /// [`BscBlockBuilder::with_precomputed_state_root`] or routed through `fn finish`)
    /// to skip the blocking `state_root_with_updates` call when a sparse-trie task has
    /// already computed the root concurrently with execution.
    fn finish_with_difflayer(
        mut self,
        state: impl StateProvider,
    ) -> Result<BlockBuilderOutcomeWithDiffLayer<BscPrimitives>, BlockExecutionError> {
        let finish_start = std::time::Instant::now();
        // `executor.finish()` runs BSC's post-execution system txs (slash spoiled
        // validator, distribute fees / finality rewards, breathe-block validator-set
        // updates). Any `state_hook` previously installed on the executor — including
        // the sparse-trie hook — captures those state changes here. Consuming the
        // executor on the next line drops the hook closure, which triggers
        // `StateHookSender::drop` → `FinishedStateUpdates` → sparse-trie task can
        // safely return from `state_root()` below.
        let (evm, result) = self.executor.finish()?;
        let (db, evm_env) = evm.finish();

        // Sparse-trie state-root collection: now that the executor (and therefore the
        // state_hook) has been dropped, the background task has all updates and is
        // finalizing. Take the handle from ctx, block on `state_root()`, and stash the
        // result in the sink — the MDBX branch below reads it.
        //
        // Failures fall through silently to the legacy `state_root_with_updates` path,
        // logged at WARN. A failure here is non-fatal but indicates the task panicked
        // or its channel was dropped; investigate via the trace target below.
        if let Some(handle_slot) = self.ctx.trie_handle.take() {
            if let Some(mut handle) = handle_slot.lock().unwrap().take() {
                let wait_start = std::time::Instant::now();
                // R2: bound the wait by the slot deadline (if set) so an in-turn block
                // never blocks unboundedly past its slot. On timeout/error we leave the
                // sink empty, so the MDBX branch below falls back to the synchronous
                // `state_root_with_updates` (bounded, correct). `None` deadline keeps the
                // legacy blocking wait (out-of-turn / bid-sim / import paths).
                let delivered = match self.ctx.state_root_deadline_ms {
                    Some(deadline_ms) => {
                        let now_ms = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis() as u64)
                            .unwrap_or(0);
                        let budget =
                            std::time::Duration::from_millis(deadline_ms.saturating_sub(now_ms));
                        match handle.take_state_root_rx().recv_timeout(budget) {
                            Ok(Ok(outcome)) => Some(outcome),
                            Ok(Err(err)) => {
                                tracing::warn!(
                                    target: "bsc::builder",
                                    parent_hash = %self.parent.hash(),
                                    block_number = %(self.parent.number + 1),
                                    %err,
                                    "Sparse-trie task error post-finish(); falling back to state_root_with_updates"
                                );
                                None
                            }
                            Err(_timeout) => {
                                // Build-path state-root missed the slot deadline (the ~DELAY_LEFT_OVER
                                // budget). A high rate here is a primary cause of empty/low-gas
                                // blocks: filling stopped but the root wasn't ready in time.
                                metrics::counter!("bsc_builder_state_root_deadline_timeout_total")
                                    .increment(1);
                                tracing::warn!(
                                    target: "bsc::builder",
                                    parent_hash = %self.parent.hash(),
                                    block_number = %(self.parent.number + 1),
                                    wait_ms = wait_start.elapsed().as_millis(),
                                    "Sparse-trie state-root not ready before slot deadline; falling back to state_root_with_updates"
                                );
                                None
                            }
                        }
                    }
                    None => match handle.state_root() {
                        Ok(outcome) => Some(outcome),
                        Err(err) => {
                            tracing::warn!(
                                target: "bsc::builder",
                                parent_hash = %self.parent.hash(),
                                block_number = %(self.parent.number + 1),
                                %err,
                                "Sparse-trie task failed post-finish(); falling back to state_root_with_updates"
                            );
                            None
                        }
                    },
                };
                // Build-path state-root instrumentation: total attempts, how long we waited (vs the
                // ~DELAY_LEFT_OVER budget), and how often we fell back to the synchronous root.
                // fallback_total / total = the build-path root fallback ratio.
                metrics::counter!("bsc_builder_state_root_total").increment(1);
                metrics::histogram!("bsc_builder_state_root_wait_duration_seconds")
                    .record(wait_start.elapsed().as_secs_f64());
                if delivered.is_none() {
                    metrics::counter!("bsc_builder_state_root_fallback_total").increment(1);
                }
                if let Some(outcome) = delivered {
                    let updates = std::sync::Arc::try_unwrap(outcome.trie_updates)
                        .unwrap_or_else(|arc| (*arc).clone());
                    if let Some(sink) = self.ctx.state_root_precomputed_sink.as_ref() {
                        *sink.lock().unwrap() = Some((outcome.state_root, updates));
                    } else {
                        // No sink registered — write into the field (which the
                        // MDBX branch also reads as fallback).
                        self.precomputed_state_root = Some((outcome.state_root, updates));
                    }
                    tracing::debug!(
                        target: "bsc::builder",
                        parent_hash = %self.parent.hash(),
                        block_number = %(self.parent.number + 1),
                        user_tx_count = self.transactions.len(),
                        state_root = %outcome.state_root,
                        wait_ms = wait_start.elapsed().as_millis(),
                        "Sparse-trie state-root delivered post-finish()"
                    );
                }
            }
        }

        let assembled_system_txs = {
            let mut inner = self.shared_ctx.inner.borrow_mut();
            std::mem::take(&mut inner.assembled_system_txs)
        };
        // merge all transitions into bundle state
        db.merge_transitions(BundleRetention::Reverts);

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
            (new_root, TrieUpdates::default(), Some(new_difflayer))
        } else if let Some((root, updates)) = self
            .ctx
            .state_root_precomputed_sink
            .as_ref()
            .and_then(|sink| sink.lock().unwrap().take())
            .or_else(|| self.precomputed_state_root.take())
        {
            // Fast path: sparse-trie background task already computed the root concurrently
            // with execution. See `crate::shared::spawn_sparse_trie_state_root` and reth 2.0
            // `--engine.share-sparse-trie-with-payload-builder` semantics for the upstream
            // mechanism we mirror.
            //
            // Preferred source is the sink on `self.ctx` (filled by the payload layer
            // post-exec); the field on `Self` is a fallback that `fn finish` populates
            // when callers route through the trait method.
            tracing::debug!(
                target: "bsc::builder",
                parent_hash = %self.parent.hash(),
                block_number = %(self.parent.number + 1),
                user_tx_count = self.transactions.len(),
                hashed_accounts = hashed_state.accounts.len(),
                hashed_storages = hashed_state.storages.len(),
                state_root = %root,
                "Using precomputed state root from sparse-trie task"
            );
            (root, updates, None)
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
            state_root_duration_ms = state_root_duration.as_millis(),
            assemble_duration_ms = assemble_duration.as_millis(),
            "Assembled block body (pre-finalize)"
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
