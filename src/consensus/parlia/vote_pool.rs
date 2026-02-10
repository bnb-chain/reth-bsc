use once_cell::sync::Lazy;
use std::{
    collections::{BinaryHeap, HashMap, HashSet},
    cmp::Reverse,
    sync::{RwLock, OnceLock},
};
use std::time::SystemTime;

use alloy_primitives::{BlockNumber, B256};

use super::vote::{VoteData, VoteEnvelope};
use crate::consensus::parlia::{consensus::FINALITY_METRICS, util::calculate_millisecond_timestamp};
use crate::metrics::BscVoteMetrics;
use crate::shared::{
    get_snapshot_provider,
};
use crate::node::evm::util::get_header_by_hash_from_cache;
use tokio::sync::broadcast;
use tokio::task;

const LOWER_LIMIT_OF_VOTE_BLOCK_NUMBER: u64 = 256;

/// Container for votes associated with a specific block hash.
#[derive(Default)]
struct VoteMessages {
    vote_messages: Vec<VoteEnvelope>,
}

/// Priority queue wrapper for vote data, ordered by target_number (ascending).
#[derive(Default)]
struct VotesPriorityQueue {
    heap: BinaryHeap<Reverse<VoteData>>,
}

impl VotesPriorityQueue {
    fn new() -> Self {
        Self { heap: BinaryHeap::new() }
    }

    fn push(&mut self, vote_data: VoteData) {
        self.heap.push(Reverse(vote_data));
    }

    fn pop(&mut self) -> Option<VoteData> {
        self.heap.pop().map(|Reverse(data)| data)
    }

    fn peek(&self) -> Option<&VoteData> {
        self.heap.peek().map(|Reverse(data)| data)
    }
}

impl PartialOrd for VoteData {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for VoteData {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.target_number.cmp(&other.target_number)
    }
}

/// Global in-memory pool of incoming Parlia votes.
///
/// This mirrors the simple approach used by the slashing pool: keep votes in
/// memory until they're consumed by another component. Votes are de-duplicated
/// by their RLP hash and organized by block hash.
struct VotePool {
    /// Hashes of votes we've already seen in this window.
    received_votes: HashSet<B256>,
    /// Collected votes organized by block hash.
    cur_votes: HashMap<B256, VoteMessages>,
    /// Priority queue for efficiently finding votes to prune.
    cur_votes_pq: VotesPriorityQueue,
    /// Blocks that have already triggered a quorum event to avoid duplicate broadcasts.
    notified_quorum_blocks: HashSet<B256>,
}

impl VotePool {
    fn new() -> Self {
        Self { 
            received_votes: HashSet::new(), 
            cur_votes: HashMap::new(),
            cur_votes_pq: VotesPriorityQueue::new(),
            notified_quorum_blocks: HashSet::new(),
        }
    }

    fn insert(&mut self, vote: VoteEnvelope) {
        let vote_hash = vote.hash();
        if self.received_votes.insert(vote_hash) {
            // Track received votes count
            VOTE_METRICS.received_votes_total.increment(1);
            
            // Use target_hash as the key for organizing votes
            let block_hash = vote.data.target_hash;
            
            // Add to priority queue if this is a new block
            if !self.cur_votes.contains_key(&block_hash) {
                self.cur_votes_pq.push(vote.data);
            }
            
            self.cur_votes.entry(block_hash).or_default().vote_messages.push(vote);
        }
    }

    fn drain(&mut self) -> Vec<VoteEnvelope> {
        self.received_votes.clear();
        self.cur_votes_pq = VotesPriorityQueue::new();
        let mut all_votes = Vec::new();
        for (_, vote_messages) in self.cur_votes.drain() {
            all_votes.extend(vote_messages.vote_messages);
        }
        all_votes
    }

    fn len(&self) -> usize { 
        self.cur_votes.values().map(|vm| vm.vote_messages.len()).sum() 
    }

    fn fetch_vote_by_block_hash(&self, block_hash: B256) -> Vec<VoteEnvelope> {
        if let Some(vote_messages) = self.cur_votes.get(&block_hash) {
            vote_messages.vote_messages.clone()
        } else {
            Vec::new()
        }
    }

    /// Prune old votes based on the latest block number.
    /// Removes votes where targetNumber + LOWER_LIMIT_OF_VOTE_BLOCK_NUMBER - 1 < latestBlockNumber
    fn prune(&mut self, latest_block_number: BlockNumber) {
        // Remove votes in the range [, latestBlockNumber - LOWER_LIMIT_OF_VOTE_BLOCK_NUMBER]
        while let Some(vote_data) = self.cur_votes_pq.peek() {
            if vote_data.target_number + LOWER_LIMIT_OF_VOTE_BLOCK_NUMBER - 1 < latest_block_number {
                // Remove from priority queue
                let vote_data = self.cur_votes_pq.pop().unwrap();
                let block_hash = vote_data.target_hash;
                
                // Remove from votes map and received_votes set
                if let Some(vote_box) = self.cur_votes.remove(&block_hash) {
                    for vote in vote_box.vote_messages {
                        let vote_hash = vote.hash();
                        self.received_votes.remove(&vote_hash);
                    }
                }
                // Also clear notified state so the same block hash can be reused in future epochs if necessary.
                self.notified_quorum_blocks.remove(&block_hash);
            } else {
                break;
            }
        }
    }
}

/// Global singleton pool.
static VOTE_POOL: Lazy<RwLock<VotePool>> = Lazy::new(|| RwLock::new(VotePool::new()));

/// Global metrics for vote operations.
static VOTE_METRICS: Lazy<BscVoteMetrics> = Lazy::new(BscVoteMetrics::default);

/// Event emitted when a block's collected votes reach the quorum (2/3).
#[derive(Clone, Debug)]
pub struct FastAttestationEvent {
    pub justified_hash: B256,
    pub justified_number: BlockNumber,
    pub finalized_hash: B256,
    pub finalized_number: BlockNumber,
    pub vote_cnt: usize,
    pub quorum_cnt: usize,
}

/// Global broadcast sender for vote quorum events.
static VOTE_QUORUM_EVENTS_TX: OnceLock<broadcast::Sender<FastAttestationEvent>> = OnceLock::new();

/// Install the global vote quorum events sender.
pub fn set_vote_quorum_events_tx(
    tx: broadcast::Sender<FastAttestationEvent>,
) -> Result<(), broadcast::Sender<FastAttestationEvent>> {
    VOTE_QUORUM_EVENTS_TX.set(tx)
}

/// Subscribe to vote quorum events if initialized.
pub fn subscribe_vote_quorum_events() -> Option<broadcast::Receiver<FastAttestationEvent>> {
    VOTE_QUORUM_EVENTS_TX.get().map(|tx| tx.subscribe())
}

/// Update vote pool size metric.
fn update_vote_pool_size_metric(size: usize) {
    VOTE_METRICS.vote_pool_size.set(size as f64);
    VOTE_METRICS.current_votes_count.set(size as f64);
}

/// Insert a single vote into the pool (deduplicated by hash).
pub fn put_vote(vote: VoteEnvelope) {
    // Capture target data before moving the vote.
    let target_hash = vote.data.target_hash;

    let mut pool = VOTE_POOL.write().expect("vote pool poisoned");
    pool.insert(vote.clone());

    // Schedule an async quorum check to avoid blocking this call.
    // We intentionally do NOT hold the lock during expensive IO/DB operations.
    if !pool.notified_quorum_blocks.contains(&target_hash) {
        task::spawn(async move {
            if let Err(e) = check_fast_atteatation(vote).await {
                tracing::warn!(target: "bsc::consensus::parlia::vote_pool", "Failed to check fast attestation: error = {:?}", e);
            }
        });
    }

    let size = pool.len();
    drop(pool);
    update_vote_pool_size_metric(size);
    
}

async fn check_fast_atteatation(vote: VoteEnvelope) -> Result<(), eyre::Error> {
    tracing::debug!(target: "bsc::consensus::parlia::vote_pool", "Checking fast attestation for vote: {:?}", vote);
    let target_hash = vote.data.target_hash;
    let target_number = vote.data.target_number;
    let source_hash = vote.data.source_hash;
    let source_number = vote.data.source_number;
    // Fetch header and parent snapshot in a blocking-friendly context.
    let snap_provider = get_snapshot_provider().
        ok_or(eyre::eyre!("Failed to get snapshot provider by hash: {}", target_hash))?;
    let snap = snap_provider.snapshot_by_hash(&target_hash).
        ok_or(eyre::eyre!("Failed to get snapshot by hash: {}", target_hash))?;

    // TODO: here only check the justified number, and skip checking the current header.
    // We only need to check currentJustifiedNumber + 1, since currentJustifiedNumber is already the latest justified.
    if snap.vote_data.target_number != target_number - 1 { 
        tracing::debug!(target: "bsc::consensus::parlia::vote_pool", "snapshot target number is not the previous block number in fast attestation: vote={:?}, parent_snap_target_number={}", vote, snap.vote_data.target_number);
        return Ok(());
    }


    let validators_len = snap.validators.len();
    let quorum_cnt = usize::div_ceil(validators_len * 2, 3);

    // Read current votes count.
    let vote_cnt = {
        let guard = VOTE_POOL.read().expect("vote pool poisoned");
        guard
            .cur_votes
            .get(&target_hash)
            .map(|vm| vm.vote_messages.len())
            .unwrap_or(0)
    };

    if vote_cnt < quorum_cnt {
        tracing::debug!(target: "bsc::consensus::parlia::vote_pool", "Votes count is less than quorum in fast attestation: vote={:?}, votes_count={}, quorum={}", vote, vote_cnt, quorum_cnt);
        return Ok(());
    }

    // Double-check and broadcast under write lock to avoid duplicate sends.
    let mut guard = VOTE_POOL.write().expect("vote pool poisoned");
    if guard.notified_quorum_blocks.contains(&target_hash) {
        return Ok(());
    }
    guard.notified_quorum_blocks.insert(target_hash);
    drop(guard);

    // record fast attestation finality duration in vote pool
    let finalized_header = get_header_by_hash_from_cache(&source_hash).
        ok_or(eyre::eyre!("Failed to get header by hash: {}", source_hash))?;
    let finalized_ms = calculate_millisecond_timestamp(&finalized_header) as u128;
    let now_ms = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    FINALITY_METRICS
        .fast_atteatation_finality_duration_ms
        .record(now_ms.abs_diff(finalized_ms) as f64);
   

    let event = FastAttestationEvent {
        justified_hash: target_hash,
        justified_number: target_number,
        finalized_hash: source_hash,
        finalized_number: source_number,
        vote_cnt,
        quorum_cnt,
    };
    let tx = VOTE_QUORUM_EVENTS_TX.get().
        ok_or(eyre::eyre!("Vote quorum event channel not initialized"))?;
    let _ = tx.send(event.clone());
    tracing::debug!(
        target: "parlia::vote",
        "quorum reached for block {}, number={}, votes={}, quorum={}",
        target_hash, target_number, vote_cnt, quorum_cnt
    );
    Ok(())
}

/// Drain all pending votes.
pub fn drain() -> Vec<VoteEnvelope> {
    let votes = VOTE_POOL.write().expect("vote pool poisoned").drain();
    update_vote_pool_size_metric(0);
    votes
}

/// Current number of queued votes.
pub fn len() -> usize { 
    VOTE_POOL.read().expect("vote pool poisoned").len() 
}

/// Check if the pool is empty.
pub fn is_empty() -> bool {
    len() == 0
}

/// Fetch votes by block hash.
pub fn fetch_vote_by_block_hash(block_hash: B256) -> Vec<VoteEnvelope> {
    VOTE_POOL.read().expect("vote pool poisoned").fetch_vote_by_block_hash(block_hash)
}

/// Prune old votes based on the latest block number.
pub fn prune(latest_block_number: BlockNumber) {
    let mut pool = VOTE_POOL.write().expect("vote pool poisoned");
    pool.prune(latest_block_number);
    let size = pool.len();
    drop(pool);
    update_vote_pool_size_metric(size);
}


