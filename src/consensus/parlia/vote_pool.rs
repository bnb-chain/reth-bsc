use lru::LruCache;
use once_cell::sync::Lazy;
use std::{
    cmp::Reverse,
    collections::{BinaryHeap, HashMap, HashSet},
    num::NonZero,
    sync::{
        atomic::{AtomicU64, Ordering},
        RwLock,
    },
};

use alloy_primitives::{BlockNumber, B256};

use super::{
    block_stats,
    malicious_vote_monitor::MaliciousVoteMonitor,
    vote::{VoteData, VoteEnvelope},
};
use crate::consensus::parlia::util::calculate_millisecond_timestamp;
use crate::metrics::{BscFinalityMetrics, BscVoteMetrics};
use crate::shared;
use std::time::SystemTime;

const LOWER_LIMIT_OF_VOTE_BLOCK_NUMBER: u64 = 256;
/// How far above our head a vote may target, mirroring go-bsc's
/// `upperLimitOfVoteBlockNumber` (itself derived from `fetcher.maxUncleDist`).
pub(crate) const UPPER_LIMIT_OF_VOTE_BLOCK_NUMBER: u64 = 11;
/// Envelope hashes to remember as rejected. ~32 B each, so a few hundred KB.
const REJECTED_VOTE_CACHE_SIZE: usize = 8192;
/// Size of the LRU cache for tracking finality notifications (matches geth's finalizedNotified)
const FINALIZED_NOTIFIED_CACHE_SIZE: usize = 21;

#[derive(Clone)]
struct VoteEntry {
    hash: B256,
    envelope: VoteEnvelope,
}

/// Container for votes associated with a specific block hash.
#[derive(Default)]
struct VoteMessages {
    vote_messages: Vec<VoteEntry>,
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
    /// Total number of votes stored in the pool.
    total_votes: usize,
    /// Malicious vote monitor for detecting rule violations.
    malicious_vote_monitor: MaliciousVoteMonitor,
}

impl VotePool {
    fn new() -> Self {
        Self {
            received_votes: HashSet::new(),
            cur_votes: HashMap::new(),
            cur_votes_pq: VotesPriorityQueue::new(),
            total_votes: 0,
            malicious_vote_monitor: MaliciousVoteMonitor::new(),
        }
    }

    /// Insert a vote and return the new vote count for its target block (0 if duplicate).
    fn insert(&mut self, vote: VoteEnvelope, pending_block_number: BlockNumber) -> usize {
        let vote_hash = vote.hash();
        if self.received_votes.insert(vote_hash) {
            // Track received votes count (geth-compatible)
            VOTE_METRICS.received_votes_total.increment(1);
            metrics::counter!("curVotes.local").increment(1);

            // Check for malicious votes
            self.malicious_vote_monitor.conflict_detect(&vote, pending_block_number);

            // Use target_hash as the key for organizing votes
            let block_hash = vote.data.target_hash;

            // Add to priority queue if this is a new block
            if !self.cur_votes.contains_key(&block_hash) {
                self.cur_votes_pq.push(vote.data);
            }
            self.cur_votes
                .entry(block_hash)
                .or_default()
                .vote_messages
                .push(VoteEntry { hash: vote_hash, envelope: vote });
            self.total_votes += 1;

            // Update geth-compatible gauges
            metrics::gauge!("curVotesPq.local").set(self.cur_votes_pq.heap.len() as f64);
            metrics::gauge!("receivedVotes.local").set(self.received_votes.len() as f64);

            // Return the new vote count for this block
            self.len_for_block(&block_hash)
        } else {
            0 // duplicate vote
        }
    }

    fn drain(&mut self) -> Vec<VoteEnvelope> {
        self.received_votes.clear();
        self.cur_votes_pq = VotesPriorityQueue::new();
        self.total_votes = 0;
        let mut all_votes = Vec::new();
        for (_, vote_messages) in self.cur_votes.drain() {
            all_votes.extend(vote_messages.vote_messages.into_iter().map(|entry| entry.envelope));
        }
        // Update geth-compatible gauges
        metrics::gauge!("curVotesPq.local").set(0.0);
        metrics::gauge!("receivedVotes.local").set(0.0);
        all_votes
    }

    fn get_votes(&self) -> Vec<VoteEnvelope> {
        let mut all_votes = Vec::new();
        for vote_messages in self.cur_votes.values() {
            all_votes.extend(vote_messages.vote_messages.iter().map(|entry| entry.envelope.clone()));
        }
        all_votes
    }

    fn len(&self) -> usize {
        self.total_votes
    }

    /// Whether this vote hash is already pooled.
    fn contains(&self, vote_hash: &B256) -> bool {
        self.received_votes.contains(vote_hash)
    }

    fn len_for_block(&self, block_hash: &B256) -> usize {
        self.cur_votes.get(block_hash).map(|vm| vm.vote_messages.len()).unwrap_or(0)
    }

    fn fetch_vote_by_block_hash(&self, block_hash: B256) -> Vec<VoteEnvelope> {
        if let Some(vote_messages) = self.cur_votes.get(&block_hash) {
            vote_messages
                .vote_messages
                .iter()
                .map(|entry| entry.envelope.clone())
                .collect()
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
                    self.total_votes = self.total_votes.saturating_sub(vote_box.vote_messages.len());
                    for vote in vote_box.vote_messages {
                        self.received_votes.remove(&vote.hash);
                    }
                }
            } else {
                break;
            }
        }
        // Update geth-compatible gauges after pruning
        metrics::gauge!("curVotesPq.local").set(self.cur_votes_pq.heap.len() as f64);
        metrics::gauge!("receivedVotes.local").set(self.received_votes.len() as f64);
    }
}

/// Global singleton pool.
static VOTE_POOL: Lazy<RwLock<VotePool>> = Lazy::new(|| RwLock::new(VotePool::new()));

/// Highest block number against which the pool has already been pruned.
/// Throttles [`put_vote`]'s lazy prune to once per observed head advance.
static LAST_PRUNED_BLOCK: AtomicU64 = AtomicU64::new(0);

/// Envelope hashes that already failed verification.
///
/// `received_votes` only records votes that were *admitted*, so without this an
/// attacker could replay one invalid envelope indefinitely and buy a fresh
/// pairing with each copy. Bounded, so the cache itself cannot be grown into a
/// memory problem; eviction only costs a repeated verification.
static REJECTED_VOTES: Lazy<RwLock<LruCache<B256, ()>>> =
    Lazy::new(|| RwLock::new(LruCache::new(NonZero::new(REJECTED_VOTE_CACHE_SIZE).unwrap())));

/// Global metrics for vote operations.
static VOTE_METRICS: Lazy<BscVoteMetrics> = Lazy::new(BscVoteMetrics::default);

/// Global metrics for finality operations (shared with consensus layer).
static FINALITY_METRICS: Lazy<BscFinalityMetrics> = Lazy::new(BscFinalityMetrics::default);

/// LRU cache to track which blocks have already been notified for finality.
/// This prevents repeated update_forkchoice calls for the same block (matches geth's finalizedNotified).
static FINALIZED_NOTIFIED: Lazy<RwLock<LruCache<B256, ()>>> =
    Lazy::new(|| RwLock::new(LruCache::new(NonZero::new(FINALIZED_NOTIFIED_CACHE_SIZE).unwrap())));

/// Update vote pool size metric.
fn update_vote_pool_size_metric(size: usize) {
    VOTE_METRICS.vote_pool_size.set(size as f64);
    VOTE_METRICS.current_votes_count.set(size as f64);
}

/// Whether a vote targeting `target_number` falls inside the admission window
/// `(head - 256, head + 11]`, matching go-bsc's `putIntoVotePool`.
///
/// Without an upper bound a peer can park votes for arbitrarily distant future
/// heights in the pool, where nothing prunes them: `prune` only evicts by the
/// lower bound, so far-future entries are unreachable by it.
///
/// `head` is `None` before the canonical-head accessor is registered during
/// startup. Votes are admitted in that case rather than rejected: treating an
/// unknown head as height 0 would discard every vote whose target exceeds 11.
fn is_within_admission_window(target_number: BlockNumber, head: Option<BlockNumber>) -> bool {
    let Some(head) = head else {
        return true;
    };
    // Saturating throughout: `target_number` is attacker-supplied, and a plain
    // add would overflow-panic in debug builds on a crafted value.
    if target_number.saturating_add(LOWER_LIMIT_OF_VOTE_BLOCK_NUMBER - 1) < head {
        return false;
    }
    target_number <= head.saturating_add(UPPER_LIMIT_OF_VOTE_BLOCK_NUMBER)
}

/// Insert a single vote into the pool (deduplicated by hash).
///
/// The vote's BLS signature is authenticated first: pool contents drive both
/// finality notification and vote-attestation assembly, and votes reach here
/// straight off the wire with nothing else checking them. Mirrors go-bsc's
/// `basicVerify` -> `VoteEnvelope.Verify` (`core/vote/vote_pool.go`).
pub fn put_vote(vote: VoteEnvelope) {
    // Height window first of all: it costs nothing, while everything below costs
    // at least a hash. go-bsc orders it ahead of verification the same way in
    // `putIntoVotePool`.
    if !is_within_admission_window(vote.data.target_number, shared::get_best_canonical_block_number())
    {
        metrics::counter!("votes.rejected.out_of_range").increment(1);
        tracing::debug!(
            target: "bsc::vote_pool",
            target_number = vote.data.target_number,
            head = ?shared::get_best_canonical_block_number(),
            "rejecting vote outside the (head-256, head+11] admission window",
        );
        return;
    }

    // Verification is a pairing, and votes arrive unsolicited from any peer, so
    // do the cheap exclusions first. Votes are gossiped, meaning the same
    // envelope reaches us from every peer that has it: verifying before
    // deduplicating buys one pairing per copy for a single useful vote. go-bsc
    // orders these the same way in `basicVerify`.
    let vote_hash = vote.hash();
    if VOTE_POOL.read().expect("vote pool poisoned").contains(&vote_hash) {
        metrics::counter!("votes.duplicate").increment(1);
        return;
    }
    // An envelope that already failed verification cannot start passing, so a
    // replay of it need not be re-verified. Without this, one invalid envelope
    // can be resent indefinitely to buy CPU.
    if REJECTED_VOTES.read().expect("rejected vote cache poisoned").peek(&vote_hash).is_some() {
        metrics::counter!("votes.rejected.replay").increment(1);
        return;
    }

    VOTE_METRICS.bls_verifications_total.increment(1);
    let started = std::time::Instant::now();
    let verified = crate::consensus::parlia::bls_signer::verify_vote_envelope(&vote);
    VOTE_METRICS.bls_verification_duration_seconds.record(started.elapsed().as_secs_f64());

    if let Err(e) = verified {
        VOTE_METRICS.bls_verification_failures_total.increment(1);
        REJECTED_VOTES.write().expect("rejected vote cache poisoned").put(vote_hash, ());
        tracing::debug!(
            target: "bsc::vote_pool",
            vote_address = %vote.vote_address,
            target_number = vote.data.target_number,
            error = %e,
            "rejecting vote with invalid BLS signature",
        );
        return;
    }

    put_vote_inner(vote);
}

/// Test-only ingress that skips signature verification, for tests exercising
/// pool/finality bookkeeping with synthetic vote addresses.
#[cfg(test)]
pub fn put_vote_unchecked(vote: VoteEnvelope) {
    put_vote_inner(vote);
}

fn put_vote_inner(vote: VoteEnvelope) {
    let target_hash = vote.data.target_hash;

    // Get pending block number for malicious vote detection scope
    let pending_block_number = shared::get_best_canonical_block_number().unwrap_or(0);

    // Lazy prune: evict votes below `head - LOWER_LIMIT_OF_VOTE_BLOCK_NUMBER`
    // once per observed head advance. Replaces geth-bsc's chain-head event
    // subscription by piggybacking on the vote ingest path (same cadence).
    // `fetch_max` keeps the watermark monotonic across racing writers; the
    // inner prune is O(0) when nothing is stale.
    let need_prune = pending_block_number > LAST_PRUNED_BLOCK.load(Ordering::Relaxed);

    let target_number = vote.data.target_number;

    let mut pool = VOTE_POOL.write().expect("vote pool poisoned");
    let votes_for_block = pool.insert(vote, pending_block_number);
    if need_prune {
        pool.prune(pending_block_number);
        LAST_PRUNED_BLOCK.fetch_max(pending_block_number, Ordering::Relaxed);
    }

    // Force prune if pool is too large, prevents memory issues during stage sync.
    const MAX_VOTES_IN_POOL: usize = 32 * 1024 * 2;
    if pool.len() > MAX_VOTES_IN_POOL {
        let force_prune = target_number.saturating_sub(LOWER_LIMIT_OF_VOTE_BLOCK_NUMBER);
        pool.prune(force_prune);
        tracing::debug!(
            target: "bsc::vote_pool",
            pool_size = pool.len(),
            force_prune_block_number = force_prune,
            "Vote pool oversized, force pruned"
        );
    }

    let size = pool.len();
    drop(pool);
    update_vote_pool_size_metric(size);

    // Report chain delay vote metrics
    if votes_for_block > 0 {
        block_stats::on_vote_received(target_hash, votes_for_block);
        maybe_notify_finality(target_hash, votes_for_block);
    }
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

    // Record early finalization latency: time from the finalized block's millisecond
    // timestamp to now, equivalent to chain/finalized/latency/early in geth.
    // The finalized block is current_justified (head - 1), identified by current_justified_number.
    if let Some(justified_header) = shared::get_canonical_header_by_number(current_justified_number) {
        let now_ms = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let block_ms = calculate_millisecond_timestamp(&justified_header);
        let latency_ms = now_ms.saturating_sub(block_ms) as f64;
        FINALITY_METRICS.finalized_latency_early_ms.set(latency_ms);
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
    use crate::consensus::parlia::bls_signer::random_test_signer;
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

        // Synthetic vote addresses/signatures: bypass BLS verification, which is
        // covered separately by `put_vote_rejects_invalid_signature`.
        put_vote_unchecked(vote_with_source(target_hash, 10, 1));
        put_vote_unchecked(vote_with_source(target_hash, 11, 2));
        put_vote_unchecked(vote_with_source(other_target_hash, 10, 3));

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

    /// A vote reaching the pool is authenticated: pool contents drive finality
    /// notification and attestation assembly, and nothing between the wire and
    /// here checks them. See go-bsc `basicVerify` -> `VoteEnvelope.Verify`.
    #[test]
    fn put_vote_rejects_unauthenticated_votes() {
        let _ = drain();

        let signer = random_test_signer();

        let data = VoteData {
            source_number: 10,
            source_hash: B256::from([0xaa; 32]),
            target_number: 100,
            target_hash: B256::from([0xbb; 32]),
        };
        let genuine = signer.sign_vote(data).expect("sign vote");

        // Baseline: a correctly signed vote is admitted.
        put_vote(genuine.clone());
        assert_eq!(fetch_vote_by_block_hash(data.target_hash).len(), 1, "genuine vote rejected");

        // Undecodable signature under a real validator's address. Before
        // verification existed this reached attestation assembly and panicked
        // in `Signature::from_bytes(..).unwrap()`.
        put_vote(VoteEnvelope {
            signature: VoteSignature::from([0x42u8; 96]),
            ..genuine.clone()
        });
        assert_eq!(
            fetch_vote_by_block_hash(data.target_hash).len(),
            1,
            "vote with undecodable signature was admitted",
        );

        // Well-formed signature by the same key, but over different vote data:
        // decodes cleanly, so only real verification catches it.
        let other = VoteData { target_number: 101, ..data };
        let mismatched = signer.sign_vote(other).expect("sign vote").signature;
        put_vote(VoteEnvelope { signature: mismatched, ..genuine.clone() });
        assert_eq!(
            fetch_vote_by_block_hash(data.target_hash).len(),
            1,
            "vote signed over different data was admitted",
        );

        // Signature valid in isolation, but attributed to another validator.
        put_vote(VoteEnvelope {
            vote_address: VoteAddress::from([0x07u8; 48]),
            ..genuine
        });
        assert_eq!(
            fetch_vote_by_block_hash(data.target_hash).len(),
            1,
            "vote under a mismatched vote address was admitted",
        );

        let _ = drain();
    }


    /// Verification is gated behind two cheap exclusions, so unsolicited traffic
    /// cannot buy unbounded pairings.
    ///
    /// Votes are gossiped, so the same envelope arrives from every peer holding
    /// it; and an envelope that failed verification can be replayed forever.
    /// Neither should cost more than one verification in total.
    #[test]
    fn repeated_envelopes_are_verified_at_most_once() {
        let _ = drain();

        let signer = random_test_signer();
        let data = VoteData {
            source_number: 800,
            source_hash: B256::from([0x81; 32]),
            target_number: 801,
            target_hash: B256::from([0x82; 32]),
        };
        let genuine = signer.sign_vote(data).expect("sign vote");

        // A valid vote, then replays of it: deduplicated by the pool.
        put_vote(genuine.clone());
        assert_eq!(fetch_vote_by_block_hash(data.target_hash).len(), 1);
        for _ in 0..10 {
            put_vote(genuine.clone());
        }
        assert_eq!(
            fetch_vote_by_block_hash(data.target_hash).len(),
            1,
            "re-relayed copies must not accumulate",
        );

        // An invalid envelope, then replays of it: remembered as rejected.
        let forged =
            VoteEnvelope { signature: VoteSignature::from([0x42u8; 96]), ..genuine.clone() };
        let forged_hash = forged.hash();
        put_vote(forged.clone());
        assert!(
            REJECTED_VOTES.read().unwrap().peek(&forged_hash).is_some(),
            "a failed envelope is remembered so replays skip the pairing",
        );
        for _ in 0..10 {
            put_vote(forged.clone());
        }
        assert_eq!(
            fetch_vote_by_block_hash(data.target_hash).len(),
            1,
            "replayed forgeries never enter the pool",
        );

        let _ = drain();
    }


    // === D2: (head-256, head+11] admission window ===

    #[test]
    fn admission_window_matches_go_bsc_bounds() {
        let head = Some(10_000u64);
        // go-bsc: reject when target+256-1 < head, i.e. target < head-255.
        assert!(!is_within_admission_window(9_744, head), "head-256 is outside");
        assert!(is_within_admission_window(9_745, head), "head-255 is the oldest admitted");
        assert!(is_within_admission_window(10_000, head), "head itself");
        assert!(is_within_admission_window(10_011, head), "head+11 is the newest admitted");
        assert!(!is_within_admission_window(10_012, head), "head+12 is outside");
    }

    /// Before the head accessor is registered we cannot place a vote, and
    /// treating an unknown head as 0 would discard everything above height 11.
    #[test]
    fn admission_window_admits_when_head_unknown() {
        assert!(is_within_admission_window(0, None));
        assert!(is_within_admission_window(40_000_000, None));
        assert!(is_within_admission_window(u64::MAX, None));
    }

    /// `target_number` is attacker-supplied. A plain `target + 256` or
    /// `head + 11` would overflow-panic in debug builds.
    #[test]
    fn admission_window_is_overflow_safe() {
        assert!(!is_within_admission_window(u64::MAX, Some(10_000)), "far future rejected");
        assert!(!is_within_admission_window(0, Some(u64::MAX)), "far past rejected");
        assert!(is_within_admission_window(u64::MAX, Some(u64::MAX)));
        // Would panic rather than return if the arithmetic were unchecked.
        assert!(!is_within_admission_window(u64::MAX - 1, Some(1)));
    }

    /// The window is wired into the ingest path, and its unknown-head guard is
    /// fail-open. No unit test can register the head provider, so `head` is
    /// always `None` here — the startup state worth pinning, since a fail-closed
    /// guard would silently discard every vote above height 11 until the
    /// provider appears.
    #[test]
    fn put_vote_admits_any_height_while_head_is_unknown() {
        let _ = drain();
        assert!(
            shared::get_best_canonical_block_number().is_none(),
            "precondition: no head provider is registered in unit tests",
        );

        let signer = random_test_signer();
        let data = VoteData {
            source_number: 39_999_999,
            source_hash: B256::from([0xcd; 32]),
            target_number: 40_000_000,
            target_hash: B256::from([0xce; 32]),
        };
        put_vote(signer.sign_vote(data).expect("sign vote"));

        assert_eq!(
            fetch_vote_by_block_hash(data.target_hash).len(),
            1,
            "an unknown head must not cause votes to be discarded",
        );

        let _ = drain();
    }

}
