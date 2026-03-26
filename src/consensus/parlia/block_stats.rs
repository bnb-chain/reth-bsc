//! Per-block statistics cache for chain delay metrics.
//!
//! Mirrors geth's `BlockStats` / `reportRecentBlocksLoop` functionality by tracking
//! block timestamps and event timestamps to compute chain delay metrics.

use alloy_primitives::B256;
use lru::LruCache;
use once_cell::sync::Lazy;
use std::{num::NonZero, sync::RwLock};

use crate::metrics::BscChainDelayMetrics;

/// Size of the block stats LRU cache.
const BLOCK_STATS_CACHE_SIZE: usize = 64;

/// Default majority vote threshold (matches geth's `defaultMajorityThreshold`).
const DEFAULT_MAJORITY_THRESHOLD: usize = 14;

/// Per-block tracking data for chain delay metrics.
struct BlockStat {
    /// Block timestamp in milliseconds (header.timestamp * 1000).
    block_timestamp_ms: i64,
    /// Whether the first vote delay has been reported.
    first_vote_reported: bool,
    /// Whether the majority vote delay has been reported.
    majority_vote_reported: bool,
}

static BLOCK_STATS: Lazy<RwLock<LruCache<B256, BlockStat>>> =
    Lazy::new(|| RwLock::new(LruCache::new(NonZero::new(BLOCK_STATS_CACHE_SIZE).unwrap())));

static CHAIN_DELAY_METRICS: Lazy<BscChainDelayMetrics> = Lazy::new(BscChainDelayMetrics::default);

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

/// Register a block's timestamp when it is first received from the network.
/// Also records the `chain.delay.block_recv` metric (delay from block creation to reception).
pub fn on_block_received(block_hash: B256, block_timestamp_secs: u64) {
    let block_ts_ms = block_timestamp_secs as i64 * 1000;
    let recv_time = now_ms();

    let delay_ms = recv_time - block_ts_ms;
    if delay_ms >= 0 {
        CHAIN_DELAY_METRICS.block_recv.record(delay_ms as f64);
    }

    let mut cache = BLOCK_STATS.write().expect("block stats poisoned");
    cache.get_or_insert(block_hash, || BlockStat {
        block_timestamp_ms: block_ts_ms,
        first_vote_reported: false,
        majority_vote_reported: false,
    });
}

/// Called when a vote is added for a block. Records first-vote and majority-vote delay
/// metrics when the respective thresholds are crossed.
///
/// `vote_count` is the *new* total number of votes for this block (after the vote was added).
pub fn on_vote_received(block_hash: B256, vote_count: usize) {
    let recv_time = now_ms();

    let mut cache = BLOCK_STATS.write().expect("block stats poisoned");
    let stat = match cache.get_mut(&block_hash) {
        Some(s) => s,
        None => return, // Block not yet received from network; can't compute delay.
    };

    // First vote
    if vote_count == 1 && !stat.first_vote_reported {
        let delay_ms = recv_time - stat.block_timestamp_ms;
        if delay_ms >= 0 {
            CHAIN_DELAY_METRICS.vote_first.record(delay_ms as f64);
        }
        stat.first_vote_reported = true;
    }

    // Majority vote
    if vote_count >= DEFAULT_MAJORITY_THRESHOLD && !stat.majority_vote_reported {
        let delay_ms = recv_time - stat.block_timestamp_ms;
        if delay_ms >= 0 {
            CHAIN_DELAY_METRICS.vote_majority.record(delay_ms as f64);
        }
        stat.majority_vote_reported = true;
    }
}
