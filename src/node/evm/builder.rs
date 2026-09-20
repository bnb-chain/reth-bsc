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
use alloy_evm::block::{BlockExecutor, GasOutput};
use alloy_evm::eth::receipt_builder::ReceiptBuilder;
use reth_chainspec::{EthChainSpec, EthereumHardforks, Hardforks};
use reth_evm::execute::{BlockBuilder, BlockBuilderOutcome, BlockExecutionError, ExecutorTx};
use reth_primitives_traits::{
    HeaderTy, NodePrimitives, Recovered, RecoveredBlock, SealedHeader, SignerRecoverable, TxTy,
};
use reth_provider::StateProvider;
use reth_trie_common::updates::TrieUpdates;
use revm::context::BlockEnv;
use revm::database::{states::bundle_state::BundleRetention, State};

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
    /// When `Some`, `finish` uses these values directly and skips the blocking
    /// `state_root_with_updates` call. Set via either:
    ///   * [`BscBlockBuilder::with_precomputed_state_root`] (fluent), or
    ///   * the `state_root_precomputed` parameter on [`BlockBuilder::finish`].
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
    /// `finish`. Returns `self` for fluent usage.
    ///
    /// Pass `None` to clear an existing value.
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

    /// Finalize the block and compute the state root.
    ///
    /// Honors `self.precomputed_state_root` (set via
    /// [`BscBlockBuilder::with_precomputed_state_root`] or the
    /// `state_root_precomputed` parameter) to skip the blocking
    /// `state_root_with_updates` call when a sparse-trie task has already computed
    /// the root concurrently with execution.
    fn finish(
        mut self,
        state: impl StateProvider,
        state_root_precomputed: Option<(alloy_primitives::B256, TrieUpdates)>,
    ) -> Result<BlockBuilderOutcome<BscPrimitives>, BlockExecutionError> {
        if state_root_precomputed.is_some() {
            self.precomputed_state_root = state_root_precomputed;
        }
        let finish_start = std::time::Instant::now();
        // `executor.finish()` runs BSC's post-execution system txs (slash spoiled
        // validator, distribute fees / finality rewards, breathe-block validator-set
        // updates). The sparse-trie `state_hook` is installed on the DB (v2.4.1), so it
        // captures those commits here too.
        let (evm, result) = self.executor.finish()?;
        let (db, evm_env) = evm.finish();

        // Finish the sparse-trie update stream. The hook lives on the DB (which outlives the
        // executor), so it is NOT dropped by `executor.finish()`; we must clear it explicitly.
        // Clearing drops the `StateRootUpdateHook`, which signals end-of-updates so the
        // background task can finalize and `state_root()` below returns. Without this the task
        // waits forever for more updates and the miner falls back after the slot deadline.
        db.set_state_hook(None);

        // Sparse-trie state-root collection: now that the update stream is finished, the
        // background task has all updates and is finalizing. Take the handle from ctx, block on
        // `state_root()`, and stash the result in the sink — the MDBX branch below reads it.
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

        // Proposer-side execution witness (`BSC_WITNESS_EVERY_BLOCK=<dir>`).
        //
        // The engine-side `--debug.witness-every-block` only fires for blocks a node
        // VALIDATES, and a proposer never validates its own block -- verified on the devnet:
        // blocks 146/156/166 were proposed by node0 and only its peers captured them. That is
        // precisely the gap that left qanet with no producer-side evidence for 29,470,741,
        // where .175 built the bad block and captured nothing.
        //
        // Named by parent hash because the block has no hash yet at this point; a later build
        // attempt for the same height overwrites the earlier one.
        if let Ok(dir) = std::env::var("BSC_WITNESS_EVERY_BLOCK") {
            // Zero-padded for correct lexical ordering -- unpadded, block 9 sorts after 96.
            let stem =
                format!("{:010}_{}.proposed", self.parent.number + 1, self.parent.hash());
            let started = std::time::Instant::now();
            match reth_engine_tree::tree::write_execution_witness(
                db,
                &state,
                &stem,
                std::path::Path::new(&dir),
            ) {
                Ok(bytes) => tracing::debug!(
                    target: "bsc::builder",
                    block_number = self.parent.number + 1,
                    bytes,
                    elapsed = ?started.elapsed(),
                    "Wrote proposer-side execution witness"
                ),
                Err(err) => tracing::warn!(
                    target: "bsc::builder",
                    block_number = self.parent.number + 1,
                    %err,
                    "Failed to write proposer-side execution witness; continuing"
                ),
            }
        }

        let state_root_start = std::time::Instant::now();
        let hashed_state =
            state.hashed_post_state(&db.bundle_state).map_err(BlockExecutionError::other)?;

        let (state_root, trie_updates) = if let Some((root, updates)) = self
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
            // when called with `state_root_precomputed`.
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
            // Producer-side verification (opt-in, `BSC_VERIFY_SEALED_STATE_ROOT=1`).
            //
            // Nothing else in the pipeline can catch a bad sparse-trie root on the producing
            // validator. This branch seals `root` straight into the header, and an importing
            // node's only check is `outcome.state_root == block.header().state_root()` --
            // which is circular, because that header root came from here. So a wrong root is
            // signed, gossiped and accepted, and the chain extends on an invalid block.
            //
            // Recomputing serially is the independent check. It is far too slow to run in-slot
            // (~900ms under a deep overlay, against a 450ms period), so this is a diagnostic
            // for devnet reproduction, not something to enable on a real validator. A
            // production-grade guard would run this off the critical path, after sealing.
            if std::env::var("BSC_VERIFY_SEALED_STATE_ROOT").is_ok_and(|v| v == "1") {
                let verify_start = std::time::Instant::now();
                match state.state_root_with_updates(hashed_state.clone()) {
                    Ok((serial_root, serial_updates)) if serial_root != root => {
                        // Which tries the two disagree about. A storage trie present on one
                        // side only, or `is_deleted` set on one side only, points straight at
                        // the wipe/deletion handling; a pure account-node path difference
                        // points at the account trie walk instead.
                        let only_sparse: Vec<String> = updates
                            .storage_tries
                            .keys()
                            .filter(|a| !serial_updates.storage_tries.contains_key(*a))
                            .take(8)
                            .map(|a| format!("{a:?}"))
                            .collect();
                        let only_serial: Vec<String> = serial_updates
                            .storage_tries
                            .keys()
                            .filter(|a| !updates.storage_tries.contains_key(*a))
                            .take(8)
                            .map(|a| format!("{a:?}"))
                            .collect();
                        let deleted_differs: Vec<String> = updates
                            .storage_tries
                            .iter()
                            .filter_map(|(a, u)| {
                                serial_updates.storage_tries.get(a).and_then(|s| {
                                    (s.is_deleted != u.is_deleted).then(|| {
                                        format!("{a:?}:sparse={},serial={}", u.is_deleted, s.is_deleted)
                                    })
                                })
                            })
                            .take(8)
                            .collect();
                        metrics::counter!("bsc_builder_sealed_state_root_mismatch_total")
                            .increment(1);
                        tracing::error!(
                            target: "bsc::builder",
                            parent_hash = %self.parent.hash(),
                            block_number = %(self.parent.number + 1),
                            sparse_root = %root,
                            serial_root = %serial_root,
                            user_tx_count = self.transactions.len(),
                            hashed_accounts = hashed_state.accounts.len(),
                            hashed_storages = hashed_state.storages.len(),
                            wiped_storages =
                                hashed_state.storages.values().filter(|s| s.wiped).count(),
                            sparse_account_nodes = updates.account_nodes.len(),
                            serial_account_nodes = serial_updates.account_nodes.len(),
                            sparse_storage_tries = updates.storage_tries.len(),
                            serial_storage_tries = serial_updates.storage_tries.len(),
                            storage_tries_only_sparse = ?only_sparse,
                            storage_tries_only_serial = ?only_serial,
                            is_deleted_differs = ?deleted_differs,
                            // What the sparse-trie task was actually fed. If the ValidatorSet
                            // never appears here, the updates were lost before the task saw
                            // them; if it does, the fault is inside the task's ingestion.
                            state_hook = %self
                                .ctx
                                .state_hook_counts
                                .as_ref()
                                .map(|c| c.snapshot())
                                .unwrap_or_else(|| "unavailable".to_string()),
                            // Streamed (slot, original -> present) for the ValidatorSet, beside
                            // what the authoritative bundle recorded for the same account. If the
                            // bundle lists slots the stream reported as unchanged, the streamed
                            // originals are stale and `is_changed()` is the wrong criterion.
                            state_hook_samples = %self
                                .ctx
                                .state_hook_counts
                                .as_ref()
                                .map(|c| c.samples())
                                .unwrap_or_default(),
                            // The comparison that matters: what the task accumulated vs the
                            // authoritative bundle delta.
                            stream_vs_bundle = %self
                                .ctx
                                .state_hook_counts
                                .as_ref()
                                .map(|c| c.diff_against(&hashed_state))
                                .unwrap_or_default(),
                            bundle_validator_set = ?hashed_state
                                .storages
                                .get(&alloy_primitives::keccak256(
                                    crate::node::evm::hook_probe::VALIDATOR_SET,
                                ))
                                .map(|s| {
                                    (
                                        s.wiped,
                                        s.storage
                                            .iter()
                                            .take(8)
                                            .map(|(k, v)| format!("{k:#x}:{v:#x}"))
                                            .collect::<Vec<_>>(),
                                    )
                                }),
                            verify_ms = verify_start.elapsed().as_millis(),
                            "Sparse-trie state root disagrees with the serial root; \
                             this block would seal an invalid state root"
                        );
                    }
                    Ok(_) => {
                        tracing::debug!(
                            target: "bsc::builder",
                            block_number = %(self.parent.number + 1),
                            verify_ms = verify_start.elapsed().as_millis(),
                            "Sealed state root verified against serial recomputation"
                        );
                    }
                    Err(err) => {
                        tracing::warn!(
                            target: "bsc::builder",
                            block_number = %(self.parent.number + 1),
                            %err,
                            "Sealed state root verification could not run"
                        );
                    }
                }
            }

            (root, updates)
        } else {
            // Fix #1: bound the synchronous state-root fallback by the slot deadline.
            //
            // When the sparse-trie precomputed root is unavailable (recv_timeout expired, or no
            // handle was spawned) we land here on the synchronous full-trie walk
            // `state_root_with_updates`. Under a deep miner overlay this walk takes ~700ms — far
            // past the block period. Running it anyway is doubly harmful: it produces a candidate
            // the miner has already given up waiting for, and it pins a CPU core ~700ms into the
            // next slot, shrinking the next block's build budget and cascading further
            // empty/low-gas blocks. If we are already at/over the state-root deadline
            // (`end_mining_timestamp_ms - STATE_ROOT_WAIT_MARGIN_MS`), abort this candidate so the
            // miner ships the best already-completed candidate on time instead of over-running.
            // The abort only makes sense as a guard for the sparse-trie fast path: it prevents a
            // slow synchronous fallback from overrunning the slot after the fast path timed out.
            // When the sparse-trie spawner is disabled (v2.4.1 migration), synchronous is the
            // only path — aborting would stop all block production — so we skip the abort and
            // always complete the walk.
            if let Some(deadline_ms) =
                self.ctx.state_root_deadline_ms.filter(|_| crate::shared::is_sparse_trie_state_root_enabled())
            {
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                if now_ms >= deadline_ms {
                    metrics::counter!("bsc_builder_sync_root_deadline_abort_total").increment(1);
                    tracing::warn!(
                        target: "bsc::builder",
                        parent_hash = %self.parent.hash(),
                        block_number = %(self.parent.number + 1),
                        now_ms,
                        deadline_ms,
                        "Synchronous state-root would miss the slot deadline; aborting candidate to avoid slot over-run"
                    );
                    return Err(BlockExecutionError::msg(format!(
                        "synchronous state-root aborted: past slot deadline (now_ms={now_ms} >= deadline_ms={deadline_ms})"
                    )));
                }
            }
            state.state_root_with_updates(hashed_state.clone()).map_err(BlockExecutionError::other)?
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
        Ok(BlockBuilderOutcome {
            execution_result: result,
            hashed_state,
            trie_updates,
            block,
            block_access_list: None,
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
