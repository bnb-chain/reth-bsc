use crate::{BscPrimitives, hardforks::BscHardforks, node::evm::{assembler::{BscBlockAssembler, BscBlockAssemblerInput}, config::{BscBlockExecutionCtx, BscBlockExecutorFactory, BscExecutionSharedCtx}, executor::BscBlockExecutor, factory::BscEvmFactory, pre_execution::{TURN_LENGTH_CACHE, VALIDATOR_CACHE}}};
use reth_evm::execute::{BlockBuilder, BlockBuilderOutcome, BlockExecutionError, ExecutorTx};
use alloy_evm::eth::receipt_builder::ReceiptBuilder;
use reth_primitives_traits::{HeaderTy, NodePrimitives, Recovered, RecoveredBlock, SealedHeader, SignerRecoverable, TxTy};
use reth_provider::{
    providers::ConsistentDbView, BlockNumReader, BlockReader, DatabaseProviderFactory,
    HeaderProvider, StateProvider,
};
use reth_provider::DBProvider;
use revm::database::{State, states::bundle_state::BundleRetention};
use alloy_evm::{Evm, block::BlockExecutor};
use reth_chainspec::{EthChainSpec, EthereumHardforks, Hardforks};
use alloy_primitives::{map::B256Set, B256};
use reth_trie::{
    hashed_cursor::{HashedCursor, HashedCursorFactory, HashedPostStateCursorFactory},
    proof::{Proof, ProofTrieNodeProviderFactory},
    trie_cursor::InMemoryTrieCursorFactory,
    MultiProofTargets, Nibbles, TrieInput,
};
use reth_trie_parallel::root::ParallelStateRoot;
use reth_trie_db::{DatabaseHashedCursorFactory, DatabaseTrieCursorFactory};
use reth_trie_sparse::{
    provider::TrieNodeProviderFactory, SerialSparseTrie, SparseStateTrie, SparseTrie,
    SparseTrieInterface,
};
use reth_trie_sparse_parallel::ParallelSparseTrie;
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

        let (state_root, trie_updates, state_root_mode) = {
            // Try sparse state root (multiproof + sparse trie), then fall back to ParallelStateRoot,
            // then fall back to serial `state_root_with_updates`.
            //
            // NOTE: When DB is behind the canonical head, we require full overlay coverage and
            // per-block trie updates to construct a correct DB+overlay view.
            let attempt_sparse = (|| -> Result<(_, _, u64, usize, usize, usize), BlockExecutionError> {
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

                // Build a base DB+overlay trie input that represents the *parent* state.
                // This is used to generate proofs against the correct view.
                let mut base_input = TrieInput::default();
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
                        base_input.append_cached_ref(nodes, &entry.hashed_state);
                    }
                }

                // Build in-memory overlays for proof generation.
                let nodes_sorted = std::sync::Arc::new(base_input.nodes.clone().into_sorted());
                let state_sorted = std::sync::Arc::new(base_input.state.clone().into_sorted());

                // We generate multiproof against DB + (overlay up to parent).
                let provider_ro = consistent_view.provider_ro().map_err(BlockExecutionError::other)?;
                let trie_cursor_factory = InMemoryTrieCursorFactory::new(
                    DatabaseTrieCursorFactory::new(provider_ro.tx_ref()),
                    &nodes_sorted,
                );
                let hashed_cursor_factory = HashedPostStateCursorFactory::new(
                    DatabaseHashedCursorFactory::new(provider_ro.tx_ref()),
                    &state_sorted,
                );

                // Build proof targets, expanding wiped storage to include all existing slots from
                // the base view (DB + overlay up to parent).
                let mut proof_targets: MultiProofTargets = hashed_state.multi_proof_targets();
                let mut wiped_storage_slots = 0usize;
                for (hashed_address, storage) in &hashed_state.storages {
                    if !storage.wiped {
                        continue;
                    }
                    let mut slots: B256Set =
                        proof_targets.get(hashed_address).cloned().unwrap_or_default();
                    let mut storage_cursor = hashed_cursor_factory
                        .hashed_storage_cursor(*hashed_address)
                        .map_err(|e| BlockExecutionError::other(std::io::Error::other(e)))?;
                    let mut current_entry = storage_cursor
                        .seek(B256::ZERO)
                        .map_err(|e| BlockExecutionError::other(std::io::Error::other(e)))?;
                    while let Some((hashed_slot, _)) = current_entry {
                        wiped_storage_slots += 1;
                        slots.insert(hashed_slot);
                        current_entry = storage_cursor
                            .next()
                            .map_err(|e| BlockExecutionError::other(std::io::Error::other(e)))?;
                    }
                    proof_targets.insert(*hashed_address, slots);
                }

                let prefix_sets = hashed_state.construct_prefix_sets();
                let multiproof = Proof::new(trie_cursor_factory.clone(), hashed_cursor_factory.clone())
                    .with_prefix_sets_mut(prefix_sets.clone())
                    .with_branch_node_masks(true)
                    .multiproof(proof_targets)
                    .map_err(|e| BlockExecutionError::other(std::io::Error::other(e)))?;

                // Use sparse trie for state root, with parallel sparse accounts trie.
                let mut sparse = SparseStateTrie::<ParallelSparseTrie, SerialSparseTrie>::new()
                    .with_updates(true);
                sparse
                    .reveal_multiproof(multiproof)
                    .map_err(|e| BlockExecutionError::other(std::io::Error::other(format!("{e:?}"))))?;

                // Provider factory for on-demand reveals during sparse updates.
                let blinded_provider_factory = ProofTrieNodeProviderFactory::new(
                    trie_cursor_factory,
                    hashed_cursor_factory,
                    std::sync::Arc::new(prefix_sets),
                );

                // Apply storage changes first so account updates can compute storage roots.
                for (hashed_address, storage) in &hashed_state.storages {
                    // storage trie must have been revealed by multiproof; otherwise treat as
                    // insufficient input and fall back.
                    let Some(storage_trie) = sparse.storage_trie_mut(hashed_address) else {
                        return Err(BlockExecutionError::other(std::io::Error::other(format!(
                            "sparse storage trie not revealed for account {hashed_address:?}"
                        ))));
                    };

                    if storage.wiped {
                        storage_trie.wipe();
                    }

                    // Defer removals until after updates (same rationale as engine-tree sparse task).
                    let mut removed_slots: Vec<Nibbles> = Vec::new();
                    for (slot, value) in &storage.storage {
                        let slot_nibbles = Nibbles::unpack(slot);
                        if value.is_zero() {
                            removed_slots.push(slot_nibbles);
                            continue;
                        }
                        storage_trie
                            .update_leaf(
                                slot_nibbles,
                                alloy_rlp::encode_fixed_size(value).to_vec(),
                                blinded_provider_factory.storage_node_provider(*hashed_address),
                            )
                            .map_err(|e| BlockExecutionError::other(std::io::Error::other(format!("{e:?}"))))?;
                    }
                    for slot_nibbles in removed_slots {
                        storage_trie
                            .remove_leaf(
                                &slot_nibbles,
                                blinded_provider_factory.storage_node_provider(*hashed_address),
                            )
                            .map_err(|e| BlockExecutionError::other(std::io::Error::other(format!("{e:?}"))))?;
                    }
                    storage_trie.root();
                }

                // Apply account changes.
                for (hashed_address, maybe_account) in &hashed_state.accounts {
                    let nibbles = Nibbles::unpack(hashed_address);
                    match maybe_account {
                        Some(account) => {
                            let keep = sparse
                                .update_account(*hashed_address, account.clone(), &blinded_provider_factory)
                                .map_err(|e| {
                                    BlockExecutionError::other(std::io::Error::other(format!("{e:?}")))
                                })?;
                            if !keep {
                                sparse
                                    .remove_account_leaf(&nibbles, &blinded_provider_factory)
                                    .map_err(|e| {
                                        BlockExecutionError::other(std::io::Error::other(format!("{e:?}")))
                                    })?;
                            }
                        }
                        None => {
                            // Ensure storage trie deletion is reflected in trie updates, even if no
                            // explicit storage update was emitted for this account.
                            if sparse.storage_trie_ref(hashed_address).is_none() {
                                let mut wiped =
                                    SparseTrie::Revealed(Box::new(SerialSparseTrie::default().with_updates(true)));
                                wiped
                                    .wipe()
                                    .map_err(|e| BlockExecutionError::other(std::io::Error::other(format!("{e:?}"))))?;
                                sparse.insert_storage_trie(*hashed_address, wiped);
                            } else {
                                let trie = sparse
                                    .storage_trie_mut(hashed_address)
                                    .expect("checked above");
                                trie.wipe();
                            }

                            sparse
                                .remove_account_leaf(&nibbles, &blinded_provider_factory)
                                .map_err(|e| {
                                    BlockExecutionError::other(std::io::Error::other(format!("{e:?}")))
                                })?;
                        }
                    }
                }

                let (state_root, trie_updates) = sparse
                    .root_with_updates(blinded_provider_factory)
                    .map_err(|e| BlockExecutionError::other(std::io::Error::other(format!("{e:?}"))))?;

                Ok((state_root, trie_updates, db_last, overlay_blocks, nodes_sorted.account_nodes.len(), wiped_storage_slots))
            })();

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

            match attempt_sparse {
                Ok((state_root, trie_updates, db_last, overlay_blocks, account_nodes, wiped_storage_slots)) => {
                    tracing::debug!(
                        target: "bsc::builder",
                        parent_num = self.parent.number,
                        parent_hash = ?self.parent.hash(),
                        db_last,
                        overlay_blocks,
                        account_nodes,
                        wiped_storage_slots,
                        "Sparse state root succeeded"
                    );
                    (state_root, trie_updates, "sparse")
                }
                Err(sparse_err) => {
                    tracing::debug!(
                        target: "bsc::builder",
                        parent_num = self.parent.number,
                        parent_hash = ?self.parent.hash(),
                        %sparse_err,
                        "Sparse state root unavailable, trying parallel state root"
                    );
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
                            (state_root, trie_updates, "parallel")
                        }
                        Err(par_err) => {
                            tracing::debug!(
                                target: "bsc::builder",
                                parent_num = self.parent.number,
                                parent_hash = ?self.parent.hash(),
                                %par_err,
                                "Parallel state root unavailable, falling back to serial"
                            );
                            let (state_root, trie_updates) = state
                                .state_root_with_updates(hashed_state.clone())
                                .map_err(BlockExecutionError::other)?;
                            (state_root, trie_updates, "serial")
                        }
                    }
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
            state_root_mode = state_root_mode,
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
