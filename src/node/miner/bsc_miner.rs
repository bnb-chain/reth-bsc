use crate::{
    consensus::parlia::provider::SnapshotProvider,
    node::{
        engine::BscBuiltPayload,
        evm::config::BscEvmConfig,
        miner::{
            payload_builder::BscPayloadBuilder, 
            util::prepare_new_attributes, 
            signer::init_global_signer,
            config::{keystore, MiningConfig}
        },
    },
    BscBlock,
};
use alloy_consensus::BlockHeader;
use alloy_primitives::{Address, B256};
use k256::ecdsa::SigningKey;
use reth::transaction_pool::PoolTransaction;
use reth::transaction_pool::TransactionPool;
use reth_ethereum_payload_builder::EthereumBuilderConfig;
use reth_payload_primitives::BuiltPayload;
use reth_primitives::{SealedBlock, TransactionSigned};
use reth_provider::{BlockNumReader, HeaderProvider, CanonStateSubscriptions, CanonStateNotification};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tracing::{debug, error, info, warn};
use reth_tasks::TaskExecutor;
use reth_basic_payload_builder::{BuildArguments, PayloadConfig};
use reth::payload::EthPayloadBuilderAttributes;
use reth_revm::cancelled::CancelOnDrop;
use reth_primitives_traits::BlockBody;
use crate::node::network::BscNewBlock;
use crate::shared::{get_block_import_sender, get_local_peer_id_or_default};
use alloy_primitives::U128;
use reth_network::message::NewBlockMessage;

pub struct MiningContext {
    parent_header: reth_primitives::SealedHeader,
    parent_snapshot: Arc<crate::consensus::parlia::snapshot::Snapshot>,
}

/// NewWorkWorker responsible for listening to canonical state changes and triggering mining.
pub struct NewWorkWorker<Provider> {
    provider: Provider,
    snapshot_provider: Arc<dyn SnapshotProvider + Send + Sync>,
    validator_address: Address,
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
        provider: Provider,
        snapshot_provider: Arc<dyn SnapshotProvider + Send + Sync>,
        validator_address: Address,
        mining_queue_tx: mpsc::UnboundedSender<MiningContext>,
    ) -> Self {
        Self {
            provider,
            snapshot_provider,
            validator_address,
            mining_queue_tx,
        }
    }

    pub async fn run(self) {
        info!("Succeed to spawn trigger worker");
        let mut notifications = self.provider.canonical_state_stream();
        
        loop {
            tokio::select! {
                notification = notifications.next() => {
                    match notification {
                        Some(event) => {
                            self.try_new_work(event.clone()).await;
                        }
                        None => {
                            warn!("Canonical state notification stream ended, exiting...");
                            break;
                        }
                    }
                }
            }
        }
    }

    async fn try_new_work(&self, new_state: CanonStateNotification<Provider::Primitives>) {
        // todo: refine it as pre cache to speedup, committed.execution_outcome().
        let committed = new_state.committed();
        let tip = committed.tip();

        
        debug!(
            "try new work, tip_block={}, committed_blocks={}",
            committed.tip().number(),
            committed.len()
        );
        
        // todo: refine check is_syncing status.
        if tip.timestamp() < SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() - 3 {
            debug!("Skip to mine new block due to maybe in syncing, validator: {}, tip: {}", self.validator_address, tip.number());
            return;
        }
        
        let parent_header = match self.provider.sealed_header(tip.number()) {
            Ok(Some(header)) => header,
            Ok(None) => {
                debug!("Skip to mine new block due to head block header not found, validator: {}, tip: {}", self.validator_address, tip.number());
                return;
            }
            Err(e) => {
                debug!("Skip to mine new block due to error getting header,validator: {}, tip: {}, due to {}", self.validator_address, tip.number(), e);
                return;
            }
        };

        let parent_snapshot = match self.snapshot_provider.snapshot(tip.number()) {
            Some(snapshot) => snapshot,
            None => {
                debug!("Skip to mine new block due to no snapshot available, validator: {}, tip: {}", self.validator_address, tip.number());
                return;
            }
        };
        
        if !parent_snapshot.validators.contains(&self.validator_address) {
            debug!("Skip to mine new block due to not authorized validator: {}, tip: {}", self.validator_address, tip.number());
            return;
        }
        
        if parent_snapshot.sign_recently(self.validator_address) {
            debug!("Skip to mine new block due to signed recently, validator: {}, tip: {}", self.validator_address, tip.number());
            return;
        }
        
        // TODO: remove it later, now just for easy to debug.
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

/// Miner that handles block production for BSC.
pub struct BscMiner<Pool, Provider> {
    pool: Pool,
    provider: Provider,
    snapshot_provider: Arc<dyn SnapshotProvider + Send + Sync>,
    validator_address: Address,
    chain_spec: Arc<crate::chainspec::BscChainSpec>,
    parlia: Arc<crate::consensus::parlia::Parlia<crate::chainspec::BscChainSpec>>,
    signing_key: Option<SigningKey>,
    mining_config: MiningConfig,
    task_executor: TaskExecutor,
    mining_queue_tx: mpsc::UnboundedSender<MiningContext>,
    mining_queue_rx: Option<mpsc::UnboundedReceiver<MiningContext>>,
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
        let signing_key = if mining_config.is_mining_enabled() {
            let key = if let Some(keystore_path) = &mining_config.keystore_path {
                let password = mining_config.keystore_password.as_deref().unwrap_or("");
                keystore::load_private_key_from_keystore(keystore_path, password)?
            } else if let Some(hex_key) = &mining_config.private_key_hex {
                keystore::load_private_key_from_hex(hex_key)?
            } else {
                return Err("No signing key configured".into());
            };

            // Derive validator address from the signing key and prefer it
            let derived_address = keystore::get_validator_address(&key);
            if derived_address != validator_address {
                if validator_address != Address::ZERO {
                    warn!(
                        "Validator address mismatch: configured={}, derived={}",
                        validator_address, derived_address
                    );
                }
                info!("Succeed to derived address from private key, address: {}", derived_address);
                validator_address = derived_address;
            }

            Some(key)
        } else {
            None
        };

        let (mining_queue_tx, mining_queue_rx) = mpsc::unbounded_channel::<MiningContext>();
        let miner = Self {
            pool: pool.clone(),
            provider: provider.clone(),
            snapshot_provider: snapshot_provider.clone(),
            validator_address,
            chain_spec: chain_spec.clone(),
            parlia: Arc::new(crate::consensus::parlia::Parlia::new(chain_spec.clone(), 200)),
            signing_key,
            mining_config: mining_config.clone(),
            task_executor: task_executor.clone(),
            mining_queue_tx,
            mining_queue_rx: Some(mining_queue_rx),
        };

        info!("Succeed to new miner instance, address: {}", validator_address);
        Ok(miner)
    }

    pub async fn start(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if !self.mining_config.is_mining_enabled() {
            info!("Skip to start mining due to miner is disabled");
            return Ok(());
        }

        if let Some(ref signing_key) = self.signing_key {
            let private_key_bytes = signing_key.as_nonzero_scalar().to_bytes();
            let private_key = B256::from_slice(&private_key_bytes);
            if let Err(e) = init_global_signer(private_key) {
                return Err(format!("Failed to initialize global signer due to {}", e).into());
            } else {
                info!("Succeed to initialize global signer");
            }
        } else {
            return Err("No signing key available, global signer not initialized".into());
        }

        self.spawn_workers().await?;

        info!("Succeed to start mining, address: {}", self.validator_address);
        Ok(())
    }

    async fn spawn_workers(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let new_work_worker = NewWorkWorker::new(
            self.provider.clone(),
            self.snapshot_provider.clone(),
            self.validator_address,
            self.mining_queue_tx.clone(),
        );
        self.task_executor.spawn_critical("new_work_eventloop", new_work_worker.run());

        // TODO: add more eventloop workers for performance like bsc miner.
        if let Some(mut mining_queue_rx) = self.mining_queue_rx.take() {
            let pool = self.pool.clone();
            let provider = self.provider.clone();
            let chain_spec = self.chain_spec.clone();
            let parlia = self.parlia.clone();
            let validator_address = self.validator_address;

            self.task_executor.spawn_critical("mining_worker", async move {
                info!("Succeed to spawn mining worker, address: 0x{:x}", validator_address);
                
                while let Some(mining_ctx) = mining_queue_rx.recv().await {
                    let next_block = mining_ctx.parent_header.number() + 1;
                    debug!("Received mining context, next_block: {}", next_block);

                     match Self::try_mine_block(
                         pool.clone(),
                         provider.clone(), 
                         chain_spec.clone(),
                         parlia.clone(),
                         validator_address,
                         mining_ctx
                     ).await {
                        Ok(()) => {
                            debug!("Succeed to mine block, next_block: {}", next_block);
                        }
                        Err(e) => {
                            error!("Failed to mine block due to {}, next_block: {}", e, next_block);
                        }
                    }
                }
                
                warn!("Mining worker stopped");
            });
        } else {
            warn!("Mining queue receiver not available");
        }

        Ok(())
    }

     async fn try_mine_block<P, Pr>(
        pool: P,
        provider: Pr,
        chain_spec: Arc<crate::chainspec::BscChainSpec>,
        parlia: Arc<crate::consensus::parlia::Parlia<crate::chainspec::BscChainSpec>>,
        validator_address: Address,
        mining_ctx: MiningContext,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
    where
        P: TransactionPool<Transaction: PoolTransaction<Consensus = TransactionSigned>> + Clone + Send + Sync,
        Pr: HeaderProvider<Header = alloy_consensus::Header>
            + BlockNumReader
            + reth_provider::StateProviderFactory
            + CanonStateSubscriptions
            + Clone
            + Send
            + Sync,
    {
        let attributes = prepare_new_attributes(
            parlia.clone(), 
            &mining_ctx.parent_snapshot, 
            &mining_ctx.parent_header, 
            validator_address
        );

        let evm_config = BscEvmConfig::new(chain_spec.clone());
        let payload_builder = BscPayloadBuilder::new(
            provider.clone(), 
            pool.clone(), 
            evm_config, 
            EthereumBuilderConfig::new()
        );
        let payload = payload_builder.build_payload(BuildArguments::<EthPayloadBuilderAttributes, BscBuiltPayload>::new(
            reth_revm::cached::CachedReads::default(),
            PayloadConfig::new(Arc::new(mining_ctx.parent_header.clone()), attributes),
            CancelOnDrop::default(),
            None,
        ))?;

        info!("Start to submit block: {} (hash: 0x{:x}, txs: {})", 
            payload.block().header().number(),
            payload.block().hash(),
            payload.block().body().transaction_count()
        );

        Self::submit_block(payload.block(), mining_ctx, provider, parlia).await?;

        info!("Succeed to mine and submit, block: {}", payload.block().header().number());
        Ok(())
    }

    /// todo: check and refine.
    async fn submit_block<Pr>(
        sealed_block: &SealedBlock<BscBlock>,
        mining_ctx: MiningContext,
        provider: Pr,
        parlia: Arc<crate::consensus::parlia::Parlia<crate::chainspec::BscChainSpec>>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> 
    where
        Pr: HeaderProvider<Header = alloy_consensus::Header>
            + BlockNumReader
            + reth_provider::StateProviderFactory
            + CanonStateSubscriptions
            + Clone
            + Send
            + Sync,
    {
        // now is focus on the basic workflow.
        // TODO: refine it later. https://github.com/bnb-chain/bsc/blob/master/consensus/parlia/parlia.go#L1702.
        let present_timestamp = parlia.present_timestamp();
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
        let parent_td = provider.header_td_by_number(parent_number)
            .map_err(|e| format!("Failed to get parent total difficulty: {}", e))?
            .unwrap_or_default();
        let current_difficulty = sealed_block.header().difficulty();
        let new_td = parent_td + current_difficulty;
        
        let td = U128::from(new_td.to::<u128>());
        let block_hash = sealed_block.hash();
        let new_block = BscNewBlock(reth_eth_wire::NewBlock { block: sealed_block.clone_block(), td });
        let msg = NewBlockMessage { hash: block_hash, block: Arc::new(new_block) };

        if let Some(sender) = get_block_import_sender() {
            // todo: check announce_new_block/announce_new_block_hash.
            // todo: check engine-api re-execute and p2p.
            let peer_id = get_local_peer_id_or_default();
            let incoming: crate::node::network::block_import::service::IncomingBlock =
                (msg, peer_id);
            if sender.send(incoming).is_err() {
                warn!("Failed to send mined block to import service: channel closed");
            } else {
                debug!("Succeed to send mined block to import service");
            }
        } else {
            warn!("Failed to send mined block due to import sender not initialised");
        }

        Ok(())
    }

}