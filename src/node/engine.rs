use crate::{
    node::{
        engine_api::payload::BscPayloadTypes,
        miner::{BscMiner, MiningConfig},
        BscNode,
    },
    BscPrimitives,
};
use crate::BscBlock;
use crate::consensus::parlia::VoteAddress;
use alloy_primitives::Address;
use alloy_eips::eip7685::Requests;
use alloy_primitives::U256;
use reth::transaction_pool::PoolTransaction;
use reth::{
    api::FullNodeTypes,
    builder::{components::PayloadServiceBuilder, BuilderContext},
    payload::{PayloadBuilderHandle, PayloadServiceCommand},
    transaction_pool::TransactionPool,
};
use reth_payload_primitives::BuiltPayloadExecutedBlock;
use reth_evm::ConfigureEvm;
use reth_payload_builder_primitives::Events;
use reth_payload_primitives::BuiltPayload;
use reth_primitives_traits::SealedBlock;
use reth_ethereum_primitives::TransactionSigned;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, error, info};

/// Distinguishes what kind of payload build produced a [`BscBuiltPayload`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BuildKind {
    /// A normal build attempt that may include user transactions from the pool.
    #[default]
    NormalAttempt,
    /// An empty-block fallback build (no user transactions; only pre-execution/system changes).
    EmptyFallback,
}

/// Built payload for BSC. This is similar to [`EthBuiltPayload`] but without sidecars as those
/// included into [`BscBlock`].
#[derive(Debug, Clone)]
pub struct BscBuiltPayload {
    /// The built block
    pub(crate) block: Arc<SealedBlock<BscBlock>>,
    /// The fees of the block
    pub(crate) fees: U256,
    /// The requests of the payload
    pub(crate) requests: Option<Requests>,
    /// What build path produced this payload.
    pub build_kind: BuildKind,
    /// Time spent selecting + executing transactions (or pre-execution changes for empty blocks).
    pub exec_duration: Duration,
    /// Time spent computing the trie root (time spent in `finish()` after execution).
    pub trie_root_duration: Duration,
    /// The executed block.
    pub(crate) executed_block: BuiltPayloadExecutedBlock<BscPrimitives>,
    /// Validators from execution context, to be written to VALIDATOR_CACHE after finalization.
    /// `None` for bid payloads and non-epoch blocks.
    pub(crate) pending_validators: Option<(Vec<Address>, Vec<VoteAddress>)>,
    /// Turn length from execution context, to be written to TURN_LENGTH_CACHE after finalization.
    /// `None` for bid payloads and blocks without turn-length changes.
    pub(crate) pending_turn_length: Option<u8>,
    /// The builder this payload came from, when it originated from an external legacy `SendBid`
    /// rather than a local transaction-pool build. `None` for local builds.
    ///
    /// Carries the address (not just a flag) because finalization needs it to stamp the block's
    /// MEV info tag — see `set_block_mev_info`. Also drives the bid-win metrics in
    /// `pick_best_payload_and_finalize()`.
    pub(crate) bid_builder: Option<Address>,
}

impl BuiltPayload for BscBuiltPayload {
    type Primitives = BscPrimitives;

    fn block(&self) -> &SealedBlock<BscBlock> {
        self.block.as_ref()
    }

    fn fees(&self) -> U256 {
        self.fees
    }

    fn requests(&self) -> Option<Requests> {
        self.requests.clone()
    }
}

#[derive(Debug, Clone, Copy, Default)]
#[non_exhaustive]
pub struct BscPayloadServiceBuilder;

impl<Node, Pool, Evm> PayloadServiceBuilder<Node, Pool, Evm> for BscPayloadServiceBuilder
where
    Node: FullNodeTypes<Types = BscNode>,
    Pool: TransactionPool<Transaction: PoolTransaction<Consensus = TransactionSigned>>
        + Clone
        + 'static,
    Evm: ConfigureEvm,
{
    async fn spawn_payload_builder_service(
        self,
        ctx: &BuilderContext<Node>,
        pool: Pool,
        _evm_config: Evm,
    ) -> eyre::Result<PayloadBuilderHandle<BscPayloadTypes>> {
        let (tx, mut rx) = mpsc::unbounded_channel();
        // Load mining configuration from environment, allow override via CLI if set globally
        let mining_config =
            if let Some(cfg) = crate::node::miner::config::get_global_mining_config() {
                cfg.clone()
            } else {
                MiningConfig::from_env()
            };

        // Register the sparse-trie state-root spawner, if enabled.
        //
        // For each build job the registered closure builds a one-shot
        // `OverlayStateProviderFactory` anchored at the parent block hash and calls the
        // v2.4.1 `spawn_payload_builder_state_root` hook to get a `StateRootHandle`. The
        // miner installs the handle's state hook on the building EVM's DB (streaming per-tx
        // state diffs to the sparse-trie task) and reads the precomputed root back in
        // `BscBlockBuilder::finish`.
        //
        // v2.4.1 port: `LazyOverlay` / `OverlayBuilder::with_lazy_overlay` were replaced by
        // `StateTrieOverlayManager` + `OverlayBuilder::with_state_trie_overlay_manager`, and the
        // old `PayloadProcessor::spawn_state_root` by the standalone
        // `state_root_strategy::spawn_payload_builder_state_root`. We mirror the engine's own
        // `overlay_builder_for_parent`: anchor at `parent_hash` and feed the in-memory canonical
        // chain to a per-spawn `StateTrieOverlayManager` so proof workers can resolve a
        // not-yet-persisted parent.
        if mining_config.use_sparse_trie_state_root {
            use alloy_consensus::Header;
            use reth_chain_state::ExecutedBlock;
            use reth_engine_tree::tree::state_root_strategy::spawn_payload_builder_state_root;
            use reth_primitives_traits::SealedHeader;
            use reth_storage_overlay::{OverlayManager, OverlayStateProviderFactory};
            use reth_tasks::{RuntimeBuilder, RuntimeConfig, TokioConfig};

            let tree_config = Arc::new(ctx.config().engine.tree_config());
            tracing::debug!(
                target: "bsc::miner",
                ?tree_config,
                "Miner sparse-trie TreeConfig (from --engine.* CLI flags)"
            );
            let provider = ctx.provider().clone();
            let worker_pool = ctx.task_executor().state_trie_overlay_worker_pool();
            let tokio_handle = ctx.task_executor().handle().clone();

            let tree_config_for_closure = tree_config.clone();
            // Cache the task Runtime so it is built at most once. Rebuilding (and dropping) a
            // worker-pool Runtime per spawn panics the storage workers with a join deadlock, so
            // we resolve it once: the engine's shared Runtime if published, else a single
            // dedicated fallback reused for the rest of the run.
            let runtime_cell: std::sync::OnceLock<reth_tasks::Runtime> = std::sync::OnceLock::new();

            let spawn_fn: crate::shared::SparseTrieSpawnFn = std::sync::Arc::new(
                move |parent: SealedHeader<Header>| {
                    let parent_hash = parent.hash();
                    // Per-spawn overlay manager fed with the in-memory canonical chain, so the
                    // proof workers can resolve a parent that hasn't been persisted yet (the
                    // common case during fast block production). The manager owns its own changeset
                    // cache internally (reth v2.5), so there is no shared cache to thread in.
                    let overlay_manager =
                        OverlayManager::<crate::BscPrimitives>::new(worker_pool.clone());
                    if let Some(cim) = crate::shared::get_canonical_in_memory_state() {
                        if let Some(state) = cim.state_by_hash(parent_hash) {
                            // chain() yields newest-to-oldest including the parent itself.
                            let blocks: Vec<ExecutedBlock<crate::BscPrimitives>> =
                                state.chain().map(|bs| bs.block()).collect();
                            metrics::histogram!("bsc_miner_overlay_depth")
                                .record(blocks.len() as f64);
                            for b in &blocks {
                                overlay_manager.insert_block(b.clone());
                            }
                            metrics::counter!("bsc_miner_sparse_trie_anchor_inmemory_total")
                                .increment(1);
                        } else {
                            metrics::counter!("bsc_miner_sparse_trie_anchor_persisted_total")
                                .increment(1);
                        }
                    } else {
                        metrics::counter!("bsc_miner_sparse_trie_anchor_nocim_total").increment(1);
                    }

                    // Anchor directly at the parent hash; the overlay manager resolves the
                    // in-memory parent trie (mirrors the engine's own payload-builder path in
                    // `payload_state_root_handle_for`).
                    let overlay_builder = overlay_manager.overlay_builder(parent_hash);
                    let overlay_factory =
                        OverlayStateProviderFactory::new(provider.clone(), overlay_builder);

                    // Resolve the Runtime once (see `runtime_cell` above): prefer the engine's
                    // shared Runtime (same rayon proof pools); if not yet published on first use,
                    // build a single dedicated fallback and reuse it (pools not shared this run).
                    let runtime = runtime_cell.get_or_init(|| {
                        reth_tasks::shared_engine_runtime().unwrap_or_else(|| {
                            tracing::warn!(
                                target: "bsc::miner",
                                "engine Runtime not yet published on first sparse-trie spawn; \
                                 building a dedicated Runtime (proof pools NOT shared this run)"
                            );
                            RuntimeBuilder::new(RuntimeConfig::default().with_tokio(
                                TokioConfig::existing_handle(tokio_handle.clone()),
                            ))
                            .build()
                            .expect("failed to build fallback sparse-trie Runtime")
                        })
                    });

                    let spawn_start = std::time::Instant::now();
                    let handle = spawn_payload_builder_state_root(
                        runtime,
                        &overlay_manager,
                        overlay_factory,
                        parent,
                        None, // tx count unknown at spawn time → full proof-worker pool
                        tree_config_for_closure.as_ref(),
                        None, // fresh manager per spawn: no preserved trie to prune
                    );
                    metrics::histogram!("bsc_miner_sparse_trie_spawn_duration_seconds")
                        .record(spawn_start.elapsed().as_secs_f64());
                    Some(handle)
                },
            );

            if crate::shared::set_sparse_trie_spawn_fn(spawn_fn).is_err() {
                tracing::warn!("Sparse-trie spawner already registered, keeping existing one");
            } else {
                info!("Sparse-trie state-root spawner registered (use_sparse_trie_state_root=true)");
            }
        }

        // Skip mining setup if disabled
        if !mining_config.is_mining_enabled() {
            info!("Mining is disabled in configuration");
        } else {
            info!("Mining is enabled - will start mining after consensus initialization");

            let mining_config_clone = mining_config.clone();
            let pool_clone = pool.clone();
            let provider_clone = ctx.provider().clone();
            let chain_spec_clone = Arc::new(ctx.config().chain.clone().as_ref().clone());
            let task_executor_clone = ctx.task_executor().clone();

            ctx.task_executor().spawn_critical_task("bsc-miner-initializer", async move {
                info!("Waiting for consensus module to initialize snapshot provider...");
                let mut attempts = 0;
                let snapshot_provider = loop {
                    if let Some(provider) = crate::shared::get_snapshot_provider() {
                        break provider.clone();
                    }
                    attempts += 1;
                    if attempts > 100 {
                        error!("Timed out waiting for snapshot provider - mining disabled");
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                };
                info!("Snapshot provider available, starting BSC mining service");

                match BscMiner::new(
                    pool_clone,
                    provider_clone,
                    snapshot_provider,
                    chain_spec_clone,
                    mining_config_clone,
                    task_executor_clone,
                ) {
                    Ok(miner) => {
                        info!("BSC miner created successfully, starting mining loop");
                        if let Err(e) = miner.start().await {
                            error!("Mining service failed: {}", e);
                        }
                    }
                    Err(e) => {
                        error!("Failed to create mining service: {}", e);
                    }
                }
            });
        }

        // Initialize global payload events channel and handler
        let (events_tx, _events_rx) = broadcast::channel::<Events<BscPayloadTypes>>(100);
        let _ = crate::shared::set_payload_events_tx(events_tx.clone());

        // Handle payload service commands (keep minimal compatibility but with shared events channel)
        ctx.task_executor().spawn_critical_task("payload-service-handler", async move {
            while let Some(message) = rx.recv().await {
                match message {
                    PayloadServiceCommand::Subscribe(tx) => {
                        let _ = tx.send(events_tx.subscribe());
                    }
                    message => debug!(?message, "BSC payload service received engine message"),
                }
            }
        });

        Ok(PayloadBuilderHandle::new(tx))
    }
}
