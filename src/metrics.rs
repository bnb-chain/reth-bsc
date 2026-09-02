//! BSC-specific metrics definitions
//!
//! This module contains all metrics specific to BSC functionality,
//! including consensus, execution, and network protocol metrics.

use metrics::Histogram;
use reth_metrics::{
    metrics::{Counter, Gauge},
    Metrics,
};

/// Metrics for the BSC block executor
///
/// Tracks execution-related metrics including block processing,
/// system contract calls, and execution errors.
#[derive(Metrics, Clone)]
#[metrics(scope = "bsc.executor")]
pub struct BscExecutorMetrics {
    /// Total number of blocks executed
    pub executed_blocks_total: Counter,

    /// Total number of block execution errors
    pub execution_errors_total: Counter,

    /// Total number of system contract calls
    pub system_contract_calls_total: Counter,

    /// Total number of system contract execution errors
    pub system_contract_errors_total: Counter,

    /// System contract execution duration in seconds
    pub system_contract_duration_seconds: Histogram,

    /// Total gas used in system transactions
    pub system_tx_gas_used_total: Counter,
}

/// Metrics for the BSC Parlia consensus engine
///
/// Tracks consensus-related metrics including validator operations,
/// block difficulty, and validation.
#[derive(Metrics, Clone)]
#[metrics(scope = "bsc.consensus")]
pub struct BscConsensusMetrics {
    /// Total number of in-turn blocks produced
    pub inturn_blocks_total: Counter,

    /// Total number of out-of-turn blocks produced
    pub noturn_blocks_total: Counter,

    /// Current block height (equivalent to chain/head/block)
    pub current_block_height: Gauge,

    /// Total number of double signs detected (equivalent to parlia/doublesign)
    pub double_signs_detected_total: Counter,

    /// Total number of intentional mining delays (equivalent to parlia/intentionalDelayMining)
    pub intentional_mining_delays_total: Counter,

    /// Total number of bad blocks detected (equivalent to chain/insert/badBlock)
    pub bad_blocks_total: Counter,
}

/// Metrics for BSC reward distribution
///
/// Tracks validator rewards and fee distribution.
#[derive(Metrics, Clone)]
#[metrics(scope = "bsc.rewards")]
pub struct BscRewardsMetrics {
    /// Total number of validator reward distributions
    pub validator_rewards_distributed_total: Counter,

    /// Total amount of validator rewards in wei
    pub validator_rewards_amount_wei_total: Counter,

    /// Total number of system reward distributions
    pub system_rewards_distributed_total: Counter,

    /// Total amount of system rewards in wei
    pub system_rewards_amount_wei_total: Counter,
}

/// Metrics for BSC vote attestation
///
/// Tracks vote attestation operations and BLS signature verification.
#[derive(Metrics, Clone)]
#[metrics(scope = "bsc.vote")]
pub struct BscVoteMetrics {
    /// Total number of votes attested (assembled into blocks)
    pub votes_attested_total: Counter,

    /// Total number of vote attestation errors (equivalent to parlia/verifyVoteAttestation/error)
    pub vote_attestation_errors_total: Counter,

    /// Total number of attestation update errors (equivalent to parlia/updateAttestation/error)
    pub attestation_update_errors_total: Counter,

    /// Total number of BLS signature verifications
    pub bls_verifications_total: Counter,

    /// Total number of BLS verification failures
    pub bls_verification_failures_total: Counter,

    /// BLS signature verification duration in seconds
    pub bls_verification_duration_seconds: Histogram,

    /// Current size of vote pool
    pub vote_pool_size: Gauge,

    /// Total number of vote signing errors (when producing votes)
    pub vote_signing_errors_total: Counter,

    /// Total number of vote journal persist errors
    pub vote_journal_errors_total: Counter,

    /// Current number of votes in the vote pool (equivalent to curVotes/local)
    pub current_votes_count: Gauge,

    /// Total number of votes received locally (cumulative)
    pub received_votes_total: Counter,
}

/// Metrics for BSC MEV operations
///
/// Tracks MEV-related operations including bid submissions and simulations.
#[derive(Metrics, Clone)]
#[metrics(scope = "bsc.mev")]
pub struct BscMevMetrics {
    /// Total number of valid MEV bids
    pub valid_bids_total: Counter,

    /// Total number of invalid MEV bids
    pub invalid_bids_total: Counter,

    /// Current number of pending MEV bids (equivalent to worker/bidExist)
    pub pending_bids: Gauge,

    /// Best bid gas used in MGas (equivalent to worker/bestBidGasUsed)
    pub best_bid_gas_used_mgas: Gauge,

    /// Bid simulation speed in MGas/s (equivalent to bid/sim/simulateSpeed)
    pub bid_simulation_speed_mgasps: Gauge,

    /// Bid simulation duration in seconds (equivalent to bid/sim/duration). Only records
    /// simulations that ran to **completion** — aborted ones (preempted, or rejected for blob/gas
    /// limits) return before this is observed, so its `_count` series is not the total number of
    /// simulations. Use [`Self::bid_simulation_started_total`] for that.
    pub bid_simulation_duration_seconds: Histogram,

    /// Total bid simulations **started**, counted at the single point every simulation passes
    /// through, before it can succeed or abort. This is the denominator the interrupt-thrash ratio
    /// is defined against ("占总模拟次数比例"): unlike
    /// `bid_simulation_duration_seconds_count` it includes aborted runs, and unlike that histogram
    /// series it carries no unit ambiguity — `_sum` on a `_seconds` histogram is seconds, whereas
    /// this is a plain count.
    pub bid_simulation_started_total: Counter,

    /// First bid simulation time in seconds (equivalent to bid/sim/sim1stBid)
    pub first_bid_simulation_seconds: Histogram,

    /// Greedy merge execution duration in seconds
    pub greedy_merge_duration_seconds: Histogram,

    /// Total number of bid wins (when the final payload is from a bid)
    pub bid_win_total: Counter,

    /// Total in-flight bid simulations preempted by a higher-value bid, i.e. every time
    /// `canBeInterrupted` allowed a new bid through. Counted in **simulations**. Denominator for
    /// the interrupt-thrash ratio that `no_interrupt_left_over` tuning is judged against.
    pub bid_interrupt_total: Counter,

    /// Of those preempted simulations, how many belonged to a block that then went on to seal
    /// without any bid — the interrupts bought nothing and the node fell back to a local payload.
    /// Counted in **simulations** too (a block that thrashed six times contributes 6, not 1), so
    /// dividing by [`Self::bid_interrupt_total`] yields a true proportion: the share of gambles
    /// that were wasted. A tightened `no_interrupt_left_over` window that kills good simulations it
    /// cannot replace in time shows up here and nowhere else.
    pub bid_interrupt_wasted_total: Counter,

    /// The subset of [`Self::bid_interrupt_wasted_total`] attributable specifically to the
    /// replacement simulation being **too late**: at seal time a simulation for this block was
    /// still in flight. Counted in **simulations**, on the same basis as the other two, so
    /// `late / wasted` is the share of wasted interrupts caused by the window being too tight
    /// (versus the replacement finishing but scoring worse or being rejected — real waste, but not
    /// evidence against `no_interrupt_left_over`).
    ///
    /// Attribution caveat: at most one simulation per parent runs at a time, so "still in flight"
    /// is a single observation about the block. Crediting the block's whole tally to lateness
    /// mirrors how `bid_interrupt_wasted_total` is weighted; in a multi-interrupt cascade the
    /// earlier replacements did finish (only to be preempted in turn).
    pub bid_interrupt_late_total: Counter,
}

/// Metrics for the BEP-675 gRPC ingress.
#[derive(Metrics, Clone)]
#[metrics(scope = "bsc.mev.grpc")]
pub struct BscMevGrpcMetrics {
    /// Total `SendBidBlock` requests that reached the gRPC handler.
    pub requests_total: Counter,

    /// Total gRPC BidBlock requests rejected by decoding or MEV admission.
    pub errors_total: Counter,

    /// Total requests rejected because the process-wide concurrency budget was exhausted.
    pub rejected_total: Counter,

    /// Current number of gRPC BidBlock requests holding an admission permit.
    pub active_requests: Gauge,

    /// RLP BidBlock decode duration in seconds.
    pub decode_duration_seconds: Histogram,

    /// End-to-end handler duration in seconds.
    pub handler_duration_seconds: Histogram,

    /// Submitted RLP payload size in bytes.
    pub payload_size_bytes: Histogram,
}

/// Metrics for BSC miner/worker operations
///
/// Tracks block production and finalization metrics.
#[derive(Metrics, Clone)]
#[metrics(scope = "bsc.miner")]
pub struct BscMinerMetrics {
    /// Best work gas used in MGas (equivalent to worker/bestWorkGasUsed)
    pub best_work_gas_used_mgas: Gauge,

    /// Block finalize duration in seconds (equivalent to worker/finalizeblock)
    pub block_finalize_duration_seconds: Histogram,

    /// Block execution duration in seconds (tx selection + execution; empty blocks include pre-exec changes).
    pub block_exec_duration_seconds: Histogram,

    /// Trie root computation duration in seconds (time spent in `finish()` after execution).
    pub block_trie_root_duration_seconds: Histogram,

    /// Total number of empty-payload candidates produced via the empty-fallback path.
    ///
    /// This is incremented when the payload job receives a completed build with
    /// `BuildKind::EmptyFallback`.
    pub empty_fallback_candidates_total: Counter,

    /// Total number of times we ultimately failed to pick/send a best payload (block production failed).
    pub no_best_payload_total: Counter,

    /// Total number of blocks produced
    pub blocks_produced_total: Counter,

    /// Block broadcast delay in nanoseconds (time from block timestamp to broadcast time)
    /// This measures how long it takes from block creation to network broadcast
    /// Note: Value is stored in nanoseconds to match Golang implementation
    pub block_broadcast_delay_seconds: Histogram,

    /// Total number of local payload rebuilds that were actually queued
    pub payload_rebuilds_attempted_total: Counter,

    /// Total number of rebuild evaluations skipped because cooldown had not elapsed
    pub payload_rebuilds_skipped_cooldown_total: Counter,

    /// Total number of rebuild evaluations skipped because estimated uplift was too low
    pub payload_rebuilds_skipped_value_total: Counter,

    /// Total number of rebuild evaluations skipped because there was not enough time left
    pub payload_rebuilds_skipped_time_total: Counter,

    /// Total number of near-deadline final-shot rebuilds used
    pub payload_rebuilds_final_shot_total: Counter,

    /// Current estimated uplift over the last local payload, in basis points
    pub payload_rebuild_estimated_uplift_bps: Gauge,
}

/// Metrics for BSC fast finality
///
/// Tracks fast finality operations and finalized blocks.
#[derive(Metrics, Clone)]
#[metrics(scope = "bsc.finality")]
pub struct BscFinalityMetrics {
    /// Current finalized block height (equivalent to chain/head/finalized)
    pub finalized_block_height: Gauge,

    /// Current justified block height (equivalent to chain/head/justified)
    pub justified_block_height: Gauge,

    /// Current safe block height (equivalent to chain/head/safe)
    pub safe_block_height: Gauge,

    /// Early finalization latency in milliseconds (BEP-648 fast path via VotePool).
    /// Measures the time from the finalized block's millisecond timestamp to when the
    /// forkchoice update is triggered, equivalent to chain/finalized/latency/early in geth.
    pub finalized_latency_early_ms: Gauge,

    /// Normal finalization latency in milliseconds (attestation assembly path).
    /// Measures the time from the source block's millisecond timestamp to when the
    /// miner successfully assembles a vote attestation confirming it as finalized.
    /// Equivalent to chain/finalized/latency/normal in geth (measured at assembly
    /// time rather than import time, so slightly earlier in the pipeline).
    pub finalized_latency_normal_ms: Gauge,
}

/// Metrics for BSC blockchain operations
///
/// Tracks blockchain-level metrics including receipts, block processing,
/// transaction sizes, and chain reorganizations.
///
/// Note: For gas-related metrics, use reth's ExecutorMetrics:
/// - `sync.execution.gas_used_histogram` for gas usage distribution
/// - `sync.execution.gas_per_second` for throughput (can convert to MGas/s by dividing by 1M)
/// - `sync.execution.execution_duration` for execution timing
#[derive(Metrics, Clone)]
#[metrics(scope = "bsc.blockchain")]
pub struct BscBlockchainMetrics {
    /// Current receipt height (equivalent to chain/head/receipt)
    pub current_receipt_height: Gauge,

    /// Block receive time difference in seconds
    pub block_receive_time_diff_seconds: Gauge,

    /// Size of transactions in the current block in bytes (equivalent to chain/insert/txsize)
    pub block_tx_size_bytes: Gauge,

    /// Total number of chain reorganizations executed (equivalent to chain/reorg/executes)
    pub reorg_executions_total: Counter,

    /// Total number of blocks added during reorganizations (equivalent to chain/reorg/add)
    pub reorg_blocks_added_total: Counter,

    /// Total number of blocks dropped during reorganizations (equivalent to chain/reorg/drop)
    pub reorg_blocks_dropped_total: Counter,

    /// Depth of the latest chain reorganization
    pub latest_reorg_depth: Gauge,
}

/// Chain delay metrics matching geth `chain/delay/*` metrics.
///
/// These track the delay between block creation (timestamp) and various events.
/// Values are recorded in milliseconds to match geth's behavior.
#[derive(Metrics, Clone)]
#[metrics(scope = "chain.delay")]
pub struct BscChainDelayMetrics {
    /// Delay from block timestamp to when block was first received from network
    pub block_recv: Histogram,
    /// Delay from block timestamp to first vote received for the block
    pub vote_first: Histogram,
    /// Delay from block timestamp to majority (14+) votes received for the block
    pub vote_majority: Histogram,
}

/// Parlia consensus metrics with geth-compatible naming.
///
/// Separated from `BscConsensusMetrics` to match geth's `parlia/*` metric namespace.
#[derive(Metrics, Clone)]
#[metrics(scope = "parlia")]
pub struct BscParliaGethMetrics {
    /// Double sign events detected (matches geth `parlia/doublesign`)
    pub doublesign: Counter,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metrics_initialization() {
        // Test that metrics can be initialized without panicking
        let _executor_metrics = BscExecutorMetrics::default();
        let _consensus_metrics = BscConsensusMetrics::default();
        let _rewards_metrics = BscRewardsMetrics::default();
        let _vote_metrics = BscVoteMetrics::default();
        let _mev_metrics = BscMevMetrics::default();
        let _miner_metrics = BscMinerMetrics::default();
        let _finality_metrics = BscFinalityMetrics::default();
        let _blockchain_metrics = BscBlockchainMetrics::default();
    }
}
