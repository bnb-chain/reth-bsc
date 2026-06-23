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
use reth_chain_state::ExecutedBlock;
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
    /// The executed block (includes difflayer if triedb produced one)
    pub(crate) executed_block: ExecutedBlock<BscPrimitives>,
    /// Validators from execution context, to be written to VALIDATOR_CACHE after finalization.
    /// `None` for bid payloads and non-epoch blocks.
    pub(crate) pending_validators: Option<(Vec<Address>, Vec<VoteAddress>)>,
    /// Turn length from execution context, to be written to TURN_LENGTH_CACHE after finalization.
    /// `None` for bid payloads and blocks without turn-length changes.
    pub(crate) pending_turn_length: Option<u8>,
    /// Whether this payload originated from an external bid (MEV bundle) rather than a local
    /// transaction pool build.  Used to track bid-win metrics in `try_return_best_payload()`.
    pub(crate) is_bid: bool,
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

        // Periodic overlay/persist sampler (diagnostic, opt-in via BSC_OVERLAY_SAMPLE_MS=<ms>).
        // Emits one `bsc::diag` line per interval so a long-RO WARN (RETH_TXN_LONG_RO_MS) can be
        // aligned — in the same log file — with the overlay depth and persist progress at that
        // instant. `persisted` flat across consecutive lines == persistence stalled; `overlay_depth`
        // = head - persisted tip (count of in-memory, not-yet-persisted blocks). Unset => disabled.
        if let Some(sample_ms) = std::env::var("BSC_OVERLAY_SAMPLE_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|ms| *ms > 0)
        {
            ctx.task_executor().handle().clone().spawn(async move {
                use alloy_consensus::BlockHeader;
                let mut ticker =
                    tokio::time::interval(std::time::Duration::from_millis(sample_ms));
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                let mut last_persisted: u64 = 0;
                loop {
                    ticker.tick().await;
                    let Some(cim) = crate::shared::get_canonical_in_memory_state() else {
                        continue;
                    };
                    let head = cim.get_canonical_head().number();
                    let overlay_depth = cim.canonical_chain().count() as u64;
                    let persisted = head.saturating_sub(overlay_depth);
                    let finalized = cim.get_finalized_num_hash().map(|n| n.number).unwrap_or(0);
                    let persist_advanced = persisted.saturating_sub(last_persisted);
                    last_persisted = persisted;
                    tracing::info!(
                        target: "bsc::diag",
                        head,
                        persisted,
                        persist_advanced,
                        overlay_depth,
                        finalized,
                        finality_lag = head.saturating_sub(finalized),
                        "overlay/persist sample"
                    );
                }
            });
        }

        // Register the sparse-trie state-root spawner, if enabled.
        //
        // We construct a long-lived `PayloadProcessor` keyed to a fresh `BscEvmConfig`
        // (built from chain_spec — same source as the rest of the BSC pipeline). For
        // each build job, the registered closure constructs a one-shot
        // `OverlayStateProviderFactory` anchored at the parent block hash and calls
        // `spawn_state_root` to get a `StateRootHandle`.
        //
        // The sparse-trie PayloadProcessor is configured from the engine-launch
        // `TreeConfig` built from the `--engine.*` CLI flags via
        // `ctx.config().engine.tree_config()`, so miner-side proof-worker counts /
        // cache sizes are CLI-tunable and match the import (engine) path.
        if mining_config.use_sparse_trie_state_root
            && !rust_eth_triedb::triedb_manager::is_triedb_active()
        {
            use alloy_consensus::BlockHeader;
            use reth_chain_state::LazyOverlay;
            use reth_engine_tree::tree::{
                payload_processor::PayloadProcessor, precompile_cache::PrecompileCacheMap,
            };
            use reth_provider::providers::{OverlayBuilder, OverlayStateProviderFactory};
            use reth_tasks::{RuntimeBuilder, RuntimeConfig, TokioConfig};
            use reth_trie_db::ChangesetCache;

            let chain_spec = Arc::new(ctx.config().chain.clone().as_ref().clone());
            let bsc_evm_config = crate::node::evm::config::BscEvmConfig::new(chain_spec);
            let tree_config = Arc::new(ctx.config().engine.tree_config());
            tracing::debug!(
                target: "bsc::miner",
                ?tree_config,
                "Miner sparse-trie PayloadProcessor TreeConfig (from --engine.* CLI flags)"
            );
            let provider = ctx.provider().clone();

            // R1: share the engine's `Runtime` (same rayon proof pools) instead of building a
            // second, competing set. The engine publishes its Runtime via
            // `reth_tasks::set_shared_engine_runtime` in `TreePayloadValidator::new`; we read it
            // here. The `PayloadProcessor` is built lazily on first use (then cached) because the
            // engine runtime is published during engine launch, which can run *after* this
            // service builder. If it is still unavailable on first use we fall back to a
            // dedicated Runtime (degraded: pools not shared) so block production never stalls.
            let tokio_handle = ctx.task_executor().handle().clone();
            let tree_config_for_closure = tree_config.clone();
            let pp_cell: std::sync::OnceLock<
                std::sync::Arc<PayloadProcessor<crate::node::evm::config::BscEvmConfig>>,
            > = std::sync::OnceLock::new();
            // Long-lived changeset cache SHARED across all miner sparse-trie spawns. Previously
            // each spawn built `ChangesetCache::default()` (a fresh, empty cache), so any overlay
            // that needed trie reverts (anchor below db_tip) missed and recomputed changesets from
            // the DB on every block — the dominant cause of the occasional 100-280ms "slow root".
            // A single cache reused across the miner's consecutive blocks warms up: the first block
            // computes the reverts (from DB) and caches them; later blocks reuse them. Cloning
            // shares the underlying Arc<RwLock<..>>; `evict` (interior-mutable) bounds growth.
            const MINER_CHANGESET_RETENTION_BLOCKS: u64 = 256;
            let miner_changeset_cache = ChangesetCache::new();
            let spawn_fn: crate::shared::SparseTrieSpawnFn = std::sync::Arc::new(
                move |parent_hash: alloy_primitives::B256,
                      parent_state_root: alloy_primitives::B256| {
                    // Walk the in-memory canonical chain to find the on-disk anchor.
                    // Without this, proof workers fail with `BlockHashNotFound` whenever
                    // the parent hasn't been persisted yet (the common case during fast
                    // block production with last_persisted_number lagging head).
                    //
                    // Mirrors `payload_validator::get_parent_lazy_overlay` from upstream
                    // reth, which the engine's own PayloadProcessor uses.
                    //
                    // `canonical_in_memory_state` is published from main.rs after launch
                    // (it's only reachable on the concrete BlockchainProvider). If for
                    // some reason it's not yet set (race during early startup), fall back
                    // to anchoring at parent_hash directly — proof workers will fail and
                    // builder will use the synchronous state_root_with_updates path.
                    let (anchor_hash, lazy_overlay) = if let Some(cim) =
                        crate::shared::get_canonical_in_memory_state()
                    {
                        // Bound the long-lived cache: drop entries well below finalized. The
                        // overlay only ever needs reverts within [finalized-RETENTION .. head], so
                        // this keeps the working set hot while preventing unbounded growth over a
                        // long run. evict() takes &self (RwLock), so it composes with the shared
                        // clone handed to the OverlayBuilder below.
                        if let Some(finalized) = cim.get_finalized_num_hash() {
                            miner_changeset_cache.evict(
                                finalized.number.saturating_sub(MINER_CHANGESET_RETENTION_BLOCKS),
                            );
                        }
                        match cim.state_by_hash(parent_hash) {
                            Some(state) => {
                                // chain() yields newest-to-oldest including self, exactly
                                // the order LazyOverlay::new requires.
                                let blocks: Vec<ExecutedBlock<crate::BscPrimitives>> =
                                    state.chain().map(|bs| bs.block()).collect();
                                // Anchor = parent of the oldest in-memory block (= on-disk tip).
                                let anchor = blocks
                                    .last()
                                    .map(|b| b.recovered_block().parent_hash())
                                    .unwrap_or(parent_hash);
                                // Instrumentation: the in-memory overlay depth (= head - on-disk
                                // tip ≈ persist lag) the proof workers must reconstruct over. We
                                // suspect a deep overlay (and the per-spawn empty ChangesetCache
                                // above) is what makes the occasional root slow — NOT tx count.
                                // Correlate this with bsc_builder_state_root_wait_duration_seconds.
                                metrics::histogram!("bsc_miner_overlay_depth")
                                    .record(blocks.len() as f64);
                                metrics::counter!("bsc_miner_sparse_trie_anchor_inmemory_total")
                                    .increment(1);
                                (anchor, Some(LazyOverlay::new(blocks)))
                            }
                            None => {
                                // Parent already persisted — anchor directly, no overlay.
                                metrics::counter!("bsc_miner_sparse_trie_anchor_persisted_total")
                                    .increment(1);
                                (parent_hash, None)
                            }
                        }
                    } else {
                        metrics::counter!("bsc_miner_sparse_trie_anchor_nocim_total").increment(1);
                        (parent_hash, None)
                    };

                    let overlay_builder = OverlayBuilder::<crate::BscPrimitives>::new(
                        anchor_hash,
                        // Shared, warm cache (see MINER_CHANGESET_RETENTION_BLOCKS above) instead of
                        // a fresh empty one per spawn — clone shares the underlying store.
                        miner_changeset_cache.clone(),
                    )
                    .with_lazy_overlay(lazy_overlay);

                    let overlay_factory = OverlayStateProviderFactory::new(
                        provider.clone(),
                        overlay_builder,
                    );
                    // R1: lazily build (once) the PayloadProcessor on the engine's shared
                    // Runtime, falling back to a dedicated one if not yet published.
                    let payload_processor = pp_cell.get_or_init(|| {
                        let runtime = reth_tasks::shared_engine_runtime().unwrap_or_else(|| {
                            tracing::warn!(
                                target: "bsc::miner",
                                "engine Runtime not yet published; building a dedicated sparse-trie \
                                 Runtime (proof pools NOT shared with the engine this run)"
                            );
                            RuntimeBuilder::new(RuntimeConfig::default().with_tokio(
                                TokioConfig::existing_handle(tokio_handle.clone()),
                            ))
                            .build()
                            .expect("failed to build fallback sparse-trie Runtime")
                        });
                        std::sync::Arc::new(PayloadProcessor::new(
                            runtime,
                            bsc_evm_config.clone(),
                            tree_config_for_closure.as_ref(),
                            PrecompileCacheMap::default(),
                        ))
                    });
                    // Instrumentation: time the spawn itself. spawn_state_root creates the proof
                    // worker pools and kicks off overlay/proof work per block (3000+ spawns/run);
                    // a slow spawn points at per-block worker-pool churn / overlay setup rather
                    // than tx execution.
                    let spawn_start = std::time::Instant::now();
                    let handle = payload_processor.spawn_state_root(
                        overlay_factory,
                        parent_state_root,
                        false, // halve_workers
                        tree_config_for_closure.as_ref(),
                    );
                    metrics::histogram!("bsc_miner_sparse_trie_spawn_duration_seconds")
                        .record(spawn_start.elapsed().as_secs_f64());
                    Some(handle)
                },
            );

            if crate::shared::set_sparse_trie_spawn_fn(spawn_fn).is_err() {
                tracing::warn!(
                    "Sparse-trie spawner already registered, keeping existing one"
                );
            } else {
                info!(
                    "Sparse-trie state-root spawner registered \
                     (use_sparse_trie_state_root=true, triedb=inactive)"
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
