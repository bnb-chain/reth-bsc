use crate::chainspec::BscChainSpec;
use crate::consensus::eip4844::{calc_blob_fee, BLOB_TX_BLOB_GAS_PER_BLOB};
use crate::consensus::parlia::Parlia;
use crate::evm::blacklist;
use crate::hardforks::BscHardforks;
use crate::node::engine::BscBuiltPayload;
use crate::node::evm::config::BscEvmConfig;
use crate::node::miner::bid_simulator::BidSimulator;
use crate::node::miner::bsc_miner::{MiningContext, SubmitContext};
use crate::node::pool::BlacklistedAddressError;
use crate::node::primitives::BscBlobTransactionSidecar;
use alloy_consensus::{BlockHeader, Transaction};
use alloy_evm::Evm;
use alloy_primitives::U256;
use reth::payload::EthPayloadBuilderAttributes;
use reth::transaction_pool::error::Eip4844PoolTransactionError;
use reth::transaction_pool::error::InvalidPoolTransactionError;
use reth::transaction_pool::BestTransactionsAttributes;
use reth::transaction_pool::{PoolTransaction, TransactionPool};
use reth_basic_payload_builder::PayloadConfig;
use reth_chain_state::{ExecutedBlock, ExecutedTrieUpdates};
use reth_chainspec::EthChainSpec;
use reth_chainspec::EthereumHardforks;
use reth_ethereum_payload_builder::EthereumBuilderConfig;
use reth_evm::block::{BlockExecutionError, BlockValidationError};
use reth_evm::execute::BlockBuilder;
use reth_evm::execute::BlockBuilderOutcome;
use reth_evm::execute::ExecutionOutcome;
use reth_evm::{ConfigureEvm, NextBlockEnvAttributes};
use reth_payload_primitives::PayloadBuilderAttributes;
use reth_payload_primitives::{BuiltPayload, PayloadBuilderError};
use reth_primitives::HeaderTy;
use reth_primitives::InvalidTransactionError;
use reth_primitives::TransactionSigned;
use reth_primitives_traits::{BlockBody, SignerRecoverable};
use reth_provider::StateProviderFactory;
use reth_revm::cached::CachedReads;
use reth_revm::cancelled::ManualCancel;
use reth_revm::{database::StateProviderDatabase, db::State};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info, trace, warn};

/// Delay left over for mining calculation
pub const DELAY_LEFT_OVER: u64 = 50;

/// Time multiplier for retry condition check
const TIME_MULTIPLIER: u32 = 2;

/// Global trace ID counter for payload building operations
static TRACE_ID_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Generate a unique trace ID for payload building
pub fn generate_trace_id() -> u64 {
    TRACE_ID_COUNTER.fetch_add(1, Ordering::Relaxed)
}

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
    /// Unique trace ID for this build operation
    pub trace_id: u64,
    /// Minimum gas tip
    pub min_gas_tip: u128,
}

/// BSC payload builder, used to build payload for bsc miner.
#[derive(Debug, Clone)]
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
    <EvmConfig as ConfigureEvm>::Primitives: reth_primitives_traits::NodePrimitives<
        BlockHeader = alloy_consensus::Header,
        SignedTx = alloy_consensus::EthereumTxEnvelope<alloy_consensus::TxEip4844>,
        Block = crate::node::primitives::BscBlock,
        Receipt = reth_ethereum_primitives::Receipt,
    >,
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
    pub async fn build_payload(
        &self,
        args: BscBuildArguments<EthPayloadBuilderAttributes>,
    ) -> Result<BscBuiltPayload, Box<dyn std::error::Error + Send + Sync>> {
        let build_start = std::time::Instant::now();
        let BscBuildArguments { mut cached_reads, config, cancel, trace_id, min_gas_tip } = args;
        let PayloadConfig { parent_header, attributes } = config;

        let state_provider = self.client.state_by_block_hash(parent_header.hash_slow())?;
        let state = StateProviderDatabase::new(&state_provider);
        let mut db = State::builder()
            .with_database(cached_reads.as_db_mut(state))
            .with_bundle_update()
            .build();

        let mut builder = self
            .evm_config
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
            warn!(
                target: "payload_builder",
                trace_id,
                %err,
                "failed to apply pre-execution changes"
            );
            PayloadBuilderError::Internal(err.into())
        })?;

        let mut total_fees = U256::ZERO;
        let mut cumulative_gas_used = 0;
        // reserve the systemtx gas
        let system_txs_gas = self.parlia.estimate_gas_reserved_for_system_txs(
            Some(parent_header.timestamp),
            parent_header.number + 1,
            attributes.timestamp,
        );
        let block_gas_limit: u64 =
            builder.evm_mut().block().gas_limit.saturating_sub(system_txs_gas);

        let base_fee = builder.evm_mut().block().basefee;
        trace!("build_payload: base_fee={}", base_fee);

        let mut sidecars_map = HashMap::new();
        let mut block_blob_count = 0;

        let mut blob_fee = None;
        let blob_params = self.chain_spec.blob_params_at_timestamp(attributes.timestamp());
        let header = self.ctx.header.as_ref().ok_or_else(|| {
            Box::new(std::io::Error::other("Missing header in mining context"))
                as Box<dyn std::error::Error + Send + Sync>
        })?;

        if BscHardforks::is_cancun_active_at_timestamp(
            &self.chain_spec,
            header.number,
            header.timestamp,
        ) {
            if let Some(excess) = header.excess_blob_gas {
                if excess != 0 {
                    blob_fee = Some(calc_blob_fee(&self.chain_spec, header));
                }
            }
        }
        let max_blob_count =
            blob_params.as_ref().map(|params| params.max_blob_count).unwrap_or_default();
        let mut best_tx_list = self.pool.best_transactions_with_attributes(
            BestTransactionsAttributes::new(base_fee, blob_fee.map(|fee| fee as u64)),
        );
        while let Some(pool_tx) = best_tx_list.next() {
            if cancel.is_cancelled() {
                break;
            }

            // filter out blacklisted transactions before executing.
            if self.chain_spec.is_nano_active_at_block(parent_header.number + 1)
                && blacklist::check_tx_basic_blacklist(pool_tx.sender(), pool_tx.to())
            {
                debug!(
                    target: "payload_builder",
                    trace_id,
                    tx = ?pool_tx.hash(),
                    "Blacklisted transaction"
                );
                best_tx_list.mark_invalid(
                    &pool_tx,
                    InvalidPoolTransactionError::other(BlacklistedAddressError()),
                );
                continue;
            }
            // filter out tx with min gas tip.
            if pool_tx.effective_tip_per_gas(base_fee).unwrap_or(0_u128) < min_gas_tip {
                // Skip packaging underpriced transactions, but do not mark them invalid.
                trace!(
                    target: "payload_builder",
                    trace_id,
                    tx = ?pool_tx.hash(),
                    effective_tip_per_gas = pool_tx.effective_tip_per_gas(base_fee).unwrap_or(0_u128),
                    min_gas_tip,
                    "Skipping underpriced transaction"
                );
                continue;
            }

            // ensure we still have capacity for this transaction
            if cumulative_gas_used + pool_tx.gas_limit() > block_gas_limit {
                // we can't fit this transaction into the block, so we need to mark it as invalid
                // which also removes all dependent transaction from the iterator before we can
                // continue
                best_tx_list.mark_invalid(
                    &pool_tx,
                    InvalidPoolTransactionError::ExceedsGasLimit(
                        pool_tx.gas_limit(),
                        block_gas_limit,
                    ),
                );
                continue;
            }

            let tx = pool_tx.to_consensus();
            let tx_start = std::time::Instant::now();
            let mut blob_tx_sidecar = None;
            trace!(
                target: "payload_builder",
                trace_id,
                block_number = parent_header.number() + 1,
                tx = ?tx.hash(),
                is_blob_tx = tx.is_eip4844(),
                tx_type = ?tx.tx_type(),
                "Processing transaction"
            );
            if let Some(blob_tx) = tx.as_eip4844() {
                let tx_blob_count = blob_tx.tx().blob_versioned_hashes.len() as u64;
                if block_blob_count + tx_blob_count > max_blob_count {
                    // we can't fit this _blob_ transaction into the block, so we mark it as
                    // invalid, which removes its dependent transactions from
                    // the iterator. This is similar to the gas limit condition
                    // for regular transactions above.
                    debug!(
                        target: "payload_builder",
                        trace_id,
                        tx = ?tx.hash(),
                        block_blob_count,
                        tx_blob_count,
                        max_blob_count,
                        "Skipping blob transaction because it would exceed the max blob count per block"
                    );
                    best_tx_list.mark_invalid(
                        &pool_tx,
                        InvalidPoolTransactionError::Eip4844(
                            Eip4844PoolTransactionError::TooManyEip4844Blobs {
                                have: block_blob_count + tx_blob_count,
                                permitted: max_blob_count,
                            },
                        ),
                    );
                    continue;
                }

                if BscHardforks::is_cancun_active_at_timestamp(
                    &self.chain_spec,
                    parent_header.number + 1,
                    attributes.timestamp(),
                ) {
                    let left = max_blob_count - block_blob_count;
                    if left < blob_tx.tx().blob_gas_used().unwrap_or(0) / BLOB_TX_BLOB_GAS_PER_BLOB
                    {
                        best_tx_list.mark_invalid(
                            &pool_tx,
                            InvalidPoolTransactionError::Eip4844(
                                Eip4844PoolTransactionError::TooManyEip4844Blobs {
                                    have: block_blob_count + tx_blob_count,
                                    permitted: max_blob_count,
                                },
                            ),
                        );
                        continue;
                    }
                }

                let blob_sidecar_result = 'sidecar: {
                    let Some(sidecar) =
                        self.pool.get_blob(*tx.hash()).map_err(PayloadBuilderError::other)?
                    else {
                        break 'sidecar Err(Eip4844PoolTransactionError::MissingEip4844BlobSidecar);
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

                blob_tx_sidecar = match blob_sidecar_result {
                    Ok(sidecar) => Some(sidecar),
                    Err(error) => {
                        warn!(
                            target: "payload_builder",
                            trace_id,
                            block_number = parent_header.number() + 1,
                            tx = ?tx.hash(),
                            ?error,
                            "Skipping blob transaction due to invalid sidecar"
                        );
                        best_tx_list
                            .mark_invalid(&pool_tx, InvalidPoolTransactionError::Eip4844(error));
                        continue;
                    }
                };
                trace!(
                    target: "payload_builder",
                    trace_id,
                    block_number = parent_header.number() + 1,
                    tx = ?tx.hash(),
                    has_sidecar = blob_tx_sidecar.is_some(),
                    "Blob transaction sidecar prepared"
                );
            }

            let gas_used = match builder.execute_transaction(tx.clone()) {
                Ok(gas_used) => gas_used,
                Err(BlockExecutionError::Validation(BlockValidationError::InvalidTx {
                    error,
                    ..
                })) => {
                    if error.is_nonce_too_low() {
                        // if the nonce is too low, we can skip this transaction
                        debug!(
                            target: "bsc::miner::payload",
                            trace_id,
                            tx_hash = %tx.hash(),
                            sender = ?tx.signer(),
                            nonce = tx.nonce(),
                            error = %error,
                            "Skipping nonce too low transaction"
                        );
                        best_tx_list.mark_invalid(
                            &pool_tx,
                            InvalidPoolTransactionError::Consensus(
                                InvalidTransactionError::NonceNotConsistent {
                                    tx: tx.nonce(),
                                    state: 0_u64, // TODO: get the nonce from the state later.
                                },
                            ),
                        );
                    } else {
                        // if the transaction is invalid, we can skip it and all of its
                        // descendants
                        debug!(
                            target: "bsc::miner::payload",
                            trace_id,
                            tx_hash = %tx.hash(),
                            sender = ?tx.signer(),
                            nonce = tx.nonce(),
                            gas_limit = tx.gas_limit(),
                            error = %error,
                            error_type = ?error,
                            "Skipping invalid transaction and its descendants"
                        );
                        best_tx_list.mark_invalid(
                            &pool_tx,
                            InvalidPoolTransactionError::Consensus(
                                InvalidTransactionError::TxTypeNotSupported,
                            ),
                        );
                    }
                    continue;
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
            let miner_fee = tx
                .effective_tip_per_gas(base_fee)
                .expect("fee is always valid; execution succeeded");
            total_fees += U256::from(miner_fee) * U256::from(gas_used);
            cumulative_gas_used += gas_used;

            let tx_duration = tx_start.elapsed();
            if tx_duration.as_micros() > 3000 {
                debug!(
                    target: "payload_builder",
                    trace_id,
                    block_number = parent_header.number() + 1,
                    tx = ?tx.hash(),
                    gas_used,
                    cumulative_gas_used,
                    duration_micros = tx_duration.as_micros(),
                    "Transaction executed successfully (slow)"
                );
            } else {
                trace!(
                    target: "payload_builder",
                    trace_id,
                    block_number = parent_header.number() + 1,
                    tx = ?tx.hash(),
                    gas_used,
                    cumulative_gas_used,
                    duration_micros = tx_duration.as_micros(),
                    "Transaction executed successfully"
                );
            }

            // Add blob tx sidecar to the payload.
            if let Some(sidecar) = blob_tx_sidecar {
                sidecars_map.insert(*tx.hash(), sidecar);
            }
        }

        // add system txs to payload.
        let finalize_start = std::time::Instant::now();
        let BlockBuilderOutcome { execution_result, hashed_state, trie_updates, block } =
            builder.finish(&state_provider)?;
        let mut sealed_block = Arc::new(block.sealed_block().clone());

        // Update miner metrics
        use crate::metrics::BscMinerMetrics;
        use once_cell::sync::Lazy;
        static MINER_METRICS: Lazy<BscMinerMetrics> = Lazy::new(BscMinerMetrics::default);

        let finalize_duration = finalize_start.elapsed().as_secs_f64();
        MINER_METRICS.block_finalize_duration_seconds.record(finalize_duration);
        MINER_METRICS.blocks_produced_total.increment(1);

        // set sidecars to seal block
        let mut blob_sidecars: Vec<BscBlobTransactionSidecar> = Vec::new();
        let transactions = &sealed_block.body().inner.transactions;

        let build_duration = build_start.elapsed();
        let avg_tx_duration_micros = if !transactions.is_empty() {
            build_duration.as_micros() / transactions.len() as u128
        } else {
            0
        };

        debug!(
            target: "payload_builder",
            trace_id,
            block_number = sealed_block.number(),
            block_hash = ?sealed_block.hash(),
            tx_count = transactions.len(),
            cumulative_gas_used,
            total_fees = %total_fees,
            build_duration_ms = build_duration.as_millis(),
            avg_tx_duration_micros,
            "Block payload built successfully"
        );

        for (index, tx) in transactions.iter().enumerate() {
            trace!(
                target: "payload_builder",
                trace_id,
                tx_index = index,
                tx_hash = ?tx.hash(),
                from = ?tx.recover_signer().ok(),
                to = ?tx.to(),
                value = ?tx.value(),
                gas_limit = tx.gas_limit(),
                gas_price = ?tx.gas_price(),
                nonce = tx.nonce(),
                "Transaction included in block"
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
            block: sealed_block.clone(),
            fees: total_fees,
            requests: Some(execution_result.requests.clone()),
            executed_block: ExecutedBlock {
                recovered_block: Arc::new(block),
                execution_output: Arc::new(ExecutionOutcome::new(
                    db.take_bundle(),
                    vec![execution_result.receipts.clone()],
                    sealed_block.header().number(),
                    vec![execution_result.requests.clone()],
                )),
                hashed_state: Arc::new(hashed_state),
            },
            executed_trie: Some(ExecutedTrieUpdates::Present(Arc::new(trie_updates))),
        };
        Ok(payload)
    }

    /// Build an empty payload without any user transactions from the pool
    /// Only contains system transactions (if any)
    pub async fn build_empty_payload(
        &self,
        args: BscBuildArguments<EthPayloadBuilderAttributes>,
    ) -> Result<BscBuiltPayload, Box<dyn std::error::Error + Send + Sync>> {
        let build_start = std::time::Instant::now();
        let BscBuildArguments { mut cached_reads, config, cancel: _, trace_id, min_gas_tip: _ } =
            args;
        let PayloadConfig { parent_header, attributes } = config;

        let state_provider = self.client.state_by_block_hash(parent_header.hash_slow())?;
        let state = StateProviderDatabase::new(&state_provider);
        let mut db = State::builder()
            .with_database(cached_reads.as_db_mut(state))
            .with_bundle_update()
            .build();

        let mut builder = self
            .evm_config
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
            warn!(
                target: "payload_builder",
                trace_id,
                %err,
                "failed to apply pre-execution changes for empty payload"
            );
            PayloadBuilderError::Internal(err.into())
        })?;

        // No user transactions - only system transactions will be added by finish()
        let total_fees = U256::ZERO;
        let cumulative_gas_used = 0;

        // Add system txs to payload and finalize
        let finalize_start = std::time::Instant::now();
        let BlockBuilderOutcome { execution_result, hashed_state, trie_updates, block } =
            builder.finish(&state_provider)?;
        let sealed_block = Arc::new(block.sealed_block().clone());

        // Update miner metrics
        use crate::metrics::BscMinerMetrics;
        use once_cell::sync::Lazy;
        static MINER_METRICS: Lazy<BscMinerMetrics> = Lazy::new(BscMinerMetrics::default);

        let finalize_duration = finalize_start.elapsed().as_secs_f64();
        MINER_METRICS.block_finalize_duration_seconds.record(finalize_duration);
        MINER_METRICS.blocks_produced_total.increment(1);

        let build_duration = build_start.elapsed();

        debug!(
            target: "payload_builder",
            trace_id,
            block_number = sealed_block.number(),
            block_hash = ?sealed_block.hash(),
            tx_count = sealed_block.body().transactions.len(),
            cumulative_gas_used,
            total_fees = %total_fees,
            build_duration_ms = build_duration.as_millis(),
            "Empty block payload built successfully (no user transactions)"
        );

        let payload = BscBuiltPayload {
            block: sealed_block.clone(),
            fees: total_fees,
            requests: Some(execution_result.requests.clone()),
            executed_block: ExecutedBlock {
                recovered_block: Arc::new(block),
                execution_output: Arc::new(ExecutionOutcome::new(
                    db.take_bundle(),
                    vec![execution_result.receipts.clone()],
                    sealed_block.header().number(),
                    vec![execution_result.requests.clone()],
                )),
                hashed_state: Arc::new(hashed_state),
            },
            executed_trie: Some(ExecutedTrieUpdates::Present(Arc::new(trie_updates))),
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
    join_handle:
        tokio::task::JoinSet<Result<BscBuiltPayload, Box<dyn std::error::Error + Send + Sync>>>,
    /// Simulator for bid management (no outer RwLock, each map has its own)
    simulator: Arc<BidSimulator<Client, Pool>>,
    /// Job start time for tracking total duration
    job_start_time: std::time::Instant,
    /// Unique trace ID for this payload job
    trace_id: u64,
}

impl<Pool, Client, EvmConfig> BscPayloadJob<Pool, Client, EvmConfig>
where
    Client: StateProviderFactory
        + reth_provider::HeaderProvider<Header = alloy_consensus::Header>
        + reth_provider::BlockHashReader
        + Clone
        + 'static,
    EvmConfig: ConfigureEvm<NextBlockEnvCtx = NextBlockEnvAttributes> + 'static,
    <EvmConfig as ConfigureEvm>::Primitives: reth_primitives_traits::NodePrimitives<
        BlockHeader = alloy_consensus::Header,
        SignedTx = alloy_consensus::EthereumTxEnvelope<alloy_consensus::TxEip4844>,
        Block = crate::node::primitives::BscBlock,
        Receipt = reth_ethereum_primitives::Receipt,
    >,
    Pool: TransactionPool<Transaction: PoolTransaction<Consensus = TransactionSigned>> + 'static,
{
    /// Creates a new BscPayloadJob and returns both the job and its handle
    pub fn new(
        parlia: Arc<crate::consensus::parlia::Parlia<crate::chainspec::BscChainSpec>>,
        mining_ctx: MiningContext,
        builder: BscPayloadBuilder<Pool, Client, EvmConfig>,
        build_args: BscBuildArguments<EthPayloadBuilderAttributes>,
        simulator: Arc<BidSimulator<Client, Pool>>, // No outer RwLock needed
        result_tx: mpsc::UnboundedSender<SubmitContext>,
    ) -> (Self, BscPayloadJobHandle) {
        let (abort_tx, abort_rx) = oneshot::channel();
        let (try_build_tx, try_build_rx) = mpsc::unbounded_channel();
        let (tx_listener_tx, tx_listener_rx) = mpsc::unbounded_channel();

        let trace_id = build_args.trace_id;

        let mining_delay = parlia.clone().delay_for_mining(
            &mining_ctx.parent_snapshot,
            mining_ctx.header.as_ref().unwrap(),
            DELAY_LEFT_OVER,
        );

        // Spawn a background task to listen for new transactions from pool
        // When tx_listener_rx is dropped (job ends), tx_listener_tx.send() will fail,
        // causing this task to exit and pool_listener to be dropped,
        // which triggers cleanup of the listener in txpool via retain_mut.
        let mut pool_listener = builder.pool.pending_transactions_listener();
        tokio::spawn(async move {
            while let Some(tx_hash) = pool_listener.recv().await {
                // If send fails, receiver is dropped (job ended), exit to cleanup listener
                if tx_listener_tx.send(tx_hash).is_err() {
                    break;
                }
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
            job_start_time: std::time::Instant::now(),
            trace_id,
        };
        let handle = BscPayloadJobHandle { abort_tx };

        debug!(
            target: "bsc::miner::payload",
            trace_id,
            block_number = job.mining_ctx.parent_header.number() + 1,
            is_inturn = job.mining_ctx.is_inturn,
            timeout = ?job.timeout,
            "Succeed to new payload job"
        );
        (job, handle)
    }

    /// Runs the payload job asynchronously with timeout support
    pub async fn start(mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut start_time = std::time::Instant::now();
        if let Err(err) = self.try_build_tx.send(()) {
            warn!(
                target: "bsc::miner::payload",
                trace_id = self.trace_id,
                block_number = self.build_args.config.parent_header.number() + 1,
                is_inturn = self.mining_ctx.is_inturn,
                error = %err,
                "Failed to send to first try build queue"
            );
            return Err(Box::new(BscPayloadJobError::BuildQueueSendError(err.to_string())));
        }

        loop {
            // Calculate remaining time from job start for outer loop
            let job_elapsed = self.job_start_time.elapsed();
            let remaining_duration = if job_elapsed < self.timeout {
                self.timeout - job_elapsed
            } else {
                // Already timeout, return immediately
                info!(
                    target: "bsc::miner::payload",
                    trace_id = self.trace_id,
                    block_number = self.build_args.config.parent_header.number() + 1,
                    is_inturn = self.mining_ctx.is_inturn,
                    job_elapsed_ms = job_elapsed.as_millis(),
                    timeout_ms = self.timeout.as_millis(),
                    "Outer loop: Job already timeout, returning best payload"
                );
                return self.try_return_best_payload();
            };

            tokio::select! {
                // Trigger the async build payload by queue.
                args = self.try_build_rx.recv() => {
                    match args {
                        Some(_) => {
                            self.retries += 1;
                            start_time = std::time::Instant::now();
                            debug!(
                                target: "bsc::miner::payload",
                                trace_id = self.trace_id,
                                block_number = self.build_args.config.parent_header.number() + 1,
                                is_inturn = self.mining_ctx.is_inturn,
                                retries = self.retries,
                                "Try new build"
                            );

                            let builder = self.builder.clone();
                            let build_args = self.build_args.clone();
                            self.join_handle.spawn(async move {
                                builder.build_payload(build_args).await
                            });
                        }
                        None => {
                            debug!(
                                target: "bsc::miner::payload",
                                trace_id = self.trace_id,
                                block_number = self.build_args.config.parent_header.number() + 1,
                                is_inturn = self.mining_ctx.is_inturn,
                                "Exit payload job by queue closed"
                            );
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
                            debug!(
                                target: "bsc::miner::payload",
                                trace_id = self.trace_id,
                                block_number = payload.block().header().number(),
                                block_hash = %payload.block().hash(),
                                is_inturn = self.mining_ctx.is_inturn,
                                tx_count = payload.block().body().transaction_count(),
                                fees = %payload.fees(),
                                cost_time = ?elapsed,
                                retries = self.retries,
                                "Succeed to try new build"
                            );
                            self.potential_payloads.push(payload);
                            let mut new_tx_count = 0;
                            // loop wait new transactions or timeout.
                            loop {
                                // Calculate remaining time from job start
                                let job_elapsed = self.job_start_time.elapsed();
                                let remaining_duration = if job_elapsed < self.timeout {
                                    self.timeout - job_elapsed
                                } else {
                                    // Already timeout, return immediately
                                    info!(
                                        target: "bsc::miner::payload",
                                        trace_id = self.trace_id,
                                        block_number = self.build_args.config.parent_header.number() + 1,
                                        is_inturn = self.mining_ctx.is_inturn,
                                        job_elapsed_ms = job_elapsed.as_millis(),
                                        timeout_ms = self.timeout.as_millis(),
                                        retries = self.retries,
                                        "Job already timeout, returning best payload immediately"
                                    );
                                    return self.try_return_best_payload();
                                };

                                tokio::select! {
                                    // Use remaining time instead of full timeout
                                    _ = tokio::time::sleep(remaining_duration) => {
                                        info!(
                                            target: "bsc::miner::payload",
                                            trace_id = self.trace_id,
                                            block_number = self.build_args.config.parent_header.number() + 1,
                                            is_inturn = self.mining_ctx.is_inturn,
                                            cost_time = ?elapsed,
                                            retries = self.retries,
                                            job_elapsed_ms = self.job_start_time.elapsed().as_millis(),
                                            "try return best payload due to has no time"
                                        );
                                        return self.try_return_best_payload();
                                    }

                                    // Abort by new head.
                                    _ = &mut self.abort_rx => {
                                        info!(
                                            target: "bsc::miner::payload",
                                            trace_id = self.trace_id,
                                            block_number = self.build_args.config.parent_header.number() + 1,
                                            is_inturn = self.mining_ctx.is_inturn,
                                            cost_time = ?elapsed,
                                            retries = self.retries,
                                            "Abort payload building by new head"
                                        );
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
                                            debug!(
                                                target: "bsc::miner::payload",
                                                trace_id = self.trace_id,
                                                block_number = self.build_args.config.parent_header.number() + 1,
                                                is_inturn = self.mining_ctx.is_inturn,
                                                retries = self.retries,
                                                new_mining_delay = ?std::time::Duration::from_millis(mining_delay),
                                                last_cost_time = ?elapsed,
                                                "try return best payload due to mining_delay < elapsed"
                                            );
                                            return self.try_return_best_payload();
                                        } else if std::time::Duration::from_millis(mining_delay) < elapsed * TIME_MULTIPLIER {
                                            if let Err(err) = self.try_build_tx.send(()) {
                                                warn!(
                                                    target: "bsc::miner::payload",
                                                    trace_id = self.trace_id,
                                                    block_number = self.build_args.config.parent_header.number() + 1,
                                                    is_inturn = self.mining_ctx.is_inturn,
                                                    retries = self.retries,
                                                    error = ?err,
                                                    "Failed to send to try build queue"
                                                );
                                                return self.try_return_best_payload();
                                            }
                                            debug!(
                                                target: "bsc::miner::payload",
                                                trace_id = self.trace_id,
                                                block_number = self.build_args.config.parent_header.number() + 1,
                                                is_inturn = self.mining_ctx.is_inturn,
                                                retries = self.retries,
                                                last_cost_time = ?elapsed,
                                                new_mining_delay = ?std::time::Duration::from_millis(mining_delay),
                                                "Succeed to send to try build queue"
                                            );
                                            break;  // Break out of the loop and wait for the next payload
                                        } else if new_tx_count >= payload_tx_count {
                                            if let Err(err) = self.try_build_tx.send(()) {
                                                warn!(
                                                    target: "bsc::miner::payload",
                                                    trace_id = self.trace_id,
                                                    block_number = self.build_args.config.parent_header.number() + 1,
                                                    is_inturn = self.mining_ctx.is_inturn,
                                                    retries = self.retries,
                                                    error = ?err,
                                                    "Failed to send to try build queue"
                                                );
                                                return self.try_return_best_payload();
                                            }
                                            debug!(
                                                target: "bsc::miner::payload",
                                                trace_id = self.trace_id,
                                                block_number = self.build_args.config.parent_header.number() + 1,
                                                is_inturn = self.mining_ctx.is_inturn,
                                                retries = self.retries,
                                                last_cost_time = ?elapsed,
                                                new_mining_delay = ?std::time::Duration::from_millis(mining_delay),
                                                "Succeed to send to try build queue"
                                            );
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
                                trace_id = self.trace_id,
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
                            warn!(
                                target: "bsc::miner::payload",
                                trace_id = self.trace_id,
                                block_number = self.build_args.config.parent_header.number() + 1,
                                is_inturn = self.mining_ctx.is_inturn,
                                cost_time = ?elapsed,
                                retries = self.retries,
                                error = %join_err,
                                "Failed to join payload build task"
                            );
                            return self.try_return_best_payload();
                        },
                        None => {
                            // No task completed, continue to next iteration
                        },
                    }
                }

                // Finish timeout by timer using remaining duration
                _ = tokio::time::sleep(remaining_duration) => {
                    let elapsed = start_time.elapsed();
                    info!(
                        target: "bsc::miner::payload",
                        trace_id = self.trace_id,
                        block_number = self.build_args.config.parent_header.number() + 1,
                        is_inturn = self.mining_ctx.is_inturn,
                        cost_time = ?elapsed,
                        retries = self.retries,
                        job_elapsed_ms = self.job_start_time.elapsed().as_millis(),
                        timeout_ms = self.timeout.as_millis(),
                        "Try return best payload due to has no time"
                    );
                    self.build_args.cancel.clone().cancel();
                    return self.try_return_best_payload();
                }

                // Abort by new head.
                _ = &mut self.abort_rx => {
                    let elapsed = start_time.elapsed();
                    info!(
                        target: "bsc::miner::payload",
                        trace_id = self.trace_id,
                        block_number = self.build_args.config.parent_header.number() + 1,
                        is_inturn = self.mining_ctx.is_inturn,
                        parent_hash = %self.build_args.config.parent_header.parent_hash(),
                        cost_time = ?elapsed,
                        retries = self.retries,
                        "Abort payload building by new head"
                    );
                    self.build_args.cancel.clone().cancel();
                    self.is_aborted = true;
                    return Err(Box::new(BscPayloadJobError::JobAborted));
                }
            }
        }
    }

    /// Try to return the best payload to result channel
    fn try_return_best_payload(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut bid_block_hash = None;
        let best_bid = self.simulator.get_best_bid(self.mining_ctx.parent_header.hash());
        if let Some(bid) = best_bid {
            info!(
                target: "bsc::miner::payload",
                trace_id = self.trace_id,
                block_number = bid.bid.block_number,
                is_inturn = self.mining_ctx.is_inturn,
                builder = ?bid.bid.builder,
                gas_fee = %bid.bid.gas_fee,
                bid_hash = %bid.bid.bid_hash,
                gas_fee = %bid.bsc_payload.fees(),
                "Found best bid"
            );
            bid_block_hash = Some(bid.bsc_payload.block.hash());
            self.potential_payloads.push(bid.bsc_payload);
        }
        if let Some(best_payload) = self.pick_best_payload() {
            let best_payload_hash = best_payload.block.hash();
            if let Err(err) = self.result_tx.send(SubmitContext {
                mining_ctx: self.mining_ctx.clone(),
                payload: best_payload,
                cancel: self.build_args.cancel.clone(),
            }) {
                let total_job_duration = self.job_start_time.elapsed();
                warn!(
                    target: "bsc::miner::payload",
                    trace_id = self.trace_id,
                    block_number = self.build_args.config.parent_header.number() + 1,
                    is_inturn = self.mining_ctx.is_inturn,
                    total_job_duration_ms = total_job_duration.as_millis(),
                    error = %err,
                    "Failed to send best payload to result channel"
                );
                return Err(Box::new(BscPayloadJobError::ResultChannelSendError(err.to_string())));
            }

            // Check if the best payload is from a bid and increment bid_win metric
            if let Some(bid_hash) = bid_block_hash {
                if best_payload_hash == bid_hash {
                    use crate::metrics::BscMevMetrics;
                    use once_cell::sync::Lazy;
                    static MEV_METRICS: Lazy<BscMevMetrics> = Lazy::new(BscMevMetrics::default);
                    MEV_METRICS.bid_win_total.increment(1);

                    debug!(
                        target: "bsc::miner::payload",
                        trace_id = self.trace_id,
                        block_number = self.build_args.config.parent_header.number() + 1,
                        bid_hash = %bid_hash,
                        "Bid payload won - incrementing bid_win metric"
                    );
                }
            }

            Ok(())
        } else {
            // No best payload available
            let total_job_duration = self.job_start_time.elapsed();

            // If in-turn, build an empty payload as fallback
            if self.mining_ctx.is_inturn {
                warn!(
                    target: "bsc::miner::payload",
                    trace_id = self.trace_id,
                    try_mine_block_number = self.build_args.config.parent_header.number() + 1,
                    is_inturn = self.mining_ctx.is_inturn,
                    total_job_duration_ms = total_job_duration.as_millis(),
                    "No best payload available, building empty payload as in-turn fallback"
                );

                // Build empty payload synchronously (blocking) and measure time
                let empty_build_start = std::time::Instant::now();
                let empty_payload_result = tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current().block_on(async {
                        self.builder.build_empty_payload(self.build_args.clone()).await
                    })
                });
                let empty_build_duration = empty_build_start.elapsed();

                match empty_payload_result {
                    Ok(empty_payload) => {
                        info!(
                            target: "bsc::miner::payload",
                            trace_id = self.trace_id,
                            block_number = empty_payload.block().header().number(),
                            block_hash = %empty_payload.block().hash(),
                            is_inturn = self.mining_ctx.is_inturn,
                            tx_count = empty_payload.block().body().transaction_count(),
                            empty_build_duration_ms = empty_build_duration.as_millis(),
                            "Successfully built empty payload as in-turn fallback"
                        );

                        if let Err(err) = self.result_tx.send(SubmitContext {
                            mining_ctx: self.mining_ctx.clone(),
                            payload: empty_payload,
                            cancel: self.build_args.cancel.clone(),
                        }) {
                            warn!(
                                target: "bsc::miner::payload",
                                trace_id = self.trace_id,
                                error = %err,
                                "Failed to send empty fallback payload"
                            );
                            return Err(Box::new(BscPayloadJobError::ResultChannelSendError(
                                err.to_string(),
                            )));
                        }
                        Ok(())
                    }
                    Err(e) => {
                        error!(
                            target: "bsc::miner::payload",
                            trace_id = self.trace_id,
                            error = %e,
                            empty_build_duration_ms = empty_build_duration.as_millis(),
                            "Failed to build empty payload as in-turn fallback"
                        );
                        Err(Box::new(BscPayloadJobError::NoPayloadsAvailable))
                    }
                }
            } else {
                // Off-turn: just return error
                warn!(
                    target: "bsc::miner::payload",
                    trace_id = self.trace_id,
                    try_mine_block_number = self.build_args.config.parent_header.number() + 1,
                    is_inturn = self.mining_ctx.is_inturn,
                    total_job_duration_ms = total_job_duration.as_millis(),
                    "No best payload available to send (off-turn)"
                );
                Err(Box::new(BscPayloadJobError::NoPayloadsAvailable))
            }
        }
    }

    /// Pick the best payload from potential payloads
    fn pick_best_payload(&mut self) -> Option<BscBuiltPayload> {
        if self.potential_payloads.is_empty() {
            return None;
        }

        // pick the payload with the highest fees as best payload.
        let best_index = self
            .potential_payloads
            .iter()
            .enumerate()
            .max_by_key(|(_, payload)| payload.fees())
            .map(|(index, _)| index)?;

        let total_len = self.potential_payloads.len();
        let best_payload = self.potential_payloads.remove(best_index);
        let total_job_duration = self.job_start_time.elapsed();

        let gas_used = best_payload.block().header().gas_used();
        let gas_limit = best_payload.block().header().gas_limit();
        let gas_usage_percent =
            if gas_limit > 0 { (gas_used as f64 / gas_limit as f64 * 100.0) as u64 } else { 0 };

        info!(
            target: "bsc::miner::payload",
            trace_id = self.trace_id,
            block_number = best_payload.block().header().number(),
            block_hash = %best_payload.block().hash(),
            is_inturn = self.mining_ctx.is_inturn,
            tx_count = best_payload.block().body().transaction_count(),
            fees = %best_payload.fees(),
            gas_used = gas_used,
            gas_limit = gas_limit,
            gas_usage_percent = gas_usage_percent,
            pick_index = best_index + 1,
            total_len = total_len,
            total_job_duration_ms = total_job_duration.as_millis(),
            "Succeed to pick the best payload"
        );

        self.potential_payloads.clear();
        Some(best_payload)
    }
}
