use alloy_primitives::U256;
use crate::consensus::parlia::{Parlia, DEFAULT_MIN_GAS_TIP};
use crate::evm::blacklist;
use crate::hardforks::BscHardforks;
use crate::node::engine::BscBuiltPayload;
use crate::node::evm::config::BscEvmConfig;
use crate::node::miner::bid_simulator::BidSimulator;
use crate::node::miner::bsc_miner::{MiningContext, SubmitContext};
use crate::node::pool::BlacklistedAddressError;
use reth_provider::StateProviderFactory;
use reth_revm::{database::StateProviderDatabase, db::State};
use reth_evm::{ConfigureEvm, NextBlockEnvAttributes};
use reth_evm::execute::BlockBuilder;
use alloy_evm::Evm;
use reth_payload_primitives::{PayloadBuilderError, BuiltPayload};
use reth::transaction_pool::{TransactionPool, PoolTransaction};
use reth_primitives::TransactionSigned;
use reth::transaction_pool::BestTransactionsAttributes;
use tracing::{debug, info};
use reth_evm::block::{BlockExecutionError, BlockValidationError};
use reth::transaction_pool::error::InvalidPoolTransactionError;
use reth_primitives::InvalidTransactionError;
use reth_evm::execute::BlockBuilderOutcome;
use reth_ethereum_payload_builder::EthereumBuilderConfig;
use reth_revm::cached::CachedReads;
use reth_primitives::HeaderTy;
use reth_revm::cancelled::ManualCancel;
use std::sync::Arc;
use reth_basic_payload_builder::PayloadConfig;
use tokio::sync::{oneshot, mpsc};
use reth::payload::EthPayloadBuilderAttributes;
use reth_payload_primitives::PayloadBuilderAttributes;
use alloy_consensus::{Transaction, BlockHeader};
use reth_primitives_traits::{SignerRecoverable, BlockBody};
use tracing::warn;
use crate::chainspec::{BscChainSpec};
use reth::transaction_pool::error::Eip4844PoolTransactionError;
use crate::node::primitives::BscBlobTransactionSidecar;
use std::collections::HashMap;
use reth_chainspec::EthChainSpec;
use reth_chainspec::EthereumHardforks;
use crate::consensus::eip4844::{calc_blob_fee, BLOB_TX_BLOB_GAS_PER_BLOB};


/// Delay left over for mining calculation
pub const DELAY_LEFT_OVER: u64 = 50;

/// Time multiplier for retry condition check
const TIME_MULTIPLIER: u32 = 2;

/// Errors that can occur during payload job execution
#[derive(Debug, thiserror::Error)]
pub enum BscPayloadJobError {
    #[error("Failed to send signal to build queue: {0}")]
    BuildQueueSendError(String),
    
    #[error("Failed to send best payload to result channel: {0}")]
    ResultChannelSendError(String),
    
    #[error("Payload building failed: {0}")]
    PayloadBuildingError(String),
    
    #[error("Task execution failed: {0}")]
    TaskExecutionError(String),
    
    #[error("Job was aborted")]
    JobAborted,
    
    #[error("Timeout occurred during payload building")]
    Timeout,
    
    #[error("No payloads available to select from")]
    NoPayloadsAvailable,
    
    #[error("Build arguments are invalid: {0}")]
    InvalidBuildArguments(String),
    
    #[error("Channel communication failed: {0}")]
    ChannelCommunicationError(String),
}

/// Build arguments for BscPayloadBuilder.
#[derive(Debug, Clone)]
pub struct BscBuildArguments<Attributes> {
    /// Previously cached disk reads
    pub cached_reads: CachedReads,
    /// How to configure the payload.
    pub config: PayloadConfig<Attributes, HeaderTy<<BscBuiltPayload as BuiltPayload>::Primitives>>,
    /// A marker that can be used to cancel the job.
    pub cancel: ManualCancel,
}

/// BSC payload builder, used to build payload for bsc miner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BscPayloadBuilder<Pool, Client, EvmConfig = BscEvmConfig> {
    /// Client providing access to node state.
    client: Client,
    /// Transaction pool.
    pool: Pool,
    /// The type responsible for creating the evm.
    evm_config: EvmConfig,
    /// Payload builder configuration, now reuse eth builder config.
    builder_config: EthereumBuilderConfig,
    /// Bsc chain spec.
    chain_spec: Arc<BscChainSpec>,
    /// Parlia consensus engine.
    parlia: Arc<Parlia<BscChainSpec>>,
    // Mining context containing header information for blob fee calculation
    ctx: MiningContext,
}

impl<Pool, Client, EvmConfig> BscPayloadBuilder<Pool, Client, EvmConfig> 
where
    Client: StateProviderFactory + 'static,
    EvmConfig: ConfigureEvm<NextBlockEnvCtx = NextBlockEnvAttributes> + 'static,
    <EvmConfig as ConfigureEvm>::Primitives: reth_primitives_traits::NodePrimitives<BlockHeader = alloy_consensus::Header, SignedTx = alloy_consensus::EthereumTxEnvelope<alloy_consensus::TxEip4844>, Block = crate::node::primitives::BscBlock>,
    Pool: TransactionPool<Transaction: PoolTransaction<Consensus = TransactionSigned>> + 'static,
{
    pub const fn new(
        client: Client,
        pool: Pool,
        evm_config: EvmConfig,
        builder_config: EthereumBuilderConfig,
        chain_spec: Arc<BscChainSpec>,
        parlia: Arc<Parlia<BscChainSpec>>,
        ctx: MiningContext,
    ) -> Self {
        Self { client, pool, evm_config, builder_config, chain_spec, parlia, ctx }
    }

    /// Builds a payload with the given arguments.
    /// 
    /// # Thread Safety
    /// 
    /// This method takes `&self` and may be called concurrently. The underlying fields
    /// (such as `client`, `pool`, etc.) are designed to be thread-safe, but callers should
    /// ensure that concurrent calls don't cause race conditions in shared state.
    /// 
    /// # Arguments
    /// 
    /// * `args` - Build arguments containing cached reads, config, cancel token
    /// 
    /// # Returns
    /// 
    /// Returns a `Result` containing the built payload or an error.
    pub async fn build_payload(&self, args: BscBuildArguments<EthPayloadBuilderAttributes>) -> Result<BscBuiltPayload, Box<dyn std::error::Error + Send + Sync>> {
        let BscBuildArguments { mut cached_reads, config, cancel } = args;
        let PayloadConfig { parent_header, attributes } = config;

        let state_provider = self.client.state_by_block_hash(parent_header.hash_slow())?;
        let state = StateProviderDatabase::new(&state_provider);
        let mut db = State::builder().with_database(cached_reads.as_db_mut(state)).with_bundle_update().build();
        
        let mut builder = self.evm_config
            .builder_for_next_block(
                &mut db,
                &parent_header,
                NextBlockEnvAttributes {
                    timestamp: attributes.timestamp(),
                    suggested_fee_recipient: attributes.suggested_fee_recipient(),
                    prev_randao: attributes.prev_randao(),
                    gas_limit: self.builder_config.gas_limit(parent_header.gas_limit),
                    parent_beacon_block_root: attributes.parent_beacon_block_root(),
                    withdrawals: Some(attributes.withdrawals().clone()),
                },
            )
            .map_err(PayloadBuilderError::other)?;

        builder.apply_pre_execution_changes().map_err(|err| {
            warn!(target: "payload_builder", %err, "failed to apply pre-execution changes");
            PayloadBuilderError::Internal(err.into())
        })?;

        let mut total_fees = U256::ZERO;
        let mut cumulative_gas_used = 0;
        // reserve the systemtx gas
        let system_txs_gas = self.parlia.estimate_gas_reserved_for_system_txs(Some(parent_header.timestamp), parent_header.number+1, attributes.timestamp);
        let block_gas_limit: u64 = builder.evm_mut().block().gas_limit.saturating_sub(system_txs_gas);

        let base_fee = builder.evm_mut().block().basefee;
        
        let mut sidecars_map = HashMap::new();
        // TODO: add min gas tip to config.
        let min_gas_tip = DEFAULT_MIN_GAS_TIP;
        let mut block_blob_count = 0;

        let mut blob_fee = None;
        let blob_params = self.chain_spec.blob_params_at_timestamp(attributes.timestamp());
        let header = self.ctx.header.as_ref().ok_or_else(|| {
            Box::new(std::io::Error::other(
                "Missing header in mining context"
            )) as Box<dyn std::error::Error + Send + Sync>
        })?;
        if header.excess_blob_gas != Some(0) {
            blob_fee = Some(calc_blob_fee(&self.chain_spec, header));
        }
        let max_blob_count = blob_params.as_ref().map(|params| params.max_blob_count).unwrap_or_default();
        let mut best_tx_list = self.pool.best_transactions_with_attributes(BestTransactionsAttributes::new(base_fee, blob_fee.map(|fee| fee as u64)));
        while let Some(pool_tx) = best_tx_list.next() {
            if cancel.is_cancelled() {
                break;
            }

            // filter out blacklisted transactions before executing.
            if self.chain_spec.is_nano_active_at_block(parent_header.number+1) 
                && blacklist::check_tx_basic_blacklist(pool_tx.sender(), pool_tx.to()) {
                tracing::debug!(target: "payload_builder", "Blacklisted transaction: {:?}", pool_tx.hash());
                best_tx_list.mark_invalid(
                    &pool_tx,
                    InvalidPoolTransactionError::other(BlacklistedAddressError()),
                );
                continue
            }
            // filter out tx with min gas tip.
            if pool_tx.effective_tip_per_gas(base_fee).unwrap_or(0_u128) < min_gas_tip {
                // Skip packaging underpriced transactions, but do not mark them invalid.
                continue
            }

            // ensure we still have capacity for this transaction
            if cumulative_gas_used + pool_tx.gas_limit() > block_gas_limit {
                // we can't fit this transaction into the block, so we need to mark it as invalid
                // which also removes all dependent transaction from the iterator before we can
                // continue
                best_tx_list.mark_invalid(
                    &pool_tx,
                    InvalidPoolTransactionError::ExceedsGasLimit(pool_tx.gas_limit(), block_gas_limit),
                );
                continue
            }

            let tx = pool_tx.to_consensus();
            let mut blob_tx_sidecar = None;
            debug!("debug payload_builder, block_number: {}, tx: {:?}, is_blob_tx: {:?}, tx_type: {:?}", parent_header.number()+1, tx.hash(), tx.is_eip4844(), tx.tx_type());
            if let Some(blob_tx) = tx.as_eip4844() {
                let tx_blob_count = blob_tx.tx().blob_versioned_hashes.len() as u64;
                if block_blob_count + tx_blob_count > max_blob_count {
                    // we can't fit this _blob_ transaction into the block, so we mark it as
                    // invalid, which removes its dependent transactions from
                    // the iterator. This is similar to the gas limit condition
                    // for regular transactions above.
                    debug!(target: "payload_builder", tx=?tx.hash(), ?block_blob_count, "skipping blob transaction because it would exceed the max blob count per block");
                    best_tx_list.mark_invalid(
                        &pool_tx,
                        InvalidPoolTransactionError::Eip4844(
                            Eip4844PoolTransactionError::TooManyEip4844Blobs {
                                have: block_blob_count + tx_blob_count,
                                permitted: max_blob_count,
                            },
                        ),
                    );
                    continue
                }

                if BscHardforks::is_cancun_active_at_timestamp(&self.chain_spec, parent_header.number+1, attributes.timestamp()) {
                    let left =  max_blob_count - block_blob_count;
                    if left < blob_tx.tx().blob_gas_used().unwrap_or(0) / BLOB_TX_BLOB_GAS_PER_BLOB {
                        best_tx_list.mark_invalid(
                            &pool_tx,
                            InvalidPoolTransactionError::Eip4844(
                                Eip4844PoolTransactionError::TooManyEip4844Blobs {
                                    have: block_blob_count + tx_blob_count,
                                    permitted: max_blob_count,
                                },
                            ),
                        );
                        continue
                    }
                }

                let blob_sidecar_result = 'sidecar: {
                    let Some(sidecar) =
                        self.pool.get_blob(*tx.hash()).map_err(PayloadBuilderError::other)?
                    else {
                        break 'sidecar Err(Eip4844PoolTransactionError::MissingEip4844BlobSidecar)
                    };

                    if self.chain_spec.is_osaka_active_at_timestamp(attributes.timestamp()) {
                        if sidecar.is_eip7594() {
                            Ok(sidecar)
                        } else {
                            Err(Eip4844PoolTransactionError::UnexpectedEip4844SidecarAfterOsaka)
                        }
                    } else if sidecar.is_eip4844() {
                        Ok(sidecar)
                    } else {
                        Err(Eip4844PoolTransactionError::UnexpectedEip7594SidecarBeforeOsaka)
                    }
                };
                debug!("debug payload_builder, block_number: {}, tx_hash: {:?}, blob_sidecar_result: {:?}", parent_header.number()+1, tx.hash(), blob_sidecar_result);

                blob_tx_sidecar = match blob_sidecar_result {
                    Ok(sidecar) => Some(sidecar),
                    Err(error) => {
                        best_tx_list.mark_invalid(&pool_tx, InvalidPoolTransactionError::Eip4844(error));
                        continue
                    }
                };
                debug!("debug payload_builder, block_number: {}, tx_hash: {:?}, blob_tx_sidecar: {:?}", parent_header.number()+1, tx.hash(), blob_tx_sidecar);
            }
            
            let gas_used = match builder.execute_transaction(tx.clone()) {
                Ok(gas_used) => gas_used,
                Err(BlockExecutionError::Validation(BlockValidationError::InvalidTx {
                    error, ..
                })) => {
                    if error.is_nonce_too_low() {
                        // if the nonce is too low, we can skip this transaction
                        debug!(target: "payload_builder", %error, ?tx, "skipping nonce too low transaction");
                    } else {
                        // if the transaction is invalid, we can skip it and all of its
                        // descendants
                        debug!(target: "payload_builder", %error, ?tx, "skipping invalid transaction and its descendants");
                        best_tx_list.mark_invalid(
                            &pool_tx,
                            InvalidPoolTransactionError::Consensus(
                                InvalidTransactionError::TxTypeNotSupported,
                            ),
                        );
                    }
                    continue
                }
                // this is an error that we should treat as fatal for this attempt
                Err(err) => return Err(Box::new(PayloadBuilderError::evm(err))),
            };

             // add to the total blob gas used if the transaction successfully executed
            if let Some(blob_tx) = tx.as_eip4844() {
                block_blob_count += blob_tx.tx().blob_versioned_hashes.len() as u64;

                // if we've reached the max blob count, we can skip blob txs entirely
                if block_blob_count == max_blob_count {
                    best_tx_list.skip_blobs();
                }
            }
            // update and add to total fees
            let miner_fee = tx.effective_tip_per_gas(base_fee).expect("fee is always valid; execution succeeded");
            total_fees += U256::from(miner_fee) * U256::from(gas_used);
            cumulative_gas_used += gas_used;

            // Add blob tx sidecar to the payload.
            if let Some(sidecar) = blob_tx_sidecar {
                sidecars_map.insert(*tx.hash(), sidecar);
            }
        }

        // add system txs to payload.
        let BlockBuilderOutcome { execution_result, block, .. } = builder.finish(&state_provider)?;
        let mut sealed_block = Arc::new(block.sealed_block().clone());
        
        // set sidecars to seal block
        let mut blob_sidecars:Vec<BscBlobTransactionSidecar>= Vec::new();
        let transactions = &sealed_block.body().inner.transactions;
        debug!("debug payload_builder, block_number: {}, block_hash: {:?}, txs: {} gas: {}, fees: {}", sealed_block.number(), sealed_block.hash(), transactions.len(), cumulative_gas_used, total_fees);
        for (index, tx) in transactions.iter().enumerate() {
            debug!("debug payload_builder, transaction {}: hash={:?}, from={:?}, to={:?}, value={:?}, gas_limit={}, gas_price={:?}, nonce={}", 
                index + 1,
                tx.hash(),
                tx.recover_signer().ok(),
                tx.to(),
                tx.value(),
                tx.gas_limit(),
                tx.gas_price(),
                tx.nonce()
            );
            if tx.is_eip4844() {
                let sidecar = sidecars_map.get(tx.hash()).unwrap();
                let bsc_blob_tx_sidecar = BscBlobTransactionSidecar {
                    inner: sidecar.as_eip4844().unwrap().clone(),
                    block_number: sealed_block.header().number(),
                    block_hash: sealed_block.hash(),
                    tx_index: index as u64,
                    tx_hash: *tx.hash(),
                };
                blob_sidecars.push(bsc_blob_tx_sidecar);
            }
        }

        let mut plain = sealed_block.clone_block();
        plain.body.sidecars = Some(blob_sidecars);
        sealed_block = Arc::new(plain.into());
    
        let payload = BscBuiltPayload {
            block: sealed_block,
            fees: total_fees,
            requests: Some(execution_result.requests),
        };
        Ok(payload)
    }
}

/// Handle for aborting a BscPayloadJob
pub struct BscPayloadJobHandle {
    abort_tx: oneshot::Sender<()>,
}

impl BscPayloadJobHandle {
    /// Abort the payload job by new head.
    pub fn abort(self) {
        let _ = self.abort_tx.send(());
    }
}

/// BscPayloadJob is used to async build payloads to get best payload.
pub struct BscPayloadJob<Pool, Client, EvmConfig = BscEvmConfig> 
where
    Pool: TransactionPool,
{
    /// Parlia consensus engine
    parlia: Arc<crate::consensus::parlia::Parlia<crate::chainspec::BscChainSpec>>,
    /// Mining context
    mining_ctx: MiningContext,
    /// The payload builder instance
    builder: Arc<BscPayloadBuilder<Pool, Client, EvmConfig>>,
    /// Timeout for payload building
    timeout: std::time::Duration,
    /// Message queue for processing build arguments
    try_build_rx: mpsc::UnboundedReceiver<()>,
    /// Sender for sending arguments back to queue
    try_build_tx: mpsc::UnboundedSender<()>,
    /// Listener for new transactions from the pool
    tx_listener: mpsc::UnboundedReceiver<alloy_primitives::B256>,
    /// Abort receiver for external termination
    abort_rx: oneshot::Receiver<()>,
    /// Abort flag
    is_aborted: bool,
    /// Sender for payload results
    result_tx: mpsc::UnboundedSender<SubmitContext>,
    /// Potential payloads vector for selecting the best one
    potential_payloads: Vec<BscBuiltPayload>,
    /// Current build arguments
    build_args: BscBuildArguments<EthPayloadBuilderAttributes>,
    /// Retry count for payload building
    retries: u32,
    /// JoinSet for managing build tasks
    join_handle: tokio::task::JoinSet<Result<BscBuiltPayload, Box<dyn std::error::Error + Send + Sync>>>,
    /// Simulator for bid management (no outer RwLock, each map has its own)
    simulator: Arc<BidSimulator<Client, Pool>>,
}

impl<Pool, Client, EvmConfig> BscPayloadJob<Pool, Client, EvmConfig>
where
    Client: StateProviderFactory + reth_provider::HeaderProvider<Header = alloy_consensus::Header> + reth_provider::BlockHashReader + Clone + 'static,
    EvmConfig: ConfigureEvm<NextBlockEnvCtx = NextBlockEnvAttributes> + 'static,
    <EvmConfig as ConfigureEvm>::Primitives: reth_primitives_traits::NodePrimitives<BlockHeader = alloy_consensus::Header, SignedTx = alloy_consensus::EthereumTxEnvelope<alloy_consensus::TxEip4844>, Block = crate::node::primitives::BscBlock>,
    Pool: TransactionPool<Transaction: PoolTransaction<Consensus = TransactionSigned>> + 'static,
{
    /// Creates a new BscPayloadJob and returns both the job and its handle
    pub fn new(
        parlia: Arc<crate::consensus::parlia::Parlia<crate::chainspec::BscChainSpec>>,
        mining_ctx: MiningContext,
        builder: BscPayloadBuilder<Pool, Client, EvmConfig>,
        build_args: BscBuildArguments<EthPayloadBuilderAttributes>,
        simulator: Arc<BidSimulator<Client, Pool>>,  // No outer RwLock needed
        result_tx: mpsc::UnboundedSender<SubmitContext>,
    ) -> (Self, BscPayloadJobHandle) {
        let (abort_tx, abort_rx) = oneshot::channel();
        let (try_build_tx, try_build_rx) = mpsc::unbounded_channel();
        let (tx_listener_tx, tx_listener_rx) = mpsc::unbounded_channel();
        
        let mining_delay = parlia.clone().delay_for_mining(
            &mining_ctx.parent_snapshot, 
            mining_ctx.header.as_ref().unwrap(), 
            DELAY_LEFT_OVER);

        // Spawn a background task to listen for new transactions from pool
        let mut pool_listener = builder.pool.pending_transactions_listener();
        tokio::spawn(async move {
            while let Some(tx_hash) = pool_listener.recv().await {
                let _ = tx_listener_tx.send(tx_hash);
            }
        });

        let job = Self {
            parlia,
            mining_ctx,
            builder: Arc::new(builder),
            timeout: std::time::Duration::from_millis(mining_delay),
            try_build_rx,
            try_build_tx: try_build_tx.clone(),
            tx_listener: tx_listener_rx,
            abort_rx,
            is_aborted: false,
            result_tx,
            potential_payloads: Vec::new(),
            build_args,
            retries: 0,
            join_handle: tokio::task::JoinSet::new(),
            simulator,
        };
        let handle = BscPayloadJobHandle {
            abort_tx,
        };

        debug!("Succeed to new payload job, block_number: {}, timeout: {:?}", job.mining_ctx.parent_header.number()+1, job.timeout);
        (job, handle)
    }

    /// Runs the payload job asynchronously with timeout support
    pub async fn start(mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut start_time = std::time::Instant::now();
        if let Err(err) = self.try_build_tx.send(()) {
            warn!("Failed to send to first try build queue due to {}, block_number: {}", err, self.build_args.config.parent_header.number()+1);
            return Err(Box::new(BscPayloadJobError::BuildQueueSendError(err.to_string())));
        }
    
        loop {
            tokio::select! {
                // Trigger the async build payload by queue.
                args = self.try_build_rx.recv() => {
                    match args {
                        Some(_) => {
                            self.retries += 1;
                            start_time = std::time::Instant::now();
                            debug!("Try new build, block_number: {}, retries: {}", 
                                self.build_args.config.parent_header.number()+1, self.retries);
                            
                            let builder = self.builder.clone();
                            let build_args = self.build_args.clone();
                            self.join_handle.spawn(async move {
                                builder.build_payload(build_args).await
                            });
                        }
                        None => {
                            debug!("Exit payload job by queue closed");
                            return Ok(());
                        }
                    }
                }
                
                // Try to join the async payload build task.
                result = self.join_handle.join_next() => {
                    match result {
                        Some(Ok(Ok(payload))) => {
                            if self.is_aborted {
                                return Err(Box::new(BscPayloadJobError::JobAborted));
                            }
                            let elapsed = start_time.elapsed();
                            let payload_tx_count = payload.block().body().transaction_count();
                            debug!("Succeed to try new build: {} (hash: 0x{:x}, txs: {}, fees: {}, cost_time: {:?}, retries: {})", 
                                payload.block().header().number(),
                                payload.block().hash(),
                                payload.block().body().transaction_count(),
                                payload.fees(),
                                elapsed,
                                self.retries
                            );
                            self.potential_payloads.push(payload);
                            let mut new_tx_count = 0;
                            // loop wait new transactions or timeout.
                            loop {
                                tokio::select! {
                                    // Finish timeout by timer.
                                    _ = tokio::time::sleep(self.timeout) => {
                                        info!("try return best payload due to has no time, cost_time: {:?}, block_number: {}, retries: {}", 
                                            elapsed, self.build_args.config.parent_header.number()+1, self.retries);
                                        return self.try_return_best_payload();
                                    }

                                    // Abort by new head.
                                    _ = &mut self.abort_rx => {
                                        info!("Abort payload building by new head, cost_time: {:?}, block_number: {}, retries: {}", 
                                            elapsed, self.build_args.config.parent_header.number()+1, self.retries);
                                        self.build_args.cancel.clone().cancel();
                                        self.is_aborted = true;
                                        return Err(Box::new(BscPayloadJobError::JobAborted));
                                    }

                                    Some(_tx_hash) = self.tx_listener.recv() => {
                                        new_tx_count+=1;
                                        let mining_delay = self.parlia.delay_for_mining(
                                            &self.mining_ctx.parent_snapshot, 
                                            self.mining_ctx.header.as_ref().unwrap(), 
                                            DELAY_LEFT_OVER);
                                        if std::time::Duration::from_millis(mining_delay) < elapsed {
                                            debug!("try return best payload due to mining_delay < elapsed, block_number: {}, retries: {}, mining_delay: {:?}, elapsed: {:?}", 
                                                self.build_args.config.parent_header.number()+1, self.retries, mining_delay, elapsed);
                                            return self.try_return_best_payload();
                                        } else if std::time::Duration::from_millis(mining_delay) < elapsed * TIME_MULTIPLIER {
                                            if let Err(err) = self.try_build_tx.send(()) {
                                                warn!("Failed to send to try build queue, block_number: {}, retries: {}, error: {:?}", 
                                                    self.build_args.config.parent_header.number()+1, self.retries, err);
                                                return self.try_return_best_payload();
                                            }
                                            debug!("Succeed to send to try build queue, block_number: {}, retries: {}, last_cost_time: {:?}, new_mining_delay: {:?}", 
                                                    self.build_args.config.parent_header.number()+1, self.retries, elapsed, std::time::Duration::from_millis(mining_delay));
                                            break;  // Break out of the loop and wait for the next payload
                                        } else if new_tx_count >= payload_tx_count {
                                            if let Err(err) = self.try_build_tx.send(()) {
                                                warn!("Failed to send to try build queue, block_number: {}, retries: {}, error: {:?}", 
                                                    self.build_args.config.parent_header.number()+1, self.retries, err);
                                                return self.try_return_best_payload();
                                            }
                                            debug!("Succeed to send to try build queue, block_number: {}, retries: {}, last_cost_time: {:?}, new_mining_delay: {:?}", 
                                                self.build_args.config.parent_header.number()+1, self.retries, elapsed, std::time::Duration::from_millis(mining_delay));
                                            break; // Break out of the loop and wait for the next payload
                                        }
                                    }
                                }
                            }
                        },
                        Some(Ok(Err(e))) => {
                            let elapsed = start_time.elapsed();
                            warn!(
                                target: "bsc::miner::payload",
                                error = %e,
                                cost_time = ?elapsed,
                                block_number = self.build_args.config.parent_header.number() + 1,
                                parent_hash = ?self.build_args.config.parent_header.hash(),
                                is_inturn = self.mining_ctx.is_inturn,
                                retries = self.retries,
                                "Failed to build payload task"
                            );
                            return self.try_return_best_payload();
                        },
                        Some(Err(join_err)) => {
                            let elapsed = start_time.elapsed();
                            warn!("Failed to join payload build task due to {}, cost_time: {:?}, block_number: {}, retries: {}", 
                                join_err, elapsed, self.build_args.config.parent_header.number()+1, self.retries);
                            return self.try_return_best_payload();
                        },
                        None => {
                            // No task completed, continue to next iteration
                        },
                    }
                }
                
                // Finish timeout by timer.
                _ = tokio::time::sleep(self.timeout) => {
                    let elapsed = start_time.elapsed();
                    info!("Try return best payload due to has no time, cost_time: {:?}, block_number: {}, retries: {}", 
                        elapsed, self.build_args.config.parent_header.number()+1, self.retries);
                    self.build_args.cancel.clone().cancel();
                    return self.try_return_best_payload();
                }
                
                // Abort by new head.
                _ = &mut self.abort_rx => {
                    let elapsed = start_time.elapsed();
                    info!("Abort payload building by new head, cost_time: {:?}, block_number: {}, parent_hash: 0x{:x}, retries: {}", 
                        elapsed, self.build_args.config.parent_header.number()+1, self.build_args.config.parent_header.parent_hash(), self.retries);
                    self.build_args.cancel.clone().cancel();
                    self.is_aborted = true;
                    return Err(Box::new(BscPayloadJobError::JobAborted));
                }
            }
        }
    }

    /// Try to return the best payload to result channel
    fn try_return_best_payload(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let best_bid = self.simulator.get_best_bid(self.mining_ctx.parent_header.hash());
        if let Some(bid) = best_bid {
            info!("Found best bid! block: {}, builder: {:?}, gas_fee: {}", bid.bid.block_number, bid.bid.builder, bid.bid.gas_fee);
            self.potential_payloads.push(bid.bsc_payload);
        }
        if let Some(best_payload) = self.pick_best_payload() {
            if let Err(err) = self.result_tx.send(SubmitContext {
                mining_ctx: self.mining_ctx.clone(),
                payload: best_payload,
                cancel: self.build_args.cancel.clone(),
            }) {
                warn!("Failed to send best payload to result channel: {}", err);
                return Err(Box::new(BscPayloadJobError::ResultChannelSendError(err.to_string())));
            }
            Ok(())
        } else {
            warn!("No best payload available to send, try_mine_block_number: {}", self.build_args.config.parent_header.number()+1);
            Err(Box::new(BscPayloadJobError::NoPayloadsAvailable))
        }
    }

    /// Pick the best payload from potential payloads
    fn pick_best_payload(&mut self) -> Option<BscBuiltPayload> {
        if self.potential_payloads.is_empty() {
            return None;
        }

        // pick the payload with the highest fees as best payload.
        let best_index = self.potential_payloads
            .iter()
            .enumerate()
            .max_by_key(|(_, payload)| payload.fees())
            .map(|(index, _)| index)?;

        let total_len = self.potential_payloads.len();
        let best_payload = self.potential_payloads.remove(best_index);
        info!("Succeed to pick the best payload: {} (hash: 0x{:x}, txs: {}, fees: {}), pick the {}th payload as best, total_len: {}", 
            best_payload.block().header().number(),
            best_payload.block().hash(),
            best_payload.block().body().transaction_count(),
            best_payload.fees(),
            best_index+1,
            total_len
        );

        self.potential_payloads.clear();
        Some(best_payload)
    }
}