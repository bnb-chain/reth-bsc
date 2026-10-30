use crate::{
    consensus::parlia::VoteAddress,
    node::{
        engine_api::payload::BscPayloadTypes,
        miner::{BscMiner, MiningConfig},
        BscNode,
    },
    BscBlock, BscPrimitives,
};
use alloy_eips::eip7685::Requests;
use alloy_primitives::{Address, U256};
use reth::{
    api::FullNodeTypes,
    builder::{components::PayloadServiceBuilder, BuilderContext},
    payload::{PayloadBuilderHandle, PayloadServiceCommand},
    transaction_pool::{PoolTransaction, TransactionPool},
};
use reth_ethereum_primitives::TransactionSigned;
use reth_evm::ConfigureEvm;
use reth_payload_builder_primitives::Events;
use reth_payload_primitives::{BuiltPayload, BuiltPayloadExecutedBlock};
use reth_primitives_traits::SealedBlock;
use std::{sync::Arc, time::Duration};
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
    /// Selection evidence only; never part of consensus or transaction-pool state.
    pub(crate) selection_diagnostics:
        Option<crate::node::miner::selection_diagnostics::SelectionDiagnostics>,
    /// Attempt metadata; deliberately excluded from block contents and consensus.
    pub(crate) diagnostics:
        Option<Arc<crate::node::miner::build_diagnostics::CandidateDiagnostics>>,
    /// The built block
    pub(crate) block: Arc<SealedBlock<BscBlock>>,
    /// Exact sealed identity committed to by an early descendant. Publication must preserve it.
    pub(crate) pipeline_seal: Option<alloy_primitives::B256>,
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
        // BidBlock revoke lockouts (validator-local MEV policy) journal to a single file under
        // the datadir rather than chaindata — go-bsc parity (`BidBlockRevokesJournal`, default
        // `bidblockrevokes.json`). Like go's config field, `BSC_BID_BLOCK_REVOKES_JOURNAL`
        // overrides the location (a relative path is resolved under the datadir) and an empty
        // value disables persistence entirely. Registered here, during component build, so it is
        // in place before any consumer lazily initializes the global manager.
        use reth_chainspec::EthChainSpec as _;
        let datadir = ctx.config().datadir.clone().resolve_datadir(ctx.chain_spec().chain());
        let revokes_journal = match std::env::var("BSC_BID_BLOCK_REVOKES_JOURNAL") {
            Ok(path) if path.is_empty() => None, // explicit opt-out: purely in-memory
            Ok(path) => {
                let path = std::path::PathBuf::from(path);
                Some(if path.is_absolute() { path } else { datadir.data_dir().join(path) })
            }
            Err(_) => Some(datadir.data_dir().join("bidblockrevokes.json")),
        };
        if let Some(path) = revokes_journal {
            if crate::shared::set_bid_block_revokes_journal(path).is_err() {
                tracing::warn!(
                    target: "payload_builder",
                    "BidBlock revoke journal path already registered; keeping the existing one"
                );
            }
        }

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
        // chain to a miner-owned overlay cache so proof workers can resolve a
        // not-yet-persisted parent without merging every ancestor on each build.
        let _ = crate::node::miner::perf_tuning::GasLimitSkips::configured();
        crate::node::miner::pipeline::configure(ctx.task_executor().handle().clone())?;
        // One miner owner spans published and unpublished parents. Engine canonical
        // state remains separate; only immutable executed block results are shared.
        let overlay_cache = Arc::new(parking_lot::Mutex::new(
            crate::node::miner::overlay::MinerOverlayCache::default(),
        ));
        let state_cache = overlay_cache.clone();
        let overlay_workers = Arc::new(std::sync::OnceLock::<Arc<reth_tasks::WorkerPool>>::new());
        let state_workers = overlay_workers.clone();
        let miner_provider = ctx.provider().clone();
        let state_executor = ctx.task_executor().handle().clone();
        let _ = crate::shared::set_private_miner_state_fn(Arc::new(move |parent, private| {
            use reth_provider::DatabaseProviderROFactory;
            use reth_storage_overlay::{OverlayManager, OverlayStateProviderFactory};
            let (blocks, state) =
                crate::node::miner::overlay::parent_snapshot(parent, private.as_ref());
            let (manager, retired) = state_cache.lock().for_parent(&blocks, || {
                state_workers.get().map_or_else(OverlayManager::default, |pool| {
                    OverlayManager::new_with_diagnostic_origin(pool.clone(), "miner")
                })
            });
            if !retired.is_empty() {
                state_executor.spawn_blocking(move || drop(retired));
            }
            let builder = state.map_or_else(
                || manager.overlay_builder(parent),
                |state| manager.overlay_builder_for_state(state),
            );
            let factory = OverlayStateProviderFactory::new(miner_provider.clone(), builder);
            Ok(Arc::new(move || Ok(Box::new(factory.database_provider_ro()?))))
        }));
        if mining_config.use_sparse_trie_state_root {
            use crate::node::miner::proof_provider::{
                MinerProofOverlayCache, MinerProofProviderFactory,
            };
            use alloy_consensus::Header;
            use reth_engine_tree::tree::state_root_strategy::{
                spawn_payload_builder_state_root, spawn_payload_builder_state_root_with_cache_wait,
            };
            use reth_primitives_traits::SealedHeader;
            use reth_storage_overlay::OverlayManager;
            use reth_tasks::{RayonConfig, RuntimeBuilder, RuntimeConfig, TokioConfig};

            let tree_config = Arc::new(ctx.config().engine.tree_config());
            tracing::debug!(
                target: "bsc::miner",
                ?tree_config,
                "Miner sparse-trie TreeConfig (from --engine.* CLI flags)"
            );
            let provider = ctx.provider().clone();
            // Streaming root/proof sessions can wait for the miner's next transaction. They
            // must never hold an import worker while waiting. The registered spawner owns
            // this runtime for the node's lifetime; no attempt creates or drops a pool.
            let proof_threads = std::thread::available_parallelism()
                .map_or(1, |n| n.get().saturating_sub(2).clamp(1, 4));
            let runtime = RuntimeBuilder::new(
                RuntimeConfig::default()
                    .with_tokio(TokioConfig::existing_handle(ctx.task_executor().handle().clone()))
                    .with_rayon(RayonConfig {
                        cpu_threads: Some(2),
                        rpc_threads: Some(1),
                        storage_threads: Some(1),
                        proof_storage_worker_threads: Some(
                            crate::node::miner::perf_tuning::storage_proof_threads(proof_threads),
                        ),
                        proof_account_worker_threads: Some(proof_threads),
                        prewarming_threads: Some(1),
                        bal_streaming_threads: Some(1),
                        state_trie_overlay_worker_threads: Some(2),
                        ..Default::default()
                    }),
            )
            .with_worker_namespace("miner")
            .build()?;
            // An empty fallback must not queue behind a normal candidate that still owns
            // a streaming root/hash/proof session. These small, persistent pools share only
            // the parent overlay cache below; they are also independent of import workers.
            let empty_runtime = RuntimeBuilder::new(
                RuntimeConfig::default()
                    .with_tokio(TokioConfig::existing_handle(ctx.task_executor().handle().clone()))
                    .with_rayon(RayonConfig {
                        cpu_threads: Some(1),
                        rpc_threads: Some(1),
                        storage_threads: Some(1),
                        proof_storage_worker_threads: Some(1),
                        proof_account_worker_threads: Some(1),
                        prewarming_threads: Some(1),
                        bal_streaming_threads: Some(1),
                        state_trie_overlay_worker_threads: Some(1),
                        ..Default::default()
                    }),
            )
            .with_worker_namespace("miner-empty")
            .build()?;
            let worker_pool = runtime.state_trie_overlay_worker_pool();
            let _ = overlay_workers.set(worker_pool.clone());
            let tree_config_for_closure = tree_config.clone();
            let root_reuse_enabled = crate::node::miner::root_reuse::enabled();
            tracing::info!(target: "bsc::miner", root_reuse_enabled, "Miner completed-parent sparse trie handoff configured");
            let root_cache =
                parking_lot::Mutex::new(crate::node::miner::root_reuse::MinerRootCache::default());
            let proof_overlay_cache = Arc::new(MinerProofOverlayCache::new(runtime.clone()));
            let spawn_fn: crate::shared::SparseTrieSpawnFn = std::sync::Arc::new(
                move |parent: SealedHeader<Header>, kind, private_parent| {
                    let private_depth =
                        private_parent.as_ref().map_or(0, |chain| chain.blocks().len());
                    let parent_hash = parent.hash();
                    let (blocks, state) = crate::node::miner::overlay::parent_snapshot(
                        parent_hash,
                        private_parent.as_ref(),
                    );
                    if state.is_some() {
                        metrics::histogram!("bsc_miner_overlay_depth").record(blocks.len() as f64);
                        metrics::counter!("bsc_miner_sparse_trie_anchor_inmemory_total")
                            .increment(1);
                    } else if crate::shared::get_canonical_in_memory_state().is_some() {
                        metrics::counter!("bsc_miner_sparse_trie_anchor_persisted_total")
                            .increment(1);
                    } else {
                        metrics::counter!("bsc_miner_sparse_trie_anchor_nocim_total").increment(1);
                    }
                    let selection_start = std::time::Instant::now();
                    let mut cache = overlay_cache.lock();
                    let selection_acquired = std::time::Instant::now();
                    let (overlay_manager, retired) = cache.for_parent(&blocks, || {
                        OverlayManager::new_with_diagnostic_origin(worker_pool.clone(), "miner")
                    });
                    drop(cache);
                    let selection_released = std::time::Instant::now();
                    metrics::histogram!("bsc_miner_overlay_manager_index_seconds", "phase" => "wait")
                        .record(selection_acquired.duration_since(selection_start).as_secs_f64());
                    metrics::histogram!("bsc_miner_overlay_manager_index_seconds", "phase" => "work")
                        .record(selection_released.duration_since(selection_acquired).as_secs_f64());
                    // Drop our previous immutable overlay reference before extending the
                    // manager's parent cache. Existing readers keep their own snapshots.
                    if crate::node::miner::pipeline::enabled() {
                        proof_overlay_cache.release_parent(parent.parent_hash);
                    }
                    if !retired.is_empty() {
                        runtime.spawn_drop(retired);
                    }

                    // Anchor directly at the parent hash; the overlay manager resolves the
                    // in-memory parent trie (mirrors the engine's own payload-builder path in
                    // `payload_state_root_handle_for`).
                    let overlay_builder = if crate::node::miner::pipeline::enabled() {
                        state.map_or_else(
                            || overlay_manager.overlay_builder(parent_hash),
                            |state| overlay_manager.overlay_builder_for_state(state),
                        )
                    } else {
                        overlay_manager.overlay_builder(parent_hash)
                    };
                    let overlay_factory = MinerProofProviderFactory::new(
                        provider.clone(),
                        overlay_builder,
                        parent_hash,
                        proof_overlay_cache.clone(),
                        matches!(kind, crate::shared::MinerRootKind::EmptyFallback),
                    );

                    let spawn_start = std::time::Instant::now();
                    // Private and canonical builds hold the same local build lease: only
                    // one live normal root stream uses these full-size miner workers.
                    let executor = match kind {
                        crate::shared::MinerRootKind::Normal => &runtime,
                        crate::shared::MinerRootKind::EmptyFallback => &empty_runtime,
                    };
                    let handle = if root_reuse_enabled &&
                        matches!(kind, crate::shared::MinerRootKind::Normal)
                    {
                        let preserved = root_cache.lock().take(&parent);
                        let (handle, pending) = spawn_payload_builder_state_root_with_cache_wait(
                            executor,
                            overlay_factory,
                            parent,
                            None,
                            tree_config_for_closure.as_ref(),
                            Some(blocks),
                            preserved,
                            match private_depth {
                                1 => "miner-pipeline1",
                                2 => "miner-pipeline2",
                                0 => "miner",
                                _ => "miner-pipeline",
                            },
                            crate::node::miner::pipeline::enabled().then(|| {
                                tree_config_for_closure
                                    .state_root_task_timeout()
                                    .unwrap_or(std::time::Duration::from_secs(2))
                            }),
                            |factory, anchor, roots| factory.with_reused_sparse_trie(anchor, roots),
                        );
                        let retired = root_cache.lock().insert(parent_hash, pending);
                        if let Some(retired) = retired {
                            runtime.spawn_drop(retired);
                        }
                        handle
                    } else {
                        spawn_payload_builder_state_root(
                            executor,
                            &overlay_manager,
                            overlay_factory,
                            parent,
                            None,
                            tree_config_for_closure.as_ref(),
                            None,
                        )
                    };
                    metrics::histogram!("bsc_miner_sparse_trie_spawn_duration_seconds")
                        .record(spawn_start.elapsed().as_secs_f64());
                    Some(handle)
                },
            );

            if crate::shared::set_sparse_trie_spawn_fn(spawn_fn).is_err() {
                tracing::warn!("Sparse-trie spawner already registered, keeping existing one");
            } else {
                info!(
                    "Sparse-trie state-root spawner registered (use_sparse_trie_state_root=true)"
                );
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

        // Handle payload service commands (keep minimal compatibility but with shared events
        // channel)
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
