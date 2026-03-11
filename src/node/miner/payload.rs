use crate::chainspec::BscChainSpec;
use crate::consensus::eip4844::{calc_blob_fee, BLOB_TX_BLOB_GAS_PER_BLOB};
use crate::consensus::parlia::Parlia;
use crate::evm::blacklist;
use crate::hardforks::BscHardforks;
use crate::node::engine::{BscBuiltPayload, BuildKind};
use crate::metrics::BscMinerMetrics;
use crate::node::evm::config::{BscEvmConfig, BscMinerRetryCache, BscNextBlockEnvAttributes};
use crate::node::evm::{request_difflayer, MinerTrieDbPrefetcher};
use crate::node::miner::bid_simulator::BidSimulator;
use crate::node::miner::bsc_miner::{MiningContext, SubmitContext};
use crate::node::pool::BlacklistedAddressError;
use crate::node::primitives::BscBlobTransactionSidecar;
use alloy_consensus::{BlockHeader, Transaction};
use alloy_evm::block::BlockExecutor;
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
use reth_primitives_traits::{Block, BlockBody, SignerRecoverable};
use reth_provider::StateProviderFactory;
use reth_revm::cached::CachedReads;
use reth_revm::cancelled::ManualCancel;
use reth_revm::state::EvmState as RethEvmState;
use reth_revm::{database::StateProviderDatabase, db::State};
use rust_eth_triedb::get_global_triedb;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use once_cell::sync::Lazy;
use tracing::{debug, info, trace, warn};

/// Delay left over for mining calculation
pub const DELAY_LEFT_OVER: u64 = 50; // Reserve finalize/broadcast slack on fast-block forks.


/// Global trace ID counter for payload building operations
static TRACE_ID_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Generate a unique trace ID for payload building
pub fn generate_trace_id() -> u64 {
    TRACE_ID_COUNTER.fetch_add(1, Ordering::Relaxed)
}

fn miner_metrics() -> &'static BscMinerMetrics {
    static MINER_METRICS: Lazy<BscMinerMetrics> = Lazy::new(BscMinerMetrics::default);
    &MINER_METRICS
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
    /// Retry-stable miner cache shared across rebuilds for the same parent.
    pub retry_cache: Option<BscMinerRetryCache>,
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
    EvmConfig: ConfigureEvm<NextBlockEnvCtx = BscNextBlockEnvAttributes> + 'static,
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
        let BscBuildArguments { mut cached_reads, config, cancel, trace_id, min_gas_tip, retry_cache } = args;
        let PayloadConfig { parent_header, attributes } = config;

        // If triedb is active, fetch parent difflayers *before* creating the block builder.
        //
        // Important: the block builder is not `Send`, and this function is spawned onto a tokio
        // JoinSet. Therefore we must not hold the builder across any `.await`.
        let parent_hash = parent_header.hash_slow();
        let triedb_parent_difflayers = if rust_eth_triedb::triedb_manager::is_triedb_active() {
            match crate::shared::get_engine_api_tx() {
                Some(engine_api_tx) => match request_difflayer(&engine_api_tx, parent_hash).await {
                    Ok(difflayers) => Some(difflayers),
                    Err(e) => {
                        warn!(
                            target: "payload_builder",
                            trace_id,
                            parent_hash = ?parent_hash,
                            error = %e,
                            "Failed to request parent difflayers for triedb prefetcher, continuing without prefetcher"
                        );
                        None
                    }
                },
                None => {
                    warn!(
                        target: "payload_builder",
                        trace_id,
                        parent_hash = ?parent_hash,
                        "Engine api tx not found; aborting payload build (triedb active)"
                    );
                    return Err(Box::new(std::io::Error::other("engine api tx not found")));
                }
            }
        } else {
            None
        };

        let state_provider = self.client.state_by_block_hash(parent_header.hash_slow())?;
        let state = StateProviderDatabase::new(&state_provider);
        let mut db = State::builder()
            .with_database(cached_reads.as_db_mut(state))
            .with_bundle_update()
            .build();

        // Build triedb prefetcher before creating the block builder so it can be carried via the
        // custom next-block env ctx into the execution ctx and consumed in `finish()`.
        let triedb_prefetcher = triedb_parent_difflayers.clone().and_then(|difflayers| {
            let mut triedb = get_global_triedb();
            let path_db = triedb.get_mut_path_db_ref().clone();
            MinerTrieDbPrefetcher::new(parent_header.state_root(), path_db, Some(difflayers)).ok()
        });

        let next_env_attributes = BscNextBlockEnvAttributes {
            inner: NextBlockEnvAttributes {
                timestamp: attributes.timestamp(),
                suggested_fee_recipient: attributes.suggested_fee_recipient(),
                prev_randao: attributes.prev_randao(),
                gas_limit: self.builder_config.gas_limit(parent_header.gas_limit),
                parent_beacon_block_root: attributes.parent_beacon_block_root(),
                withdrawals: Some(attributes.withdrawals().clone()),
            },
            parent_difflayers: triedb_parent_difflayers.clone(),
            triedb_prefetcher: triedb_prefetcher.clone(),
            miner_retry_cache: retry_cache.clone(),
        };

        let mut builder = self
            .evm_config
            .builder_for_next_block(&mut db, &parent_header, next_env_attributes)
            .map_err(PayloadBuilderError::other)?;

        // Wire miner triedb prefetcher via state hook (if enabled).
        //
        // NOTE: This must be set before `apply_pre_execution_changes()` so any state access/touches
        // performed during pre-execution are also prefetched.
        if let Some(prefetcher) = triedb_prefetcher.clone() {
            let pf = prefetcher.clone();
            builder
                .executor_mut()
                .set_state_hook(Some(Box::new(move |_, update: &RethEvmState| {
                    pf.on_state_update(update);
                })));
            debug!(
                target: "payload_builder",
                trace_id,
                parent_hash = ?parent_hash,
                "Started triedb prefetcher for miner payload build"
            );
        }

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

        // Total time spent selecting + executing user transactions.
        let exec_start = std::time::Instant::now();
        // Everything before `exec_start` is treated as "prepare" time for this payload attempt.
        let prepare_duration = exec_start.duration_since(build_start);
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
            let tx_hash = *tx.hash();
            let tx_nonce = tx.nonce();
            let tx_gas_limit = tx.gas_limit();
            let tx_signer = tx.signer();
            let tx_effective_tip = tx.effective_tip_per_gas(base_fee);
            let tx_start = std::time::Instant::now();
            let mut blob_tx_sidecar = None;
            let mut executed_blob_count = None;
            trace!(
                target: "payload_builder",
                trace_id,
                block_number = parent_header.number() + 1,
                tx = ?tx_hash,
                is_blob_tx = tx.is_eip4844(),
                tx_type = ?tx.tx_type(),
                "Processing transaction"
            );
            if let Some(blob_tx) = tx.as_eip4844() {
                let tx_blob_count = blob_tx.tx().blob_versioned_hashes.len() as u64;
                executed_blob_count = Some(tx_blob_count);
                if block_blob_count + tx_blob_count > max_blob_count {
                    // we can't fit this _blob_ transaction into the block, so we mark it as
                    // invalid, which removes its dependent transactions from
                    // the iterator. This is similar to the gas limit condition
                    // for regular transactions above.
                    debug!(
                        target: "payload_builder",
                        trace_id,
                        tx = ?tx_hash,
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
                            tx = ?tx_hash,
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

            let gas_used = match builder.execute_transaction(tx) {
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
                            tx_hash = %tx_hash,
                            sender = ?tx_signer,
                            nonce = tx_nonce,
                            error = %error,
                            "Skipping nonce too low transaction"
                        );
                        best_tx_list.mark_invalid(
                            &pool_tx,
                            InvalidPoolTransactionError::Consensus(
                                InvalidTransactionError::NonceNotConsistent {
                                    tx: tx_nonce,
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
                            tx_hash = %tx_hash,
                            sender = ?tx_signer,
                            nonce = tx_nonce,
                            gas_limit = tx_gas_limit,
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
                Err(err) => {
                    return Err(Box::new(std::io::Error::other(err.to_string())));
                }
            };

            // add to the total blob gas used if the transaction successfully executed
            if let Some(tx_blob_count) = executed_blob_count {
                block_blob_count += tx_blob_count;

                // if we've reached the max blob count, we can skip blob txs entirely
                if block_blob_count == max_blob_count {
                    best_tx_list.skip_blobs();
                }
            }
            // update and add to total fees
            let miner_fee = tx_effective_tip
                .expect("fee is always valid; execution succeeded");
            total_fees += U256::from(miner_fee) * U256::from(gas_used);
            cumulative_gas_used += gas_used;

            let tx_duration = tx_start.elapsed();
            if tx_duration.as_micros() > 3000 {
                debug!(
                    target: "payload_builder",
                    trace_id,
                    block_number = parent_header.number() + 1,
                    tx = ?tx_hash,
                    gas_used = ?gas_used,
                    cumulative_gas_used = ?cumulative_gas_used,
                    duration_micros = tx_duration.as_micros(),
                    "Transaction executed successfully (slow)"
                );
            } else {
                trace!(
                    target: "payload_builder",
                    trace_id,
                    block_number = parent_header.number() + 1,
                    tx = ?tx_hash,
                    gas_used = ?gas_used,
                    cumulative_gas_used = ?cumulative_gas_used,
                    duration_micros = tx_duration.as_micros(),
                    "Transaction executed successfully"
                );
            }

            // Add blob tx sidecar to the payload.
            if let Some(sidecar) = blob_tx_sidecar {
                sidecars_map.insert(tx_hash, sidecar);
            }
        }
        let exec_duration = exec_start.elapsed();

        // add system txs to payload.
        let finalize_start = std::time::Instant::now();
        let out = builder.finish_with_difflayer(&state_provider)?;
        let BlockBuilderOutcome { execution_result, hashed_state, trie_updates, block } = out.inner;
        let difflayer = out.difflayer;

        let block_hash = block.sealed_block().hash();
        let mut plain_block = block.sealed_block().clone_block();

        let finalize_elapsed = finalize_start.elapsed();
        let finalize_duration = finalize_elapsed.as_secs_f64();
        miner_metrics().block_finalize_duration_seconds.record(finalize_duration);
        miner_metrics().blocks_produced_total.increment(1);

        // set sidecars to seal block
        let mut blob_sidecars: Vec<BscBlobTransactionSidecar> = Vec::new();
        let block_number = plain_block.header.number();
        let transactions = &plain_block.body.inner.transactions;

        let build_duration = build_start.elapsed();
        let avg_tx_duration_micros = if !transactions.is_empty() {
            build_duration.as_micros() / transactions.len() as u128
        } else {
            0
        };

        debug!(
            target: "payload_builder",
            trace_id,
            block_number,
            block_hash = ?block_hash,
            tx_count = transactions.len(),
            cumulative_gas_used,
            total_fees = %total_fees,
            prepare_duration_ms = prepare_duration.as_millis(),
            exec_duration_ms = exec_duration.as_millis(),
            trie_root_duration_ms = finalize_elapsed.as_millis(),
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
                    block_number,
                    block_hash,
                    tx_index: u64::try_from(index).unwrap_or(u64::MAX),
                    tx_hash: *tx.hash(),
                };
                blob_sidecars.push(bsc_blob_tx_sidecar);
            }
        }

        plain_block.body.sidecars = Some(blob_sidecars);
        let sealed_block = Arc::new(plain_block.seal_unchecked(block_hash));

        let payload = BscBuiltPayload {
            block: sealed_block.clone(),
            fees: total_fees,
            requests: Some(execution_result.requests.clone()),
            build_kind: BuildKind::NormalAttempt,
            exec_duration,
            trie_root_duration: finalize_elapsed,
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
            difflayer, // Pass the difflayer to payload, reth will store it
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
        let BscBuildArguments {
            mut cached_reads,
            config,
            cancel: _,
            trace_id,
            min_gas_tip: _,
            retry_cache,
        } = args;
        let PayloadConfig { parent_header, attributes } = config;

        // If triedb is active, fetch parent difflayers *before* creating the block builder.
        //
        // Important: the block builder is not `Send`, and this function may be spawned onto a tokio
        // JoinSet. Therefore we must not hold the builder across any `.await`.
        let parent_hash = parent_header.hash_slow();
        let triedb_parent_difflayers = if rust_eth_triedb::triedb_manager::is_triedb_active() {
            match crate::shared::get_engine_api_tx() {
                Some(engine_api_tx) => match request_difflayer(&engine_api_tx, parent_hash).await {
                    Ok(difflayers) => Some(difflayers),
                    Err(e) => {
                        warn!(
                            target: "payload_builder",
                            trace_id,
                            parent_hash = ?parent_hash,
                            error = %e,
                            "Failed to request parent difflayers for triedb prefetcher (empty payload), continuing without prefetcher"
                        );
                        None
                    }
                },
                None => {
                    warn!(
                        target: "payload_builder",
                        trace_id,
                        parent_hash = ?parent_hash,
                        "Engine api tx not found; aborting empty payload build (triedb active)"
                    );
                    return Err(Box::new(std::io::Error::other("engine api tx not found")));
                }
            }
        } else {
            None
        };

        let state_provider = self.client.state_by_block_hash(parent_header.hash_slow())?;
        let state = StateProviderDatabase::new(&state_provider);
        let mut db = State::builder()
            .with_database(cached_reads.as_db_mut(state))
            .with_bundle_update()
            .build();

        // Build triedb prefetcher before creating the block builder so it can be carried via the
        // custom next-block env ctx into the execution ctx and consumed in `finish()`.
        let triedb_prefetcher = triedb_parent_difflayers.clone().and_then(|difflayers| {
            let mut triedb = get_global_triedb();
            let path_db = triedb.get_mut_path_db_ref().clone();
            MinerTrieDbPrefetcher::new(parent_header.state_root(), path_db, Some(difflayers)).ok()
        });

        let mut builder = self
            .evm_config
            .builder_for_next_block(
                &mut db,
                &parent_header,
                BscNextBlockEnvAttributes {
                    inner: NextBlockEnvAttributes {
                        timestamp: attributes.timestamp(),
                        suggested_fee_recipient: attributes.suggested_fee_recipient(),
                        prev_randao: attributes.prev_randao(),
                        gas_limit: self.builder_config.gas_limit(parent_header.gas_limit),
                        parent_beacon_block_root: attributes.parent_beacon_block_root(),
                        withdrawals: Some(attributes.withdrawals().clone()),
                    },
                    parent_difflayers: triedb_parent_difflayers.clone(),
                    triedb_prefetcher: triedb_prefetcher.clone(),
                    miner_retry_cache: retry_cache.clone(),
                },
            )
            .map_err(PayloadBuilderError::other)?;

        // Wire miner triedb prefetcher via state hook (if enabled).
        //
        // NOTE: This must be set before `apply_pre_execution_changes()` so any state access/touches
        // performed during pre-execution are also prefetched.
        if let Some(prefetcher) = triedb_prefetcher.clone() {
            let pf = prefetcher.clone();
            builder
                .executor_mut()
                .set_state_hook(Some(Box::new(move |_, update: &RethEvmState| {
                    pf.on_state_update(update);
                })));
            debug!(
                target: "payload_builder",
                trace_id,
                parent_hash = ?parent_hash,
                "Started triedb prefetcher for miner empty payload build"
            );
        }

        // Total time spent executing pre-execution changes (no user txs for empty payloads).
        let exec_start = std::time::Instant::now();
        // Everything before `exec_start` is treated as "prepare" time for this empty payload attempt.
        let prepare_duration = exec_start.duration_since(build_start);
        builder.apply_pre_execution_changes().map_err(|err| {
            warn!(
                target: "payload_builder",
                trace_id,
                %err,
                "failed to apply pre-execution changes for empty payload"
            );
            PayloadBuilderError::Internal(err.into())
        })?;
        let exec_duration = exec_start.elapsed();

        // No user transactions - only system transactions will be added by finish()
        let total_fees = U256::ZERO;
        let cumulative_gas_used = 0;

        // Add system txs to payload and finalize
        let finalize_start = std::time::Instant::now();
        let out = builder.finish_with_difflayer(&state_provider)?;
        let BlockBuilderOutcome { execution_result, hashed_state, trie_updates, block } = out.inner;
        let difflayer = out.difflayer;
        let finalize_elapsed = finalize_start.elapsed();

        let sealed_block = Arc::new(block.sealed_block().clone());

        let finalize_duration = finalize_start.elapsed().as_secs_f64();
        miner_metrics().block_finalize_duration_seconds.record(finalize_duration);
        miner_metrics().blocks_produced_total.increment(1);

        let build_duration = build_start.elapsed();

        debug!(
            target: "payload_builder",
            trace_id,
            block_number = sealed_block.number(),
            block_hash = ?sealed_block.hash_slow(),
            tx_count = sealed_block.body().transactions.len(),
            cumulative_gas_used,
            total_fees = %total_fees,
            prepare_duration_ms = prepare_duration.as_millis(),
            exec_duration_ms = exec_duration.as_millis(),
            trie_root_duration_ms = finalize_elapsed.as_millis(),
            build_duration_ms = build_duration.as_millis(),
            "Empty block payload built successfully (no user transactions)"
        );

        let payload = BscBuiltPayload {
            block: sealed_block.clone(),
            fees: total_fees,
            requests: Some(execution_result.requests.clone()),
            build_kind: BuildKind::EmptyFallback,
            exec_duration,
            trie_root_duration: finalize_elapsed,
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
            difflayer, // Pass the difflayer to payload, reth will store it
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
    /// Mining context
    mining_ctx: MiningContext,
    /// The payload builder instance
    builder: Arc<BscPayloadBuilder<Pool, Client, EvmConfig>>,
    /// Timeout for payload building
    timeout: std::time::Duration,
    /// Expected end timestamp (milliseconds since UNIX epoch).
    ///
    /// Initialized in `new()` as: `now_ms + parlia.delay_for_ramanujan_fork(... )`.
    expected_end_timestamp_ms: u128,
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
    /// Candidate-ready timestamps keyed by block hash.
    candidate_ready_at: HashMap<alloy_primitives::B256, std::time::Instant>,
}

enum RetryDecision {
    Retry,
    Submit,
}

impl<Pool, Client, EvmConfig> BscPayloadJob<Pool, Client, EvmConfig>
where
    Client: StateProviderFactory
        + reth_provider::HeaderProvider<Header = alloy_consensus::Header>
        + reth_provider::BlockHashReader
        + Clone
        + 'static,
    EvmConfig: ConfigureEvm<NextBlockEnvCtx = BscNextBlockEnvAttributes> + 'static,
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
        let retry_cache = BscMinerRetryCache::default();
        {
            let mut cache = retry_cache.inner.lock().unwrap();
            cache.parent_header = Some((*mining_ctx.parent_header).clone());
            cache.parent_snapshot = Some(mining_ctx.parent_snapshot.clone());
        }
        let mut build_args = build_args;
        build_args.retry_cache = Some(retry_cache);

        let mining_delay = parlia.clone().delay_for_mining(
            &mining_ctx.parent_snapshot,
            mining_ctx.header.as_ref().unwrap(),
            DELAY_LEFT_OVER,
        );

        let now_ms = Self::unix_now_ms();
        let expected_end_delay_ms = parlia.delay_for_ramanujan_fork(
            &mining_ctx.parent_snapshot,
            mining_ctx.header.as_ref().unwrap(),
        );
        let expected_end_timestamp_ms = now_ms + expected_end_delay_ms as u128;

        let mut pool_listener = builder.pool.pending_transactions_listener();
        tokio::spawn(async move {
            while let Some(tx_hash) = pool_listener.recv().await {
                if tx_listener_tx.send(tx_hash).is_err() {
                    break;
                }
            }
        });

        let job = Self {
            mining_ctx,
            builder: Arc::new(builder),
            timeout: std::time::Duration::from_millis(mining_delay),
            expected_end_timestamp_ms,
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
            candidate_ready_at: HashMap::new(),
        };
        let handle = BscPayloadJobHandle { abort_tx };

        debug!(
            target: "bsc::miner::payload",
            trace_id,
            block_number = job.mining_ctx.parent_header.number() + 1,
            is_inturn = job.mining_ctx.is_inturn,
            timeout = ?job.timeout,
            expected_end_timestamp_ms = job.expected_end_timestamp_ms,
            "Succeed to new payload job"
        );
        (job, handle)
    }

    /// Runs the payload job asynchronously with timeout support
    pub async fn start(mut self) -> Result<(), Box<BscPayloadJobError>> {
        self.apply_offturn_backoff().await?;
        // Match upstream BSC worker timing: the out-of-turn backoff happens before the mining
        // budget starts counting down.
        self.job_start_time = std::time::Instant::now();

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

        let mut build_started_at = std::time::Instant::now();
        loop {
            let remaining_duration = self.remaining_build_budget();
            if remaining_duration.is_zero() {
                info!(
                    target: "bsc::miner::payload",
                    trace_id = self.trace_id,
                    block_number = self.build_args.config.parent_header.number() + 1,
                    is_inturn = self.mining_ctx.is_inturn,
                    job_elapsed_ms = self.job_start_time.elapsed().as_millis(),
                    timeout_ms = self.timeout.as_millis(),
                    "Outer loop: Job already timeout, returning best payload"
                );
                return self.try_return_best_payload().await;
            }

            tokio::select! {
                args = self.try_build_rx.recv() => {
                    match args {
                        Some(_) => {
                            self.retries += 1;
                            build_started_at = std::time::Instant::now();
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
                result = self.join_handle.join_next() => {
                    match result {
                        Some(Ok(Ok(payload))) => {
                            if self.is_aborted {
                                return Err(Box::new(BscPayloadJobError::JobAborted));
                            }
                            let elapsed = build_started_at.elapsed();
                            let payload_tx_count = payload.block().body().transaction_count();
                            debug!(
                                target: "bsc::miner::payload",
                                trace_id = self.trace_id,
                                block_number = payload.block().header().number(),
                                block_hash = %payload.block().hash(),
                                is_inturn = self.mining_ctx.is_inturn,
                                build_kind = ?payload.build_kind,
                                tx_count = payload_tx_count,
                                fees = %payload.fees(),
                                cost_time = ?elapsed,
                                retries = self.retries,
                                "Succeed to try new build"
                            );
                            self.record_candidate(payload);

                            match self.wait_for_retry_or_submit(payload_tx_count, elapsed).await? {
                                RetryDecision::Retry => {
                                    if !self.schedule_retry_build() {
                                        return self.try_return_best_payload().await;
                                    }
                                }
                                RetryDecision::Submit => {
                                    return self.try_return_best_payload().await;
                                }
                            }
                        },
                        Some(Ok(Err(e))) => {
                            let elapsed = build_started_at.elapsed();
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
                            return self.try_return_best_payload().await;
                        },
                        Some(Err(join_err)) => {
                            let elapsed = build_started_at.elapsed();
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
                            return self.try_return_best_payload().await;
                        },
                        None => {}
                    }
                }
                _ = tokio::time::sleep(remaining_duration) => {
                    let elapsed = build_started_at.elapsed();
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
                    return self.try_return_best_payload().await;
                }
                _ = &mut self.abort_rx => {
                    let elapsed = build_started_at.elapsed();
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

    async fn apply_offturn_backoff(&mut self) -> Result<(), Box<BscPayloadJobError>> {
        if self.mining_ctx.is_inturn {
            return Ok(());
        }

        let before_sealing_ms = self.expected_end_timestamp_ms.saturating_sub(Self::unix_now_ms());
        let block_interval_ms = u128::from(self.mining_ctx.parent_snapshot.block_interval);
        if before_sealing_ms <= block_interval_ms {
            return Ok(());
        }

        let wait_ms = before_sealing_ms - block_interval_ms;
        let wait_duration = std::time::Duration::from_millis(u64::try_from(wait_ms).unwrap_or(u64::MAX));
        debug!(
            target: "bsc::miner::payload",
            trace_id = self.trace_id,
            block_number = self.build_args.config.parent_header.number() + 1,
            wait_ms,
            "Applying off-turn backoff before first payload build"
        );

        tokio::select! {
            _ = tokio::time::sleep(wait_duration) => Ok(()),
            _ = &mut self.abort_rx => {
                self.build_args.cancel.clone().cancel();
                self.is_aborted = true;
                Err(Box::new(BscPayloadJobError::JobAborted))
            }
        }
    }

    fn unix_now_ms() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    }

    fn remaining_build_budget(&self) -> std::time::Duration {
        self.timeout.saturating_sub(self.job_start_time.elapsed())
    }

    fn record_candidate(&mut self, payload: BscBuiltPayload) {
        self.candidate_ready_at.insert(payload.block().hash(), std::time::Instant::now());
        self.potential_payloads.push(payload);
    }

    fn schedule_retry_build(&mut self) -> bool {
        miner_metrics().payload_retries_total.increment(1);
        match self.try_build_tx.send(()) {
            Ok(()) => true,
            Err(err) => {
                warn!(
                    target: "bsc::miner::payload",
                    trace_id = self.trace_id,
                    block_number = self.build_args.config.parent_header.number() + 1,
                    is_inturn = self.mining_ctx.is_inturn,
                    retries = self.retries,
                    error = ?err,
                    "Failed to send to try build queue"
                );
                false
            }
        }
    }

    async fn wait_for_retry_or_submit(
        &mut self,
        payload_tx_count: usize,
        build_elapsed: std::time::Duration,
    ) -> Result<RetryDecision, Box<BscPayloadJobError>> {
        let wait_started = std::time::Instant::now();
        let mut new_tx_count = 0usize;

        loop {
            let remaining = self.remaining_build_budget();
            if remaining.is_zero() {
                miner_metrics().payload_retry_wait_seconds.record(wait_started.elapsed().as_secs_f64());
                return Ok(RetryDecision::Submit);
            }
            if remaining <= build_elapsed {
                miner_metrics().payload_rebuild_skipped_total.increment(1);
                miner_metrics().payload_retry_wait_seconds.record(wait_started.elapsed().as_secs_f64());
                return Ok(RetryDecision::Submit);
            }

            let retry_cutoff = build_elapsed.checked_mul(2).unwrap_or(std::time::Duration::MAX);
            let collect_wait = if new_tx_count > 0 && remaining > retry_cutoff {
                Some(remaining - retry_cutoff)
            } else {
                None
            };

            match collect_wait {
                Some(wait_for_more) => {
                    tokio::select! {
                        _ = tokio::time::sleep(wait_for_more) => {
                            miner_metrics().payload_retry_wait_seconds.record(wait_started.elapsed().as_secs_f64());
                            return Ok(RetryDecision::Retry);
                        }
                        _ = &mut self.abort_rx => {
                            self.build_args.cancel.clone().cancel();
                            self.is_aborted = true;
                            return Err(Box::new(BscPayloadJobError::JobAborted));
                        }
                        msg = self.tx_listener.recv() => {
                            if msg.is_none() {
                                miner_metrics().payload_retry_wait_seconds.record(wait_started.elapsed().as_secs_f64());
                                return Ok(RetryDecision::Submit);
                            }
                            new_tx_count += 1;
                        }
                    }
                }
                None => {
                    tokio::select! {
                        _ = tokio::time::sleep(remaining) => {
                            miner_metrics().payload_retry_wait_seconds.record(wait_started.elapsed().as_secs_f64());
                            return Ok(RetryDecision::Submit);
                        }
                        _ = &mut self.abort_rx => {
                            self.build_args.cancel.clone().cancel();
                            self.is_aborted = true;
                            return Err(Box::new(BscPayloadJobError::JobAborted));
                        }
                        msg = self.tx_listener.recv() => {
                            if msg.is_none() {
                                miner_metrics().payload_retry_wait_seconds.record(wait_started.elapsed().as_secs_f64());
                                return Ok(RetryDecision::Submit);
                            }
                            new_tx_count += 1;
                        }
                    }
                }
            }

            let remaining = self.remaining_build_budget();
            if remaining <= build_elapsed {
                miner_metrics().payload_rebuild_skipped_total.increment(1);
                miner_metrics().payload_retry_wait_seconds.record(wait_started.elapsed().as_secs_f64());
                return Ok(RetryDecision::Submit);
            }
            let retry_cutoff = build_elapsed.checked_mul(2).unwrap_or(std::time::Duration::MAX);
            if remaining <= retry_cutoff || payload_tx_count == 0 || new_tx_count >= payload_tx_count {
                miner_metrics().payload_retry_wait_seconds.record(wait_started.elapsed().as_secs_f64());
                return Ok(RetryDecision::Retry);
            }
        }
    }

    fn drain_ready_candidates(&mut self) {
        while let Some(result) = self.join_handle.try_join_next() {
            match result {
                Ok(Ok(payload)) => {
                    let tx_count = payload.block().body().transaction_count();
                    debug!(
                        target: "bsc::miner::payload",
                        trace_id = self.trace_id,
                        block_number = payload.block().header().number(),
                        block_hash = %payload.block().hash(),
                        is_inturn = self.mining_ctx.is_inturn,
                        build_kind = ?payload.build_kind,
                        tx_count,
                        fees = %payload.fees(),
                        "Drained additional ready payload candidate while returning best payload"
                    );
                    self.record_candidate(payload);
                }
                Ok(Err(err)) => {
                    debug!(
                        target: "bsc::miner::payload",
                        trace_id = self.trace_id,
                        try_mine_block_number = self.build_args.config.parent_header.number() + 1,
                        is_inturn = self.mining_ctx.is_inturn,
                        error = %err,
                        "Candidate build task failed while draining ready payloads"
                    );
                }
                Err(err) => {
                    debug!(
                        target: "bsc::miner::payload",
                        trace_id = self.trace_id,
                        try_mine_block_number = self.build_args.config.parent_header.number() + 1,
                        is_inturn = self.mining_ctx.is_inturn,
                        error = %err,
                        "Join failed while draining ready payloads"
                    );
                }
            }
        }
    }

    /// Try to return the best payload to result channel
    async fn try_return_best_payload(&mut self) -> Result<(), Box<BscPayloadJobError>> {
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
            self.record_candidate(bid.bsc_payload);
        }

        if self.potential_payloads.is_empty() {
            let builder = self.builder.clone();
            let args = self.build_args.clone();
            self.join_handle.spawn(async move { builder.build_empty_payload(args).await });

            let wait_started = std::time::Instant::now();
            let outcome = tokio::select! {
                _ = &mut self.abort_rx => None,
                res = self.join_handle.join_next() => Some(res),
            };
            miner_metrics().payload_first_candidate_wait_seconds.record(wait_started.elapsed().as_secs_f64());

            match outcome {
                None => {
                    info!(
                        target: "bsc::miner::payload",
                        trace_id = self.trace_id,
                        block_number = self.build_args.config.parent_header.number() + 1,
                        is_inturn = self.mining_ctx.is_inturn,
                        waited_ms = wait_started.elapsed().as_millis(),
                        "Abort while waiting for first payload candidate"
                    );
                    self.build_args.cancel.clone().cancel();
                    self.is_aborted = true;
                    return Err(Box::new(BscPayloadJobError::JobAborted));
                }
                Some(Some(Ok(Ok(payload)))) => {
                    let tx_count = payload.block().body().transaction_count();
                    debug!(
                        target: "bsc::miner::payload",
                        trace_id = self.trace_id,
                        block_number = payload.block().header().number(),
                        block_hash = %payload.block().hash(),
                        is_inturn = self.mining_ctx.is_inturn,
                        build_kind = ?payload.build_kind,
                        tx_count,
                        is_empty_block = tx_count == 0,
                        fees = %payload.fees(),
                        waited_ms = wait_started.elapsed().as_millis(),
                        "Received first payload candidate while returning best payload"
                    );
                    self.record_candidate(payload);
                }
                Some(Some(Ok(Err(err)))) => {
                    debug!(
                        target: "bsc::miner::payload",
                        trace_id = self.trace_id,
                        try_mine_block_number = self.build_args.config.parent_header.number() + 1,
                        is_inturn = self.mining_ctx.is_inturn,
                        waited_ms = wait_started.elapsed().as_millis(),
                        error = %err,
                        "Candidate build task failed while waiting for first payload candidate"
                    );
                }
                Some(Some(Err(err))) => {
                    debug!(
                        target: "bsc::miner::payload",
                        trace_id = self.trace_id,
                        try_mine_block_number = self.build_args.config.parent_header.number() + 1,
                        is_inturn = self.mining_ctx.is_inturn,
                        waited_ms = wait_started.elapsed().as_millis(),
                        error = %err,
                        "Join failed while waiting for first payload candidate"
                    );
                }
                Some(None) => {
                    debug!(
                        target: "bsc::miner::payload",
                        trace_id = self.trace_id,
                        try_mine_block_number = self.build_args.config.parent_header.number() + 1,
                        is_inturn = self.mining_ctx.is_inturn,
                        waited_ms = wait_started.elapsed().as_millis(),
                        "No background tasks available while waiting for first payload candidate"
                    );
                }
            }
        }

        self.drain_ready_candidates();

        if let Some(best_payload) = self.pick_best_payload() {
            let best_payload_hash = best_payload.block.hash();
            if let Some(ready_at) = self.candidate_ready_at.remove(&best_payload_hash) {
                miner_metrics().payload_ready_to_submit_seconds.record(ready_at.elapsed().as_secs_f64());
            }
            self.candidate_ready_at.clear();

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

            if let Some(bid_hash) = bid_block_hash {
                if best_payload_hash == bid_hash {
                    use crate::metrics::BscMevMetrics;
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
            let total_job_duration = self.job_start_time.elapsed();
            miner_metrics().no_best_payload_total.increment(1);
            self.candidate_ready_at.clear();

            if self.mining_ctx.is_inturn {
                warn!(
                    target: "bsc::miner::payload",
                    trace_id = self.trace_id,
                    try_mine_block_number = self.build_args.config.parent_header.number() + 1,
                    is_inturn = self.mining_ctx.is_inturn,
                    total_job_duration_ms = total_job_duration.as_millis(),
                    "No best payload available (inturn)"
                );
            } else {
                info!(
                    target: "bsc::miner::payload",
                    trace_id = self.trace_id,
                    try_mine_block_number = self.build_args.config.parent_header.number() + 1,
                    is_inturn = self.mining_ctx.is_inturn,
                    total_job_duration_ms = total_job_duration.as_millis(),
                    "No best payload available to send (off-turn)"
                );
            }

            Err(Box::new(BscPayloadJobError::NoPayloadsAvailable))
        }
    }

    /// Pick the best payload from potential payloads
    fn pick_best_payload(&mut self) -> Option<BscBuiltPayload> {
        if self.potential_payloads.is_empty() {
            return None;
        }

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
            exec_duration_ms = best_payload.exec_duration.as_millis(),
            trie_root_duration_ms = best_payload.trie_root_duration.as_millis(),
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
