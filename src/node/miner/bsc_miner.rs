use crate::node::miner::bid_simulator::{BidRuntime, BidSimulator};
use crate::node::miner::payload::BscBuildArguments;
use crate::{
    chainspec::BscChainSpec,
    consensus::parlia::{provider::SnapshotProvider, Parlia},
    metrics::BscConsensusMetrics,
    node::{
        engine::BscBuiltPayload,
        evm::config::BscEvmConfig,
        miner::{
            config::MiningConfig,
            payload::{BscPayloadBuilder, BscPayloadJob, BscPayloadJobHandle},
            signer::init_global_signer_from_k256,
            util::prepare_new_attributes,
        },
        network::{
            block_import::service::{IncomingBlock, IncomingMinedBlock},
            BscNewBlock,
        },
    },
    shared::{
        get_block_import_mined_sender, get_block_import_sender, get_local_peer_id_or_default,
    },
};
use alloy_consensus::BlockHeader;
use alloy_primitives::{Address, Sealable, U128};
use k256::ecdsa::SigningKey;
use reth::transaction_pool::PoolTransaction;
use reth::transaction_pool::TransactionPool;
use reth_basic_payload_builder::{PayloadConfig, PrecachedState};
use reth_chainspec::EthChainSpec;
use reth_ethereum_payload_builder::EthereumBuilderConfig;
use reth_network::message::NewBlockMessage;
use reth_payload_primitives::BuiltPayload;
use reth_primitives_traits::SealedHeader;
use reth_ethereum_primitives::TransactionSigned;
use reth_primitives_traits::BlockBody;
use reth_provider::{
    BlockNumReader, CanonStateNotification, CanonStateSubscriptions, HeaderProvider,
};
use reth_revm::cancelled::ManualCancel;
use reth_tasks::TaskExecutor;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_stream::StreamExt;
use tracing::{debug, error, info, trace, warn};

#[derive(Clone, Debug)]
pub struct MiningContext {
    pub header: Option<alloy_consensus::Header>, // tmp header for payload building.
    pub parent_header: reth_primitives_traits::SealedHeader,
    pub parent_snapshot: Arc<crate::consensus::parlia::snapshot::Snapshot>,
    pub is_inturn: bool,
    pub cached_reads: Option<reth_revm::cached::CachedReads>,
    /// Block timestamp in milliseconds, computed via `block_time_for_ramanujan_fork`.
    pub block_timestamp_ms: u64,
    /// End timestamp of the mining job (UNIX epoch ms), computed via `delay_for_ramanujan_fork`.
    pub end_mining_timestamp_ms: u128,
}

#[derive(Clone)]
pub struct SubmitContext {
    pub mining_ctx: MiningContext,
    pub payload: BscBuiltPayload,
    pub cancel: ManualCancel,
}

/// NewWorkWorker responsible for listening to canonical state changes and triggering mining.
pub struct NewWorkWorker<Provider> {
    validator_address: Address,
    provider: Provider,
    snapshot_provider: Arc<dyn SnapshotProvider + Send + Sync>,
    mining_queue_tx: mpsc::UnboundedSender<MiningContext>,
    consensus: Arc<Parlia<BscChainSpec>>,
    pre_cached: Option<PrecachedState>,
    /// Hash of the tip block for which mining was last triggered, used to suppress
    /// periodic-tick retries when no new canonical head has arrived.
    last_triggered_tip: Option<alloy_primitives::B256>,
}

/// Skip mining when isolated (no peers or network handle not yet installed) to avoid
/// producing a small fork-chain that peers do not know about after reconnect.
fn is_network_ready_to_mine(tip_number: u64) -> bool {
    let Some(network) = crate::shared::get_network_handle() else {
        debug!(
            target: "bsc::miner",
            tip_number,
            "Skip mining due to network handle not yet available"
        );
        return false;
    };

    use reth_network::PeersInfo;
    if network.num_connected_peers() == 0 {
        debug!(
            target: "bsc::miner",
            tip_number,
            "Skip mining due to no peers connected"
        );
        return false;
    }

    true
}

impl<Provider> NewWorkWorker<Provider>
where
    Provider: HeaderProvider<Header = alloy_consensus::Header>
        + BlockNumReader
        + reth_provider::StateProviderFactory
        + CanonStateSubscriptions
        + reth_provider::NodePrimitivesProvider
        + Clone
        + Send
        + Sync
        + 'static,
{
    pub fn new(
        validator_address: Address,
        provider: Provider,
        snapshot_provider: Arc<dyn SnapshotProvider + Send + Sync>,
        mining_queue_tx: mpsc::UnboundedSender<MiningContext>,
        consensus: Arc<Parlia<BscChainSpec>>,
    ) -> Self {
        Self {
            validator_address,
            provider,
            snapshot_provider,
            mining_queue_tx,
            consensus,
            pre_cached: None,
            last_triggered_tip: None,
        }
    }

    pub async fn run(mut self) {
        info!("Succeed to spawn new work worker, address: {}", self.validator_address);

        let mut notifications = self.provider.canonical_state_stream();
        debug!(target: "bsc::miner", "Subscribed to canonical_state_stream");

        // Don't block the canonical notifications loop on potentially slow startup checks (DB
        // reads / snapshot locks). If this blocks, we can miss the first few canonical commits and
        // never emit the per-commit "Try new work" log.
        let startup_tip = self.get_tip_header_at_startup();
        if let Some(ref tip_header) = startup_tip {
            debug!("Try new work at startup, tip_block={}", tip_header.number());
            let validator_address = self.validator_address;
            let provider = self.provider.clone();
            let snapshot_provider = Arc::clone(&self.snapshot_provider);
            let mining_queue_tx = self.mining_queue_tx.clone();
            let consensus = Arc::clone(&self.consensus);
            let tip_header = tip_header.clone();
            tokio::spawn(async move {
                let worker = NewWorkWorker::new(
                    validator_address,
                    provider,
                    snapshot_provider,
                    mining_queue_tx,
                    consensus,
                );
                worker.try_new_work(&tip_header).await;
            });
        }

        // Periodic ticker: retries try_new_work when no canonical events arrive.
        // This is essential for deadlock recovery when all validators restart simultaneously
        // and the sync gate times out — without this ticker, try_new_work would never be
        // re-invoked after the startup attempt.
        let mut periodic_tick =
            tokio::time::interval(Duration::from_secs(3));
        periodic_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Burn the first immediate tick so we don't double-fire with the startup spawn above.
        periodic_tick.tick().await;

        loop {
            tokio::select! {
            biased; // prefer canonical events over the ticker
            notification = notifications.next() => {
            let Some(event) = notification else {
                warn!("Canonical state notification stream ended, exiting...");
                break;
            };
            let event = event; // rebind to avoid move issues
                    let committed = event.committed();
                    let tip = committed.tip();
                    let is_reorg = matches!(event, CanonStateNotification::Reorg { .. });
                    debug!(
                        target: "bsc::miner",
                        tip_block = committed.tip().number(),
                        hash = ?committed.tip().hash(),
                        parent_hash = ?committed.tip().parent_hash(),
                        miner = ?committed.tip().beneficiary(),
                        diff = %committed.tip().difficulty(),
                        committed_blocks = committed.len(),
                        is_reorg,
                        "Try new work"
                    );

                    // If this is a reorg event, validate it using bsc fork choice rules
                    if let CanonStateNotification::Reorg { old, new } = &event {
                        match self.validate_reorg(old, new).await {
                            Ok(true) => {
                                // Reorg is valid, proceed with mining
                                debug!(
                                    target: "bsc::miner",
                                    old_tip_number = old.tip().number(),
                                    new_tip_number = new.tip().number(),
                                    old_tip_hash = ?old.tip().hash(),
                                    new_tip_hash = ?new.tip().hash(),
                                    "Reorg validated by fork choice rules, proceeding with mining"
                                );
                            }
                            Ok(false) => {
                                // Reorg is invalid according to fork choice rules, skip mining
                                warn!(
                                    target: "bsc::miner",
                                    old_tip_number = old.tip().number(),
                                    new_tip_number = new.tip().number(),
                                    old_tip_hash = ?old.tip().hash(),
                                    new_tip_hash = ?new.tip().hash(),
                                    "Reorg rejected by fork choice rules, skipping mining on this tip"
                                );
                                continue;
                            }
                            Err(e) => {
                                // Validation failed (engine not initialized or headers unavailable)
                                // Log the error but proceed with mining to maintain availability
                                warn!(
                                    target: "bsc::miner",
                                    old_tip_number = old.tip().number(),
                                    new_tip_number = new.tip().number(),
                                    old_tip_hash = ?old.tip().hash(),
                                    new_tip_hash = ?new.tip().hash(),
                                    error = %e,
                                    "Failed to validate reorg, proceeding with mining"
                                );
                            }
                        }
                    }

                    let tip_header = tip.clone_sealed_header();

                    // Produce and broadcast a local vote for this new canonical head, if eligible
                    if let Some(sp) = crate::shared::get_snapshot_provider() {
                        let sp = Arc::clone(sp);
                        let spec = self.consensus.spec.clone();
                        match self.provider.header(tip_header.hash()) {
                            Ok(Some(h)) => {
                                tracing::debug!(target: "bsc::vote", "Succeed to get header for tip block, validator: {}, tip: {}", self.validator_address, tip_header.number());
                                tokio::spawn(async move {
                                    crate::node::vote_producer::maybe_produce_and_broadcast_for_head(
                                        spec,
                                        sp.as_ref(),
                                        &h,
                                    );
                                });
                            }
                            Ok(None) => {
                                if let Some(h) = crate::node::evm::util::get_header_by_hash_from_cache(&tip_header.hash()) {
                                    tracing::debug!(target: "bsc::vote", "Succeed to get header for tip block from cache, validator: {}, tip: {}", self.validator_address, tip_header.number());
                                    tokio::spawn(async move {
                                        crate::node::vote_producer::maybe_produce_and_broadcast_for_head(
                                            spec,
                                            sp.as_ref(),
                                            &h,
                                        );
                                    });
                                } else {
                                    tracing::error!(target: "bsc::vote", "Failed to get header for tip block, validator: {}, tip: {}", self.validator_address, tip_header.number());
                                }
                            }
                            Err(e) => {
                                tracing::error!(target: "bsc::vote", "Failed to get header for tip block, validator: {}, tip: {}, due to {}", self.validator_address, tip_header.number(), e);
                            }
                        }
                    }

                    self.cache_for_next(&committed);

                    self.last_triggered_tip = Some(tip_header.hash());
                    self.try_new_work(&tip_header).await;
                }
            _ = periodic_tick.tick() => {
                // Periodic retry: fires when no canonical events have arrived recently.
                // Critical for breaking the all-validators-restart deadlock: once the
                // sync gate timeout elapses, this ticker drives try_new_work to actually
                // attempt mining.
                if let Some(tip) = self.get_tip_header_at_startup() {
                    if self.last_triggered_tip == Some(tip.hash()) {
                        // A canonical event already triggered mining for this tip.
                        // But if no new canonical events are arriving (all-validators-restart
                        // deadlock), we must keep retrying via the ticker. Clear the guard
                        // so the next tick fires even if the tip hasn't changed.
                        self.last_triggered_tip = None;
                        continue;
                    }
                    debug!(
                        target: "bsc::miner",
                        tip_number = tip.number(),
                        "Periodic sync-gate retry"
                    );
                    self.last_triggered_tip = Some(tip.hash());
                    self.try_new_work(&tip).await;
                    // Clear so the next tick retries if try_new_work was skipped (e.g.
                    // backfill still active). If a canonical event fires before the next
                    // tick it will set last_triggered_tip again, preventing a duplicate.
                    self.last_triggered_tip = None;
                }
            }
            } // end tokio::select!
        }
    }

    /// Validate if a reorg is justified according to BSC fork choice rules.
    ///
    /// # Arguments
    ///
    /// * `old` - The old chain that was reverted
    /// * `new` - The new chain that replaced it
    ///
    /// # Returns
    ///
    /// Returns a `Result<bool, Box<dyn Error>>`:
    /// - `Ok(true)` - Reorg is valid and justified, should proceed with mining
    /// - `Ok(false)` - Reorg is invalid according to fork choice rules, should skip mining
    /// - `Err(error)` - Validation failed (engine not initialized or headers unavailable), error contains reason
    async fn validate_reorg<N>(
        &self,
        old: &Arc<reth::providers::Chain<N>>,
        new: &Arc<reth::providers::Chain<N>>,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>>
    where
        N: reth_primitives_traits::NodePrimitives,
    {
        debug!(
            target: "bsc::miner",
            old_tip_number = old.tip().number(),
            old_tip_hash = ?old.tip().hash(),
            new_tip_number = new.tip().number(),
            new_tip_hash = ?new.tip().hash(),
            "Reorg detected, validating with fork choice rules"
        );

        let forkchoice_engine = crate::shared::get_fork_choice_engine().ok_or_else(
            || -> Box<dyn std::error::Error + Send + Sync> {
                "Fork choice engine not initialized".into()
            },
        )?;

        let old_header = match self.provider.sealed_header_by_hash(old.tip().hash()) {
            Ok(Some(header)) => header,
            Ok(None) => {
                // Old header not found (may have been pruned), accept the reorg as valid
                debug!(
                    target: "bsc::miner",
                    old_tip_hash = ?old.tip().hash(),
                    "Old header not found, accepting reorg as valid"
                );
                return Ok(true);
            }
            Err(e) => {
                return Err(format!("Failed to get old header: {}", e).into());
            }
        };

        let new_header = self
            .provider
            .sealed_header_by_hash(new.tip().hash())
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
                format!("Failed to get new header: {}", e).into()
            })?
            .ok_or_else(|| -> Box<dyn std::error::Error + Send + Sync> {
                format!("New header not found for block hash {:?}", new.tip().hash()).into()
            })?;

        match forkchoice_engine.is_need_reorg(new_header.header(), old_header.header()).await {
            Ok(true) => {
                debug!(
                    target: "bsc::miner",
                    "Reorg validated by fork choice rules (is_need_reorg=true)"
                );
                Ok(true)
            }
            Ok(false) => {
                debug!(
                    target: "bsc::miner",
                    "Reorg rejected by fork choice rules (is_need_reorg=false)"
                );
                Ok(false)
            }
            Err(e) => Err(format!("Fork choice validation error: {}", e).into()),
        }
    }

    fn get_tip_header_at_startup(&self) -> Option<reth_primitives_traits::SealedHeader> {
        let best_number = self.provider.best_block_number().ok()?;
        let tip_header = self.provider.sealed_header(best_number).ok()??;
        Some(tip_header)
    }

    /// Cache state from the current block for building the next block.
    ///
    /// Extracts changed accounts and storage from the execution outcome and stores them
    /// in a cache associated with the tip block hash for faster subsequent block building.
    fn cache_for_next(
        &mut self,
        committed: &Arc<
            reth::providers::Chain<<Provider as reth_provider::NodePrimitivesProvider>::Primitives>,
        >,
    ) {
        // Build pre-cache from execution outcome
        let mut cached = reth_revm::cached::CachedReads::default();
        let new_execution_outcome = committed.execution_outcome();

        for (addr, acc) in new_execution_outcome.bundle_accounts_iter() {
            if let Some(info) = acc.info.clone() {
                // Pre-cache existing accounts and their storage
                // This only includes changed accounts and storage but is better than nothing
                let storage =
                    acc.storage.iter().map(|(key, slot)| (*key, slot.present_value)).collect();
                cached.insert_account(addr, info, storage);
            }
        }

        self.pre_cached = Some(PrecachedState { block: committed.tip().hash(), cached });
    }

    /// Returns the pre-cached reads for the given parent header if it matches the cached state's block.
    fn maybe_pre_cached(
        &self,
        parent: alloy_primitives::B256,
    ) -> Option<reth_revm::cached::CachedReads> {
        self.pre_cached.as_ref().filter(|pc| pc.block == parent).map(|pc| pc.cached.clone())
    }

    async fn try_new_work<H>(&self, tip: &SealedHeader<H>)
    where
        H: alloy_consensus::BlockHeader + Sealable,
    {
        // Check if mining is disabled via miner_stop RPC
        if !crate::shared::is_mining_enabled() {
            debug!("Skip mining: mining is disabled via miner_stop RPC");
            return;
        }

        if !is_network_ready_to_mine(tip.number()) {
            return;
        }

        let parent_header = match self.provider.sealed_header_by_hash(tip.hash()) {
            Ok(Some(header)) => {
                trace!(
                    target: "bsc::miner",
                    tip_number = tip.number(),
                    tip_hash = ?tip.hash(),
                    parent_header_hash = ?header.hash(),
                    "Found parent header for mining"
                );
                header
            }
            Ok(None) => {
                warn!(
                    target: "bsc::miner",
                    tip_number = tip.number(),
                    tip_hash = ?tip.hash(),
                    "Skip to mine new block due to head block header not found"
                );
                return;
            }
            Err(e) => {
                warn!(
                    target: "bsc::miner",
                    tip_number = tip.number(),
                    tip_hash = ?tip.hash(),
                    error = %e,
                    "Skip to mine new block due to error getting header"
                );
                return;
            }
        };

        let parent_snapshot = match self.snapshot_provider.snapshot_by_hash(&tip.hash()) {
            Some(snapshot) => snapshot,
            None => {
                debug!(
                    "Skip to mine new block due to no snapshot available, validator: {}, tip: {}",
                    self.validator_address,
                    tip.number()
                );
                return;
            }
        };

        if !parent_snapshot.validators.contains(&self.validator_address) {
            debug!(
                "Skip to mine new block due to not authorized, validator: {}, tip: {}",
                self.validator_address,
                tip.number()
            );
            return;
        }

        let mut is_inturn = true;
        if !parent_snapshot.is_inturn(self.validator_address) {
            is_inturn = false;
            debug!(
                "Try off-turn mining, validator: {}, next_block: {}",
                self.validator_address,
                tip.number() + 1
            );
        }

        if parent_snapshot.sign_recently(self.validator_address) {
            debug!(
                "Skip to mine new block due to signed recently, validator: {}, tip: {}",
                self.validator_address,
                tip.number()
            );
            return;
        }

        let parent_hash = parent_header.hash();
        let mining_ctx = MiningContext {
            header: None,
            parent_header,
            parent_snapshot: Arc::new(parent_snapshot),
            is_inturn,
            cached_reads: self.maybe_pre_cached(parent_hash),
            block_timestamp_ms: 0,
            end_mining_timestamp_ms: 0,
        };

        debug!("Queuing mining context, next_block: {}", tip.number() + 1);
        if let Err(e) = self.mining_queue_tx.send(mining_ctx) {
            error!("Failed to send mining context to queue due to {}", e);
        }
    }
}

/// MainWorkWorker responsible for processing mining tasks and block building.
/// Built payloads are sent to ResultWorkWorker for submission.
pub struct MainWorkWorker<Pool, Provider> {
    validator_address: Address,
    pool: Pool,
    provider: Provider,
    chain_spec: Arc<crate::chainspec::BscChainSpec>,
    parlia: Arc<crate::consensus::parlia::Parlia<crate::chainspec::BscChainSpec>>,
    mining_queue_rx: mpsc::UnboundedReceiver<MiningContext>,
    payload_tx: mpsc::UnboundedSender<SubmitContext>,
    running_job_handle: Option<BscPayloadJobHandle>,
    payload_job_join_set:
        JoinSet<Result<(), Box<crate::node::miner::payload::BscPayloadJobError>>>,
    simulator: Arc<BidSimulator<Provider, Pool>>, // No outer RwLock, each map has its own lock
    desired_gas_limit: u64,
    desired_min_gas_tip: u128,
}

impl<Pool, Provider> MainWorkWorker<Pool, Provider>
where
    Pool: TransactionPool<Transaction: PoolTransaction<Consensus = TransactionSigned>>
        + Clone
        + 'static,
    Provider: HeaderProvider<Header = alloy_consensus::Header>
        + BlockNumReader
        + reth_provider::StateProviderFactory
        + CanonStateSubscriptions
        + Clone
        + Send
        + Sync
        + 'static,
{
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        validator_address: Address,
        pool: Pool,
        provider: Provider,
        chain_spec: Arc<crate::chainspec::BscChainSpec>,
        parlia: Arc<crate::consensus::parlia::Parlia<crate::chainspec::BscChainSpec>>,
        mining_queue_rx: mpsc::UnboundedReceiver<MiningContext>,
        simulator: Arc<BidSimulator<Provider, Pool>>, // No outer RwLock needed
        payload_tx: mpsc::UnboundedSender<SubmitContext>,
        desired_gas_limit: u64,
        desired_min_gas_tip: u128,
    ) -> Self {
        Self {
            pool,
            provider,
            chain_spec,
            parlia,
            validator_address,
            mining_queue_rx,
            payload_tx,
            running_job_handle: None,
            simulator,
            payload_job_join_set: JoinSet::new(),
            desired_gas_limit,
            desired_min_gas_tip,
        }
    }

    pub async fn run(mut self) {
        info!("Succeed to spawn main work worker, address: {}", self.validator_address);

        loop {
            tokio::select! {
                mining_ctx = self.mining_queue_rx.recv() => {
                    match mining_ctx {
                        Some(ctx) => {
                            let next_block = ctx.parent_header.number() + 1;
                            let parent_hash = ctx.parent_header.hash();
                            if !self.recheck_mining_ctx(&ctx) {
                                continue;
                            }
                            match self.try_mine_block(ctx).await {
                                Ok(()) => {
                                    debug!("Succeed to try mine block, next_block: {}, parent_hash: 0x{:x}", next_block, parent_hash);
                                }
                                Err(e) => {
                                    error!("Failed to mine block due to {}, next_block: {}, parent_hash: 0x{:x}", e, next_block, parent_hash);
                                }
                            }
                        }
                        None => {
                            warn!("Mining queue closed, exiting main work worker");
                            break;
                        }
                    }
                }

                _ = tokio::time::sleep(std::time::Duration::from_millis(200)) => {
                    self.check_payload_job_results().await;
                }
            }
        }

        warn!("Mining worker stopped");
    }

    /// Check if the mining context is still valid (parent is still the canonical head).
    ///
    /// This is a best-effort check to avoid wasting resources on stale mining contexts.
    /// It does NOT guarantee complete accuracy due to:
    /// - Race conditions: The canonical head may change between this check and actual mining
    /// - Time window: Multiple chain events may occur in quick succession
    ///
    /// Purpose: Skip obviously stale contexts to reduce unnecessary work, not to provide
    /// strict correctness guarantees.
    fn recheck_mining_ctx(&self, ctx: &MiningContext) -> bool {
        let parent_hash = ctx.parent_header.hash();
        let current_best = match self.provider.best_block_number() {
            Ok(num) => num,
            Err(_) => return true, // On error, proceed to avoid blocking mining
        };

        if ctx.parent_header.number() != current_best {
            debug!(
                target: "bsc::miner",
                ctx_parent_number = ctx.parent_header.number(),
                ctx_parent_hash = ?parent_hash,
                current_best_number = current_best,
                "Discarding stale mining context due to chain head number changed"
            );
            return false;
        }

        if let Ok(Some(canonical_header)) = self.provider.sealed_header(current_best) {
            if canonical_header.hash() != parent_hash {
                debug!(
                    target: "bsc::miner",
                    ctx_parent_number = ctx.parent_header.number(),
                    ctx_parent_hash = ?parent_hash,
                    canonical_hash = ?canonical_header.hash(),
                    "Discarding stale mining context due to same-height reorg"
                );
                return false;
            }
        }

        true
    }

    async fn try_mine_block(
        &mut self,
        mut mining_ctx: MiningContext,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if let Some(handle) = self.running_job_handle.take() {
            handle.abort();
        }

        let parent_header = mining_ctx.parent_header.clone();
        let block_number = parent_header.number() + 1;
        let attributes = prepare_new_attributes(
            &mut mining_ctx,
            self.parlia.clone(),
            &parent_header,
            self.validator_address,
        );

        // Read dynamic config from shared state (updated by miner_* RPC), fall back to init values
        let gas_limit = crate::shared::get_miner_gas_limit().unwrap_or(self.desired_gas_limit);

        let evm_config = BscEvmConfig::new(self.chain_spec.clone());
        let payload_builder = BscPayloadBuilder::new(
            self.provider.clone(),
            self.pool.clone(),
            evm_config,
            EthereumBuilderConfig::new().with_gas_limit(gas_limit),
            self.chain_spec.clone(),
            self.parlia.clone(),
            mining_ctx.clone(),
        );
        let build_args = BscBuildArguments {
            cached_reads: mining_ctx.cached_reads.clone().unwrap_or_default(),
            // `% 1000` matches `set_millisecond_part_of_timestamp`, which seals the same
            // remainder into the header's mix_hash at finalize time.
            milli_remainder: mining_ctx.block_timestamp_ms % 1000,
            config: PayloadConfig::new(Arc::new(mining_ctx.parent_header.clone()), attributes, alloy_rpc_types_engine::PayloadId::new([0u8; 8])),
            cancel: ManualCancel::default(),
            trace_id: crate::node::miner::payload::generate_trace_id(),
            min_gas_tip: crate::shared::get_miner_gas_tip()
                .map(|v| v as u128)
                .unwrap_or(self.desired_min_gas_tip),
            // Filled in by BscPayloadJob::start when sparse-trie state-root is enabled
            // and the engine has registered a spawner. Falls back to legacy path when None.
            state_root_precomputed: std::sync::Arc::new(std::sync::Mutex::new(None)),
            trie_handle: std::sync::Arc::new(std::sync::Mutex::new(None)),
            // R2: bound the sparse-trie state_root() wait to this slot so an in-turn
            // block never blocks past its deadline (then falls back to sync root).
            state_root_deadline_ms: Some(
                (mining_ctx.end_mining_timestamp_ms as u64)
                    .saturating_sub(crate::node::miner::payload::STATE_ROOT_WAIT_MARGIN_MS),
            ),
        };

        let parent_hash = mining_ctx.parent_header.hash();
        let (payload_job, job_handle) = BscPayloadJob::new(
            self.parlia.clone(),
            mining_ctx,
            payload_builder,
            build_args,
            self.simulator.clone(),
            self.payload_tx.clone(),
        );

        let start_time = std::time::Instant::now();
        self.running_job_handle = Some(job_handle);
        self.payload_job_join_set.spawn(async move { payload_job.start().await });
        debug!("Succeed to async start payload job, cost_time: {:?}, block_number: {}, parent_hash: 0x{:x}",
            start_time.elapsed(), block_number, parent_hash);

        Ok(())
    }

    /// Check and print completed payload job tasks results
    pub async fn check_payload_job_results(&mut self) {
        while let Some(result) = self.payload_job_join_set.try_join_next() {
            match result {
                Ok(Ok(())) => {
                    trace!("Succeed to execute payload job");
                }
                Ok(Err(e)) => {
                    trace!("Failed to execute payload job due to {}", e);
                }
                Err(join_err) => {
                    error!("Failed to execute payload job due to task panicked or was cancelled, join_err: {}", join_err);
                }
            }
        }
    }
}

/// Worker responsible for submitting the sealed block to engine-tree and other peers.
///
/// Delay scheduling (out-of-turn back-off) is handled upstream in [`BscPayloadJob`], so
/// every [`SubmitContext`] that arrives here is already ready to be submitted immediately.
pub struct ResultWorkWorker<Provider> {
    /// Validator address
    validator_address: Address,
    /// Provider for blockchain data
    provider: Provider,
    /// Receiver for payloads that are ready to submit (delay already applied by payload job)
    payload_rx: mpsc::UnboundedReceiver<SubmitContext>,
    /// Consensus metrics for tracking double signs and block turn stats
    consensus_metrics: BscConsensusMetrics,
    /// Flag for submitting built payload
    submit_built_payload: bool,
}

impl<Provider> ResultWorkWorker<Provider>
where
    Provider: HeaderProvider + BlockNumReader + Send + Sync + Clone + 'static,
{
    /// Creates a new ResultWorkWorker instance.
    pub fn new(
        validator_address: Address,
        provider: Provider,
        payload_rx: mpsc::UnboundedReceiver<SubmitContext>,
        submit_built_payload: bool,
    ) -> Self {
        tracing::info!("ResultWorkWorker created, submit_built_payload: {}", submit_built_payload);
        Self {
            validator_address,
            provider,
            payload_rx,
            consensus_metrics: BscConsensusMetrics::default(),
            submit_built_payload,
        }
    }

    /// Run the result worker to process and submit payloads
    pub async fn run(mut self) {
        info!("Starting ResultWorkWorker for validator: {}", self.validator_address);

        loop {
            match self.payload_rx.recv().await {
                Some(submit_ctx) => {
                    let is_inturn = submit_ctx.mining_ctx.is_inturn;
                    let block_number = submit_ctx.payload.block().number();
                    let block_hash = submit_ctx.payload.block().hash();
                    match self.submit_payload(submit_ctx.payload).await {
                        Ok(()) => {
                            info!(
                                target: "bsc::miner",
                                block_number,
                                block_hash = %block_hash,
                                is_inturn,
                                "Succeed to submit block"
                            );
                        }
                        Err(e) => {
                            error!(
                                target: "bsc::miner",
                                block_number,
                                block_hash = %block_hash,
                                is_inturn,
                                error = %e,
                                "Failed to submit block"
                            );
                        }
                    }
                }
                None => {
                    warn!(
                        target: "bsc::miner",
                        "Main payload channel closed, stopping ResultWorkWorker"
                    );
                    break;
                }
            }
        }

        warn!("ResultWorkWorker stopped");
    }

    /// Submit a built payload to the engine-tree/network
    async fn submit_payload(
        &self,
        payload: BscBuiltPayload,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let sealed_block = payload.block();
        let block_hash = sealed_block.hash();
        let block_number = sealed_block.number();
        let parent_hash = sealed_block.header().parent_hash;
        let best_block_number = self.provider.best_block_number()?;
        if block_number <= best_block_number {
            debug!(
                target: "bsc::miner",
                block_number,
                best_block_number,
                "Skip to submit block due to block number is not greater than best block number"
            );
            return Ok(());
        }

        // Check if parent is still canonical (handles reorg during delayed submission)
        let parent_number = block_number.saturating_sub(1);
        match self.provider.sealed_header(parent_number) {
            Ok(Some(canonical_parent)) => {
                if canonical_parent.hash() != parent_hash {
                    debug!(
                        target: "bsc::miner",
                        block_number,
                        parent_number,
                        expected_parent_hash = %parent_hash,
                        canonical_parent_hash = %canonical_parent.hash(),
                        "Skip to submit block due to parent no longer canonical (reorg occurred)"
                    );
                    return Ok(());
                }
            }
            Ok(None) => {
                // Parent header not found - likely reorged away
                debug!(
                    target: "bsc::miner",
                    block_number,
                    parent_number,
                    parent_hash = %parent_hash,
                    "Skip to submit block due to parent not found in canonical chain"
                );
                return Ok(());
            }
            Err(e) => {
                // Provider/DB error - log warning and skip to avoid potential issues
                warn!(
                    target: "bsc::miner",
                    block_number,
                    parent_number,
                    error = %e,
                    "Failed to query canonical parent header, skipping block submission"
                );
                return Ok(());
            }
        }

        // Check double sign via the cache shared with the BidBlock path (`try_submit_winning_bid_block`
        // in payload.rs) — a validator must not sign two different blocks at the same height on the
        // same parent, regardless of which path produced either one.
        if !crate::shared::check_and_record_mined_block(block_number, parent_hash) {
            error!(
                "Reject Double Sign!! block: {}, hash: 0x{:x}, root: 0x{:x}, ParentHash: 0x{:x}",
                block_number,
                block_hash,
                sealed_block.header().state_root,
                parent_hash
            );
            // Update double sign metrics (both reth-bsc native and geth-compatible)
            self.consensus_metrics.double_signs_detected_total.increment(1);
            metrics::counter!("parlia.doublesign").increment(1);
            return Ok(());
        }

        let block_hash = sealed_block.hash();
        let difficulty = sealed_block.header().difficulty();
        let turn_status = if difficulty == crate::consensus::parlia::constants::DIFF_INTURN {
            // Update in-turn block metric
            self.consensus_metrics.inturn_blocks_total.increment(1);
            "inturn"
        } else {
            // Update out-of-turn block metric
            self.consensus_metrics.noturn_blocks_total.increment(1);
            "offturn"
        };
        debug!(
            target: "bsc::miner",
            block_number,
            hash = ?block_hash,
            parent_hash = ?parent_hash,
            txs = sealed_block.body().transaction_count(),
            gas_used = sealed_block.gas_used(),
            build_kind = ?payload.build_kind,
            exec_duration_ms = payload.exec_duration.as_millis(),
            trie_root_duration_ms = payload.trie_root_duration.as_millis(),
            turn_status,
            "Submitting block"
        );

        // Update miner metrics: best work gas used (in MGas)
        use crate::metrics::BscMinerMetrics;
        use once_cell::sync::Lazy;
        static MINER_METRICS: Lazy<BscMinerMetrics> = Lazy::new(BscMinerMetrics::default);

        // Count empty-fallback payloads at submission time (this preserves the signal even if the
        // payload job saw multiple candidates).
        if payload.build_kind == crate::node::engine::BuildKind::EmptyFallback {
            MINER_METRICS.empty_fallback_candidates_total.increment(1);
            warn!(
                target: "bsc::miner",
                block_hash = %sealed_block.hash(),
                block_number = sealed_block.number(),
                "Submitting empty-fallback block"
            );
        }

        // Record payload build timings.
        MINER_METRICS
            .block_exec_duration_seconds
            .record(payload.exec_duration.as_secs_f64());
        MINER_METRICS
            .block_trie_root_duration_seconds
            .record(payload.trie_root_duration.as_secs_f64());
        MINER_METRICS.blocks_produced_total.increment(1);

        let gas_used_mgas = sealed_block.gas_used() as f64 / 1_000_000.0;
        MINER_METRICS.best_work_gas_used_mgas.set(gas_used_mgas);

        // Calculate and record block broadcast delay
        // This is the time from block timestamp to current broadcast time, in nanoseconds
        let block_timestamp = sealed_block.header().timestamp;
        let now =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();

        // Convert block timestamp (seconds) to nanoseconds
        let block_timestamp_nanos = block_timestamp as u128 * 1_000_000_000;
        // Get current time in nanoseconds
        let now_nanos = now.as_nanos();

        // Calculate delay in nanoseconds (broadcast_time - block_timestamp)
        let broadcast_delay_nanos = if now_nanos >= block_timestamp_nanos {
            (now_nanos - block_timestamp_nanos) as f64
        } else {
            // In case of clock skew, record as 0
            0.0
        };

        MINER_METRICS.block_broadcast_delay_seconds.record(broadcast_delay_nanos);

        debug!(
            target: "bsc::miner",
            block_number,
            block_timestamp,
            broadcast_delay_nanos,
            "Block broadcast delay recorded"
        );

        // TODO: wait more times when huge chain import.
        // Note: sidechain blocks are already filtered by parent canonical check above.
        let parent_td = self
            .provider
            .header_td_by_number(parent_number)
            .map_err(|e| format!("Failed to get parent total difficulty due to {}", e))?
            .unwrap_or_default();
        let current_difficulty = sealed_block.header().difficulty();
        let new_td = parent_td + current_difficulty;

        let td = U128::from(new_td.to::<u128>());
        let new_block =
            BscNewBlock(reth_eth_wire::NewBlock { block: sealed_block.clone_block(), td });
        let msg =
            NewBlockMessage { hash: block_hash, block: Arc::new(new_block), td: Some(new_td) };

        if self.submit_built_payload {
            if let Some(sender) = get_block_import_mined_sender() {
                let incoming: IncomingMinedBlock = (payload, msg.clone());
                if sender.send(incoming).is_err() {
                    warn!("Failed to send mined block to import service due to channel closed");
                    return Err(
                        "Failed to send mined block to import service due to channel closed".into(),
                    );
                } else {
                    debug!("Succeed to send mined block to import service");
                }
            } else {
                warn!("Failed to send mined block due to import sender not initialised");
                return Err(
                    "Failed to send mined block due to import sender not initialised".into()
                );
            }
        } else if let Some(sender) = get_block_import_sender() {
            let peer_id = get_local_peer_id_or_default();
            let incoming: IncomingBlock = (msg.clone(), peer_id);
            if sender.send(incoming).is_err() {
                warn!("Failed to send built block to import service due to channel closed");
                return Err(
                    "Failed to send built block to import service due to channel closed".into()
                );
            } else {
                debug!("Succeed to send built block to import service");
            }
        } else {
            warn!("Failed to send built block due to import sender not initialised");
            return Err("Failed to send built block due to import sender not initialised".into());
        }

        Ok(())
    }
}

pub struct MevWorkWorker<Provider, Pool> {
    simulator: Arc<BidSimulator<Provider, Pool>>, // No outer RwLock, each map has its own lock
    bid_simulate_req_rx: mpsc::UnboundedReceiver<BidRuntime<Pool, BscEvmConfig>>,
    bid_simulate_req_tx: mpsc::UnboundedSender<BidRuntime<Pool, BscEvmConfig>>,
    provider: Provider,
    mev_running: Arc<AtomicBool>,
}

impl<Provider, Pool> MevWorkWorker<Provider, Pool>
where
    Provider: HeaderProvider<Header = alloy_consensus::Header>
        + BlockNumReader
        + reth_provider::StateProviderFactory
        + CanonStateSubscriptions
        + Clone
        + Send
        + Sync
        + 'static,
    Pool: TransactionPool<Transaction: PoolTransaction<Consensus = TransactionSigned>> + 'static,
{
    pub fn new(simulator: Arc<BidSimulator<Provider, Pool>>, provider: Provider) -> Self {
        let (bid_simulate_req_tx, bid_simulate_req_rx) =
            mpsc::unbounded_channel::<BidRuntime<Pool, BscEvmConfig>>();
        Self {
            simulator,
            bid_simulate_req_rx,
            bid_simulate_req_tx,
            provider,
            mev_running: Arc::new(AtomicBool::new(false)),
        }
    }

    pub async fn run(mut self) {
        info!("Starting MevWorkWorker");
        self.mev_running.store(true, Ordering::Relaxed);

        // Set global MEV running status
        if let Err(e) = crate::shared::set_mev_running(self.mev_running.clone()) {
            warn!("MEV running status already set: {:?}", e);
        }

        let mut send_bid_interval = tokio::time::interval(Duration::from_millis(20));
        let mut clear_bid_interval = tokio::time::interval(Duration::from_millis(1000));

        loop {
            tokio::select! {
                bid_runtime = self.bid_simulate_req_rx.recv() => {
                    match bid_runtime {
                        Some(bid_runtime) => {
                            // A finished simulation may produce a recommit request
                            // (go-bsc simBid defer): the follow-up simulation of a
                            // better bid that was parked during this run.
                            if let Some(req) = self.simulator.bid_simulate(bid_runtime) {
                                if let Err(e) = self.bid_simulate_req_tx.send(req) {
                                    error!("Failed to send recommit bid simulate request due to channel closed: {}", e);
                                }
                            }
                        }
                        None => {
                            warn!("Bid simulate request channel closed");
                            break;
                        }
                    }
                }

                // Interval for checking bid packages
                _ = send_bid_interval.tick() => {
                    // Attempt to send bids
                    self.get_bid_and_send();
                    // Process any admitted BEP-675 BidBlocks.
                    self.process_bid_block();
                }

                _ = clear_bid_interval.tick() => {
                    let last_block_number = self.provider.last_block_number().unwrap_or(0);
                    self.simulator.clear(last_block_number);
                }
            }
        }
    }

    /// Send a bid to the miner's bid simulator (reads from global queue)
    fn get_bid_and_send(&self) {
        // Read bid packages from the global queue
        if let Some(bid_package) = crate::shared::pop_bid_package() {
            debug!(
                "Popped bid package from queue, block: {}, committing to simulator",
                bid_package.block_number
            );
            if let Some(req) = self.simulator.commit_new_bid(bid_package) {
                if let Err(e) = self.bid_simulate_req_tx.send(req) {
                    error!("Failed to send bid simulate request due to channel closed: {}", e);
                }
            }
        }
    }

    /// Pop an admitted BEP-675 BidBlock (from `mev_sendBidBlock`) and verify/blind-sign/seal it into
    /// the simulator's best-bid-block store (go-bsc `newBidBlockLoop` → `AddBidBlock`).
    fn process_bid_block(&self) {
        if let Some(decoded) = crate::shared::pop_bid_block_package() {
            debug!(
                "Popped BidBlock from queue, block: {}, builder: {}",
                decoded.block_number(),
                decoded.builder
            );
            self.simulator.commit_bid_block(decoded);
        }
    }
}

/// Miner that handles block production for BSC.
pub struct BscMiner<Pool, Provider> {
    validator_address: Address,
    signing_key: SigningKey,
    new_work_worker: NewWorkWorker<Provider>,
    main_work_worker: MainWorkWorker<Pool, Provider>,
    result_work_worker: ResultWorkWorker<Provider>,
    mev_work_worker: MevWorkWorker<Provider, Pool>,
    task_executor: TaskExecutor,
}

impl<Pool, Provider> BscMiner<Pool, Provider>
where
    Pool: TransactionPool<Transaction: PoolTransaction<Consensus = TransactionSigned>>
        + Clone
        + 'static,
    Provider: HeaderProvider<Header = alloy_consensus::Header>
        + BlockNumReader
        + reth_provider::StateProviderFactory
        + CanonStateSubscriptions
        + Clone
        + Send
        + Sync
        + 'static,
{
    pub fn new(
        pool: Pool,
        provider: Provider,
        snapshot_provider: Arc<dyn SnapshotProvider + Send + Sync>,
        chain_spec: Arc<crate::chainspec::BscChainSpec>,
        mining_config: MiningConfig,
        task_executor: TaskExecutor,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        mining_config.validate()?;
        let validator_address = mining_config
            .validator_address
            .ok_or(eyre::eyre!("No validator address configured"))?;
        let signing_key =
            mining_config.signing_key.clone().ok_or(eyre::eyre!("No signing key configured"))?;

        let (mining_queue_tx, mining_queue_rx) = mpsc::unbounded_channel::<MiningContext>();
        let (payload_tx, payload_rx) = mpsc::unbounded_channel::<SubmitContext>();

        let chain_id = chain_spec.as_ref().chain().id();
        let desired_gas_limit = mining_config.get_gas_limit(chain_id);
        let desired_min_gas_tip = mining_config.get_min_gas_tip();
        info!(
            "Mining configuration: validator={}, chain_id={}, gas_limit={}, min_gas_tip={}",
            validator_address, chain_id, desired_gas_limit, desired_min_gas_tip
        );

        // Initialize dynamic miner config in shared state so miner_* RPC can update them
        crate::shared::init_miner_dynamic_config(
            desired_gas_limit,
            desired_min_gas_tip as u64,
            validator_address,
        );

        let parlia = Arc::new(crate::consensus::parlia::Parlia::new(chain_spec.clone(), 200));
        let new_work_worker = NewWorkWorker::new(
            validator_address,
            provider.clone(),
            snapshot_provider.clone(),
            mining_queue_tx.clone(),
            parlia.clone(),
        );

        let parlia = Arc::new(crate::consensus::parlia::Parlia::new(chain_spec.clone(), 200));
        let simulator = Arc::new(BidSimulator::new(
            provider.clone(),
            pool.clone(),
            chain_spec.clone(),
            parlia.clone(),
            validator_address,
            snapshot_provider.clone(),
            mining_config.validator_commission.unwrap_or(100),
            mining_config.greedy_merge,
            mining_config.get_no_interrupt_left_over(),
        ));
        let main_work_worker = MainWorkWorker::new(
            validator_address,
            pool.clone(),
            provider.clone(),
            chain_spec.clone(),
            parlia.clone(),
            mining_queue_rx,
            simulator.clone(),
            payload_tx,
            desired_gas_limit,
            desired_min_gas_tip,
        );

        let result_work_worker = ResultWorkWorker::new(
            validator_address,
            provider.clone(),
            payload_rx,
            mining_config.submit_built_payload,
        );

        let mev_work_worker = MevWorkWorker::new(simulator.clone(), provider.clone());

        let miner = Self {
            validator_address,
            signing_key,
            new_work_worker,
            main_work_worker,
            result_work_worker,
            mev_work_worker,
            task_executor,
        };
        info!("Succeed to new miner, address: {}", validator_address);
        Ok(miner)
    }

    pub async fn start(self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if let Err(e) = init_global_signer_from_k256(&self.signing_key) {
            return Err(format!("Failed to initialize global signer due to {}", e).into());
        } else {
            info!("Succeed to initialize global signer");
        }
        self.spawn_workers()
    }

    fn spawn_workers(self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.task_executor.spawn_critical_task("mev_work_worker", self.mev_work_worker.run());
        self.task_executor.spawn_critical_task("new_work_worker", self.new_work_worker.run());
        self.task_executor.spawn_critical_task("main_work_worker", self.main_work_worker.run());
        self.task_executor.spawn_critical_task("result_work_worker", self.result_work_worker.run());
        info!("Succeed to start mining, address: {}", self.validator_address);
        Ok(())
    }
}
