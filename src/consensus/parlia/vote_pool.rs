use lru::LruCache;
use once_cell::sync::Lazy;
use std::{
    cmp::Reverse,
    collections::{BinaryHeap, HashMap, HashSet},
    num::NonZero,
    sync::RwLock,
};

use alloy_primitives::{BlockNumber, B256};

use super::vote::{VoteData, VoteEnvelope};
use crate::metrics::BscVoteMetrics;
use crate::shared;

const LOWER_LIMIT_OF_VOTE_BLOCK_NUMBER: u64 = 256;
/// Size of the LRU cache for tracking finality notifications (matches geth's finalizedNotified)
const FINALIZED_NOTIFIED_CACHE_SIZE: usize = 21;

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
}

impl VotePool {
    fn new() -> Self {
        Self {
            received_votes: HashSet::new(),
            cur_votes: HashMap::new(),
            cur_votes_pq: VotesPriorityQueue::new(),
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

    fn get_votes(&self) -> Vec<VoteEnvelope> {
        let mut all_votes = Vec::new();
        for vote_messages in self.cur_votes.values() {
            all_votes.extend(vote_messages.vote_messages.clone());
        }
        all_votes
    }

    fn len(&self) -> usize {
        self.cur_votes.values().map(|vm| vm.vote_messages.len()).sum()
    }

    fn len_for_block(&self, block_hash: &B256) -> usize {
        self.cur_votes.get(block_hash).map(|vm| vm.vote_messages.len()).unwrap_or(0)
    }

    fn fetch_vote_by_block_hash(&self, block_hash: B256) -> Vec<VoteEnvelope> {
        if let Some(vote_messages) = self.cur_votes.get(&block_hash) {
            vote_messages.vote_messages.clone()
        } else {
            Vec::new()
        }
    }

    fn fetch_vote_by_block_hash_and_source_number(
        &self,
        block_hash: B256,
        source_number: BlockNumber,
    ) -> Vec<VoteEnvelope> {
        self.fetch_vote_by_block_hash(block_hash)
            .into_iter()
            .filter(|vote| vote.data.source_number == source_number)
            .collect()
    }

    /// Prune old votes based on the latest block number.
    /// Removes votes where targetNumber + LOWER_LIMIT_OF_VOTE_BLOCK_NUMBER - 1 < latestBlockNumber
    fn prune(&mut self, latest_block_number: BlockNumber) {
        // Remove votes in the range [, latestBlockNumber - LOWER_LIMIT_OF_VOTE_BLOCK_NUMBER]
        while let Some(vote_data) = self.cur_votes_pq.peek() {
            if vote_data.target_number + LOWER_LIMIT_OF_VOTE_BLOCK_NUMBER - 1 < latest_block_number
            {
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

/// LRU cache to track which blocks have already been notified for finality.
/// This prevents repeated update_forkchoice calls for the same block (matches geth's finalizedNotified).
static FINALIZED_NOTIFIED: Lazy<RwLock<LruCache<B256, ()>>> =
    Lazy::new(|| RwLock::new(LruCache::new(NonZero::new(FINALIZED_NOTIFIED_CACHE_SIZE).unwrap())));

/// Update vote pool size metric.
fn update_vote_pool_size_metric(size: usize) {
    VOTE_METRICS.vote_pool_size.set(size as f64);
    VOTE_METRICS.current_votes_count.set(size as f64);
}

/// Insert a single vote into the pool (deduplicated by hash).
pub fn put_vote(vote: VoteEnvelope) {
    let target_hash = vote.data.target_hash;
    let mut pool = VOTE_POOL.write().expect("vote pool poisoned");
    pool.insert(vote);
    let votes_for_block = pool.len_for_block(&target_hash);
    let size = pool.len();
    drop(pool);
    update_vote_pool_size_metric(size);
    maybe_notify_finality(target_hash, votes_for_block);
}

/// Drain all pending votes.
pub fn drain() -> Vec<VoteEnvelope> {
    let votes = VOTE_POOL.write().expect("vote pool poisoned").drain();
    update_vote_pool_size_metric(0);
    votes
}

/// Snapshot all pending votes without removing them.
pub fn get_votes() -> Vec<VoteEnvelope> {
    VOTE_POOL.read().expect("vote pool poisoned").get_votes()
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

/// Fetch votes by block hash and source block number.
pub fn fetch_vote_by_block_hash_and_source_number(
    block_hash: B256,
    source_number: BlockNumber,
) -> Vec<VoteEnvelope> {
    VOTE_POOL
        .read()
        .expect("vote pool poisoned")
        .fetch_vote_by_block_hash_and_source_number(block_hash, source_number)
}

/// Prune old votes based on the latest block number.
pub fn prune(latest_block_number: BlockNumber) {
    let mut pool = VOTE_POOL.write().expect("vote pool poisoned");
    pool.prune(latest_block_number);
    let size = pool.len();
    drop(pool);
    update_vote_pool_size_metric(size);
}

fn maybe_notify_finality(target_hash: B256, votes_for_block: usize) {
    // Check if we've already notified for this block (de-duplication)
    {
        let cache = FINALIZED_NOTIFIED.read().expect("finalized notified cache poisoned");
        if cache.peek(&target_hash).is_some() {
            return;
        }
    }

    let head_number = match shared::get_best_canonical_block_number() {
        Some(number) => number,
        None => return,
    };
    let head = match shared::get_canonical_header_by_number(head_number) {
        Some(header) => header,
        None => return,
    };
    if head.hash_slow() != target_hash {
        return;
    }

    let sp = match shared::get_snapshot_provider() {
        Some(provider) => provider,
        None => return,
    };
    let snap = match sp.snapshot_by_hash(&target_hash) {
        Some(snap) => snap,
        None => return,
    };
    if snap.validators.is_empty() {
        return;
    }

    let current_justified_number = snap.vote_data.target_number;
    if head.number == 0 || head.number - 1 != current_justified_number {
        return;
    }

    let quorum = usize::div_ceil(snap.validators.len() * 2, 3);
    if votes_for_block < quorum {
        return;
    }

    let eligible_votes = fetch_vote_by_block_hash(target_hash)
        .into_iter()
        .filter(|vote| {
            vote.data.source_number == current_justified_number
                && vote.data.target_number == head.number
        })
        .count();

    if eligible_votes < quorum {
        return;
    }

    // Mark as notified before sending to avoid duplicate notifications
    {
        let mut cache = FINALIZED_NOTIFIED.write().expect("finalized notified cache poisoned");
        cache.put(target_hash, ());
    }

    if let Some(engine) = shared::get_fork_choice_engine() {
        tokio::spawn(async move {
            let _ = engine.update_forkchoice(&head).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::consensus::parlia::vote::{VoteAddress, VoteData, VoteEnvelope, VoteSignature};
    use alloy_primitives::B256;

    fn vote_with_source(target_hash: B256, source_number: u64, unique: u8) -> VoteEnvelope {
        let mut address = VoteAddress::default();
        address[0] = unique;
        let mut signature = VoteSignature::default();
        signature[0] = unique;
        VoteEnvelope {
            vote_address: address,
            signature,
            data: VoteData {
                source_number,
                source_hash: B256::from([source_number as u8; 32]),
                target_number: 100,
                target_hash,
            },
        }
    }

    #[test]
    fn fetch_votes_filters_by_source_number() {
        // Ensure global pool has a clean state across tests.
        let _ = drain();

        let target_hash = B256::from([0x11; 32]);
        let other_target_hash = B256::from([0x22; 32]);

        put_vote(vote_with_source(target_hash, 10, 1));
        put_vote(vote_with_source(target_hash, 11, 2));
        put_vote(vote_with_source(other_target_hash, 10, 3));

        let all_for_target = fetch_vote_by_block_hash(target_hash);
        assert_eq!(all_for_target.len(), 2);

        let source_10 = fetch_vote_by_block_hash_and_source_number(target_hash, 10);
        assert_eq!(source_10.len(), 1);
        assert_eq!(source_10[0].data.source_number, 10);

        let source_11 = fetch_vote_by_block_hash_and_source_number(target_hash, 11);
        assert_eq!(source_11.len(), 1);
        assert_eq!(source_11[0].data.source_number, 11);

        let source_12 = fetch_vote_by_block_hash_and_source_number(target_hash, 12);
        assert!(source_12.is_empty());

        let _ = drain();
    }
}
