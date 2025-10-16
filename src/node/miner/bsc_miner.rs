use crate::{
    consensus::parlia::provider::SnapshotProvider,
    node::{
        engine::BscBuiltPayload,
        evm::config::BscEvmConfig,
        miner::{
            payload_builder::BscPayloadBuilder, 
            util::prepare_new_attributes, 
            signer::init_global_signer_from_k256,
            config::{keystore, MiningConfig}
        },
    },
    BscBlock,
};
use alloy_consensus::BlockHeader;
use alloy_primitives::{Address, Sealable};
use k256::ecdsa::SigningKey;
use reth::transaction_pool::PoolTransaction;
use reth::transaction_pool::TransactionPool;
use reth_ethereum_payload_builder::EthereumBuilderConfig;
use reth_payload_primitives::BuiltPayload;
use reth_primitives::{SealedBlock, TransactionSigned};
use reth_primitives_traits::SealedHeader;
use reth_provider::{BlockNumReader, HeaderProvider, CanonStateSubscriptions};
use reth_tasks::TaskExecutor;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tracing::{debug, error, info, warn};
use reth_basic_payload_builder::{BuildArguments, PayloadConfig};
use reth::payload::EthPayloadBuilderAttributes;
use reth_revm::cancelled::CancelOnDrop;
use reth_primitives_traits::BlockBody;
use crate::node::network::BscNewBlock;
use crate::shared::{get_block_import_sender, get_local_peer_id_or_default};
use alloy_primitives::U128;
use reth_network::message::NewBlockMessage;
use reth_ethereum_engine_primitives::BlobSidecars;
use crate::node::primitives::BscBlobTransactionSidecar;

pub struct MiningContext {
    parent_header: reth_primitives::SealedHeader,
    parent_snapshot: Arc<crate::consensus::parlia::snapshot::Snapshot>,
}

/// NewWorkWorker responsible for listening to canonical state changes and triggering mining.
pub struct NewWorkWorker<Provider> {
    validator_address: Address,
    provider: Provider,
    snapshot_provider: Arc<dyn SnapshotProvider + Send + Sync>,
    mining_queue_tx: mpsc::UnboundedSender<MiningContext>,
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
    ) -> Self {
        Self {
            validator_address,
            provider,
            snapshot_provider,
            mining_queue_tx,
        }
    }

    pub async fn run(self) {
        info!("Succeed to spawn new work worker, address: {}", self.validator_address);
        
        if let Some(tip_header) = self.get_tip_header_at_startup() {
            debug!("try to mine block at startup, block_number: {}", tip_header.number() + 1);
            self.try_new_work(&tip_header).await;
        }
        
        let mut notifications = self.provider.canonical_state_stream();
        loop {
            match notifications.next().await {
                Some(event) => {
                    // todo: refine it as pre cache to speedup, committed.execution_outcome().
                    let committed = event.committed();
                    let tip = committed.tip();
                    debug!(
                        "try new work, tip_block={}, committed_blocks={}",
                        committed.tip().number(),
                        committed.len()
                    );
                    let tip_header = tip.clone_sealed_header();
                    self.try_new_work(&tip_header).await;
                }
                None => {
                    warn!("Canonical state notification stream ended, exiting...");
                    break;
                }
            }
        }
    }

    fn get_tip_header_at_startup(&self) -> Option<reth_primitives::SealedHeader> {
        let best_number = self.provider.best_block_number().ok()?;
        let tip_header = self.provider.sealed_header(best_number).ok()??;
        Some(tip_header)
    }

    async fn try_new_work<H>(&self, tip: &SealedHeader<H>) 
    where
        H: alloy_consensus::BlockHeader + Sealable,
    {
        // todo: refine check is_syncing status.
        if tip.timestamp() < SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() - 3 {
            debug!("Skip to mine new block due to maybe in syncing, validator: {}, tip: {}", self.validator_address, tip.number());
            return;
        }
        
        let parent_header = match self.provider.sealed_header_by_hash(tip.hash()) {
            Ok(Some(header)) => header,
            Ok(None) => {
                debug!("Skip to mine new block due to head block header not found, validator: {}, tip: {}", self.validator_address, tip.number());
                return;
            }
            Err(e) => {
                debug!("Skip to mine new block due to error getting header, validator: {}, tip: {}, due to {}", self.validator_address, tip.number(), e);
                return;
            }
        };

        let parent_snapshot = match self.snapshot_provider.snapshot_by_hash(&tip.hash()) {
            Some(snapshot) => snapshot,
            None => {
                debug!("Skip to mine new block due to no snapshot available, validator: {}, tip: {}", self.validator_address, tip.number());
                return;
            }
        };
        
        if !parent_snapshot.validators.contains(&self.validator_address) {
            debug!("Skip to mine new block due to not authorized, validator: {}, tip: {}", self.validator_address, tip.number());
            return;
        }
        
        if parent_snapshot.sign_recently(self.validator_address) {
            debug!("Skip to mine new block due to signed recently, validator: {}, tip: {}", self.validator_address, tip.number());
            return;
        }
        
        // TODO: remove it later, now just for easy to debug.
        // TODO: support backoff if not inturn.
        if !parent_snapshot.is_inturn(self.validator_address) {
            debug!("Skip to produce due to is not inturn, validator: {}, tip: {}", self.validator_address, tip.number());
            return;
        }

        let mining_ctx = MiningContext {
            parent_header,
            parent_snapshot: Arc::new(parent_snapshot),
        };

        debug!("Queuing mining context, next_block: {}", tip.number() + 1);
        if let Err(e) = self.mining_queue_tx.send(mining_ctx) {
            error!("Failed to send mining context to queue due to {}", e);
        }
    }
}

/// MainWorkWorker responsible for processing mining tasks and block building,
/// submit the seal block to engine-tree and other peers.
pub struct MainWorkWorker<Pool, Provider> {
    validator_address: Address,
    pool: Pool,
    provider: Provider,
    chain_spec: Arc<crate::chainspec::BscChainSpec>,
    parlia: Arc<crate::consensus::parlia::Parlia<crate::chainspec::BscChainSpec>>,
    mining_queue_rx: mpsc::UnboundedReceiver<MiningContext>,
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
    pub fn new(
        validator_address: Address,
        pool: Pool,
        provider: Provider,
        chain_spec: Arc<crate::chainspec::BscChainSpec>,
        parlia: Arc<crate::consensus::parlia::Parlia<crate::chainspec::BscChainSpec>>,
        mining_queue_rx: mpsc::UnboundedReceiver<MiningContext>,
    ) -> Self {
        Self {
            pool,
            provider,
            chain_spec,
            parlia,
            validator_address,
            mining_queue_rx,
        }
    }

    pub async fn run(mut self) {
        info!("Succeed to spawn main work worker, address: {}", self.validator_address);
        
        loop {
            match self.mining_queue_rx.recv().await {
                Some(ctx) => {
                    let next_block = ctx.parent_header.number() + 1;
                    debug!("Received mining context, next_block: {}", next_block);

                    match self.try_mine_block(ctx).await {
                        Ok(()) => {
                            debug!("Succeed to mine block, next_block: {}", next_block);
                        }
                        Err(e) => {
                            error!("Failed to mine block due to {}, next_block: {}", e, next_block);
                        }
                    }
                }
                None => {
                    warn!("Mining queue closed, exiting main work worker");
                    break;
                }
            }
        }
        
        warn!("Mining worker stopped");
    }

    async fn try_mine_block(
        &self,
        mining_ctx: MiningContext,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let attributes = prepare_new_attributes(
            self.parlia.clone(), 
            &mining_ctx.parent_snapshot, 
            &mining_ctx.parent_header, 
            self.validator_address
        );

        let evm_config = BscEvmConfig::new(self.chain_spec.clone());
        let payload_builder = BscPayloadBuilder::new(
            self.provider.clone(), 
            self.pool.clone(), 
            evm_config, 
            EthereumBuilderConfig::new(),
            self.chain_spec.clone(),
        );
        let payload = payload_builder.build_payload(
            BuildArguments::<EthPayloadBuilderAttributes, BscBuiltPayload>::new(
                reth_revm::cached::CachedReads::default(),
                PayloadConfig::new(Arc::new(mining_ctx.parent_header.clone()), attributes),
                CancelOnDrop::default(),
                None,)
        )?;
        info!("Start to submit block: {} (hash: 0x{:x}, txs: {})", 
            payload.block().header().number(),
            payload.block().hash(),
            payload.block().body().transaction_count()
        );

        self.submit_block(payload.block(), mining_ctx, payload.sidecars()).await?;
        info!("Succeed to mine and submit, block: {}", payload.block().header().number());
        Ok(())
    }

    /// todo: check and refine.
    async fn submit_block(
        &self,
        sealed_block: &SealedBlock<BscBlock>,
        mining_ctx: MiningContext,
        sidecars: Option<BlobSidecars>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // now is focus on the basic workflow.
        // TODO: refine it later. https://github.com/bnb-chain/bsc/blob/master/consensus/parlia/parlia.go#L1702.
        let present_timestamp = self.parlia.present_timestamp();
        if sealed_block.header().timestamp > present_timestamp {
            let delay_ms = (sealed_block.header().timestamp - present_timestamp) * 1000;
            tracing::info!(
                target: "bsc::miner",
                block_number = sealed_block.header().number,
                block_timestamp = sealed_block.header().timestamp,
                present_timestamp = present_timestamp,
                delay_ms = delay_ms,
                "Block timestamp is in the future, waiting before submission"
            );
            tokio::time::sleep(tokio::time::Duration::from_millis(delay_ms)).await;
            tracing::debug!(
                target: "bsc::miner",
                block_number = sealed_block.header().number,
                "Finished waiting, proceeding with block submission"
            );
        }

        let parent_number = mining_ctx.parent_header.number();
        let parent_td = self.provider.header_td_by_number(parent_number)
            .map_err(|e| format!("Failed to get parent total difficulty due to {}", e))?
            .unwrap_or_default();
        let current_difficulty = sealed_block.header().difficulty();
        let new_td = parent_td + current_difficulty;
        

        let mut block = sealed_block.clone_block();
        let sidecars_vec = match sidecars {
            Some(reth_ethereum_engine_primitives::BlobSidecars::Eip4844(vec4844)) => {
                let block_number = sealed_block.header().number();
                let block_hash = sealed_block.hash();
        
                let mut sidecar_iter = vec4844.into_iter();
                let mut out = Vec::new();
                for (idx, tx) in block.body.inner.transactions.iter().enumerate() {
                    if tx.is_eip4844() {
                        if let Some(inner) = sidecar_iter.next() {
                            out.push(BscBlobTransactionSidecar {
                                inner,
                                block_number,
                                block_hash,
                                tx_index: idx as u64,
                                tx_hash: tx.hash().clone(),
                            });
                        }
                    }
                }
                out
            }
            _ => Vec::new(),
        };
        block.body.sidecars = if sidecars_vec.is_empty() { None } else { Some(sidecars_vec) };
        let td = U128::from(new_td.to::<u128>());
        let block_hash = sealed_block.hash();
        let new_block = BscNewBlock(reth_eth_wire::NewBlock { block, td });
        debug!("debug submit_block, new_block: {:?} sidecar count: {:?}", new_block, new_block.0.block.body.sidecars.as_ref().map(|s| s.len()).unwrap_or(0));
        let msg = NewBlockMessage { hash: block_hash, block: Arc::new(new_block) };

        if let Some(sender) = get_block_import_sender() {
            let peer_id = get_local_peer_id_or_default();
            let incoming: crate::node::network::block_import::service::IncomingBlock =
                (msg, peer_id);
            if sender.send(incoming).is_err() {
                warn!("Failed to send mined block to import service due to channel closed");
                return Err("Failed to send mined block to import service due to channel closed".into());
            } else {
                debug!("Succeed to send mined block to import service");
            }
        } else {
            warn!("Failed to send mined block due to import sender not initialised");
            return Err("Failed to send mined block due to import sender not initialised".into());
        }

        Ok(())
    }
}

/// Miner that handles block production for BSC.
pub struct BscMiner<Pool, Provider> {
    validator_address: Address,
    signing_key: SigningKey,
    new_work_worker: NewWorkWorker<Provider>,
    main_work_worker: MainWorkWorker<Pool, Provider>,
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

        // We'll derive and trust the validator address from the configured signing key when possible.
        // If not available, fall back to configured address (may be ZERO when disabled).
        let mut validator_address = mining_config.validator_address.unwrap_or(Address::ZERO);
        let signing_key = if let Some(keystore_path) = &mining_config.keystore_path {
            let password = mining_config.keystore_password.as_deref().unwrap_or("");
            keystore::load_private_key_from_keystore(keystore_path, password)?
        } else if let Some(hex_key) = &mining_config.private_key_hex {
            keystore::load_private_key_from_hex(hex_key)?
        } else {
            return Err("No signing key configured".into());
        };
        // Derive validator address from the signing key and prefer it.
        let derived_address = keystore::get_validator_address(&signing_key);
        if derived_address != validator_address {
            if validator_address != Address::ZERO {
                warn!(
                    "Validator address mismatch, configured: {}, derived: {}",
                    validator_address, derived_address
                );
            }
            info!("Succeed to derived address from private key, address: {}", derived_address);
            validator_address = derived_address;
        }
        
        let (mining_queue_tx, mining_queue_rx) = mpsc::unbounded_channel::<MiningContext>();
        let new_work_worker = NewWorkWorker::new(
            validator_address,
            provider.clone(),
            snapshot_provider.clone(),
            mining_queue_tx.clone(),
        );
        let main_work_worker = MainWorkWorker::new(
            validator_address,
            pool.clone(),
            provider.clone(),
            chain_spec.clone(),
            Arc::new(crate::consensus::parlia::Parlia::new(chain_spec.clone(), 200)),
            mining_queue_rx,
        );
        
        let miner = Self {
            validator_address,
            signing_key,
            new_work_worker,
            main_work_worker,
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
        self.task_executor.spawn_critical("new_work_worker", self.new_work_worker.run());
        self.task_executor.spawn_critical("main_work_worker", self.main_work_worker.run());
        info!("Succeed to start mining, address: {}", self.validator_address);
        Ok(())
    }

}
