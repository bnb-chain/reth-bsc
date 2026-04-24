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

            ctx.task_executor().spawn_critical("bsc-miner-initializer", async move {
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
        ctx.task_executor().spawn_critical("payload-service-handler", async move {
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
