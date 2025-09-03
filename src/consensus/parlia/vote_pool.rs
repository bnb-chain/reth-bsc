use once_cell::sync::Lazy;
use std::{collections::{BinaryHeap, HashMap, HashSet}, sync::RwLock, cmp::Reverse};

use alloy_primitives::{BlockNumber, B256};

use super::vote::{VoteData, VoteEnvelope};

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
            } else {
                break;
            }
        }
    }
}

/// Global singleton pool.
static VOTE_POOL: Lazy<RwLock<VotePool>> = Lazy::new(|| RwLock::new(VotePool::new()));

/// Insert a single vote into the pool (deduplicated by hash).
pub fn put_vote(vote: VoteEnvelope) {
    VOTE_POOL.write().expect("vote pool poisoned").insert(vote);
}

/// Drain all pending votes.
pub fn drain() -> Vec<VoteEnvelope> {
    VOTE_POOL.write().expect("vote pool poisoned").drain()
}

/// Current number of queued votes.
pub fn len() -> usize { 
    VOTE_POOL.read().expect("vote pool poisoned").len() 
}

/// Fetch votes by block hash.
pub fn fetch_vote_by_block_hash(block_hash: B256) -> Vec<VoteEnvelope> {
    VOTE_POOL.read().expect("vote pool poisoned").fetch_vote_by_block_hash(block_hash)
}

/// Prune old votes based on the latest block number.
pub fn prune(latest_block_number: BlockNumber) {
    VOTE_POOL.write().expect("vote pool poisoned").prune(latest_block_number)
}


