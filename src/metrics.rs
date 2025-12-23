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

    /// Bid simulation duration in seconds (equivalent to bid/sim/duration)
    pub bid_simulation_duration_seconds: Histogram,

    /// First bid simulation time in seconds (equivalent to bid/sim/sim1stBid)
    pub first_bid_simulation_seconds: Histogram,

    /// Greedy merge execution duration in seconds
    pub greedy_merge_duration_seconds: Histogram,

    /// Total number of bid wins (when the final payload is from a bid)
    pub bid_win_total: Counter,
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

    /// Total number of blocks produced
    pub blocks_produced_total: Counter,

    /// Block broadcast delay in nanoseconds (time from block timestamp to broadcast time)
    /// This measures how long it takes from block creation to network broadcast
    /// Note: Value is stored in nanoseconds to match Golang implementation
    pub block_broadcast_delay_seconds: Histogram,
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
