use crate::chainspec::BscChainSpec;
use crate::consensus::eip4844::{calc_blob_fee, is_blob_eligible_block, BLOB_TX_BLOB_GAS_PER_BLOB};
use crate::consensus::parlia::util::calculate_millisecond_timestamp;
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
use either::Either;
use reth::payload::EthPayloadBuilderAttributes;
use reth::transaction_pool::error::Eip4844PoolTransactionError;
use reth::transaction_pool::error::InvalidPoolTransactionError;
use reth::transaction_pool::BestTransactionsAttributes;
use reth::transaction_pool::{PoolTransaction, TransactionPool};
use reth_basic_payload_builder::PayloadConfig;
use reth_chainspec::EthChainSpec;
use reth_ethereum_payload_builder::EthereumBuilderConfig;
use reth_evm::block::{BlockExecutionError, BlockValidationError};
use reth_evm::execute::BlockBuilder;
use reth_evm::execute::BlockBuilderOutcome;
use reth_evm::{ConfigureEvm, NextBlockEnvAttributes};
use reth_execution_types::BlockExecutionOutput;
use reth_payload_primitives::PayloadBuilderAttributes;
use reth_payload_primitives::{BuiltPayload, BuiltPayloadExecutedBlock, PayloadBuilderError};
use reth_primitives::HeaderTy;
use reth_primitives::InvalidTransactionError;
use reth_primitives::TransactionSigned;
use reth_primitives_traits::{BlockBody, SignerRecoverable};
use reth_provider::StateProviderFactory;
use reth_revm::cached::CachedReads;
use reth_revm::cancelled::ManualCancel;
use reth_revm::{database::StateProviderDatabase, db::State};
use revm::context_interface::block::Block;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info, trace, warn};

/// Delay left over for mining calculation
pub const DELAY_LEFT_OVER: u64 = 50;

/// Minimum estimated fee uplift required for a normal rebuild, expressed in basis points.
const NORMAL_REBUILD_UPLIFT_BPS: u64 = 1_500;

/// Higher uplift threshold required for the single near-deadline rebuild.
const FINAL_SHOT_UPLIFT_BPS: u64 = 3_000;

/// Normal rebuild cooldown, expressed as a fraction of the last completed build duration.
const NORMAL_COOLDOWN_NUM: u32 = 1;
const NORMAL_COOLDOWN_DEN: u32 = 2;

/// Minimum time left required for the final-shot rebuild, expressed as a multiple of the last
/// completed build duration.
const FINAL_SHOT_TIME_NUM: u32 = 115;
const FINAL_SHOT_TIME_DEN: u32 = 100;

/// Final-shot rebuilds are only allowed in the near-deadline window.
const FINAL_SHOT_WINDOW_NUM: u32 = 2;
const FINAL_SHOT_WINDOW_DEN: u32 = 1;

/// Safety margin that must remain after a rebuild finishes.
const FINALIZE_MARGIN_MS: u64 = 40;

/// Synthetic comparison base for empty payloads so dust does not look infinitely valuable.
const EMPTY_PAYLOAD_COMPARISON_BASE_WEI: u128 = 50_000_000_000_000;

/// Cap the per-tx fee estimate so a single high-gas transaction does not dominate the uplift
/// accumulator.
const ESTIMATED_FEE_GAS_CAP: u64 = 210_000;

/// Global trace ID counter for payload building operations
static TRACE_ID_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Generate a unique trace ID for payload building
pub fn generate_trace_id() -> u64 {
    TRACE_ID_COUNTER.fetch_add(1, Ordering::Relaxed)
}

fn initial_out_of_turn_build_wait(
    parlia: &Parlia<BscChainSpec>,
    mining_ctx: &MiningContext,
) -> std::time::Duration {
    if mining_ctx.is_inturn {
        return std::time::Duration::ZERO;
    }

    let Some(header) = mining_ctx.header.as_ref() else {
        return std::time::Duration::ZERO;
    };

    let present_timestamp = parlia.present_millis_timestamp();
    let block_timestamp = calculate_millisecond_timestamp(header);
    let before_sealing = block_timestamp.saturating_sub(present_timestamp);
    let wait_ms = before_sealing.saturating_sub(mining_ctx.parent_snapshot.block_interval);

    std::time::Duration::from_millis(wait_ms)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LocalRebuildPolicyInput {
    current_payload_fees: U256,
    estimated_new_fees: U256,
    last_build_duration: std::time::Duration,
    since_last_build: std::time::Duration,
    remaining_duration: std::time::Duration,
    final_shot_used: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocalRebuildAction {
    ReturnBestPayload,
    RebuildNow { final_shot: bool },
    WaitForMoreValue,
    WaitForCooldown(std::time::Duration),
}

fn duration_mul_ratio(
    duration: std::time::Duration,
    numerator: u32,
    denominator: u32,
) -> std::time::Duration {
    let scaled_millis = duration.as_millis().saturating_mul(numerator as u128) / denominator as u128;
    std::time::Duration::from_millis(scaled_millis.min(u64::MAX as u128) as u64)
}

fn local_rebuild_comparison_base(current_payload_fees: U256) -> U256 {
    if current_payload_fees.is_zero() {
        U256::from(EMPTY_PAYLOAD_COMPARISON_BASE_WEI)
    } else {
        current_payload_fees
    }
}

fn estimated_uplift_meets_threshold(
    estimated_new_fees: U256,
    comparison_base: U256,
    threshold_bps: u64,
) -> bool {
    estimated_new_fees.saturating_mul(U256::from(10_000_u64))
        >= comparison_base.saturating_mul(U256::from(threshold_bps))
}

fn estimated_uplift_bps(current_payload_fees: U256, estimated_new_fees: U256) -> u64 {
    let comparison_base = local_rebuild_comparison_base(current_payload_fees);
    if comparison_base.is_zero() {
        return 0;
    }

    (estimated_new_fees.saturating_mul(U256::from(10_000_u64)) / comparison_base).to::<u64>()
}

fn miner_metrics() -> &'static crate::metrics::BscMinerMetrics {
    use once_cell::sync::Lazy;
    static MINER_METRICS: Lazy<crate::metrics::BscMinerMetrics> =
        Lazy::new(crate::metrics::BscMinerMetrics::default);
    &MINER_METRICS
}

fn local_rebuild_action(input: LocalRebuildPolicyInput) -> LocalRebuildAction {
    let finalize_margin = std::time::Duration::from_millis(FINALIZE_MARGIN_MS);
    if input.remaining_duration < input.last_build_duration.saturating_add(finalize_margin) {
        return LocalRebuildAction::ReturnBestPayload;
    }

    let comparison_base = local_rebuild_comparison_base(input.current_payload_fees);
    let normal_cooldown =
        duration_mul_ratio(input.last_build_duration, NORMAL_COOLDOWN_NUM, NORMAL_COOLDOWN_DEN);
    let final_shot_min_remaining =
        duration_mul_ratio(input.last_build_duration, FINAL_SHOT_TIME_NUM, FINAL_SHOT_TIME_DEN);
    let final_shot_max_remaining =
        duration_mul_ratio(input.last_build_duration, FINAL_SHOT_WINDOW_NUM, FINAL_SHOT_WINDOW_DEN);

    if !input.final_shot_used
        && input.remaining_duration >= final_shot_min_remaining
        && input.remaining_duration <= final_shot_max_remaining
        && estimated_uplift_meets_threshold(
            input.estimated_new_fees,
            comparison_base,
            FINAL_SHOT_UPLIFT_BPS,
        )
    {
        return LocalRebuildAction::RebuildNow { final_shot: true };
    }

    if input.since_last_build < normal_cooldown {
        return LocalRebuildAction::WaitForCooldown(
            normal_cooldown.saturating_sub(input.since_last_build),
        );
    }

    if estimated_uplift_meets_threshold(
        input.estimated_new_fees,
        comparison_base,
        NORMAL_REBUILD_UPLIFT_BPS,
    ) {
        return LocalRebuildAction::RebuildNow { final_shot: false };
    }

    LocalRebuildAction::WaitForMoreValue
}
fn validate_bsc_sidecar(
    sidecar: &alloy_eips::eip7594::BlobTransactionSidecarVariant,
) -> Result<(), Eip4844PoolTransactionError> {
    // BSC only accepts legacy (EIP-4844) sidecars.
    if sidecar.is_eip4844() {
        Ok(())
    } else {
        Err(Eip4844PoolTransactionError::UnexpectedEip7594SidecarBeforeOsaka)
    }
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
                    extra_data: self.builder_config.extra_data.clone(),
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
            builder.evm_mut().block().gas_limit().saturating_sub(system_txs_gas);

        let base_fee = builder.evm_mut().block().basefee();
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
        let blob_eligible =
            is_blob_eligible_block(&self.chain_spec, header.number, header.timestamp);
        let mut max_blob_count =
            blob_params.as_ref().map(|params| params.max_blob_count).unwrap_or_default();
        if !blob_eligible {
            max_blob_count = 0;
        }
        let mut best_tx_list = self.pool.best_transactions_with_attributes(
            BestTransactionsAttributes::new(base_fee, blob_fee.map(|fee| fee as u64)),
        );
        if !blob_eligible {
            best_tx_list.skip_blobs();
        }
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
                    &InvalidPoolTransactionError::other(BlacklistedAddressError()),
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
                    &InvalidPoolTransactionError::ExceedsGasLimit(
                        pool_tx.gas_limit(),
                        block_gas_limit,
                    ),
                );
                continue;
            }

            let tx = pool_tx.to_consensus();
            if tx.is_eip4844() && !blob_eligible {
                best_tx_list.skip_blobs();
                continue;
            }
            let tx_start = std::time::Instant::now();
            let mut blob_tx_sidecar: Option<
                Arc<alloy_eips::eip7594::BlobTransactionSidecarVariant>,
            > = None;
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
                        &InvalidPoolTransactionError::Eip4844(
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
                            &InvalidPoolTransactionError::Eip4844(
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

                    // BSC: Always accept legacy (EIP-4844) sidecars and reject EIP-7594 sidecars.
                    if let Err(err) = validate_bsc_sidecar(sidecar.as_ref()) {
                        Err(err)
                    } else {
                        Ok(sidecar)
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
                            .mark_invalid(&pool_tx, &InvalidPoolTransactionError::Eip4844(error));
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
                            &InvalidPoolTransactionError::Consensus(
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
                            &InvalidPoolTransactionError::Consensus(
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

        let requests = execution_result.requests.clone();
        let execution_outcome =
            BlockExecutionOutput { state: db.take_bundle(), result: execution_result };
        let executed: BuiltPayloadExecutedBlock<_> = BuiltPayloadExecutedBlock {
            recovered_block: Arc::new(block),
            execution_output: Arc::new(execution_outcome),
            hashed_state: Either::Left(Arc::new(hashed_state)),
            trie_updates: Either::Left(Arc::new(trie_updates)),
        };

        let payload = BscBuiltPayload {
            block: sealed_block.clone(),
            fees: total_fees,
            requests: Some(requests),
            executed_block: executed,
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
                    extra_data: self.builder_config.extra_data.clone(),
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

        let requests = execution_result.requests.clone();
        let execution_outcome =
            BlockExecutionOutput { state: db.take_bundle(), result: execution_result };
        let executed: BuiltPayloadExecutedBlock<_> = BuiltPayloadExecutedBlock {
            recovered_block: Arc::new(block),
            execution_output: Arc::new(execution_outcome),
            hashed_state: Either::Left(Arc::new(hashed_state)),
            trie_updates: Either::Left(Arc::new(trie_updates)),
        };

        let payload = BscBuiltPayload {
            block: sealed_block.clone(),
            fees: total_fees,
            requests: Some(requests),
            executed_block: executed,
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
    /// Pending block base fee used for cheap tx uplift estimates
    pending_basefee: u64,
    /// Duration of the last completed local build
    last_local_build_duration: Option<std::time::Duration>,
    /// Completion time of the last completed local build
    last_local_build_finished_at: Option<std::time::Instant>,
    /// Fees of the latest local payload snapshot used as the rebuild comparison baseline
    current_local_payload_fees: U256,
    /// Estimated fees from txs that arrived since the last completed local build
    estimated_new_local_fees: U256,
    /// Whether the job has already used its single near-deadline rebuild
    final_shot_used: bool,
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
        let pending_basefee = builder.pool.block_info().pending_basefee;

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
            pending_basefee,
            last_local_build_duration: None,
            last_local_build_finished_at: None,
            current_local_payload_fees: U256::ZERO,
            estimated_new_local_fees: U256::ZERO,
            final_shot_used: false,
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
        let initial_wait = initial_out_of_turn_build_wait(&self.parlia, &self.mining_ctx);
        if !initial_wait.is_zero() {
            debug!(
                target: "bsc::miner::payload",
                trace_id = self.trace_id,
                block_number = self.build_args.config.parent_header.number() + 1,
                wait_ms = initial_wait.as_millis(),
                "Applying out-of-turn backoff before starting payload build"
            );
            tokio::select! {
                _ = tokio::time::sleep(initial_wait) => {}
                _ = &mut self.abort_rx => {
                    self.build_args.cancel.clone().cancel();
                    self.is_aborted = true;
                    return Err(Box::new(BscPayloadJobError::JobAborted));
                }
            }
        }

        // The job timeout is the budget for payload building attempts. When we intentionally
        // back off out-of-turn to match go-bsc behavior, start accounting that budget only
        // after the wait completes.
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
                            self.record_local_build(&payload, elapsed);
                            self.potential_payloads.push(payload);
                            let mut wait_for_more_txs = None;
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

                                    _ = async {
                                        let wait_duration =
                                            wait_for_more_txs.expect("guarded by wait_for_more_txs.is_some()");
                                        tokio::time::sleep(wait_duration).await;
                                    }, if wait_for_more_txs.is_some() => {
                                        wait_for_more_txs = None;

                                        let fresh_job_elapsed = self.job_start_time.elapsed();
                                        let fresh_remaining_duration = if fresh_job_elapsed < self.timeout {
                                            self.timeout - fresh_job_elapsed
                                        } else {
                                            std::time::Duration::ZERO
                                        };

                                        match self.evaluate_local_rebuild_action(fresh_remaining_duration) {
                                            Some(action) => {
                                                self.record_local_rebuild_decision_metrics(action);
                                                match action {
                                                    LocalRebuildAction::RebuildNow { final_shot } => {
                                                        if final_shot {
                                                            self.final_shot_used = true;
                                                        }
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
                                                            estimated_new_local_fees = %self.estimated_new_local_fees,
                                                            current_local_payload_fees = %self.current_local_payload_fees,
                                                            remaining_duration_ms = fresh_remaining_duration.as_millis(),
                                                            last_cost_time = ?elapsed,
                                                            final_shot,
                                                            "Queued another payload build after local uplift re-evaluation"
                                                        );
                                                        break;
                                                    }
                                                    LocalRebuildAction::ReturnBestPayload => {
                                                        debug!(
                                                            target: "bsc::miner::payload",
                                                            trace_id = self.trace_id,
                                                            block_number = self.build_args.config.parent_header.number() + 1,
                                                            is_inturn = self.mining_ctx.is_inturn,
                                                            retries = self.retries,
                                                            estimated_new_local_fees = %self.estimated_new_local_fees,
                                                            current_local_payload_fees = %self.current_local_payload_fees,
                                                            remaining_duration_ms = fresh_remaining_duration.as_millis(),
                                                            last_cost_time = ?elapsed,
                                                            "Returning best payload because there is not enough time left for another value-gated rebuild"
                                                        );
                                                        return self.try_return_best_payload();
                                                    }
                                                    LocalRebuildAction::WaitForCooldown(wait_duration) => {
                                                        wait_for_more_txs = Some(wait_duration);
                                                    }
                                                    LocalRebuildAction::WaitForMoreValue => {}
                                                }
                                            }
                                            None => {}
                                        }
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

                                    Some(tx_hash) = self.tx_listener.recv() => {
                                        self.estimated_new_local_fees = self
                                            .estimated_new_local_fees
                                            .saturating_add(self.estimate_pending_tx_fee_uplift(&tx_hash));
                                        while let Ok(tx_hash) = self.tx_listener.try_recv() {
                                            self.estimated_new_local_fees = self
                                                .estimated_new_local_fees
                                                .saturating_add(self.estimate_pending_tx_fee_uplift(&tx_hash));
                                        }

                                        let fresh_job_elapsed = self.job_start_time.elapsed();
                                        let fresh_remaining_duration = if fresh_job_elapsed < self.timeout {
                                            self.timeout - fresh_job_elapsed
                                        } else {
                                            std::time::Duration::ZERO
                                        };

                                        match self.evaluate_local_rebuild_action(fresh_remaining_duration) {
                                            Some(action) => {
                                                self.record_local_rebuild_decision_metrics(action);
                                                match action {
                                                    LocalRebuildAction::RebuildNow { final_shot } => {
                                                        if final_shot {
                                                            self.final_shot_used = true;
                                                        }
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
                                                            estimated_new_local_fees = %self.estimated_new_local_fees,
                                                            current_local_payload_fees = %self.current_local_payload_fees,
                                                            remaining_duration_ms = fresh_remaining_duration.as_millis(),
                                                            last_cost_time = ?elapsed,
                                                            final_shot,
                                                            "Queued another payload build after batching local fee uplift"
                                                        );
                                                        break;
                                                    }
                                                    LocalRebuildAction::ReturnBestPayload => {
                                                        debug!(
                                                            target: "bsc::miner::payload",
                                                            trace_id = self.trace_id,
                                                            block_number = self.build_args.config.parent_header.number() + 1,
                                                            is_inturn = self.mining_ctx.is_inturn,
                                                            retries = self.retries,
                                                            estimated_new_local_fees = %self.estimated_new_local_fees,
                                                            current_local_payload_fees = %self.current_local_payload_fees,
                                                            remaining_duration_ms = fresh_remaining_duration.as_millis(),
                                                            last_cost_time = ?elapsed,
                                                            "Returning best payload because there is not enough time left for another value-gated rebuild"
                                                        );
                                                        return self.try_return_best_payload();
                                                    }
                                                    LocalRebuildAction::WaitForCooldown(wait_duration) => {
                                                        wait_for_more_txs = Some(wait_duration);
                                                    }
                                                    LocalRebuildAction::WaitForMoreValue => {
                                                        wait_for_more_txs = None;
                                                    }
                                                }
                                            }
                                            None => {
                                                wait_for_more_txs = None;
                                            }
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

    fn record_local_build(
        &mut self,
        payload: &BscBuiltPayload,
        build_duration: std::time::Duration,
    ) {
        self.last_local_build_duration = Some(build_duration);
        self.last_local_build_finished_at = Some(std::time::Instant::now());
        self.current_local_payload_fees = payload.fees();
        self.estimated_new_local_fees = U256::ZERO;
    }

    fn estimate_pending_tx_fee_uplift(&self, tx_hash: &alloy_primitives::B256) -> U256 {
        let Some(pool_tx) = self.builder.pool.get(tx_hash) else {
            return U256::ZERO;
        };

        let effective_tip = pool_tx.effective_tip_per_gas(self.pending_basefee).unwrap_or_default();
        if effective_tip < self.build_args.min_gas_tip {
            return U256::ZERO;
        }

        U256::from(effective_tip)
            .saturating_mul(U256::from(pool_tx.gas_limit().min(ESTIMATED_FEE_GAS_CAP)))
    }

    fn evaluate_local_rebuild_action(
        &self,
        remaining_duration: std::time::Duration,
    ) -> Option<LocalRebuildAction> {
        let last_build_duration = self.last_local_build_duration?;
        let last_build_finished_at = self.last_local_build_finished_at?;

        Some(local_rebuild_action(LocalRebuildPolicyInput {
            current_payload_fees: self.current_local_payload_fees,
            estimated_new_fees: self.estimated_new_local_fees,
            last_build_duration,
            since_last_build: last_build_finished_at.elapsed(),
            remaining_duration,
            final_shot_used: self.final_shot_used,
        }))
    }

    fn record_local_rebuild_decision_metrics(&self, action: LocalRebuildAction) {
        let metrics = miner_metrics();
        metrics.payload_rebuild_estimated_uplift_bps.set(
            estimated_uplift_bps(
                self.current_local_payload_fees,
                self.estimated_new_local_fees,
            ) as f64,
        );

        match action {
            LocalRebuildAction::RebuildNow { final_shot } => {
                metrics.payload_rebuilds_attempted_total.increment(1);
                if final_shot {
                    metrics.payload_rebuilds_final_shot_total.increment(1);
                }
            }
            LocalRebuildAction::WaitForCooldown(_) => {
                metrics.payload_rebuilds_skipped_cooldown_total.increment(1);
            }
            LocalRebuildAction::WaitForMoreValue => {
                metrics.payload_rebuilds_skipped_value_total.increment(1);
            }
            LocalRebuildAction::ReturnBestPayload => {
                metrics.payload_rebuilds_skipped_time_total.increment(1);
            }
        }
    }

    /// Try to return the best payload to result channel
    fn try_return_best_payload(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut bid_block_hash = None;
        let best_bid = self.simulator.get_best_bid(self.mining_ctx.parent_header.hash());
        if let Some(bid) = best_bid {
            let bid_info = bid.bid;
            if let Some(bsc_payload) = bid.bsc_payload {
                info!(
                    target: "bsc::miner::payload",
                    trace_id = self.trace_id,
                    block_number = bid_info.block_number,
                    is_inturn = self.mining_ctx.is_inturn,
                    builder = ?bid_info.builder,
                    gas_fee = %bid_info.gas_fee,
                    bid_hash = %bid_info.bid_hash,
                    gas_fee = %bsc_payload.fees(),
                    "Found best bid"
                );
                bid_block_hash = Some(bsc_payload.block.hash());
                self.potential_payloads.push(bsc_payload);
            } else {
                warn!(
                    target: "bsc::miner::payload",
                    trace_id = self.trace_id,
                    block_number = bid_info.block_number,
                    builder = ?bid_info.builder,
                    bid_hash = %bid_info.bid_hash,
                    "Best bid missing built payload"
                );
            }
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

#[cfg(test)]
mod tests {
    use super::{
        initial_out_of_turn_build_wait, local_rebuild_action, validate_bsc_sidecar,
        LocalRebuildAction, LocalRebuildPolicyInput,
    };
    use crate::chainspec::BscChainSpec;
    use crate::consensus::parlia::Parlia;
    use crate::consensus::parlia::Snapshot;
    use crate::node::miner::bsc_miner::MiningContext;
    use alloy_consensus::BlobTransactionSidecar;
    use alloy_consensus::Header;
    use alloy_eips::eip4844::{Blob, Bytes48};
    use alloy_eips::eip7594::{
        BlobTransactionSidecarEip7594, BlobTransactionSidecarVariant, CELLS_PER_EXT_BLOB,
    };
    use alloy_primitives::{Address, B256, U256};
    use reth::transaction_pool::error::Eip4844PoolTransactionError;
    use reth_primitives::SealedHeader;
    use std::sync::Arc;
    use std::time::Duration;

    fn test_parlia() -> Parlia<BscChainSpec> {
        let chain_spec = Arc::new(BscChainSpec { inner: crate::chainspec::bsc::bsc_mainnet() });
        Parlia::new(chain_spec, 200)
    }

    fn test_mining_context(
        parlia: &Parlia<BscChainSpec>,
        block_interval: u64,
        delay_ms: u64,
        is_inturn: bool,
    ) -> MiningContext {
        let now_ms = parlia.present_millis_timestamp();
        let parent_ts_ms = now_ms.saturating_sub(block_interval);
        let parent_header = Header {
            number: 1,
            timestamp: parent_ts_ms / 1000,
            mix_hash: B256::ZERO,
            ..Default::default()
        };
        let mut header = Header {
            number: 2,
            parent_hash: parent_header.hash_slow(),
            beneficiary: Address::with_last_byte(1),
            timestamp: (now_ms + delay_ms) / 1000,
            ..Default::default()
        };
        crate::consensus::parlia::util::set_millisecond_part_of_timestamp(
            now_ms + delay_ms,
            &mut header,
        );

        let mut snapshot = Snapshot::new(
            vec![Address::with_last_byte(1)],
            1,
            parent_header.hash_slow(),
            200,
            None,
        );
        snapshot.block_interval = block_interval;

        MiningContext {
            header: Some(header),
            parent_header: SealedHeader::new(parent_header.clone(), parent_header.hash_slow()),
            parent_snapshot: Arc::new(snapshot),
            is_inturn,
            cached_reads: None,
        }
    }

    fn simulate_value_gated_rebuilds_after_first_build(
        current_payload_fees: U256,
        tx_arrivals: &[(u64, U256)],
        build_duration: Duration,
        timeout: Duration,
    ) -> usize {
        let mut rebuilds = 0;
        let mut estimated_new_fees = U256::ZERO;
        let mut wait_deadline_ms: Option<u64> = None;
        let final_shot_used = false;

        for &(arrival_ms, estimated_fees) in tx_arrivals {
            loop {
                let Some(deadline_ms) = wait_deadline_ms else {
                    break;
                };
                if deadline_ms > arrival_ms {
                    break;
                }

                match local_rebuild_action(LocalRebuildPolicyInput {
                    current_payload_fees,
                    estimated_new_fees,
                    last_build_duration: build_duration,
                    since_last_build: Duration::from_millis(deadline_ms),
                    remaining_duration: timeout.saturating_sub(Duration::from_millis(deadline_ms)),
                    final_shot_used,
                }) {
                    LocalRebuildAction::RebuildNow { final_shot: _ } => {
                        rebuilds += 1;
                        return rebuilds;
                    }
                    LocalRebuildAction::ReturnBestPayload | LocalRebuildAction::WaitForMoreValue => {
                        break;
                    }
                    LocalRebuildAction::WaitForCooldown(wait_duration) => {
                        wait_deadline_ms =
                            Some(deadline_ms + wait_duration.as_millis() as u64);
                    }
                }
            }

            estimated_new_fees = estimated_new_fees.saturating_add(estimated_fees);
            match local_rebuild_action(LocalRebuildPolicyInput {
                current_payload_fees,
                estimated_new_fees,
                last_build_duration: build_duration,
                since_last_build: Duration::from_millis(arrival_ms),
                remaining_duration: timeout.saturating_sub(Duration::from_millis(arrival_ms)),
                final_shot_used,
            }) {
                LocalRebuildAction::RebuildNow { final_shot: _ } => {
                    rebuilds += 1;
                    return rebuilds;
                }
                LocalRebuildAction::ReturnBestPayload | LocalRebuildAction::WaitForMoreValue => {
                    wait_deadline_ms = None;
                }
                LocalRebuildAction::WaitForCooldown(wait_duration) => {
                    wait_deadline_ms = Some(arrival_ms + wait_duration.as_millis() as u64);
                }
            }
        }

        while let Some(deadline_ms) = wait_deadline_ms {
            match local_rebuild_action(LocalRebuildPolicyInput {
                current_payload_fees,
                estimated_new_fees,
                last_build_duration: build_duration,
                since_last_build: Duration::from_millis(deadline_ms),
                remaining_duration: timeout.saturating_sub(Duration::from_millis(deadline_ms)),
                final_shot_used,
            }) {
                LocalRebuildAction::RebuildNow { final_shot: _ } => {
                    rebuilds += 1;
                    return rebuilds;
                }
                LocalRebuildAction::ReturnBestPayload | LocalRebuildAction::WaitForMoreValue => {
                    return rebuilds;
                }
                LocalRebuildAction::WaitForCooldown(wait_duration) => {
                    wait_deadline_ms = Some(deadline_ms + wait_duration.as_millis() as u64);
                }
            }
        }

        rebuilds
    }

    #[test]
    fn bsc_sidecar_accepts_eip4844() {
        let sidecar = BlobTransactionSidecar::default();
        let variant = BlobTransactionSidecarVariant::Eip4844(sidecar);
        assert!(validate_bsc_sidecar(&variant).is_ok());
    }

    #[test]
    fn bsc_sidecar_rejects_eip7594() {
        let blob = Blob::default();
        let commitment = Bytes48::default();
        let cell_proofs = vec![Bytes48::default(); CELLS_PER_EXT_BLOB];
        let sidecar = BlobTransactionSidecarEip7594::new(vec![blob], vec![commitment], cell_proofs);
        let variant = BlobTransactionSidecarVariant::Eip7594(sidecar);

        assert!(matches!(
            validate_bsc_sidecar(&variant),
            Err(Eip4844PoolTransactionError::UnexpectedEip7594SidecarBeforeOsaka)
        ));
    }

    #[test]
    fn out_of_turn_wait_matches_geth_style_backoff() {
        let parlia = test_parlia();
        let ctx = test_mining_context(&parlia, 450, 900, false);
        let wait = initial_out_of_turn_build_wait(&parlia, &ctx);
        assert!(wait >= Duration::from_millis(449));
        assert!(wait <= Duration::from_millis(450));

        let inturn_ctx = test_mining_context(&parlia, 450, 900, true);
        assert_eq!(initial_out_of_turn_build_wait(&parlia, &inturn_ctx), Duration::ZERO);
    }

    #[test]
    fn local_rebuild_policy_skips_when_uplift_is_below_threshold() {
        let action = local_rebuild_action(LocalRebuildPolicyInput {
            current_payload_fees: U256::from(1_000_000_u64),
            estimated_new_fees: U256::from(100_000_u64),
            last_build_duration: Duration::from_millis(100),
            since_last_build: Duration::from_millis(60),
            remaining_duration: Duration::from_millis(300),
            final_shot_used: false,
        });

        assert_eq!(action, LocalRebuildAction::WaitForMoreValue);
    }

    #[test]
    fn local_rebuild_policy_rebuilds_after_cooldown_when_uplift_is_high_enough() {
        let action = local_rebuild_action(LocalRebuildPolicyInput {
            current_payload_fees: U256::from(1_000_000_u64),
            estimated_new_fees: U256::from(200_000_u64),
            last_build_duration: Duration::from_millis(100),
            since_last_build: Duration::from_millis(60),
            remaining_duration: Duration::from_millis(300),
            final_shot_used: false,
        });

        assert_eq!(action, LocalRebuildAction::RebuildNow { final_shot: false });
    }

    #[test]
    fn local_rebuild_policy_returns_best_when_remaining_time_cannot_cover_rebuild() {
        let action = local_rebuild_action(LocalRebuildPolicyInput {
            current_payload_fees: U256::from(1_000_000_u64),
            estimated_new_fees: U256::from(500_000_u64),
            last_build_duration: Duration::from_millis(100),
            since_last_build: Duration::from_millis(80),
            remaining_duration: Duration::from_millis(139),
            final_shot_used: false,
        });

        assert_eq!(action, LocalRebuildAction::ReturnBestPayload);
    }

    #[test]
    fn local_rebuild_policy_allows_one_final_shot_in_near_deadline_window() {
        let action = local_rebuild_action(LocalRebuildPolicyInput {
            current_payload_fees: U256::from(1_000_000_u64),
            estimated_new_fees: U256::from(350_000_u64),
            last_build_duration: Duration::from_millis(100),
            since_last_build: Duration::from_millis(20),
            remaining_duration: Duration::from_millis(180),
            final_shot_used: false,
        });

        assert_eq!(action, LocalRebuildAction::RebuildNow { final_shot: true });
    }

    #[test]
    fn local_rebuild_policy_does_not_allow_second_final_shot() {
        let action = local_rebuild_action(LocalRebuildPolicyInput {
            current_payload_fees: U256::from(1_000_000_u64),
            estimated_new_fees: U256::from(350_000_u64),
            last_build_duration: Duration::from_millis(100),
            since_last_build: Duration::from_millis(20),
            remaining_duration: Duration::from_millis(180),
            final_shot_used: true,
        });

        assert_eq!(action, LocalRebuildAction::WaitForCooldown(Duration::from_millis(30)));
    }

    #[test]
    fn local_rebuild_policy_uses_synthetic_baseline_for_empty_payloads() {
        let action = local_rebuild_action(LocalRebuildPolicyInput {
            current_payload_fees: U256::ZERO,
            estimated_new_fees: U256::from(1_000_000_000_000_u64),
            last_build_duration: Duration::from_millis(100),
            since_last_build: Duration::from_millis(60),
            remaining_duration: Duration::from_millis(300),
            final_shot_used: false,
        });

        assert_eq!(action, LocalRebuildAction::WaitForMoreValue);
    }

    #[test]
    fn trickle_load_with_low_estimated_uplift_does_not_rebuild() {
        let arrivals: Vec<(u64, U256)> =
            (10..=200).step_by(10).map(|ms| (ms, U256::from(5_000_u64))).collect();
        let rebuilds = simulate_value_gated_rebuilds_after_first_build(
            U256::from(1_000_000_u64),
            &arrivals,
            Duration::from_millis(100),
            Duration::from_millis(300),
        );

        assert_eq!(rebuilds, 0);
    }

    #[test]
    fn meaningful_uplift_after_cooldown_triggers_exactly_one_rebuild() {
        let arrivals = vec![(60, U256::from(200_000_u64))];
        let rebuilds = simulate_value_gated_rebuilds_after_first_build(
            U256::from(1_000_000_u64),
            &arrivals,
            Duration::from_millis(100),
            Duration::from_millis(300),
        );

        assert_eq!(rebuilds, 1);
    }

    #[test]
    fn cooldown_timer_can_trigger_rebuild_without_another_tx_arrival() {
        let arrivals = vec![
            (10, U256::from(50_000_u64)),
            (20, U256::from(50_000_u64)),
            (30, U256::from(50_000_u64)),
        ];
        let rebuilds = simulate_value_gated_rebuilds_after_first_build(
            U256::from(1_000_000_u64),
            &arrivals,
            Duration::from_millis(100),
            Duration::from_millis(300),
        );

        assert_eq!(rebuilds, 1);
    }

    #[test]
    fn realistic_short_slot_with_slow_first_build_skips_second_rebuild() {
        let arrivals = vec![(20, U256::from(200_000_u64))];
        let rebuilds = simulate_value_gated_rebuilds_after_first_build(
            U256::from(1_000_000_u64),
            &arrivals,
            Duration::from_millis(331),
            Duration::from_millis(419),
        );

        assert_eq!(rebuilds, 0);
    }
}
