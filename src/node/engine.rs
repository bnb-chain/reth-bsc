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

        // Register the sparse-trie state-root spawner, if enabled.
        //
        // We construct a long-lived `PayloadProcessor` keyed to a fresh `BscEvmConfig`
        // (built from chain_spec — same source as the rest of the BSC pipeline). For
        // each build job, the registered closure constructs a one-shot
        // `OverlayStateProviderFactory` anchored at the parent block hash and calls
        // `spawn_state_root` to get a `StateRootHandle`.
        //
        // `TreeConfig::default()` is used here as a workaround — the engine-launch
        // `TreeConfig` (which honors `--engine.*` CLI flags) is not reachable from
        // `BscPayloadServiceBuilder`. Default values are reasonable for the sparse-trie
        // background task; revisit if performance testing shows we need CLI-tunable
        // worker counts or cache sizes.
        if mining_config.use_sparse_trie_state_root
            && !rust_eth_triedb::triedb_manager::is_triedb_active()
        {
            use alloy_consensus::BlockHeader;
            use reth_chain_state::LazyOverlay;
            use reth_engine_tree::tree::{
                payload_processor::PayloadProcessor, precompile_cache::PrecompileCacheMap,
                TreeConfig,
            };
            use reth_provider::providers::{OverlayBuilder, OverlayStateProviderFactory};
            use reth_tasks::{Runtime, RuntimeBuilder, RuntimeConfig, TokioConfig};
            use reth_trie_db::ChangesetCache;

            let chain_spec = Arc::new(ctx.config().chain.clone().as_ref().clone());
            let bsc_evm_config = crate::node::evm::config::BscEvmConfig::new(chain_spec);
            let tree_config = Arc::new(TreeConfig::default());
            let provider = ctx.provider().clone();

            // Build a dedicated `reth_tasks::Runtime` for the sparse-trie pools,
            // attached to the existing tokio handle so we don't spin up a second tokio
            // executor. Rayon pools default to `available_parallelism()`, matching what
            // the engine uses for its own PayloadProcessor.
            //
            // TODO: in a follow-up, share the engine's Runtime instead of constructing
            // a parallel one — this currently means two sets of rayon pools competing
            // for CPU. Acceptable for a first cut; revisit if perf testing shows
            // contention.
            let tokio_handle = ctx.task_executor().handle().clone();
            let runtime = RuntimeBuilder::new(
                RuntimeConfig::default().with_tokio(TokioConfig::existing_handle(tokio_handle)),
            )
            .build()
            .map_err(|e| eyre::eyre!("failed to build sparse-trie Runtime: {e}"))?;
            let _: &Runtime = &runtime; // type-check anchor

            let payload_processor = std::sync::Arc::new(PayloadProcessor::new(
                runtime,
                bsc_evm_config,
                tree_config.as_ref(),
                PrecompileCacheMap::default(),
            ));

            let tree_config_for_closure = tree_config.clone();
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
                                (anchor, Some(LazyOverlay::new(blocks)))
                            }
                            None => {
                                // Parent already persisted — anchor directly, no overlay.
                                (parent_hash, None)
                            }
                        }
                    } else {
                        (parent_hash, None)
                    };

                    let overlay_builder = OverlayBuilder::<crate::BscPrimitives>::new(
                        anchor_hash,
                        ChangesetCache::default(),
                    )
                    .with_lazy_overlay(lazy_overlay);

                    let overlay_factory = OverlayStateProviderFactory::new(
                        provider.clone(),
                        overlay_builder,
                    );
                    Some(payload_processor.spawn_state_root(
                        overlay_factory,
                        parent_state_root,
                        false, // halve_workers
                        tree_config_for_closure.as_ref(),
                    ))
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
