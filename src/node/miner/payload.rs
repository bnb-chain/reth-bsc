use crate::chainspec::BscChainSpec;
use crate::consensus::eip4844::{calc_blob_fee, is_blob_eligible_block, BLOB_TX_BLOB_GAS_PER_BLOB};
use crate::consensus::parlia::util::calculate_millisecond_timestamp;
use crate::consensus::parlia::{Parlia, Snapshot};
use crate::evm::blacklist;
use crate::hardforks::BscHardforks;
use crate::metrics::{BscConsensusMetrics, BscMinerMetrics};
use crate::node::engine::{BscBuiltPayload, BuildKind};
use crate::node::evm::config::{BscEvmConfig, BscNextBlockEnvAttributes, ValidatorCacheSink};
use crate::node::evm::pre_execution::{TURN_LENGTH_CACHE, VALIDATOR_CACHE};
use crate::node::miner::bid_block::{validate_bid_block_blob_kzg, BidBlockTask};
use crate::node::miner::bid_simulator::BidSimulator;
use crate::node::miner::block_mev_info::{set_block_mev_info, BlockMevInfoVersion};
use crate::node::miner::bsc_miner::{MiningContext, SubmitContext};
use crate::node::miner::util::finalize_new_header;
use crate::node::pool::BlacklistedAddressError;
use crate::node::primitives::{BscBlobTransactionSidecar, BscBlock};
use alloy_consensus::{BlockHeader, Transaction};
use alloy_eips::eip4895::Withdrawals;
use alloy_evm::Evm;
use alloy_primitives::U256;
use reth_node_ethereum::engine::EthPayloadAttributes;
use reth::transaction_pool::error::Eip4844PoolTransactionError;
use reth::transaction_pool::error::InvalidPoolTransactionError;
use reth::transaction_pool::BestTransactionsAttributes;
use reth::transaction_pool::{PoolTransaction, TransactionPool};
use reth_basic_payload_builder::PayloadConfig;
use reth_chainspec::EthChainSpec;
use reth_ethereum_payload_builder::EthereumBuilderConfig;
use reth_ethereum_primitives::TransactionSigned;
use reth_evm::block::{BlockExecutionError, BlockValidationError};
use reth_evm::execute::BlockBuilder;
use reth_evm::execute::BlockBuilderOutcome;
use reth_evm::{ConfigureEvm, NextBlockEnvAttributes};
use reth_execution_types::BlockExecutionOutput;
use reth_payload_primitives::{BuiltPayload, BuiltPayloadExecutedBlock, PayloadBuilderError};
use once_cell::sync::Lazy;
use revm::context_interface::Block as EvmBlock;
use reth_primitives_traits::{HeaderTy, SealedHeader};
use reth_primitives_traits::transaction::error::InvalidTransactionError;
use reth_primitives_traits::{Block, BlockBody, RecoveredBlock, SealedBlock, SignerRecoverable};
use reth_provider::StateProviderFactory;
use reth_revm::cached::CachedReads;
use reth_revm::cancelled::ManualCancel;
use reth_revm::{database::StateProviderDatabase, db::State};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info, trace, warn};

/// Milliseconds reserved at the end of each block period for state-root computation.
///
/// Geth-BSC uses 50ms because its native hashdb-based trie is faster and more predictable.
/// reth-bsc's MDBX-based root calculation has higher and less stable latency, so we reserve
/// 120ms to avoid missing the block deadline or eating into the next block's time budget.
pub const DELAY_LEFT_OVER: u64 = 120;

/// Adaptive end-of-slot reserve, tuned at runtime via env (no recompile needed for testing).
///
/// Empty blocks under load cluster at the *peaks* of the overlay depth (head − finalized): there
/// the background sparse-trie root can't finalize within the default 120ms reserve, so `finish`
/// waits past the slot deadline and the block degrades to empty-fallback. For the ~97% of blocks
/// at normal depth the root is ready in ~20ms, so the default reserve is left untouched. When the
/// overlay is deep we reserve more of the slot for the root (fill stops earlier → exec ends earlier
/// → the finalize tail gets a larger window, and fewer txs are filled → the finalize tail is
/// smaller), turning a would-be empty block into an on-time smaller block.
/// See docs/design-adaptive-overlay-depth.md.
///
/// Env knobs (read once at startup, cached):
/// - `BSC_MINING_ADAPTIVE_RESERVE` = on/off (default on); off → fixed `DELAY_LEFT_OVER` (A/B base).
/// - `BSC_MINING_ROOT_RESERVE_DEPTH_LOW`  (default 15) — at/below this, default reserve.
/// - `BSC_MINING_ROOT_RESERVE_DEPTH_HIGH` (default 40) — at/above this, max reserve.
/// - `BSC_MINING_ROOT_RESERVE_MAX_MS`     (default 280) — reserve used at/above DEPTH_HIGH.
#[derive(Debug, Clone, Copy)]
struct AdaptiveReserveConfig {
    enabled: bool,
    depth_low: u64,
    depth_high: u64,
    reserve_max_ms: u64,
}

/// Default knob values (also the fallback when the corresponding env var is unset/unparseable).
const ROOT_RESERVE_MAX_MS: u64 = 280;
const ROOT_RESERVE_DEPTH_LOW: u64 = 15;
const ROOT_RESERVE_DEPTH_HIGH: u64 = 40;

fn adaptive_reserve_config() -> &'static AdaptiveReserveConfig {
    static CFG: std::sync::OnceLock<AdaptiveReserveConfig> = std::sync::OnceLock::new();
    CFG.get_or_init(|| {
        let env_u64 = |k: &str, default: u64| {
            std::env::var(k).ok().and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(default)
        };
        let enabled = std::env::var("BSC_MINING_ADAPTIVE_RESERVE")
            .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "on" | "yes"))
            .unwrap_or(true);
        let cfg = AdaptiveReserveConfig {
            enabled,
            depth_low: env_u64("BSC_MINING_ROOT_RESERVE_DEPTH_LOW", ROOT_RESERVE_DEPTH_LOW),
            depth_high: env_u64("BSC_MINING_ROOT_RESERVE_DEPTH_HIGH", ROOT_RESERVE_DEPTH_HIGH),
            reserve_max_ms: env_u64("BSC_MINING_ROOT_RESERVE_MAX_MS", ROOT_RESERVE_MAX_MS),
        };
        tracing::info!(target: "bsc::miner", ?cfg, "Adaptive root-reserve config");
        cfg
    })
}

/// Effective end-of-slot reserve (ms) given the current in-memory overlay depth.
///
/// Linearly interpolates `DELAY_LEFT_OVER..=reserve_max_ms` between `depth_low` and `depth_high`.
/// At/below `depth_low` (or when disabled) returns the unchanged default, so normal blocks are
/// unaffected. The branch ordering also makes a misconfigured `depth_high <= depth_low` behave as a
/// step at `depth_low` (no divide-by-zero).
pub fn effective_delay_left_over(overlay_depth: u64) -> u64 {
    let cfg = adaptive_reserve_config();
    if !cfg.enabled || overlay_depth <= cfg.depth_low {
        DELAY_LEFT_OVER
    } else if overlay_depth >= cfg.depth_high {
        cfg.reserve_max_ms
    } else {
        let span = (cfg.depth_high - cfg.depth_low) as f64;
        let t = (overlay_depth - cfg.depth_low) as f64 / span;
        DELAY_LEFT_OVER + (t * cfg.reserve_max_ms.saturating_sub(DELAY_LEFT_OVER) as f64) as u64
    }
}

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

/// Consensus metrics shared across all payload jobs (tracks intentional mining delays).
static CONSENSUS_METRICS: Lazy<BscConsensusMetrics> = Lazy::new(BscConsensusMetrics::default);

/// Module-level miner metrics instance shared across all payload builds.
static MINER_METRICS: Lazy<BscMinerMetrics> = Lazy::new(BscMinerMetrics::default);

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
    let scaled_millis =
        duration.as_millis().saturating_mul(numerator as u128) / denominator as u128;
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

/// R2: margin (ms) reserved before a slot's `end_mining_timestamp_ms` when bounding the
/// sparse-trie `state_root()` wait, leaving room to finalize before the slot deadline.
pub const STATE_ROOT_WAIT_MARGIN_MS: u64 = 30;

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
    /// Precomputed `(state_root, trie_updates)` from a sparse-trie background task.
    ///
    /// Filled in by `BscPayloadJob::start` after exec completes, by calling
    /// `StateRootHandle::state_root()` on the handle obtained from
    /// `crate::shared::spawn_sparse_trie_state_root`. The builder consumes this in
    /// `finish` to skip the blocking `state_root_with_updates` call when a value is
    /// present. `None` triggers the legacy synchronous path (fallback).
    ///
    /// `Arc<Mutex<...>>` so `#[derive(Clone)]` on `BscBuildArguments` still works; the
    /// builder takes (`Option::take`) the value, retries see `None`.
    pub state_root_precomputed:
        Arc<Mutex<Option<(alloy_primitives::B256, reth_trie_common::updates::TrieUpdates)>>>,
    /// Sparse-trie state-root handle for this job.
    ///
    /// Spawned once in `BscPayloadJob::start` (gated by
    /// `MiningConfig::use_sparse_trie_state_root`) and consumed by the first build attempt
    /// inside `build_payload`:
    ///   1. take handle from this slot
    ///   2. `handle.state_hook()` → install via `executor.set_state_hook(Some(_))`
    ///   3. run tx exec (state diffs flow to the background task)
    ///   4. drop hook (`set_state_hook(None)`) to signal task to finalize
    ///   5. `handle.state_root()` → write the `(state_root, trie_updates)` into
    ///      [`Self::state_root_precomputed`], which `finish` then consumes
    ///
    /// `Arc<Mutex<Option<_>>>` because `StateRootHandle` is `!Clone` (it owns
    /// single-consumer channels) and `BscBuildArguments` derives `Clone`. First retry
    /// takes the handle; subsequent retries see `None` and fall back to the legacy
    /// synchronous state-root path (still correct, just slower for that retry).
    pub trie_handle: Arc<Mutex<Option<reth_engine_tree::tree::StateRootHandle>>>,
    /// Absolute wall-clock deadline (epoch ms) for bounding the sparse-trie
    /// `state_root()` wait in `finish`; threaded into the build ctx. Set from
    /// `MiningContext::end_mining_timestamp_ms` minus [`STATE_ROOT_WAIT_MARGIN_MS`].
    /// `None` = legacy unbounded blocking wait.
    pub state_root_deadline_ms: Option<u64>,
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
    #[allow(clippy::too_many_arguments)]
    pub fn new(
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
        args: BscBuildArguments<EthPayloadAttributes>,
    ) -> Result<BscBuiltPayload, Box<dyn std::error::Error + Send + Sync>> {
        let build_start = std::time::Instant::now();
        let BscBuildArguments {
            mut cached_reads,
            config,
            cancel,
            trace_id,
            min_gas_tip,
            state_root_precomputed,
            // R3: the job-level handle is ignored here; build_payload spawns a fresh one
            // per attempt below, so retries (value-gated rebuilds) also get the
            // precomputed root instead of only the first attempt.
            trie_handle: _,
            state_root_deadline_ms,
        } = args;
        let PayloadConfig { parent_header, attributes, payload_id: _, parent_block_info: _ } = config;

        let parent_hash = parent_header.hash_slow();

        // R3: spawn a fresh sparse-trie state-root handle for THIS build attempt. The
        // job-level handle was single-use — the first attempt consumed it and any retry
        // (e.g. a value-gated rebuild) fell back to the synchronous `state_root_with_updates`,
        // so ~half of in-turn blocks paid the full sync root cost. A fresh handle per attempt
        // is cheap now that R1 shares the engine's proof pools. `None` keeps the sync path
        // (sparse-trie disabled or no spawner registered).
        let trie_handle: Arc<Mutex<Option<reth_engine_tree::tree::StateRootHandle>>> = {
            let use_sparse_trie = crate::node::miner::config::get_global_mining_config()
                .is_some_and(|c| c.use_sparse_trie_state_root);
            Arc::new(Mutex::new(if use_sparse_trie {
                crate::shared::spawn_sparse_trie_state_root(parent_header.as_ref().clone())
            } else {
                None
            }))
        };

        let state_provider = self.client.state_by_block_hash(parent_header.hash_slow())?;
        let state = StateProviderDatabase::new(&state_provider);
        let mut db = State::builder()
            .with_database(cached_reads.as_db_mut(state))
            .with_bundle_update()
            .build();

        // Sinks transport current_validators / turn_length from the builder (which is consumed by
        // `finish`) back to this layer so they can be written to cache after
        // finalize_new_header() assigns the definitive block hash.
        let validator_cache_sink: ValidatorCacheSink = Arc::new(Mutex::new(None));
        let turn_length_sink: Arc<Mutex<Option<u8>>> = Arc::new(Mutex::new(None));

        // Sink for the sparse-trie precomputed state root. This MUST be a fresh per-attempt
        // Arc<Mutex<>> — NOT a clone of the job-level `state_root_precomputed`. The job-level Arc
        // is shared by every build attempt (including the deadline-spawned empty-fallback build),
        // and inside `finish` the write (after the sparse-trie wait) and the read-back
        // (`sink.take()`) are separated by `merge_transitions` + `hashed_post_state` over all txs
        // (~hundreds of ms for a full block). With a shared sink, a concurrent second finish (e.g.
        // the empty build, which has no trie_handle so it jumps straight to the take()) steals the
        // root this attempt deposited, forcing this attempt onto the slow synchronous
        // `state_root_with_updates`. A per-attempt sink makes write→read strictly intra-attempt, so
        // the full build reads back its OWN precomputed root. When the sparse-trie path is not active
        // (flag off), this Mutex stays `None` and the builder falls through to
        // `state_root_with_updates`.
        let state_root_precomputed_sink: Arc<
            Mutex<Option<(alloy_primitives::B256, reth_trie_common::updates::TrieUpdates)>>,
        > = Arc::new(Mutex::new(None));

        let next_env_attributes = BscNextBlockEnvAttributes {
            inner: NextBlockEnvAttributes {
                timestamp: attributes.timestamp,
                suggested_fee_recipient: attributes.suggested_fee_recipient,
                prev_randao: attributes.prev_randao,
                gas_limit: self.builder_config.gas_limit(parent_header.gas_limit),
                parent_beacon_block_root: attributes.parent_beacon_block_root,
                withdrawals: attributes.withdrawals.as_ref().map(|w| Withdrawals::new(w.clone())),
                extra_data: crate::shared::get_miner_extra()
                    .filter(|b| !b.is_empty())
                    .unwrap_or_else(|| self.builder_config.extra_data.clone()),
                slot_number: None,
            },
            validator_cache_sink: Some(validator_cache_sink.clone()),
            turn_length_sink: Some(turn_length_sink.clone()),
            state_root_precomputed_sink: Some(state_root_precomputed_sink),
            // Forward the Arc<Mutex<>> holding the StateRootHandle into ctx so `finish`
            // can take it after executor.finish() runs the BSC post-execution system txs
            // (slash / reward / validator-set updates) with the state_hook installed.
            // See `BscBlockExecutionCtx::trie_handle` doc.
            trie_handle: Some(trie_handle.clone()),
            state_root_deadline_ms,
        };

        let mut builder = self
            .evm_config
            .builder_for_next_block(&mut db, &parent_header, next_env_attributes)
            .map_err(PayloadBuilderError::other)?;

        // Wire the sparse-trie state-root task's update hook onto the building EVM's DB.
        //
        // v2.4.1 installs the hook at the DB level (it fires on every state commit) rather than
        // on the executor. It must be set before `apply_pre_execution_changes()` so pre-execution
        // state touches are also streamed to the task. The hook is dropped when the executor is
        // consumed in `finish`, which signals end-of-updates; `finish` then reads the precomputed
        // root from the same handle (forwarded via `ctx.trie_handle`) with a slot-deadline-bounded
        // wait. When no spawner is registered, `trie_handle` is empty and this is a no-op
        // (synchronous fallback).
        if let Some(handle) = trie_handle.lock().unwrap().as_mut() {
            builder
                .evm_mut()
                .db_mut()
                .set_state_hook(Some(Box::new(handle.take_execution_hook())));
            debug!(
                target: "payload_builder",
                trace_id,
                parent_hash = ?parent_hash,
                "Installed sparse-trie state-root hook on building EVM DB"
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
            builder.evm_mut().block().gas_limit().saturating_sub(system_txs_gas);

        let base_fee = builder.evm_mut().block().basefee();
        trace!("build_payload: base_fee={}", base_fee);

        let mut sidecars_map = HashMap::new();
        let mut block_blob_count = 0;

        let mut blob_fee = None;
        let blob_params = self.chain_spec.blob_params_at_timestamp(attributes.timestamp);
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
                    attributes.timestamp,
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
                        // best_tx_list.mark_invalid(
                        //     &pool_tx,
                        //     InvalidPoolTransactionError::Consensus(
                        //         InvalidTransactionError::NonceNotConsistent {
                        //             tx: tx.nonce(),
                        //             state: 0_u64, // TODO: get the nonce from the state later.
                        //         },
                        //     ),
                        // );
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
                Err(err) => {
                    return Err(Box::new(std::io::Error::other(err.to_string())));
                }
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
            total_fees += U256::from(miner_fee) * U256::from(gas_used.tx_gas_used());
            cumulative_gas_used += gas_used.tx_gas_used();

            let tx_duration = tx_start.elapsed();
            if tx_duration.as_micros() > 3000 {
                debug!(
                    target: "payload_builder",
                    trace_id,
                    block_number = parent_header.number() + 1,
                    tx = ?tx.hash(),
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
                    tx = ?tx.hash(),
                    gas_used = ?gas_used,
                    cumulative_gas_used = ?cumulative_gas_used,
                    duration_micros = tx_duration.as_micros(),
                    "Transaction executed successfully"
                );
            }

            // Add blob tx sidecar to the payload.
            if let Some(sidecar) = blob_tx_sidecar {
                sidecars_map.insert(*tx.hash(), sidecar);
            }
        }
        let exec_duration = exec_start.elapsed();

        // add system txs to payload.
        let finalize_start = std::time::Instant::now();

        // Sparse-trie state-root collection happens INSIDE `finish`, NOT here. The reason:
        // BSC's post-execution (slash, fee distribution, validator-set updates) runs as
        // system txs via `executor.finish()` inside `finish`. Those system txs change state
        // and must be captured by the `state_hook` we installed before exec. If we dropped
        // the hook here (before `finish`), the sparse-trie task would compute a state root
        // missing those changes — diverging from the canonical state-root and causing
        // consensus split / slashing.
        //
        // The handle was forwarded into `ctx.trie_handle` via attrs; builder.rs takes
        // it after executor.finish() and calls `state_root()` once the hook is
        // naturally dropped (executor consumption triggers `StateHookSender::drop`
        // which sends `FinishedStateUpdates`).
        // The job-level `state_root_precomputed` Arc is vestigial: this attempt uses its own
        // per-attempt sink (see `state_root_precomputed_sink` above). Kept bound to avoid an
        // unused-variable warning until the field is removed from BscBuildArguments.
        let _ = &state_root_precomputed;
        let BlockBuilderOutcome { execution_result, hashed_state, trie_updates, block, block_access_list: _ } =
            builder.finish(&state_provider, None)?;

        let mut sealed_block = Arc::new(block.sealed_block().clone());

        // Update miner metrics
        let finalize_elapsed = finalize_start.elapsed();
        let finalize_duration = finalize_elapsed.as_secs_f64();
        MINER_METRICS.block_finalize_duration_seconds.record(finalize_duration);

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
                    block_number: sealed_block.header().number(),
                    block_hash: sealed_block.hash(),
                    tx_index: u64::try_from(index).unwrap_or(u64::MAX),
                    tx_hash: *tx.hash(),
                    version: 0,
                };
                blob_sidecars.push(bsc_blob_tx_sidecar);
            }
        }

        let mut plain = sealed_block.clone_block();
        plain.body.sidecars = if blob_sidecars.is_empty() { None } else { Some(blob_sidecars) };
        sealed_block = Arc::new(plain.into());

        let requests = execution_result.requests.clone();
        let execution_outcome =
            BlockExecutionOutput { state: db.take_bundle(), result: execution_result };
        let executed_block = BuiltPayloadExecutedBlock {
            recovered_block: Arc::new(block),
            execution_output: Arc::new(execution_outcome),
            hashed_state: Arc::new(hashed_state),
            trie_updates: Arc::new(trie_updates),
        };

        // Read validator/turn-length data transported via sinks from the now-consumed builder.
        let pending_validators = validator_cache_sink.lock().unwrap().take();
        let pending_turn_length = turn_length_sink.lock().unwrap().take();

        let payload = BscBuiltPayload {
            block: sealed_block.clone(),
            fees: total_fees,
            requests: Some(requests),
            build_kind: BuildKind::NormalAttempt,
            exec_duration,
            trie_root_duration: finalize_elapsed,
            executed_block,
            pending_validators,
            pending_turn_length,
            bid_builder: None,
        };
        Ok(payload)
    }

    /// Build an empty payload without any user transactions from the pool
    /// Only contains system transactions (if any)
    pub async fn build_empty_payload(
        &self,
        args: BscBuildArguments<EthPayloadAttributes>,
    ) -> Result<BscBuiltPayload, Box<dyn std::error::Error + Send + Sync>> {
        let build_start = std::time::Instant::now();
        let BscBuildArguments {
            mut cached_reads,
            config,
            cancel: _,
            trace_id,
            min_gas_tip: _,
            state_root_precomputed,
            trie_handle,
            state_root_deadline_ms: _,
        } = args;
        let PayloadConfig { parent_header, attributes, payload_id: _, parent_block_info: _ } = config;

        let parent_hash = parent_header.hash_slow();
        let _ = parent_hash;

        let state_provider = self.client.state_by_block_hash(parent_header.hash_slow())?;
        let state = StateProviderDatabase::new(&state_provider);
        let mut db = State::builder()
            .with_database(cached_reads.as_db_mut(state))
            .with_bundle_update()
            .build();

        // Sinks for empty-payload builds (same delayed-seal mechanism as normal builds).
        let validator_cache_sink: ValidatorCacheSink = Arc::new(Mutex::new(None));
        let turn_length_sink: Arc<Mutex<Option<u8>>> = Arc::new(Mutex::new(None));

        let mut builder = self
            .evm_config
            .builder_for_next_block(
                &mut db,
                &parent_header,
                BscNextBlockEnvAttributes {
                    inner: NextBlockEnvAttributes {
                        timestamp: attributes.timestamp,
                        suggested_fee_recipient: attributes.suggested_fee_recipient,
                        prev_randao: attributes.prev_randao,
                        gas_limit: self.builder_config.gas_limit(parent_header.gas_limit),
                        parent_beacon_block_root: attributes.parent_beacon_block_root,
                        withdrawals: attributes
                            .withdrawals
                            .as_ref()
                            .map(|w| Withdrawals::new(w.clone())),
                        extra_data: crate::shared::get_miner_extra()
                            .filter(|b| !b.is_empty())
                            .unwrap_or_else(|| self.builder_config.extra_data.clone()),
                        slot_number: None,
                    },
                    validator_cache_sink: Some(validator_cache_sink.clone()),
                    turn_length_sink: Some(turn_length_sink.clone()),
                    // Empty-fallback build never installs a sparse-trie hook (trie_handle: None
                    // below), so it must NOT read from any sparse-trie sink. Passing None makes the
                    // builder compute this empty block's own (cheap) state root via
                    // `state_root_with_updates` instead of stealing a full build's precomputed root
                    // out of a shared sink (which both starved the full build and risked sealing a
                    // foreign root onto the empty block).
                    state_root_precomputed_sink: None,
                    // Empty-payload path: don't engage sparse-trie (would still be
                    // correct but the setup overhead isn't worth it for ~0-tx blocks).
                    trie_handle: None,
                    state_root_deadline_ms: None,
                },
            )
            .map_err(PayloadBuilderError::other)?;

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

        // Add system txs to payload and finalize.
        let finalize_start = std::time::Instant::now();
        //
        // Empty-payload path: we skip the sparse-trie state-root machinery here. The
        // empty path is the "give up and seal whatever we have" branch and only runs
        // BSC system txs (slash / fee distribution) — state delta is minimal so the
        // legacy `state_root_with_updates` cost is acceptable. The handle (if any)
        // stays in `trie_handle` and is dropped when the spawned task ends.
        let _ = (&state_root_precomputed, &trie_handle);
        let BlockBuilderOutcome { execution_result, hashed_state, trie_updates, block, block_access_list: _ } =
            builder.finish(&state_provider, None)?;
        let finalize_elapsed = finalize_start.elapsed();

        let sealed_block = Arc::new(block.sealed_block().clone());

        // Update miner metrics
        let finalize_duration = finalize_start.elapsed().as_secs_f64();
        MINER_METRICS.block_finalize_duration_seconds.record(finalize_duration);

        let build_duration = build_start.elapsed();

        debug!(
            target: "payload_builder",
            trace_id,
            block_number = sealed_block.number(),
            block_hash = ?sealed_block.hash(),
            tx_count = sealed_block.body().transactions.len(),
            cumulative_gas_used,
            total_fees = %total_fees,
            prepare_duration_ms = prepare_duration.as_millis(),
            exec_duration_ms = exec_duration.as_millis(),
            trie_root_duration_ms = finalize_elapsed.as_millis(),
            build_duration_ms = build_duration.as_millis(),
            "Empty block payload built successfully (no user transactions)"
        );

        let requests = execution_result.requests.clone();
        let execution_outcome =
            BlockExecutionOutput { state: db.take_bundle(), result: execution_result };
        let executed_block = BuiltPayloadExecutedBlock {
            recovered_block: Arc::new(block),
            execution_output: Arc::new(execution_outcome),
            hashed_state: Arc::new(hashed_state),
            trie_updates: Arc::new(trie_updates),
        };

        // Read validator/turn-length data transported via sinks from the now-consumed builder.
        let pending_validators = validator_cache_sink.lock().unwrap().take();
        let pending_turn_length = turn_length_sink.lock().unwrap().take();

        let payload = BscBuiltPayload {
            block: sealed_block.clone(),
            fees: total_fees,
            requests: Some(requests),
            build_kind: BuildKind::EmptyFallback,
            exec_duration,
            trie_root_duration: finalize_elapsed,
            executed_block,
            pending_validators,
            pending_turn_length,
            bid_builder: None,
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
    /// Best (sealed, unexecuted) BEP-675 BidBlock for this parent, collected from the simulator.
    /// Competes against `potential_payloads` by fee; if it wins it is broadcast first and verified
    /// on import (zero-simulate), so it is intentionally kept out of `potential_payloads`.
    bid_block_candidate: Option<BidBlockTask>,
    /// Current build arguments
    build_args: BscBuildArguments<EthPayloadAttributes>,
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
        build_args: BscBuildArguments<EthPayloadAttributes>,
        simulator: Arc<BidSimulator<Client, Pool>>, // No outer RwLock needed
        result_tx: mpsc::UnboundedSender<SubmitContext>,
    ) -> (Self, BscPayloadJobHandle) {
        let (abort_tx, abort_rx) = oneshot::channel();
        let (try_build_tx, try_build_rx) = mpsc::unbounded_channel();
        let (tx_listener_tx, tx_listener_rx) = mpsc::unbounded_channel();

        let trace_id = build_args.trace_id;

        // Adaptive root reserve: when the in-memory overlay (head − finalized) is deep, reserve
        // more of the slot for the background state root so it finalizes before the deadline
        // instead of degrading the block to empty-fallback. Normal-depth blocks keep the default
        // reserve (overlay_depth ≤ ROOT_RESERVE_DEPTH_LOW). Falls back to depth 0 (default reserve)
        // when finalized is not yet available. See docs/design-adaptive-overlay-depth.md.
        let block_number = mining_ctx.parent_header.number() + 1;
        let overlay_depth = crate::shared::get_canonical_in_memory_state()
            .and_then(|cim| cim.get_finalized_num_hash())
            .map(|f| block_number.saturating_sub(f.number))
            .unwrap_or(0);
        let effective_reserve = effective_delay_left_over(overlay_depth);
        metrics::histogram!("bsc_miner_effective_reserve_ms").record(effective_reserve as f64);
        let mining_delay = parlia.clone().delay_for_mining(
            &mining_ctx.parent_snapshot,
            mining_ctx.header.as_ref().unwrap(),
            effective_reserve,
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
            bid_block_candidate: None,
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
            end_mining_timestamp_ms = job.mining_ctx.end_mining_timestamp_ms,
            "Succeed to new payload job"
        );
        (job, handle)
    }

    /// Runs the payload job asynchronously with timeout support
    pub async fn start(mut self) -> Result<(), Box<BscPayloadJobError>> {
        // Sparse-trie state-root (`--mining.use-sparse-trie-state-root`):
        // R3 spawns a fresh background task PER build attempt inside `build_payload`
        // (not once here), so every attempt — including value-gated rebuilds — gets the
        // precomputed root rather than only the first attempt. Each attempt installs
        // `handle.state_hook()` before exec, drops it after to finalize, then `finish`
        // calls `state_root()` (bounded by R2's slot deadline) and falls back to
        // synchronous `state_root_with_updates` on miss / no spawner.

        let mut start_time = std::time::Instant::now();
        let initial_wait = initial_out_of_turn_build_wait(&self.parlia, &self.mining_ctx);
        if !initial_wait.is_zero() {
            debug!(
                target: "bsc::miner::payload",
                trace_id = self.trace_id,
                block_number = self.build_args.config.parent_header.number() + 1,
                wait_ms = initial_wait.as_millis(),
                "Applying out-of-turn backoff; starting speculative build"
            );

            // Kick off a speculative build before sleeping so the sparse-trie
            // background task can warm the storage slots state-root will need.
            // Without this the speculative work only starts after the backoff ends,
            // leaving ~one slot for both cache warm-up and state-root computation
            // over thousands of txs — which repeatedly times out and degrades the
            // block to EmptyFallback. The spawned build's result is picked up by
            // the outer loop's join_next() branch, so the try_build_tx kickoff
            // below is skipped when a speculative build is already in flight.
            self.retries += 1;
            start_time = std::time::Instant::now();
            {
                let builder = self.builder.clone();
                let build_args = self.build_args.clone();
                self.join_handle.spawn(async move { builder.build_payload(build_args).await });
            }

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

        // Skip the normal first-build kickoff if a speculative build from the
        // out-of-turn backoff is already running or has completed into the JoinSet.
        if self.join_handle.is_empty() {
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
                                build_kind = ?payload.build_kind,
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

                                        if let Some(action) =
                                            self.evaluate_local_rebuild_action(fresh_remaining_duration)
                                        {
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
        metrics.payload_rebuild_estimated_uplift_bps.set(estimated_uplift_bps(
            self.current_local_payload_fees,
            self.estimated_new_local_fees,
        ) as f64);

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

    /// Fetch the best bid for the current parent block, push it into `potential_payloads`,
    /// and return its block hash (or `None` if no valid bid exists).
    fn collect_best_bid(&mut self) {
        let Some(best_bid) = self.simulator.get_best_bid(self.mining_ctx.parent_header.hash())
        else {
            return;
        };
        let bid_info = best_bid.bid;
        if let Some(bsc_payload) = best_bid.bsc_payload {
            info!(
                target: "bsc::miner::payload",
                trace_id = self.trace_id,
                block_number = bid_info.block_number,
                is_inturn = self.mining_ctx.is_inturn,
                builder = ?bid_info.builder,
                bid_gas_fee = %bid_info.gas_fee,
                bid_hash = %bid_info.bid_hash,
                payload_fees = %bsc_payload.fees(),
                "Found best bid"
            );
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

    /// Collect the best (sealed, unexecuted) BEP-675 BidBlock for this parent so it can compete by
    /// fee against the local block and any legacy SendBid in `try_return_best_payload`
    /// (go-bsc `selectBidBlock`).
    ///
    /// Unlike the legacy/local payloads, the BidBlock is **not** pushed into `potential_payloads`:
    /// under zero-simulate it is never executed before selection, so it has no `BscBuiltPayload`.
    /// If it wins it is broadcast first and verified on import.
    fn collect_best_bid_block(&mut self) {
        let parent_hash = self.mining_ctx.parent_header.hash();
        match self.simulator.best_bid_block(parent_hash) {
            Some(task) => {
                info!(
                    target: "bsc::miner::payload",
                    trace_id = self.trace_id,
                    bid_fees = %task.gas_fee,
                    bid_hash = %task.bid_hash,
                    builder = ?task.builder,
                    "Found best (unexecuted) BidBlock candidate"
                );
                self.bid_block_candidate = Some(task);
            }
            // Log the miss with the key we looked up. A silent miss is indistinguishable from
            // "no builder bid at all", which is what made the sample-before-wait bug above cost a
            // day to find: the builder saw `send success`, the validator logged `BidBlock queued`,
            // and nothing anywhere reported that the bid was never used.
            None => debug!(
                target: "bsc::miner::payload",
                trace_id = self.trace_id,
                %parent_hash,
                parent_number = self.mining_ctx.parent_header.number,
                "No BidBlock candidate stored for this parent"
            ),
        }
    }

    /// Ensure `potential_payloads` has at least one candidate, then drain background build tasks
    /// within the submission deadline to maximise the chance of a better (non-empty / higher-fee)
    /// payload.
    ///
    /// Phase 1 — guarantee a candidate: if `potential_payloads` is empty, spawn an empty-block
    /// build and block until its result (or an abort signal) arrives.
    ///
    /// Phase 2 — collect better candidates: loop over remaining background builds in 50 ms slices
    /// until the pre-computed `end_mining_timestamp_ms` deadline (+ 150 ms grace) is reached or
    /// all background tasks finish, whichever comes first.
    fn collect_payload_candidates(&mut self) -> Result<(), Box<BscPayloadJobError>> {
        // Phase 1: guarantee at least one candidate.
        if self.potential_payloads.is_empty() {
            let builder = self.builder.clone();
            let args = self.build_args.clone();
            self.join_handle.spawn(async move { builder.build_empty_payload(args).await });

            // JoinSet::len() counts tasks whose results have not yet been collected via
            // join_next(), regardless of whether the task has already finished executing.
            // After spawn() above, len() is guaranteed ≥ 1.
            let bg_tasks = self.join_handle.len();

            enum WaitFirst<T> {
                Aborted,
                Joined(T),
            }

            let abort_rx = &mut self.abort_rx;
            let join_handle = &mut self.join_handle;
            let wait_started = std::time::Instant::now();
            let outcome = tokio::task::block_in_place(move || {
                tokio::runtime::Handle::current().block_on(async move {
                    tokio::select! {
                        _ = abort_rx => WaitFirst::Aborted,
                        res = join_handle.join_next() => WaitFirst::Joined(res),
                    }
                })
            });

            let waited = wait_started.elapsed();
            match outcome {
                WaitFirst::Aborted => {
                    info!(
                        target: "bsc::miner::payload",
                        trace_id = self.trace_id,
                        block_number = self.build_args.config.parent_header.number() + 1,
                        is_inturn = self.mining_ctx.is_inturn,
                        bg_tasks,
                        waited_ms = waited.as_millis(),
                        "Abort while waiting for first payload candidate"
                    );
                    self.build_args.cancel.clone().cancel();
                    self.is_aborted = true;
                    return Err(Box::new(BscPayloadJobError::JobAborted));
                }
                WaitFirst::Joined(Some(Ok(Ok(payload)))) => {
                    let tx_count = payload.block().body().transaction_count();
                    let is_empty_block = tx_count == 0;
                    debug!(
                        target: "bsc::miner::payload",
                        trace_id = self.trace_id,
                        block_number = payload.block().header().number(),
                        block_hash = %payload.block().hash(),
                        is_inturn = self.mining_ctx.is_inturn,
                        build_kind = ?payload.build_kind,
                        tx_count,
                        is_empty_block,
                        fees = %payload.fees(),
                        bg_tasks,
                        waited_ms = waited.as_millis(),
                        "Received first payload candidate while returning best payload"
                    );
                    self.potential_payloads.push(payload);
                }
                WaitFirst::Joined(Some(Ok(Err(err)))) => {
                    debug!(
                        target: "bsc::miner::payload",
                        trace_id = self.trace_id,
                        try_mine_block_number = self.build_args.config.parent_header.number() + 1,
                        is_inturn = self.mining_ctx.is_inturn,
                        bg_tasks,
                        waited_ms = waited.as_millis(),
                        error = %err,
                        "Candidate build task failed while waiting for first payload candidate"
                    );
                }
                WaitFirst::Joined(Some(Err(err))) => {
                    debug!(
                        target: "bsc::miner::payload",
                        trace_id = self.trace_id,
                        try_mine_block_number = self.build_args.config.parent_header.number() + 1,
                        is_inturn = self.mining_ctx.is_inturn,
                        bg_tasks,
                        waited_ms = waited.as_millis(),
                        error = %err,
                        "Join failed while waiting for first payload candidate"
                    );
                }
                WaitFirst::Joined(None) => {
                    // Should not happen: we just spawned a task above, so JoinSet is non-empty.
                    debug!(
                        target: "bsc::miner::payload",
                        trace_id = self.trace_id,
                        try_mine_block_number = self.build_args.config.parent_header.number() + 1,
                        is_inturn = self.mining_ctx.is_inturn,
                        bg_tasks,
                        waited_ms = waited.as_millis(),
                        "Unexpected: JoinSet empty while waiting for first payload candidate"
                    );
                }
            }
        }

        // Phase 2: collect better candidates until deadline.
        while !self.join_handle.is_empty() {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis();
            if now_ms >= self.mining_ctx.end_mining_timestamp_ms + 150 {
                debug!(
                    target: "bsc::miner::payload",
                    trace_id = self.trace_id,
                    try_mine_block_number = self.build_args.config.parent_header.number() + 1,
                    is_inturn = self.mining_ctx.is_inturn,
                    bg_tasks = self.join_handle.len(),
                    now_ms,
                    end_mining_timestamp_ms = self.mining_ctx.end_mining_timestamp_ms,
                    "Skip waiting for additional payload candidates due to timeout"
                );
                break;
            }

            // Remaining time we can still spend waiting for background builds.
            let try_mine_block_number = self.build_args.config.parent_header.number() + 1;
            let mut remaining_ms =
                if self.mining_ctx.parent_snapshot.last_block_in_one_turn(try_mine_block_number) {
                    self.mining_ctx.end_mining_timestamp_ms.saturating_sub(now_ms) as u64
                } else {
                    (self.mining_ctx.end_mining_timestamp_ms.saturating_sub(now_ms) as u64)
                        .saturating_mul(3)
                };
            if remaining_ms > 50 {
                remaining_ms = 50;
            }
            let remaining = std::time::Duration::from_millis(remaining_ms);

            enum WaitMore<T> {
                Deadline,
                Aborted,
                Joined(T),
            }

            let abort_rx = &mut self.abort_rx;
            let join_handle = &mut self.join_handle;
            let wait_started = std::time::Instant::now();
            let outcome = tokio::task::block_in_place(move || {
                tokio::runtime::Handle::current().block_on(async move {
                    tokio::select! {
                        _ = abort_rx => WaitMore::Aborted,
                        _ = tokio::time::sleep(remaining) => WaitMore::Deadline,
                        res = join_handle.join_next() => WaitMore::Joined(res),
                    }
                })
            });

            let waited = wait_started.elapsed();
            match outcome {
                WaitMore::Deadline => {
                    debug!(
                        target: "bsc::miner::payload",
                        trace_id = self.trace_id,
                        try_mine_block_number = self.build_args.config.parent_header.number() + 1,
                        is_inturn = self.mining_ctx.is_inturn,
                        waited_ms = waited.as_millis(),
                        bg_tasks = self.join_handle.len(),
                        end_mining_timestamp_ms = self.mining_ctx.end_mining_timestamp_ms,
                        "No background payload candidate finished within wait slice"
                    );
                    // Keep waiting in further slices until we hit the expected end timestamp (+grace)
                    // or until all background tasks have completed.
                    continue;
                }
                WaitMore::Aborted => {
                    info!(
                        target: "bsc::miner::payload",
                        trace_id = self.trace_id,
                        try_mine_block_number = self.build_args.config.parent_header.number() + 1,
                        is_inturn = self.mining_ctx.is_inturn,
                        waited_ms = waited.as_millis(),
                        "Abort while waiting for additional payload candidates"
                    );
                    self.build_args.cancel.clone().cancel();
                    self.is_aborted = true;
                    return Err(Box::new(BscPayloadJobError::JobAborted));
                }
                WaitMore::Joined(Some(Ok(Ok(payload)))) => {
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
                        waited_ms = waited.as_millis(),
                        "Received additional payload candidate while returning best payload"
                    );
                    self.potential_payloads.push(payload);
                    // Continue loop until deadline or no more bg tasks.
                }
                WaitMore::Joined(Some(Ok(Err(err)))) => {
                    debug!(
                        target: "bsc::miner::payload",
                        trace_id = self.trace_id,
                        try_mine_block_number = self.build_args.config.parent_header.number() + 1,
                        is_inturn = self.mining_ctx.is_inturn,
                        waited_ms = waited.as_millis(),
                        error = %err,
                        "Candidate build task failed while waiting for additional payload candidates"
                    );
                    // Continue waiting, as other tasks may still succeed.
                }
                WaitMore::Joined(Some(Err(err))) => {
                    debug!(
                        target: "bsc::miner::payload",
                        trace_id = self.trace_id,
                        try_mine_block_number = self.build_args.config.parent_header.number() + 1,
                        is_inturn = self.mining_ctx.is_inturn,
                        waited_ms = waited.as_millis(),
                        error = %err,
                        "Join failed while waiting for additional payload candidates"
                    );
                    // Continue waiting, as other tasks may still succeed.
                }
                WaitMore::Joined(None) => {
                    // No task finished at the moment; break to avoid spinning.
                    break;
                }
            }
        }

        Ok(())
    }

    /// Sleep until this slot's submission deadline so that BidBlocks arriving later in the slot
    /// are still visible when selection runs — go-bsc defers `selectBidBlock` to the end of the
    /// build window for exactly this reason.
    ///
    /// The local path already spends this time: [`Self::submit_payload`] sleeps the same remainder
    /// before sending. Waiting *before* selection instead of after therefore costs the no-bid case
    /// nothing — the local payload still goes out at the same instant, because `submit_payload`
    /// then computes `delay_ms == 0` — while giving a builder the whole slot to bid rather than
    /// only the instant before this job first sampled the store.
    ///
    /// Without it, `collect_best_bid_block` runs at t≈0 of the slot and every bid that arrives
    /// afterwards is silently ignored despite being admitted by `mev_sendBidBlock` (whose
    /// `bidMustBefore` deadline is far later). Measured on a local 10-node cluster: the store was
    /// read 68 ms before the bid for that very block was written, and the validator sealed an
    /// empty block with the bid sitting unused under the correct key.
    fn wait_for_submission_deadline(&mut self) -> Result<(), Box<BscPayloadJobError>> {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let delay_ms = self.mining_ctx.end_mining_timestamp_ms.saturating_sub(now_ms) as u64;
        if delay_ms == 0 {
            return Ok(());
        }

        let abort_rx = &mut self.abort_rx;
        let aborted = tokio::task::block_in_place(move || {
            tokio::runtime::Handle::current().block_on(async move {
                tokio::select! {
                    _ = abort_rx => true,
                    _ = tokio::time::sleep(std::time::Duration::from_millis(delay_ms)) => false,
                }
            })
        });

        if aborted {
            info!(
                target: "bsc::miner::payload",
                trace_id = self.trace_id,
                block_number = self.build_args.config.parent_header.number() + 1,
                delay_ms,
                "Abort while waiting for the submission deadline"
            );
            self.build_args.cancel.clone().cancel();
            self.is_aborted = true;
            return Err(Box::new(BscPayloadJobError::JobAborted));
        }
        Ok(())
    }

    /// Try to return the best payload to result channel
    fn try_return_best_payload(&mut self) -> Result<(), Box<BscPayloadJobError>> {
        self.collect_best_bid();

        // Sample "is a replacement simulation still in flight" here, right after the last moment a
        // finished bid could still have been collected — not down at the metric site. A simulation
        // that completes after this point is already too late to be used, yet its `simulating_bid`
        // entry is removed the instant it completes, so a later check would report it as on-time.
        // On the common path the gap is a few ms and either point would do; the reason to sample
        // early is `collect_payload_candidates`, which blocks on `join_next` when no candidate has
        // landed yet — precisely the fat-block-under-load case this metric exists to detect, where
        // a late check would be biased toward reporting "on time".
        let replacement_in_flight =
            self.simulator.is_simulating(self.mining_ctx.parent_header.hash());

        self.collect_payload_candidates()?;

        // Hold the slot open before deciding, then sample the BidBlock store. Both must happen
        // after candidate collection: `collect_payload_candidates` returns immediately when the
        // local build finished and no background tasks remain (the empty-mempool case), so without
        // the wait this decision is made at t≈0 and no bid can ever be seen.
        self.wait_for_submission_deadline()?;
        self.collect_best_bid_block();

        // BEP-675 zero-simulate selection (go-bsc `selectBidBlock`): if the unexecuted BidBlock's
        // deposit-derived fee beats every local / legacy-SendBid candidate, commit to it. The block
        // is broadcast immediately and only then executed + state-root-verified on import — the
        // validator never re-executes before proposing. A losing BidBlock is simply dropped here.
        if self.try_submit_winning_bid_block() {
            return Ok(());
        }

        let best_payload = self.pick_best_payload_and_finalize(replacement_in_flight)?;

        self.submit_payload(best_payload)?;

        Ok(())
    }

    /// Re-assemble the vote attestation and re-seal a winning BidBlock's header at selection time
    /// (see the call site in [`Self::try_submit_winning_bid_block`] for why). Falls back to the
    /// block's admission-time seal — never fails the submission — if the snapshot provider is
    /// unavailable.
    fn refresh_bid_block_vote_attestation(
        &self,
        block: &RecoveredBlock<BscBlock>,
    ) -> SealedBlock<BscBlock> {
        let Some(snapshot_provider) = crate::shared::get_snapshot_provider().cloned() else {
            warn!(
                target: "bsc::miner::payload",
                trace_id = self.trace_id,
                "Snapshot provider unavailable; submitting BidBlock with admission-time vote attestation"
            );
            return block.sealed_block().clone();
        };

        refresh_and_reseal_bid_block(
            self.parlia.clone(),
            &self.mining_ctx.parent_snapshot,
            self.mining_ctx.parent_header.header(),
            &snapshot_provider,
            block,
            self.trace_id,
        )
    }

    /// If a collected BidBlock candidate out-bids every local/legacy payload by fee, hand its sealed
    /// block to the block-import service for broadcast-then-verify and return `true` (the local work
    /// is then discarded, matching go-bsc's `commitWork` early-return after `enqueueBidBlockTask`).
    ///
    /// Returns `false` when there is no candidate, it does not win, or the import sender is missing —
    /// in which case the caller falls back to submitting the best local payload.
    fn try_submit_winning_bid_block(&mut self) -> bool {
        let Some(bid) = self.bid_block_candidate.take() else { return false };

        let best_local_fee =
            self.potential_payloads.iter().map(|p| p.fees()).max().unwrap_or(U256::ZERO);
        if bid.gas_fee <= best_local_fee {
            info!(
                target: "bsc::miner::payload",
                trace_id = self.trace_id,
                bid_hash = %bid.bid_hash,
                bid_fees = %bid.gas_fee,
                best_local_fee = %best_local_fee,
                "BidBlock did not out-bid local candidates; discarding"
            );
            return false;
        }

        // Refresh the vote attestation with the freshest votes available now that this BidBlock
        // has won selection, and re-seal — go-bsc defers `assembleVoteAttestation` + the ECDSA
        // seal to `engine.Seal()`, called only after `selectBidBlock` picks the winner, to
        // maximize the window for BFT votes to arrive. This BidBlock's own attestation was
        // assembled at admission time (`simulate_bid_block`, potentially a full slot earlier);
        // without this refresh it would carry stale votes while the local-block path (via
        // `finalize_payload`, called at this same selection point) does not.
        let sealed = self.refresh_bid_block_vote_attestation(&bid.block);
        let block_number = sealed.header().number();
        let block_hash = sealed.hash();
        let parent_hash = sealed.header().parent_hash;

        // Late canonical-state re-check, mirroring the guards `ResultWorkWorker::submit_payload`
        // applies to the local/legacy path: a reorg during the build window can leave this
        // BidBlock's parent no longer canonical, or a competing block already at/above this
        // height. Catching that here — before broadcast — is strictly better than only finding
        // out via a failed `engine.new_payload` after the block has already been announced to
        // peers and the builder punished for a staleness issue that wasn't its fault.
        if let Some(best_block_number) = crate::shared::get_best_canonical_block_number() {
            if block_number <= best_block_number {
                debug!(
                    target: "bsc::miner::payload",
                    trace_id = self.trace_id,
                    bid_hash = %bid.bid_hash,
                    block_number,
                    best_block_number,
                    "BidBlock stale: block number not greater than best block number; discarding"
                );
                return false;
            }
        }

        let parent_number = block_number.saturating_sub(1);
        match crate::shared::get_canonical_header_by_number_from_provider(parent_number) {
            Some(canonical_parent) if canonical_parent.hash_slow() == parent_hash => {}
            Some(canonical_parent) => {
                debug!(
                    target: "bsc::miner::payload",
                    trace_id = self.trace_id,
                    bid_hash = %bid.bid_hash,
                    block_number,
                    parent_number,
                    expected_parent_hash = %parent_hash,
                    canonical_parent_hash = %canonical_parent.hash_slow(),
                    "BidBlock's parent no longer canonical (reorg occurred); discarding"
                );
                return false;
            }
            None => {
                debug!(
                    target: "bsc::miner::payload",
                    trace_id = self.trace_id,
                    bid_hash = %bid.bid_hash,
                    block_number,
                    parent_number,
                    "BidBlock's parent not found in canonical chain; discarding"
                );
                return false;
            }
        }

        // Expensive blob KZG verification on the winning block, BEFORE broadcast (go-bsc
        // `prepareBidBlockTask` → `validateBidBlockBlobTxs`). Cheap structural sidecar checks already
        // ran at admission; under zero-simulate full re-execution is deferred to after broadcast, so
        // this is the last gate that can stop a bad-blob block from being proposed. On failure revoke
        // the builder and fall back to the local payload (go-bsc `bidBlockFallback`).
        let body = sealed.body();
        let blob_sidecars = body.sidecars.as_deref().unwrap_or(&[]);
        if let Err(e) =
            validate_bid_block_blob_kzg(&body.inner.transactions, blob_sidecars, bid.system_tx_start)
        {
            warn!(
                target: "bsc::miner::payload",
                trace_id = self.trace_id,
                bid_hash = %bid.bid_hash,
                builder = ?bid.builder,
                error = %e,
                "BidBlock blob KZG validation failed; revoking builder and falling back to local payload"
            );
            crate::shared::get_bid_block_permission_manager().revoke(
                bid.builder,
                format!("BidBlock blob KZG invalid: {e}"),
                bid.bid_hash,
                block_number,
            );
            return false;
        }

        let Some(sender) = crate::shared::get_bid_block_import_sender() else {
            warn!(
                target: "bsc::miner::payload",
                trace_id = self.trace_id,
                bid_hash = %bid.bid_hash,
                "BidBlock won but import sender is not initialised; falling back to local payload"
            );
            return false;
        };

        // Double-sign guard shared with the local/legacy path (`ResultWorkWorker::submit_payload`
        // in bsc_miner.rs, via `crate::shared::check_and_record_mined_block`): the validator must
        // not sign a BidBlock at the same height on the same parent as a block it already sealed,
        // whether that prior block came from the local path or another BidBlock. This must be the
        // last check before the point of no return (the send below) — every earlier `return false`
        // in this function falls back to submitting the local payload for this same
        // (block_number, parent_hash), which must remain free to record and claim it.
        if !crate::shared::check_and_record_mined_block(block_number, parent_hash) {
            error!(
                target: "bsc::miner::payload",
                trace_id = self.trace_id,
                bid_hash = %bid.bid_hash,
                block_number,
                parent_hash = %parent_hash,
                "Reject Double Sign!! BidBlock at height already sealed on this parent; discarding"
            );
            return false;
        }

        // go-bsc's `Parlia.Seal` waits until the header's timestamp (`delayForRamanujanFork`)
        // before releasing the sealed block, so a bid-won block is never announced before its
        // own time — otherwise the wall-clock future bound peers (and our own import) enforce
        // on the sync path would reject it. Mirror that timing with a delayed background send,
        // the same pattern `submit_payload` uses for the local path.
        let delay = bid_block_broadcast_delay(sealed.header(), now_unix_ms());

        let trace_id = self.trace_id;
        let (builder_addr, bid_hash, gas_fee, system_tx_start) =
            (bid.builder, bid.bid_hash, bid.gas_fee, bid.system_tx_start);
        tokio::spawn(async move {
            if let Err(e) = send_bid_block_after_delay(
                sender,
                (sealed, builder_addr, bid_hash, gas_fee, system_tx_start),
                delay,
            )
            .await
            {
                // The slot was never actually broadcast — release it so a later legitimate
                // block at this height is not wrongly rejected as a double sign.
                crate::shared::forget_recorded_mined_block(block_number, parent_hash);
                warn!(
                    target: "bsc::miner::payload",
                    trace_id,
                    bid_hash = %bid_hash,
                    error = %e,
                    "Failed to hand BidBlock to import service"
                );
                return;
            }

            use crate::metrics::BscMevMetrics;
            use once_cell::sync::Lazy;
            static MEV_METRICS: Lazy<BscMevMetrics> = Lazy::new(BscMevMetrics::default);
            MEV_METRICS.bid_win_total.increment(1);

            info!(
                target: "bsc::miner::payload",
                trace_id,
                block_number,
                block_hash = %block_hash,
                bid_hash = %bid_hash,
                builder = ?builder_addr,
                bid_fees = %gas_fee,
                best_local_fee = %best_local_fee,
                "[BID BLOCK selected] broadcasting before verification (zero-simulate)"
            );
        });
        true
    }

    /// Send `payload` to the result channel, respecting the submission deadline.
    ///
    /// If the deadline has already passed (`delay_ms == 0`) the payload is sent immediately;
    /// otherwise it is handed off to a background task that sleeps for the remaining duration
    /// before sending.  The submitter re-validates canonical state on receipt, so stale
    /// contexts (e.g. a reorg occurred during the delay) are discarded safely.
    fn submit_payload(&mut self, payload: BscBuiltPayload) -> Result<(), Box<BscPayloadJobError>> {
        let block_number = payload.block().number();
        let block_hash = payload.block.hash();
        let is_inturn = self.mining_ctx.is_inturn;

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let delay_ms = self.mining_ctx.end_mining_timestamp_ms.saturating_sub(now_ms) as u64;

        let submit_ctx = SubmitContext {
            mining_ctx: self.mining_ctx.clone(),
            payload,
            cancel: self.build_args.cancel.clone(),
        };

        if delay_ms == 0 {
            if let Err(err) = self.result_tx.send(submit_ctx) {
                let total_job_duration = self.job_start_time.elapsed();
                warn!(
                    target: "bsc::miner::payload",
                    trace_id = self.trace_id,
                    block_number,
                    is_inturn,
                    total_job_duration_ms = total_job_duration.as_millis(),
                    error = %err,
                    "Failed to send best payload to result channel"
                );
                return Err(Box::new(BscPayloadJobError::ResultChannelSendError(err.to_string())));
            }
        } else {
            // Out-of-turn: a background task holds the SubmitContext and sends it after the
            // sleep; submit_payload() re-validates canonical state on arrival so stale
            // contexts (e.g. a reorg happened during the wait) are discarded safely.
            CONSENSUS_METRICS.intentional_mining_delays_total.increment(1);
            let result_tx = self.result_tx.clone();
            let trace_id = self.trace_id;
            info!(
                target: "bsc::miner::payload",
                trace_id,
                block_number,
                block_hash = %block_hash,
                is_inturn,
                delay_ms,
                "Block scheduled for delayed submission"
            );
            tokio::spawn(async move {
                tokio::time::sleep(tokio::time::Duration::from_millis(delay_ms)).await;
                if let Err(e) = result_tx.send(submit_ctx) {
                    warn!(
                        target: "bsc::miner::payload",
                        trace_id,
                        block_number,
                        error = %e,
                        "Failed to send delayed payload to result channel"
                    );
                }
            });
        }

        Ok(())
    }

    /// Select the highest-fee payload from `potential_payloads`, finalize it, then return it.
    ///
    /// Selection is by fees only; finalization (difficulty, vote attestation, ECDSA seal,
    /// cache updates) is delegated to [`finalize_payload`].
    /// `replacement_in_flight` is sampled by the caller at bid-collection time — see the comment at
    /// that sample point for why it cannot be read here instead.
    fn pick_best_payload_and_finalize(
        &mut self,
        replacement_in_flight: bool,
    ) -> Result<BscBuiltPayload, Box<BscPayloadJobError>> {
        let total_job_duration = self.job_start_time.elapsed();
        let try_mine_block_number = self.build_args.config.parent_header.number() + 1;

        if self.potential_payloads.is_empty() {
            MINER_METRICS.no_best_payload_total.increment(1);
            if self.mining_ctx.is_inturn {
                warn!(
                    target: "bsc::miner::payload",
                    trace_id = self.trace_id,
                    try_mine_block_number,
                    is_inturn = self.mining_ctx.is_inturn,
                    total_job_duration_ms = total_job_duration.as_millis(),
                    "No best payload available (inturn)"
                );
            } else {
                info!(
                    target: "bsc::miner::payload",
                    trace_id = self.trace_id,
                    try_mine_block_number,
                    is_inturn = self.mining_ctx.is_inturn,
                    total_job_duration_ms = total_job_duration.as_millis(),
                    "No best payload available to send (off-turn)"
                );
            }
            return Err(Box::new(BscPayloadJobError::NoPayloadsAvailable));
        }

        let best_index = self
            .potential_payloads
            .iter()
            .enumerate()
            .max_by_key(|(_, payload)| payload.fees())
            .map(|(index, _)| index)
            .expect("potential_payloads is non-empty");

        let total_len = self.potential_payloads.len();
        let mut best_payload = self.potential_payloads.remove(best_index);
        self.potential_payloads.clear();

        let gas_used = best_payload.block().header().gas_used();
        let gas_limit = best_payload.block().header().gas_limit();
        let gas_usage_percent =
            if gas_limit > 0 { (gas_used as f64 / gas_limit as f64 * 100.0) as u64 } else { 0 };

        finalize_payload(
            &mut best_payload,
            self.parlia.clone(),
            &self.mining_ctx.parent_snapshot,
            &self.mining_ctx.parent_header,
            self.mining_ctx.block_timestamp_ms,
        )
        .map_err(|e| {
            warn!(
                target: "bsc::miner::payload",
                trace_id = self.trace_id,
                try_mine_block_number,
                total_job_duration_ms = total_job_duration.as_millis(),
                error = %e,
                "Failed to finalize best payload"
            );
            Box::new(BscPayloadJobError::PayloadBuildingError(e.to_string()))
        })?;

        {
            use crate::metrics::BscMevMetrics;
            use once_cell::sync::Lazy;
            static MEV_METRICS: Lazy<BscMevMetrics> = Lazy::new(BscMevMetrics::default);
            if best_payload.bid_builder.is_some() {
                MEV_METRICS.bid_win_total.increment(1);
            } else {
                // Simulations were preempted for bids that then failed to win the block: the
                // interrupts killed work and bought nothing, and we sealed a local payload instead.
                // Against `bid_interrupt_total` this is the thrash ratio that a tightened
                // `no_interrupt_left_over` window has to be judged on — a preempted simulation
                // records no metric of its own (aborted runs return before
                // `bid_simulation_duration_seconds` is recorded), so this is the only place the
                // cost surfaces.
                //
                // Weighted by how many simulations were preempted, not one per block: the failure
                // mode under test is a cascade (bid → interrupt → bid → interrupt → nothing
                // finishes), and counting that once would flatten a six-deep cascade to look like a
                // single unlucky coin-flip. Keeping the same unit as `bid_interrupt_total` also
                // makes the ratio a true proportion rather than blocks-per-interrupt.
                let parent_hash = self.mining_ctx.parent_header.hash();
                let wasted = self.simulator.take_interrupt_count(parent_hash);
                if wasted > 0 {
                    MEV_METRICS.bid_interrupt_wasted_total.increment(wasted);

                    // Split out the cause. A replacement still in flight when the bid was needed is
                    // the one outcome that actually indicts the window: it was preempted with
                    // "enough" time left by `can_be_interrupted`'s arithmetic, and that arithmetic
                    // was wrong. Waste where the replacement *did* finish but lost on merit or was
                    // rejected would have happened under the old 500ms window too.
                    if replacement_in_flight {
                        MEV_METRICS.bid_interrupt_late_total.increment(wasted);
                    }
                }
            }
        }

        info!(
            target: "bsc::miner::payload",
            trace_id = self.trace_id,
            block_number = best_payload.block().header().number(),
            block_hash = %best_payload.block().hash(),
            is_inturn = self.mining_ctx.is_inturn,
            is_bid = best_payload.bid_builder.is_some(),
            tx_count = best_payload.block().body().transaction_count(),
            fees = %best_payload.fees(),
            exec_duration_ms = best_payload.exec_duration.as_millis(),
            trie_root_duration_ms = best_payload.trie_root_duration.as_millis(),
            gas_used,
            gas_limit,
            gas_usage_percent,
            pick_index = best_index + 1,
            total_len,
            total_job_duration_ms = total_job_duration.as_millis(),
            "Succeed to pick and finalize the best payload"
        );

        Ok(best_payload)
    }
}

/// Core of [`BscPayloadJob::refresh_bid_block_vote_attestation`], split out as a free function so
/// it's testable without constructing a full payload job.
///
/// Refreshes `block`'s vote attestation and re-seals via
/// [`crate::node::miner::util::refresh_vote_attestation_and_seal`]; on any internal failure that
/// function restores the original attestation/seal and returns `Ok(())`, so the only externally
/// observable outcomes are "refreshed" or "unchanged" — never a broken block. When the header
/// hash changes (any successful re-seal changes it, since the seal is part of `extra_data`, which
/// is part of the header hash), patches the blob sidecars' cached `block_hash` (set at admission
/// time) before the block goes out over P2P, mirroring `finalize_payload`'s equivalent patch for
/// the local-block path.
/// Milliseconds since the unix epoch, for comparison against a header's millisecond timestamp.
fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// How long to hold a won BidBlock before releasing it, so it is never announced before its own
/// header timestamp.
///
/// go-bsc's `Parlia.Seal` waits out `delayForRamanujanFork` for exactly this reason: a block
/// announced early trips the wall-clock future bound that peers — and our own import path —
/// enforce, so an early broadcast gets the block rejected rather than propagated. Zero once the
/// timestamp has passed, which is the common case for a block sealed at the end of its slot.
fn bid_block_broadcast_delay(header: &alloy_consensus::Header, now_ms: u64) -> std::time::Duration {
    let target_ms = crate::consensus::parlia::util::calculate_millisecond_timestamp(header);
    std::time::Duration::from_millis(target_ms.saturating_sub(now_ms))
}

/// Waits out `delay`, then hands the BidBlock to the import service, which is where broadcast
/// actually begins.
///
/// Split out from `try_submit_winning_bid_block` so the "not before its timestamp" invariant can be
/// tested against a channel instead of a P2P capture: the `send` below *is* the start of broadcast,
/// so a test that observes when the receiver sees the block observes the real timing.
async fn send_bid_block_after_delay<T>(
    sender: &tokio::sync::mpsc::UnboundedSender<T>,
    block: T,
    delay: std::time::Duration,
) -> Result<(), tokio::sync::mpsc::error::SendError<T>> {
    if !delay.is_zero() {
        tokio::time::sleep(delay).await;
    }
    sender.send(block)
}

fn refresh_and_reseal_bid_block(
    parlia: Arc<Parlia<BscChainSpec>>,
    parent_snapshot: &Snapshot,
    parent_header: &alloy_consensus::Header,
    snapshot_provider: &Arc<dyn crate::consensus::parlia::SnapshotProvider + Send + Sync>,
    block: &RecoveredBlock<BscBlock>,
    trace_id: u64,
) -> SealedBlock<BscBlock> {
    let mut plain_block = block.clone_block();
    if let Err(e) = crate::node::miner::util::refresh_vote_attestation_and_seal(
        parlia,
        parent_snapshot,
        parent_header,
        &mut plain_block.header,
        snapshot_provider,
    ) {
        warn!(
            target: "bsc::miner::payload",
            trace_id,
            error = %e,
            "Failed to refresh BidBlock vote attestation; submitting with admission-time attestation"
        );
        return block.sealed_block().clone();
    }

    let final_hash = plain_block.header.hash_slow();
    if let Some(ref mut sidecars) = plain_block.body.sidecars {
        for sidecar in sidecars.iter_mut() {
            sidecar.block_hash = final_hash;
        }
    }
    plain_block.seal_unchecked(final_hash)
}

/// Finalize a built payload in-place.
///
/// Runs `finalize_new_header()` on the payload's header (sets difficulty, prepares validators
/// for epoch blocks, assembles vote attestation, and ECDSA-seals the header), then:
///
/// 1. Writes `pending_validators` / `pending_turn_length` to the global caches keyed by
///    the now-deterministic final block hash.
/// 2. Rebuilds `executed_block.recovered_block` with the finalized header so the engine
///    tree can identify the block by its correct hash.
/// 3. Rebuilds `block` (sealed block with sidecars) with the finalized header.
///
/// This function is intentionally separate from the builder path so that finalization is
/// deferred until `pick_best_payload_and_finalize()` chooses the winning payload — giving more time for
/// FF votes to arrive.
fn finalize_payload(
    payload: &mut BscBuiltPayload,
    parlia: Arc<Parlia<BscChainSpec>>,
    parent_snapshot: &Snapshot,
    parent_header: &SealedHeader<alloy_consensus::Header>,
    block_timestamp_ms: u64,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let snapshot_provider = crate::shared::get_snapshot_provider().cloned().ok_or_else(|| {
        Box::new(std::io::Error::other("Snapshot provider not available"))
            as Box<dyn std::error::Error + Send + Sync>
    })?;

    let senders = payload.executed_block.recovered_block.senders().to_vec();
    let mut existing_sidecars = payload.block.clone_block().body.sidecars;
    let mut plain_block = payload.executed_block.recovered_block.sealed_block().clone_block();

    // Tag legacy `SendBid` winners with their builder (go-bsc `setBidMevInfo`, the Bid case).
    // Must run before `finalize_new_header()`, which ECDSA-seals the header: a tag written after
    // the seal would not be covered by it. Local builds leave `requests_hash` empty, which is what
    // lets consumers read "untagged" as "locally built" — see `block_mev_info`.
    //
    // The BEP-675 BidBlock path tags its own header in `simulate_bid_block()` instead, because
    // those blocks never become a `BscBuiltPayload` and so never reach this function.
    if let Some(builder) = payload.bid_builder {
        let prague_active = parlia.chain_spec().is_prague_active_at_block_and_timestamp(
            plain_block.header.number,
            plain_block.header.timestamp,
        );
        set_block_mev_info(
            &mut plain_block.header,
            BlockMevInfoVersion::Bid,
            builder,
            prague_active,
        );
    }

    finalize_new_header(
        parlia,
        parent_snapshot,
        parent_header,
        &mut plain_block.header,
        &snapshot_provider,
        block_timestamp_ms,
    )
    .map_err(|e| {
        Box::new(std::io::Error::other(e.to_string())) as Box<dyn std::error::Error + Send + Sync>
    })?;

    let final_hash = plain_block.header.hash_slow();
    if let Some((validators, vote_addresses)) = payload.pending_validators.take() {
        VALIDATOR_CACHE.lock().unwrap().insert(final_hash, (validators, vote_addresses));
        tracing::debug!(
            "Updated validator cache after finalize, block_number: {}, block_hash: {}",
            plain_block.header.number,
            final_hash
        );
    }
    if let Some(turn_length) = payload.pending_turn_length.take() {
        TURN_LENGTH_CACHE.lock().unwrap().insert(final_hash, turn_length);
        tracing::debug!(
            "Updated turn length cache after finalize, block_number: {}, block_hash: {}",
            plain_block.header.number,
            final_hash
        );
    }

    payload.executed_block.recovered_block =
        Arc::new(RecoveredBlock::new_unhashed(plain_block.clone(), senders));

    let mut finalized_with_sidecars = plain_block;
    // Update block_hash in each sidecar to reflect the final (post-seal) hash.
    // The sidecar block_hash was set to the pre-finalization hash at build time;
    // finalize_new_header() changes the header (difficulty, extra_data, ECDSA seal),
    // so the hash must be patched here before the sidecars are transmitted over P2P.
    if let Some(ref mut sidecars) = existing_sidecars {
        for sidecar in sidecars.iter_mut() {
            sidecar.block_hash = final_hash;
        }
    }
    finalized_with_sidecars.body.sidecars = existing_sidecars;
    payload.block = Arc::new(finalized_with_sidecars.into());

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        bid_block_broadcast_delay, initial_out_of_turn_build_wait, local_rebuild_action,
        refresh_and_reseal_bid_block, send_bid_block_after_delay, validate_bsc_sidecar,
        LocalRebuildAction, LocalRebuildPolicyInput,
    };
    use crate::chainspec::BscChainSpec;
    use crate::consensus::parlia::Parlia;
    use crate::consensus::parlia::Snapshot;
    use crate::node::miner::bsc_miner::MiningContext;
    use crate::node::primitives::BscBlock;
    use alloy_consensus::BlobTransactionSidecar;
    use alloy_consensus::Header;
    use alloy_eips::eip4844::{Blob, Bytes48};
    use alloy_eips::eip7594::{
        BlobTransactionSidecarEip7594, BlobTransactionSidecarVariant, CELLS_PER_EXT_BLOB,
    };
    use alloy_primitives::{Address, B256, U256};
    use reth::transaction_pool::error::Eip4844PoolTransactionError;
    use reth_primitives_traits::{RecoveredBlock, SealedHeader};
    use std::sync::Arc;
    use std::time::Duration;

    /// A won BidBlock must not be released before its own header timestamp.
    ///
    /// The delay is derived from the header, so this pins the arithmetic against a real header
    /// rather than re-deriving it. `calculate_millisecond_timestamp` is `timestamp * 1000` plus the
    /// millisecond part packed into the last 8 bytes of `mix_hash`, and that sub-second part
    /// matters: BSC slots are shorter than a second, so truncating it would under-delay by up to
    /// 999ms.
    #[test]
    fn bid_block_broadcast_delay_waits_out_the_header_timestamp() {
        let header_at = |secs: u64, millis: u64| {
            let mut h = Header { timestamp: secs, ..Default::default() };
            crate::consensus::parlia::util::set_millisecond_part_of_timestamp(millis, &mut h);
            h
        };

        // Header 750ms in the future -> wait exactly that long.
        let h = header_at(1_000, 250);
        assert_eq!(
            bid_block_broadcast_delay(&h, 1_000_000 - 750 + 250),
            Duration::from_millis(750)
        );

        // The millisecond part is honoured, not truncated to the second.
        assert_eq!(bid_block_broadcast_delay(&h, 1_000_000), Duration::from_millis(250));

        // Already due, and already past: no delay, and no panic on the underflow.
        assert!(bid_block_broadcast_delay(&h, 1_000_250).is_zero());
        assert!(bid_block_broadcast_delay(&h, 1_000_251).is_zero());
        assert!(bid_block_broadcast_delay(&h, u64::MAX).is_zero());
    }

    /// The delay must actually withhold the block, not merely be computed.
    ///
    /// `send_bid_block_after_delay`'s send is where broadcast begins — the import service takes it
    /// from there — so observing when the receiver sees the block observes the real broadcast
    /// timing, without a P2P capture. Time is paused, so this asserts ordering deterministically
    /// instead of racing a wall clock.
    #[tokio::test(start_paused = true)]
    async fn bid_block_send_is_withheld_until_the_delay_elapses() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<u64>();
        let delay = Duration::from_millis(600);

        let task = tokio::spawn(async move { send_bid_block_after_delay(&tx, 7u64, delay).await });

        // One millisecond short of due: still withheld.
        tokio::time::advance(Duration::from_millis(599)).await;
        assert!(
            rx.try_recv().is_err(),
            "BidBlock was released before its timestamp; peers would reject it as future-dated"
        );

        // Past due: released.
        tokio::time::advance(Duration::from_millis(2)).await;
        task.await.expect("send task panicked").expect("receiver is still alive");
        assert_eq!(rx.try_recv().expect("BidBlock must be released once due"), 7);
    }

    /// A block already past its timestamp — the common case, sealed at the end of its slot — must go
    /// out without waiting.
    #[tokio::test(start_paused = true)]
    async fn bid_block_send_is_immediate_when_already_due() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<u64>();

        send_bid_block_after_delay(&tx, 9u64, Duration::ZERO)
            .await
            .expect("receiver is still alive");

        assert_eq!(
            rx.try_recv().expect("a due BidBlock must not be delayed"),
            9,
            "zero delay must not defer the send"
        );
    }

    /// A closed receiver must surface as an error, because the caller uses it to release the
    /// double-sign slot: the block was never broadcast, so a later block at this height must remain
    /// free to claim it.
    #[tokio::test(start_paused = true)]
    async fn bid_block_send_reports_a_closed_receiver() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<u64>();
        drop(rx);

        let err = send_bid_block_after_delay(&tx, 1u64, Duration::from_millis(50))
            .await
            .expect_err("a closed receiver must be reported so the slot can be released");
        assert_eq!(err.0, 1, "the undelivered block is returned to the caller");
    }

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
            block_timestamp_ms: now_ms + delay_ms,
            end_mining_timestamp_ms: 0,
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
            while let Some(deadline_ms) = wait_deadline_ms {
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
                    LocalRebuildAction::ReturnBestPayload
                    | LocalRebuildAction::WaitForMoreValue => {
                        break;
                    }
                    LocalRebuildAction::WaitForCooldown(wait_duration) => {
                        wait_deadline_ms = Some(deadline_ms + wait_duration.as_millis() as u64);
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

    // ---- refresh_and_reseal_bid_block (BEP-675 vote-attestation-at-selection fix) ----

    struct MockBidBlockSnapshotProvider {
        snapshot: Snapshot,
    }

    impl crate::consensus::parlia::provider::SnapshotProvider for MockBidBlockSnapshotProvider {
        fn snapshot_by_hash(&self, _block_hash: &B256) -> Option<Snapshot> {
            Some(self.snapshot.clone())
        }
        fn insert(&self, _snapshot: Snapshot) {}
    }

    fn ensure_bid_block_test_signer() {
        crate::node::miner::signer::init_test_signer();
    }

    fn luban_chain_spec() -> Arc<BscChainSpec> {
        Arc::new(BscChainSpec::from(
            reth_chainspec::ChainSpecBuilder::mainnet()
                .with_fork(crate::hardforks::bsc::BscHardfork::Luban, reth_chainspec::ForkCondition::Block(0))
                .build(),
        ))
    }

    /// A non-epoch header (block 1, so `assemble_vote_attestation` skips — it only assembles from
    /// block 3 onward) carrying a fake attestation in `extra_data`: Vanity (32) + Attestation (RLP)
    /// + Seal (65). This exercises the pure strip-and-reseal path without needing a vote pool.
    fn header_with_fake_attestation() -> Header {
        use crate::consensus::parlia::vote::{VoteAttestation, VoteData};
        let att = VoteAttestation {
            vote_address_set: 0xFF,
            agg_signature: Default::default(),
            data: VoteData { source_number: 1, source_hash: B256::ZERO, target_number: 0, target_hash: B256::ZERO },
            extra: bytes::Bytes::new(),
        };
        let mut extra = vec![0u8; crate::consensus::parlia::EXTRA_VANITY_LEN];
        extra.extend_from_slice(alloy_rlp::encode(&att).as_ref());
        extra.extend_from_slice(&[0u8; crate::consensus::parlia::EXTRA_SEAL_LEN]);
        Header { number: 1, extra_data: alloy_primitives::Bytes::from(extra), ..Default::default() }
    }

    fn bid_block_with_sidecar(header: Header, sidecar_block_hash: B256) -> RecoveredBlock<BscBlock> {
        let body = crate::node::primitives::BscBlockBody {
            inner: reth_ethereum_primitives::BlockBody {
                transactions: Vec::new(),
                ommers: Vec::new(),
                withdrawals: None,
            },
            sidecars: Some(vec![crate::node::primitives::BscBlobTransactionSidecar {
                inner: BlobTransactionSidecar {
                    blobs: Vec::new(),
                    commitments: Vec::new(),
                    proofs: Vec::new(),
                },
                block_number: header.number,
                block_hash: sidecar_block_hash,
                tx_index: 0,
                tx_hash: B256::ZERO,
                version: 0,
            }]),
        };
        RecoveredBlock::new_unhashed(BscBlock { header, body }, Vec::new())
    }

    #[test]
    fn refresh_and_reseal_strips_stale_attestation_and_patches_sidecar_hash() {
        ensure_bid_block_test_signer();
        let chain_spec = luban_chain_spec();
        let parlia = Arc::new(Parlia::new(chain_spec, 200));
        let parent_snap = Snapshot::new(vec![Address::with_last_byte(1)], 0, B256::random(), 200, None);
        let parent_header = Header { number: 0, parent_hash: B256::random(), ..Default::default() };

        let header = header_with_fake_attestation();
        let original_hash = header.hash_slow();
        let block = bid_block_with_sidecar(header, original_hash);

        let snapshot_provider: Arc<dyn crate::consensus::parlia::SnapshotProvider + Send + Sync> =
            Arc::new(MockBidBlockSnapshotProvider { snapshot: parent_snap.clone() });

        let refreshed = refresh_and_reseal_bid_block(
            parlia,
            &parent_snap,
            &parent_header,
            &snapshot_provider,
            &block,
            0,
        );

        // The stale attestation was stripped (block 1 is below the vote-assembly floor of 3, so
        // no new attestation replaces it) and the header re-sealed — vanity + seal only, and a
        // different hash than the admission-time seal.
        assert_eq!(
            refreshed.header().extra_data.len(),
            crate::consensus::parlia::EXTRA_VANITY_LEN + crate::consensus::parlia::EXTRA_SEAL_LEN
        );
        assert_ne!(refreshed.hash(), original_hash);

        // The sidecar's cached block_hash (set at admission time, before this refresh) must be
        // patched to the new post-reseal hash before the block goes out over P2P.
        let sidecar = refreshed.body().sidecars.as_ref().unwrap().first().unwrap();
        assert_eq!(sidecar.block_hash, refreshed.hash());
    }

    #[test]
    fn refresh_and_reseal_is_a_noop_pre_luban() {
        ensure_bid_block_test_signer();
        // Mainnet spec without the Luban fork override: inactive at block 1.
        let chain_spec = Arc::new(BscChainSpec::from(reth_chainspec::ChainSpecBuilder::mainnet().build()));
        let parlia = Arc::new(Parlia::new(chain_spec, 200));
        let parent_snap = Snapshot::new(vec![Address::with_last_byte(1)], 0, B256::random(), 200, None);
        let parent_header = Header { number: 0, parent_hash: B256::random(), ..Default::default() };

        let header = header_with_fake_attestation();
        let original_hash = header.hash_slow();
        let block = bid_block_with_sidecar(header, original_hash);

        let snapshot_provider: Arc<dyn crate::consensus::parlia::SnapshotProvider + Send + Sync> =
            Arc::new(MockBidBlockSnapshotProvider { snapshot: parent_snap.clone() });

        let refreshed = refresh_and_reseal_bid_block(
            parlia,
            &parent_snap,
            &parent_header,
            &snapshot_provider,
            &block,
            0,
        );

        // Pre-Luban, refresh_vote_attestation_and_seal returns early without touching the header
        // at all — the block (and its sidecar's block_hash) must come back byte-for-byte unchanged.
        assert_eq!(refreshed.hash(), original_hash);
        let sidecar = refreshed.body().sidecars.as_ref().unwrap().first().unwrap();
        assert_eq!(sidecar.block_hash, original_hash);
    }
}
