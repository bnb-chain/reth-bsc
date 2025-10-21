use alloy_primitives::U256;
use crate::node::engine::BscBuiltPayload;
use crate::node::evm::config::BscEvmConfig;
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
use std::sync::Arc;
use reth_basic_payload_builder::{BuildArguments, PayloadConfig};
use reth_revm::cancelled::CancelOnDrop;
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
    ) -> Self {
        Self { client, pool, evm_config, builder_config, chain_spec }
    }

    // todo: check more and refine it later.
    pub async fn build_payload(&self, args: BuildArguments<EthPayloadBuilderAttributes, BscBuiltPayload>) -> Result<BscBuiltPayload, Box<dyn std::error::Error + Send + Sync>> {
        let BuildArguments { mut cached_reads, config, cancel, best_payload: _best_payload } = args;
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
        let block_gas_limit: u64 = builder.evm_mut().block().gas_limit;
        let base_fee = builder.evm_mut().block().basefee;
        
        let mut sidecars_map = HashMap::new();
        let mut block_blob_count = 0;

        // todo: calc blob fee.
        let blob_params = self.chain_spec.blob_params_at_timestamp(attributes.timestamp());
        let max_blob_count = blob_params.as_ref().map(|params| params.max_blob_count).unwrap_or_default();
        let mut best_tx_list = self.pool.best_transactions_with_attributes(BestTransactionsAttributes::new(base_fee, None));
        while let Some(pool_tx) = best_tx_list.next() {
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

            if cancel.is_cancelled() {
                break;
            }

            let tx = pool_tx.to_consensus();
            let mut blob_tx_sidecar = None;
            debug!("debug payload_builder, tx: {:?} is_blob_tx: {:?} tx_type: {:?}", tx.hash(), tx.is_eip4844(), tx.tx_type());
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
                debug!("debug payload_builder, tx_hash: {:?}, blob_sidecar_result: {:?}", tx.hash(), blob_sidecar_result);

                blob_tx_sidecar = match blob_sidecar_result {
                    Ok(sidecar) => Some(sidecar),
                    Err(error) => {
                        best_tx_list.mark_invalid(&pool_tx, InvalidPoolTransactionError::Eip4844(error));
                        continue
                    }
                };
                debug!("debug payload_builder, tx_hash: {:?}, blob_tx_sidecar: {:?}", tx.hash(), blob_tx_sidecar);
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
            if let Some(miner_fee) = tx.effective_tip_per_gas(base_fee) {
                total_fees += U256::from(miner_fee) * U256::from(gas_used);
            }
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
        debug!("debug payload_builder, block_number: {}, block_hash: {:?}, contains {} transactions:", sealed_block.number(), sealed_block.hash(), transactions.len());
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
                if let Some(sidecar) = sidecars_map.get(tx.hash()) {
                    if let Some(eip4844_sidecar) = sidecar.as_eip4844() {
                        let bsc_blob_tx_sidecar = BscBlobTransactionSidecar {
                            inner: eip4844_sidecar.clone(),
                            block_number: sealed_block.header().number(),
                            block_hash: sealed_block.hash(),
                            tx_index: index as u64,
                            tx_hash: *tx.hash(),
                        };
                        blob_sidecars.push(bsc_blob_tx_sidecar);
                    }
                }
            }
        }

        let mut plain = sealed_block.clone_block();
        plain.body.sidecars = Some(blob_sidecars);
        sealed_block = Arc::new(plain.into());
    
        debug!("debug payload_builder, sealed_block: {:?}", sealed_block);
        let payload = BscBuiltPayload {
            block: sealed_block,
            fees: total_fees,
            requests: Some(execution_result.requests),
        };
        Ok(payload)
    }
}

/// Handle for controlling a BscPayloadJob
pub struct BscPayloadJobHandle {
    abort_tx: oneshot::Sender<()>,
}

impl BscPayloadJobHandle {
    /// Abort the payload job
    pub fn abort(self) {
        let _ = self.abort_tx.send(());
    }
    
}

/// BscPayloadJob is used to async build payloads to get best payload.
pub struct BscPayloadJob<Pool, Client, EvmConfig = BscEvmConfig> {
    /// The payload builder instance
    builder: Arc<BscPayloadBuilder<Pool, Client, EvmConfig>>,
    /// Timeout for payload building
    timeout: std::time::Duration,
    /// Message queue for processing build arguments
    try_build_rx: mpsc::UnboundedReceiver<Arc<BuildArguments<EthPayloadBuilderAttributes, BscBuiltPayload>>>,
    /// Sender for sending arguments back to queue
    try_build_tx: mpsc::UnboundedSender<Arc<BuildArguments<EthPayloadBuilderAttributes, BscBuiltPayload>>>,
    /// Cancel handle that automatically cancels the job when dropped
    cancel: CancelOnDrop,
    /// Abort receiver for external termination
    abort_rx: oneshot::Receiver<()>,
    /// Abort flag
    is_aborted: bool,
    /// Sender for payload results
    result_tx: mpsc::UnboundedSender<BscBuiltPayload>,
    /// Potential payloads vector for selecting the best one
    potential_payloads: Vec<BscBuiltPayload>,
    /// Current build arguments
    build_args: Arc<BuildArguments<EthPayloadBuilderAttributes, BscBuiltPayload>>,
    /// Retry count for payload building
    retries: u32,
    // TODO: enrich retry, mev workflows.
}

impl<Pool, Client, EvmConfig> BscPayloadJob<Pool, Client, EvmConfig>
where
    Client: StateProviderFactory + 'static,
    EvmConfig: ConfigureEvm<NextBlockEnvCtx = NextBlockEnvAttributes> + 'static,
    <EvmConfig as ConfigureEvm>::Primitives: reth_primitives_traits::NodePrimitives<BlockHeader = alloy_consensus::Header, SignedTx = alloy_consensus::EthereumTxEnvelope<alloy_consensus::TxEip4844>, Block = crate::node::primitives::BscBlock>,
    Pool: TransactionPool<Transaction: PoolTransaction<Consensus = TransactionSigned>> + 'static,
{
    /// Creates a new BscPayloadJob and returns both the job and its handle
    pub fn new(
        builder: BscPayloadBuilder<Pool, Client, EvmConfig>,
        build_args: BuildArguments<EthPayloadBuilderAttributes, BscBuiltPayload>,
        result_tx: mpsc::UnboundedSender<BscBuiltPayload>,
    ) -> (Self, BscPayloadJobHandle) {
        let (abort_tx, abort_rx) = oneshot::channel();
        let (try_build_tx, try_build_rx) = mpsc::unbounded_channel();
        
        // Clone cancel before moving build_args
        let cancel = build_args.cancel.clone();
        
        // Store current args by cloning from build_args
        let build_args_arc = Arc::new(BuildArguments {
            cached_reads: build_args.cached_reads.clone(),
            config: build_args.config.clone(),
            cancel: build_args.cancel.clone(),
            best_payload: build_args.best_payload.clone(),
        });
        
        
        let job = Self {
            builder: Arc::new(builder),
            timeout: std::time::Duration::from_millis(500), // TODO: refine it more.
            try_build_rx,
            try_build_tx: try_build_tx.clone(),
            cancel,
            abort_rx,
            is_aborted: false,
            result_tx,
            potential_payloads: Vec::new(),
            build_args: build_args_arc,
            retries: 0,
        };
        
        let handle = BscPayloadJobHandle {
            abort_tx,
        };
        
        (job, handle)
    }

    /// Runs the payload job asynchronously with timeout support
    pub async fn start(mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let start_time = std::time::Instant::now();
        let mut build_tasks = tokio::task::JoinSet::new();
        
        // Send initial build_args to the queue
        if let Err(err) = self.try_build_tx.send(self.build_args.clone()) {
            warn!("Failed to send initial build args to try build queue: {}", err);
            return Ok(());
        }
        
        loop {
            tokio::select! {
                // Listen for new arguments from queue
                args = self.try_build_rx.recv() => {
                    match args {
                        Some(args) => {
                            self.retries += 1;
                            debug!("Received new build arguments, starting payload building (retries: {})", self.retries);
                            
                            // Start building payload (non-blocking)
                            let builder = self.builder.clone();
                            let args_clone = BuildArguments {
                                cached_reads: args.cached_reads.clone(),
                                config: args.config.clone(),
                                cancel: args.cancel.clone(),
                                best_payload: args.best_payload.clone(),
                            };
                            build_tasks.spawn(async move {
                                builder.build_payload(args_clone).await
                            });
                        }
                        None => {
                            debug!("Try build queue closed, exiting payload job");
                            break Ok(());
                        }
                    }
                }
                
                // Handle build task completion (non-blocking)
                result = build_tasks.join_next() => {
                    match result {
                        Some(Ok(Ok(payload))) => {
                            if self.is_aborted {
                                break Ok(());
                            }
                            let elapsed = start_time.elapsed();
                            debug!("Built payload: {} (hash: 0x{:x}, txs: {}, fees: {}, cost_time: {:?}, retries: {})", 
                                payload.block().header().number(),
                                payload.block().hash(),
                                payload.block().body().transaction_count(),
                                payload.fees(),
                                elapsed,
                                self.retries
                            );
                            self.potential_payloads.push(payload);

                            // TODO: refine it later.
                            if elapsed < self.timeout / 2 {
                                if let Err(err) = self.try_build_tx.send(self.build_args.clone()) {
                                    warn!("Failed to send args to try build queue: {}", err);
                                    return Err(Box::new(PayloadBuilderError::other(std::io::Error::new(std::io::ErrorKind::Other, "Failed to send args to try build queue"))));
                                }
                            } else {
                                if let Some(best_payload) = self.pick_best_payload() {
                                    info!("Succeed to pick the best payload: {} (hash: 0x{:x}, txs: {}, fees: {})", 
                                        best_payload.block().header().number(),
                                        best_payload.block().hash(),
                                        best_payload.block().body().transaction_count(),
                                        best_payload.fees()
                                    );
                                    if let Err(err) = self.result_tx.send(best_payload) {
                                        warn!("Failed to send best payload to result channel: {}", err);
                                    }
                                }
                                return Ok(());
                            }
                        },
                        Some(Ok(Err(e))) => {
                            let elapsed = start_time.elapsed();
                            warn!("Payload building failed after {:?} (retries: {}): {}", elapsed, self.retries, e);
                        },
                        Some(Err(join_err)) => {
                            let elapsed = start_time.elapsed();
                            warn!("Payload building task failed after {:?} (retries: {}): {}", elapsed, self.retries, join_err);
                        },
                        None => {
                            // No task completed, continue to next iteration
                        },
                    }
                }
                
                // normal finish by timer
                _ = tokio::time::sleep(self.timeout) => {
                    let elapsed = start_time.elapsed();
                    warn!("Payload building timed out after {:?}", elapsed);
                    drop(std::mem::take(&mut self.cancel));
                }
                
                // abort by new head
                _ = &mut self.abort_rx => {
                    let elapsed = start_time.elapsed();
                    info!("Abort payload building by new head, cost_time: {:?}", elapsed);
                    drop(std::mem::take(&mut self.cancel));
                    self.is_aborted = true;
                }
            }
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

        let best_payload = self.potential_payloads.remove(best_index);
        
        // Clear other potential payloads to avoid memory buildup
        self.potential_payloads.clear();
        
        Some(best_payload)
    }
}