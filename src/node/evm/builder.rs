use crate::{
    hardforks::BscHardforks,
    node::evm::{
        assembler::{BscBlockAssembler, BscBlockAssemblerInput},
        config::{BscBlockExecutionCtx, BscBlockExecutorFactory, BscExecutionSharedCtx},
        executor::BscBlockExecutor,
        factory::BscEvmFactory,
        pre_execution::{TURN_LENGTH_CACHE, VALIDATOR_CACHE},
    },
    node::BscNode,
    BscPrimitives,
};
use alloy_consensus::BlockHeader as _;
use alloy_evm::eth::receipt_builder::ReceiptBuilder;
use alloy_evm::{block::BlockExecutor, Evm};
use alloy_primitives::BlockHash;
use reth::builder::NodeAdapter;
use reth_chainspec::{EthChainSpec, EthereumHardforks, Hardforks};
use reth_engine_primitives::BSCEngineMessageError;
use reth_engine_tree::engine::EngineApiRequest;
use reth_engine_tree::tree::CustomRequestMessage;
use reth_evm::execute::{
    BlockBuilder, BlockBuilderOutcome, BlockBuilderOutcomeWithDiffLayer, BlockExecutionError,
    ExecutorTx,
};
use reth_node_builder::rpc::EngineApiTx;
use reth_primitives_traits::{
    HeaderTy, NodePrimitives, Recovered, RecoveredBlock, SealedHeader, SignerRecoverable, TxTy,
};
use reth_provider::StateProvider;
use reth_trie_common::updates::TrieUpdates;
use revm::database::{states::bundle_state::BundleRetention, State};
use rust_eth_triedb::get_global_triedb;
use rust_eth_triedb_common::DiffLayers;
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
        self,
        state: impl StateProvider,
    ) -> Result<BlockBuilderOutcome<BscPrimitives>, BlockExecutionError> {
        Ok(self.finish_with_difflayer(state)?.inner)
    }

    fn finish_with_difflayer(
        mut self,
        state: impl StateProvider,
    ) -> Result<BlockBuilderOutcomeWithDiffLayer<BscPrimitives>, BlockExecutionError> {
        let finish_start = std::time::Instant::now();
        let (evm, result) = self.executor.finish()?;
        let (db, evm_env) = evm.finish();

        let assembled_system_txs = self.shared_ctx.inner.borrow().assembled_system_txs.clone();
        // merge all transitions into bundle state
        db.merge_transitions(BundleRetention::Reverts);

        // calculate the state root using triedb
        let state_root_start = std::time::Instant::now();
        let hashed_state = state.hashed_post_state(&db.bundle_state);

        // Use triedb to calculate state root
        let (state_root, trie_updates, produced_difflayer) = if rust_eth_triedb::triedb_manager::is_triedb_active() {
            let mut triedb = get_global_triedb();
            let trie_hashed_state = hashed_state.to_triedb_hashed_post_state();

            // Miner-side: feed one-shot targets derived from the final triedb hashed post state,
            // then finish the prefetcher.
            let prefetch_state = self.ctx.triedb_prefetcher.take().and_then(|p| {
                use alloy_primitives::map::B256Set;
                use reth_trie::MultiProofTargets;

                let build_started = std::time::Instant::now();
                let mut targets = MultiProofTargets::with_capacity(trie_hashed_state.states.len());
                let mut storage_accounts: usize = 0;
                let mut storage_slots: usize = 0;

                for (hashed_address, slots) in trie_hashed_state.storage_states.iter() {
                    let mut storage_set =
                        B256Set::with_capacity_and_hasher(slots.len(), Default::default());
                    for (hashed_slot, _) in slots.iter() {
                        storage_set.insert(*hashed_slot);
                    }
                    storage_slots += storage_set.len();
                    if !storage_set.is_empty() {
                        storage_accounts += 1;
                    }
                    targets.insert(*hashed_address, storage_set);
                }

                for hashed_address in trie_hashed_state.states.keys() {
                    targets.entry(*hashed_address).or_insert_with(B256Set::default);
                }

                tracing::debug!(
                    target: "bsc::builder",
                    one_shot_build_ms = build_started.elapsed().as_millis(),
                    accounts = trie_hashed_state.states.len(),
                    storage_accounts,
                    storage_slots,
                    "Submitting one-shot triedb prefetch targets"
                );

                p.prefetch_targets(targets);
                let finish_started = std::time::Instant::now();
                let res = p.finish();
                tracing::debug!(
                    target: "bsc::builder",
                    prefetch_finish_ms = finish_started.elapsed().as_millis(),
                    had_prefetch_state = res.is_some(),
                    "Finished triedb prefetcher"
                );
                res
            });
            // let had_prefetch_state = prefetch_state.is_some();
            let parent_state_root = (**self.parent).state_root();
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

            // Diagnostic: on a background thread, recompute the root without prefetch_state and
            // compare it with the prefetched root. This helps validate that prefetching only
            // affects performance, not correctness.
            //
            // Note: `get_global_triedb()` returns a cloned/owned triedb instance, so this doesn't
            // contend with the main thread's triedb.
            // if had_prefetch_state {
            //     let parent_hash = self.parent.hash();
            //     let block_number = self.parent.number + 1;
            //     let parent_state_root_diag = parent_state_root;
            //     let difflayers_diag = self.ctx.parent_difflayers.clone();
            //     let trie_hashed_state_diag = trie_hashed_state.clone();
            //     let root_with_prefetch = new_root;
            //     let triedb_calc_with_prefetch_ms = triedb_calc_with_prefetch_ms;

            //     if let Ok(handle) = tokio::runtime::Handle::try_current() {
            //         let _ = handle.spawn_blocking(move || {
            //         let diag_started = std::time::Instant::now();
            //         let mut triedb = get_global_triedb();
            //         match triedb.intermediate_hashed_post_state(
            //             parent_state_root_diag,
            //             difflayers_diag.as_ref(),
            //             &trie_hashed_state_diag,
            //             None,
            //         ) {
            //             Ok(root_no_prefetch) => {
            //                 let diag_ms = diag_started.elapsed().as_millis();
            //                 if root_no_prefetch != root_with_prefetch {
            //                     tracing::warn!(
            //                         target: "bsc::builder",
            //                         parent_hash = %parent_hash,
            //                         block_number = %block_number,
            //                         parent_state_root = %parent_state_root_diag,
            //                         got = %root_with_prefetch,
            //                         got_no_prefetch = %root_no_prefetch,
            //                         has_parent_difflayers = difflayers_diag.is_some(),
            //                         calc_with_prefetch_ms = triedb_calc_with_prefetch_ms,
            //                         recompute_no_prefetch_ms = diag_ms,
            //                         "triedb root differs when recomputing without prefetch_state"
            //                     );
            //                 } else {
            //                     tracing::debug!(
            //                         target: "bsc::builder",
            //                         parent_hash = %parent_hash,
            //                         block_number = %block_number,
            //                         calc_with_prefetch_ms = triedb_calc_with_prefetch_ms,
            //                         recompute_no_prefetch_ms = diag_ms,
            //                         "triedb recompute without prefetch_state matches"
            //                     );
            //                 }
            //             }
            //             Err(err) => {
            //                 tracing::warn!(
            //                     target: "bsc::builder",
            //                     parent_hash = %parent_hash,
            //                     block_number = %block_number,
            //                     error = ?err,
            //                     calc_with_prefetch_ms = triedb_calc_with_prefetch_ms,
            //                     diag_ms = diag_started.elapsed().as_millis(),
            //                     "failed to recompute triedb root without prefetch_state"
            //                 );
            //             }
            //         }
            //         });
            //     } else {
            //         tracing::debug!(
            //             target: "bsc::builder",
            //             parent_hash = %parent_hash,
            //             block_number = %block_number,
            //             "tokio runtime not available; skipping triedb recompute without prefetch_state"
            //         );
            //     }
            // }

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

        let (transactions, senders): (Vec<_>, Vec<_>) =
            self.transactions.into_iter().map(|tx| tx.into_parts()).unzip();

        let bsc_input: BscBlockAssemblerInput<'_, '_, BscBlockExecutorFactory> =
            BscBlockAssemblerInput {
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
            VALIDATOR_CACHE
                .lock()
                .unwrap()
                .insert(block.header.hash_slow(), (validators, vote_addresses));
        }
        if let Some(turn_length) = self.shared_ctx.inner.borrow().turn_length {
            TURN_LENGTH_CACHE.lock().unwrap().insert(block.header.hash_slow(), turn_length);
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

pub async fn request_difflayer(
    engine_api_tx: &EngineApiTx<NodeAdapter<BscNode>>,
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
