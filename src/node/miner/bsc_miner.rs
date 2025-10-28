use crate::{
    consensus::parlia::provider::SnapshotProvider,
    node::{
        engine::BscBuiltPayload,
        evm::config::BscEvmConfig,
        miner::{
            payload::{BscPayloadBuilder, BscPayloadJob, BscPayloadJobHandle}, 
            util::prepare_new_attributes, 
            signer::init_global_signer_from_k256,
            config::{keystore, MiningConfig}
        },
        network::BscNewBlock,
    },
    shared::{get_block_import_sender, get_local_peer_id_or_default},
};
use alloy_consensus::BlockHeader;
use alloy_primitives::{Address, Sealable};
use k256::ecdsa::SigningKey;
use reth::transaction_pool::PoolTransaction;
use reth::transaction_pool::TransactionPool;
use reth_ethereum_payload_builder::EthereumBuilderConfig;
use reth_payload_primitives::BuiltPayload;
use reth_primitives::TransactionSigned;
use reth_primitives_traits::{SealedHeader, BlockBody};
use reth_provider::{BlockNumReader, HeaderProvider, CanonStateSubscriptions};
use reth_tasks::TaskExecutor;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_stream::StreamExt;
use tracing::{debug, error, info, warn};
use reth_basic_payload_builder::PayloadConfig;
use crate::node::miner::payload::BscBuildArguments;
use reth_revm::cancelled::ManualCancel;
use alloy_primitives::U128;
use reth_network::message::{NewBlockMessage, PeerMessage};
use alloy_rlp::Encodable;
use alloy_primitives::keccak256;
use std::sync::Mutex;
use lru::LruCache;

/// Maximum number of recently mined blocks to track for double signing prevention
const RECENT_MINED_BLOCKS_CACHE_SIZE: usize = 100;

#[derive(Clone)]
pub struct MiningContext {
    pub header: Option<reth_primitives::Header>, // tmp header for payload building.
    pub parent_header: reth_primitives::SealedHeader,
    pub parent_snapshot: Arc<crate::consensus::parlia::snapshot::Snapshot>,
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
            debug!("Try new work at startup, tip_block={}", tip_header.number());
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
                        "Try new work, tip_block={}, hash={}, parent_hash={}, miner={}, diff={}, committed_blocks={}",
                        committed.tip().number(),
                        format!("0x{:x}", committed.tip().hash()),
                        format!("0x{:x}", committed.tip().parent_hash()),
                        committed.tip().beneficiary(),
                        committed.tip().difficulty(),
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
        
        if !parent_snapshot.is_inturn(self.validator_address) {
            debug!("Try off-turn mining, validator: {}, next_block: {}", self.validator_address, tip.number() + 1);
        }

        let mining_ctx = MiningContext {
            header: None,
            parent_header,
            parent_snapshot: Arc::new(parent_snapshot),
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
    payload_job_join_set: JoinSet<Result<(), Box<dyn std::error::Error + Send + Sync>>>,
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
        payload_tx: mpsc::UnboundedSender<SubmitContext>,
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
            payload_job_join_set: JoinSet::new(),
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
                
                _ = tokio::time::sleep(std::time::Duration::from_millis(200)) => {
                    self.check_payload_job_results().await;
                }
            }
        }
        
        warn!("Mining worker stopped");
    }

    async fn try_mine_block(
        &mut self,
        mut mining_ctx: MiningContext,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if let Some(handle) = self.running_job_handle.take() {
            handle.abort();
        }
        
        let parent_snapshot = mining_ctx.parent_snapshot.clone();
        let parent_header = mining_ctx.parent_header.clone();
        let block_number = parent_header.number() + 1;
        let attributes = prepare_new_attributes(
            &mut mining_ctx,
            self.parlia.clone(), 
            &parent_snapshot, 
            &parent_header, 
            self.validator_address
        );

        let evm_config = BscEvmConfig::new(self.chain_spec.clone());
        let payload_builder = BscPayloadBuilder::new(
            self.provider.clone(), 
            self.pool.clone(), 
            evm_config, 
            EthereumBuilderConfig::new(),
            self.chain_spec.clone(),
            self.parlia.clone(),
        );
        let build_args = BscBuildArguments {
            cached_reads: reth_revm::cached::CachedReads::default(),
            config: PayloadConfig::new(Arc::new(mining_ctx.parent_header.clone()), attributes),
            cancel: ManualCancel::default(),
        };
                
        let (payload_job, job_handle) = BscPayloadJob::new(
            self.parlia.clone(), 
            mining_ctx,
            payload_builder, 
            build_args, 
            self.payload_tx.clone()
        );
        
        let start_time = std::time::Instant::now();
        self.running_job_handle = Some(job_handle);
        self.payload_job_join_set.spawn(async move {
            payload_job.start().await
        });
        debug!("Succeed to async start payload job, cost_time: {:?}, block_number: {}",
            start_time.elapsed(), block_number);
        
        Ok(())
    }

    /// Check and print completed payload job tasks results
    pub async fn check_payload_job_results(&mut self) {
        while let Some(result) = self.payload_job_join_set.try_join_next() {
            match result {
                Ok(Ok(())) => {
                    debug!("Succeed to execute payload job");
                }
                Ok(Err(e)) => {
                    warn!("Failed to execute payload job due to {}", e);
                }
                Err(join_err) => {
                    error!("Failed to execute payload job due to task panicked or was cancelled, join_err: {}", join_err);
                }
            }
        }
    }

}

/// Worker responsible for submitting the seal block to engine-tree and other peers.
pub struct ResultWorkWorker<Provider> {
    /// Validator address
    validator_address: Address,
    /// Provider for blockchain data
    provider: Provider,
    /// Parlia consensus engine
    parlia: Arc<crate::consensus::parlia::Parlia<crate::chainspec::BscChainSpec>>,
    /// Receiver for built payloads
    payload_rx: mpsc::UnboundedReceiver<SubmitContext>,
    /// Receiver for delayed payloads
    delay_submit_rx: mpsc::UnboundedReceiver<BscBuiltPayload>,
    /// Sender for delayed payloads
    delay_submit_tx: mpsc::UnboundedSender<BscBuiltPayload>,
    /// LRU cache to track recently mined blocks to prevent double signing
    recent_mined_blocks: Arc<Mutex<LruCache<u64, Vec<alloy_primitives::B256>>>>,
}

impl<Provider> ResultWorkWorker<Provider>
where
    Provider: HeaderProvider + BlockNumReader + Send + Sync + Clone + 'static,
{
    /// Creates a new ResultWorkWorker instance
    pub fn new(
        validator_address: Address,
        provider: Provider,
        parlia: Arc<crate::consensus::parlia::Parlia<crate::chainspec::BscChainSpec>>,
        payload_rx: mpsc::UnboundedReceiver<SubmitContext>,
    ) -> Self {
        let (delay_submit_tx, delay_submit_rx) = mpsc::unbounded_channel::<BscBuiltPayload>();
        let recent_mined_blocks = Arc::new(Mutex::new(LruCache::new(std::num::NonZeroUsize::new(RECENT_MINED_BLOCKS_CACHE_SIZE).unwrap())));
        Self {
            validator_address,
            provider,
            parlia,
            payload_rx,
            delay_submit_tx,
            delay_submit_rx,
            recent_mined_blocks,
        }
    }

    /// Create and start a delay submit task
    fn start_delay_task(
        payload: BscBuiltPayload,
        delay_ms: u64,
        delay_submit_tx: mpsc::UnboundedSender<BscBuiltPayload>,
        cancel: ManualCancel,
    ) {
        tokio::spawn(async move {
            tokio::time::sleep(tokio::time::Duration::from_millis(delay_ms)).await;
            if !cancel.is_cancelled() {
                if let Err(e) = delay_submit_tx.send(payload) {
                    error!("Failed to send delayed payload to channel: {}", e);
                }
            } else {
                debug!("Delay submit task is cancelled, block_hash: {}, block_number: {}", 
                    payload.block().hash(), payload.block().number());
            }
        });
    }

    /// Run the result worker to process and submit payloads
    pub async fn run(mut self) {
        info!("Starting ResultWorkWorker for validator: {}", self.validator_address);

        loop {
            tokio::select! {
                submit_ctx = self.payload_rx.recv() => {
                    match submit_ctx {
                        Some(submit_ctx) => {
                            let payload = submit_ctx.payload;
                            let block_number = payload.block().number();
                            let block_hash = payload.block().hash();
                            let delay_ms = self.parlia.delay_for_ramanujan_fork(&submit_ctx.mining_ctx.parent_snapshot, payload.block().header());
                            debug!("Check submit delay, block {} (hash: 0x{:x}), delay_ms: {}", block_number, block_hash, delay_ms);
                            if delay_ms == 0 {
                                match self.submit_payload(payload).await {
                                    Ok(()) => {
                                        info!("Succeed to submit block {} (hash: 0x{:x})", block_number, block_hash);
                                    }
                                    Err(e) => {
                                        error!("Failed to submit block {} (hash: 0x{:x}): {}", block_number, block_hash, e);
                                    }
                                }
                            } else {
                                Self::start_delay_task(
                                    payload,
                                    delay_ms,
                                    self.delay_submit_tx.clone(),
                                    submit_ctx.cancel.clone(),
                                );
                                info!(
                                    "Block {} scheduled for delayed submission in {}ms",
                                    block_number, delay_ms
                                );
                            }
                        }
                        None => {
                            warn!("Main payload channel closed, stopping ResultWorkWorker");
                            break;
                        }
                    }
                }
                
                delayed_payload = self.delay_submit_rx.recv() => {
                    match delayed_payload {
                        Some(payload) => {
                            let block_number = payload.block().number();
                            let block_hash = payload.block().hash();                            
                            match self.submit_payload(payload).await {
                                Ok(()) => {
                                    info!("Succeed to submit delayed block {} (hash: 0x{:x})", block_number, block_hash);
                                }
                                Err(e) => {
                                    error!("Failed to submit delayed block {} (hash: 0x{:x}): {}", block_number, block_hash, e);
                                }
                            }
                        }
                        None => {
                            warn!("Delay payload channel closed, stopping ResultWorkWorker");
                            break;
                        }
                    }
                }
            }
        }

        warn!("ResultWorkWorker stopped");
    }

    /// Submit a built payload to the engine-tree/network
    async fn submit_payload(&self, payload: BscBuiltPayload) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let sealed_block = payload.block();
        let block_hash = sealed_block.hash();
        let block_number = sealed_block.number();
        let parent_hash = sealed_block.header().parent_hash;
        if block_number <= self.provider.chain_info()?.best_number {
            debug!("Skip to submit block due to block number is less than last block number, block_number: {}, last_block_number: {}", 
                block_number, self.provider.last_block_number()?);
            return Ok(());
        }

        {   // check double sign
            let mut cache = self.recent_mined_blocks.lock().unwrap();
            if let Some(prev_parents) = cache.get(&block_number) {
                let mut double_sign = false;
                for prev_parent in prev_parents {
                    if *prev_parent == parent_hash {
                        error!("Reject Double Sign!! block: {}, hash: 0x{:x}, root: 0x{:x}, ParentHash: 0x{:x}", 
                            block_number, block_hash, sealed_block.header().state_root, parent_hash);
                        double_sign = true;
                        break;
                    }
                }
                if double_sign {
                    return Ok(());
                }
                let mut updated_parents = prev_parents.clone();
                updated_parents.push(parent_hash);
                cache.put(block_number, updated_parents);
            } else {
                cache.put(block_number, vec![parent_hash]);
            }
        }

        let block_hash = sealed_block.hash();
        let difficulty = sealed_block.header().difficulty();
        let turn_status = if difficulty == crate::consensus::parlia::constants::DIFF_INTURN { 
            "inturn" 
        } else { 
            "offturn" 
        };
        debug!("Submitting block {} (hash: 0x{:x}, parent_hash: 0x{:x}, txs: {}, gas_used: {}, turn_status: {})", 
               block_number, 
               block_hash, 
               parent_hash,
               sealed_block.body().transaction_count(),
               sealed_block.gas_used(),
               turn_status);

        // TODO: wait more times when huge chain import.
        // TODO: only canonical head can broadcast, avoid sidechain blocks.
        let parent_number = block_number.saturating_sub(1);
        let parent_td = self.provider.header_td_by_number(parent_number)
            .map_err(|e| format!("Failed to get parent total difficulty due to {}", e))?
            .unwrap_or_default();
        let current_difficulty = sealed_block.header().difficulty();
        let new_td = parent_td + current_difficulty;
        
        let td = U128::from(new_td.to::<u128>());
        let new_block = BscNewBlock(reth_eth_wire::NewBlock { 
            block: sealed_block.clone_block(), 
            td 
        });
        let msg = NewBlockMessage { 
            hash: block_hash, 
            block: Arc::new(new_block) 
        };

        // TODO: just commit state, handle FCU and send block to P2P, it avoid re-execution again.
        if let Some(sender) = get_block_import_sender() {
            let peer_id = get_local_peer_id_or_default();
            let incoming: crate::node::network::block_import::service::IncomingBlock =
                (msg.clone(), peer_id);
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

        // Targeted ETH NewBlock/NewBlockHashes to EVN peers for full broadcast parity.
        if let Some(net) = crate::shared::get_network_handle() {
            let peers = crate::node::network::evn_peers::snapshot();
            let nb_msg = msg.clone();
            for (peer_id, info) in peers {
                if info.is_evn {
                    // Send full NewBlock to EVN peers
                    net.send_eth_message(peer_id, PeerMessage::NewBlock(nb_msg.clone()));
                }
            }
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
    result_work_worker: ResultWorkWorker<Provider>,
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
        let (payload_tx, payload_rx) = mpsc::unbounded_channel::<SubmitContext>();
        
        let new_work_worker = NewWorkWorker::new(
            validator_address,
            provider.clone(),
            snapshot_provider.clone(),
            mining_queue_tx.clone(),
        );
        
        let parlia = Arc::new(crate::consensus::parlia::Parlia::new(chain_spec.clone(), 200));
        let main_work_worker = MainWorkWorker::new(
            validator_address,
            pool.clone(),
            provider.clone(),
            chain_spec.clone(),
            parlia.clone(),
            mining_queue_rx,
            payload_tx,
        );
        
        let result_work_worker = ResultWorkWorker::new(
            validator_address,
            provider.clone(),
            parlia.clone(),
            payload_rx,
        );
        
        let miner = Self {
            validator_address,
            signing_key,
            new_work_worker,
            main_work_worker,
            result_work_worker,
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
        // EVN: Auto-build StakeHub add/remove NodeIDs txs if configured
        if let Some(cfg) = crate::node::network::evn::get_global_evn_config() {
            if let Some(nonce) = cfg.nodeids_nonce {
                let sys = crate::system_contracts::SystemContract::new(self.main_work_worker.chain_spec.as_ref().clone());

                if !cfg.nodeids_to_add.is_empty() {
                    let tx = sys.add_node_ids_tx(cfg.nodeids_to_add.clone(), nonce);
                    match crate::node::miner::signer::sign_system_transaction(tx) {
                        Ok(signed) => {
                            let mut out = Vec::new();
                            signed.encode(&mut out);
                            let hash = keccak256(&out);
                            info!(
                                target: "bsc::evn",
                                action = "addNodeIDs",
                                count = cfg.nodeids_to_add.len(),
                                nonce = nonce,
                                tx_hash = format!("0x{:x}", hash),
                                raw_rlp = format!("0x{}", alloy_primitives::hex::encode(&out)),
                                "Built StakeHub.addNodeIDs transaction"
                            );
                        }
                        Err(e) => warn!(target: "bsc::evn", error = %e, "Failed to sign addNodeIDs transaction"),
                    }
                }

                if !cfg.nodeids_to_remove.is_empty() {
                    let tx = sys.remove_node_ids_tx(cfg.nodeids_to_remove.clone(), nonce);
                    match crate::node::miner::signer::sign_system_transaction(tx) {
                        Ok(signed) => {
                            let mut out = Vec::new();
                            signed.encode(&mut out);
                            let hash = keccak256(&out);
                            info!(
                                target: "bsc::evn",
                                action = "removeNodeIDs",
                                count = cfg.nodeids_to_remove.len(),
                                nonce = nonce,
                                tx_hash = format!("0x{:x}", hash),
                                raw_rlp = format!("0x{}", alloy_primitives::hex::encode(&out)),
                                "Built StakeHub.removeNodeIDs transaction"
                            );
                        }
                        Err(e) => warn!(target: "bsc::evn", error = %e, "Failed to sign removeNodeIDs transaction"),
                    }
                }
            }
        }
        self.spawn_workers()
    }

    fn spawn_workers(self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.task_executor.spawn_critical("new_work_worker", self.new_work_worker.run());
        self.task_executor.spawn_critical("main_work_worker", self.main_work_worker.run());
        self.task_executor.spawn_critical("result_work_worker", self.result_work_worker.run());
        info!("Succeed to start mining, address: {}", self.validator_address);
        Ok(())
    }

}
