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
/// Votes retained per target hash once we hold the target block, matching
/// go-bsc's `maxCurVoteAmountPerBlock`. One per validator suffices.
const MAX_CUR_VOTE_AMOUNT_PER_BLOCK: usize = 21;
/// Votes retained per target hash for future targets whose sender we managed to
/// authenticate, matching go-bsc's `maxFutureVoteAmountPerBlock`.
///
/// Applied *only* when `future_vote_sender_is_validator` returned `Some(true)`.
/// A cap over unauthenticatable contents is a censorship tool, not a safety
/// limit: a future vote is signature-checked and nothing more, and a signature
/// proves the signer holds the key in the envelope, not that the key belongs to
/// a validator. Capping such a bucket lets any peer mint keys, self-sign enough
/// envelopes to fill it, and have genuine validator votes refused —
/// permanently, since votes are broadcast once and never re-sent. Reported by
/// Hashdit Bot on #491.
///
/// NOTE: go-bsc applies its cap unconditionally. `basicVerify` uses
/// `maxFutureVoteAmountPerBlock` with only `vote.Verify()` behind it,
/// `VerifyVote` runs solely for current votes, and there is no per-peer
/// accounting for future votes in `core/vote/vote_pool.go`. Worth raising
/// upstream rather than assuming reth-bsc is the only client affected.
const MAX_FUTURE_VOTE_AMOUNT_PER_BLOCK: usize = 50;
/// Targets examined by one promotion pass.
///
/// Promotion runs on every import, so a bounded slice per pass still drains a
/// backlog quickly, while a flood cannot put unbounded work on the critical path
/// between importing a block and the next one. Whatever is left over is picked up
/// by the next pass.
const MAX_PROMOTION_TARGETS_PER_PASS: usize = 64;
/// Votes origin-checked by one promotion pass.
///
/// The per-target budget matters more than the per-pass one: future buckets whose
/// sender could not be authenticated are deliberately uncapped, so a single target
/// can hold tens of thousands of votes, each costing a provider read and two
/// snapshot reads to judge. `promote_judged` leaves the unjudged remainder future
/// and re-queues the target, so a large bucket drains across passes.
const MAX_PROMOTION_VOTES_PER_PASS: usize = 512;
/// Hard ceiling on pooled votes. Exceeding it triggers a prune and, failing
/// that, shedding of future votes.
const MAX_VOTES_IN_POOL: usize = 32 * 1024 * 2;
/// Size of the LRU cache for tracking finality notifications (matches geth's finalizedNotified)
const FINALIZED_NOTIFIED_CACHE_SIZE: usize = 21;

#[derive(Clone)]
struct VoteEntry {
    hash: B256,
    envelope: VoteEnvelope,
}

/// One future target considered for promotion, captured under the read lock.
///
/// `votes` may be a prefix of the target's bucket when the pass runs out of
/// budget; the remainder stays future and is judged by a later pass.
struct PromotionCandidate {
    data: VoteData,
    votes: Vec<VoteEntry>,
    /// More than `head - 11` behind, so judge it now and drop what fails rather
    /// than holding it forever.
    ///
    /// The bound is the *upper* admission limit, matching go-bsc's
    /// `transferVotesFromFutureToCur`, which sweeps
    /// `TargetNumber + upperLimitOfVoteBlockNumber < latestBlockNumber`
    /// unconditionally and lets `VerifyVote` drop what cannot be verified.
    /// Upstream never prunes `futureVotes` by the 256-block bound — this sweep is
    /// what clears them.
    expired: bool,
}

/// Result of moving one target out of the future pool.
struct PromotionOutcome {
    /// At least one vote survived the origin check and reached the current pool.
    promoted: bool,
    /// Votes without a verdict remain, so the target must stay queued.
    requeue: bool,
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
    /// Votes whose target block we do not hold yet, keyed by target hash.
    future_votes: HashMap<B256, VoteMessages>,
    /// Priority queue over `future_votes`, ordered by target number.
    future_votes_pq: VotesPriorityQueue,
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
            future_votes: HashMap::new(),
            future_votes_pq: VotesPriorityQueue::new(),
            total_votes: 0,
            malicious_vote_monitor: MaliciousVoteMonitor::new(),
        }
    }

    /// Whether this target hash already holds its maximum current votes.
    ///
    /// Applies to current votes only. Those have passed the origin check, so the
    /// cap can only ever refuse a vote we know to be surplus — one validator's
    /// vote per target is all that counts, and the cap sits at the validator
    /// count. Future votes are intentionally uncapped; see the note above the
    /// absent `MAX_FUTURE_VOTE_AMOUNT_PER_BLOCK`.
    fn is_at_capacity(&self, block_hash: &B256, is_future: bool, authenticated: bool) -> bool {
        if is_future {
            // Only cap a future bucket whose sender we authenticated; see the
            // note on MAX_FUTURE_VOTE_AMOUNT_PER_BLOCK.
            return authenticated
                && self
                    .future_votes
                    .get(block_hash)
                    .is_some_and(|vm| vm.vote_messages.len() >= MAX_FUTURE_VOTE_AMOUNT_PER_BLOCK);
        }
        self.cur_votes
            .get(block_hash)
            .is_some_and(|vm| vm.vote_messages.len() >= MAX_CUR_VOTE_AMOUNT_PER_BLOCK)
    }

    /// Insert a vote and return the new *current* vote count for its target
    /// block. Returns 0 for duplicates and for future votes, which must not
    /// drive finality notification until they are promoted.
    fn insert(
        &mut self,
        vote: VoteEnvelope,
        pending_block_number: BlockNumber,
        is_future: bool,
    ) -> usize {
        let vote_hash = vote.hash();
        if !self.received_votes.insert(vote_hash) {
            return 0; // duplicate vote
        }

        VOTE_METRICS.received_votes_total.increment(1);
        metrics::counter!(if is_future { "futureVotes.local" } else { "curVotes.local" })
            .increment(1);

        // Check for malicious votes
        self.malicious_vote_monitor.conflict_detect(&vote, pending_block_number);

        let block_hash = vote.data.target_hash;
        let vote_data = vote.data;
        {
            let (votes, pq) = if is_future {
                (&mut self.future_votes, &mut self.future_votes_pq)
            } else {
                (&mut self.cur_votes, &mut self.cur_votes_pq)
            };
            // Only push to the queue for a hash we are not already tracking, so
            // the queue holds one entry per target rather than one per vote.
            if !votes.contains_key(&block_hash) {
                pq.push(vote_data);
            }
            votes
                .entry(block_hash)
                .or_default()
                .vote_messages
                .push(VoteEntry { hash: vote_hash, envelope: vote });
        }
        self.total_votes += 1;

        metrics::gauge!("curVotesPq.local").set(self.cur_votes_pq.heap.len() as f64);
        metrics::gauge!("futureVotesPq.local").set(self.future_votes_pq.heap.len() as f64);
        metrics::gauge!("receivedVotes.local").set(self.received_votes.len() as f64);

        if is_future {
            0
        } else {
            self.len_for_block(&block_hash)
        }
    }

    fn drain(&mut self) -> Vec<VoteEnvelope> {
        self.received_votes.clear();
        self.cur_votes_pq = VotesPriorityQueue::new();
        self.future_votes_pq = VotesPriorityQueue::new();
        self.future_votes.clear();
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

    /// Promotes future votes whose target we now hold, mirroring go-bsc's
    /// `transferVotesFromFutureToCur`.
    ///
    /// Two phases, as upstream: entries older than `latest - 11` are promoted
    /// unconditionally (they can no longer be "future"), then entries at or
    /// below `latest` are promoted only once their target block is actually
    /// known, with the rest pushed back for a later pass.
    ///
    /// Returns the target hashes that gained current votes, so the caller can
    /// run finality notification after releasing the pool lock.
    /// Future entries whose target height we have reached, with their votes
    /// cloned out so the origin checks can run *without* the pool lock held.
    ///
    /// Read-only: nothing moves until `apply_promotion` runs with the verdicts.
    fn promotion_candidates(&self, latest: BlockNumber) -> Vec<PromotionCandidate> {
        let mut candidates = Vec::new();
        let mut vote_budget = MAX_PROMOTION_VOTES_PER_PASS;

        for Reverse(vd) in self.future_votes_pq.heap.iter() {
            if candidates.len() >= MAX_PROMOTION_TARGETS_PER_PASS || vote_budget == 0 {
                break;
            }
            if vd.target_number > latest {
                continue;
            }
            let votes: Vec<VoteEntry> = self
                .future_votes
                .get(&vd.target_hash)
                .map(|vm| vm.vote_messages.iter().take(vote_budget).cloned().collect())
                .unwrap_or_default();
            vote_budget -= votes.len();
            candidates.push(PromotionCandidate {
                expired: vd.target_number.saturating_add(UPPER_LIMIT_OF_VOTE_BLOCK_NUMBER) < latest,
                votes,
                data: *vd,
            });
        }

        candidates
    }

    /// Moves the targets in `resolved` into the current pool, using verdicts
    /// computed outside the lock. Targets absent from `resolved` are ones whose
    /// block we still do not hold; they stay future.
    fn apply_promotion(
        &mut self,
        latest: BlockNumber,
        resolved: &HashMap<B256, HashMap<B256, bool>>,
    ) -> Vec<B256> {
        let mut promoted = Vec::new();
        let mut deferred = Vec::new();

        while let Some(vd) = self.future_votes_pq.peek() {
            if vd.target_number > latest {
                break;
            }
            let vd = *vd;
            self.future_votes_pq.pop();
            match resolved.get(&vd.target_hash) {
                Some(verdicts) => {
                    let outcome = self.promote_judged(vd.target_hash, verdicts);
                    if outcome.promoted {
                        promoted.push(vd.target_hash);
                    }
                    // Re-queue outside the loop: pushing here would let the same
                    // entry be popped again on this pass, forever.
                    if outcome.requeue {
                        deferred.push(vd);
                    }
                }
                None => deferred.push(vd),
            }
        }

        for vd in deferred {
            self.future_votes_pq.push(vd);
        }
        metrics::gauge!("futureVotesPq.local").set(self.future_votes_pq.heap.len() as f64);
        promoted
    }

    /// Moves one target's future votes into the current pool, dropping those the
    /// origin check rejected. Returns whether anything survived, and whether the
    /// target must stay queued.
    ///
    /// Votes that arrived between the verdicts being taken and this write have no
    /// verdict of their own. They are left in the future pool and the target is
    /// re-queued, so the next pass judges them rather than this one guessing.
    fn promote_judged(
        &mut self,
        block_hash: B256,
        verdicts: &HashMap<B256, bool>,
    ) -> PromotionOutcome {
        let Some(box_) = self.future_votes.remove(&block_hash) else {
            return PromotionOutcome { promoted: false, requeue: false };
        };

        let mut valid = Vec::with_capacity(box_.vote_messages.len());
        let mut unjudged = Vec::new();
        for entry in box_.vote_messages {
            match verdicts.get(&entry.hash) {
                Some(true) => valid.push(entry),
                Some(false) => {
                    // Drop from the dedup set too, so a later legitimate copy is
                    // not mistaken for a duplicate.
                    self.received_votes.remove(&entry.hash);
                    self.total_votes = self.total_votes.saturating_sub(1);
                    metrics::counter!("votes.rejected.origin_on_promote").increment(1);
                }
                None => unjudged.push(entry),
            }
        }

        let requeue = !unjudged.is_empty();
        if requeue {
            self.future_votes.entry(block_hash).or_default().vote_messages.extend(unjudged);
        }

        if valid.is_empty() {
            return PromotionOutcome { promoted: false, requeue };
        }

        let data = valid[0].envelope.data;
        if !self.cur_votes.contains_key(&block_hash) {
            self.cur_votes_pq.push(data);
        }
        self.cur_votes.entry(block_hash).or_default().vote_messages.extend(valid);
        metrics::gauge!("curVotesPq.local").set(self.cur_votes_pq.heap.len() as f64);
        PromotionOutcome { promoted: true, requeue }
    }

    /// Drops future votes, furthest-ahead target first, until at least `target`
    /// entries have been released. Returns how many votes were dropped.
    ///
    /// The escape hatch for a flood that pruning cannot reach because every
    /// entry is still inside the admission window. Furthest-ahead first because
    /// those are the least likely to be promoted soon.
    fn shed_future_votes(&mut self, target: usize) -> usize {
        let mut order: Vec<VoteData> = self.future_votes_pq.heap.iter().map(|r| r.0).collect();
        order.sort_by_key(|vd| std::cmp::Reverse(vd.target_number));

        let mut shed = 0usize;
        for vd in order {
            if shed >= target {
                break;
            }
            if let Some(box_) = self.future_votes.remove(&vd.target_hash) {
                shed += box_.vote_messages.len();
                self.total_votes = self.total_votes.saturating_sub(box_.vote_messages.len());
                for entry in box_.vote_messages {
                    self.received_votes.remove(&entry.hash);
                }
            }
        }
        // Rebuild the queue over what survived.
        self.future_votes_pq = VotesPriorityQueue::new();
        let surviving: Vec<VoteData> = self
            .future_votes
            .values()
            .filter_map(|vm| vm.vote_messages.first().map(|e| e.envelope.data))
            .collect();
        for vd in surviving {
            self.future_votes_pq.push(vd);
        }
        metrics::gauge!("futureVotesPq.local").set(self.future_votes_pq.heap.len() as f64);
        shed
    }

    /// Prune old votes based on the latest block number.
    /// Removes votes where targetNumber + LOWER_LIMIT_OF_VOTE_BLOCK_NUMBER - 1 < latestBlockNumber
    fn prune(&mut self, latest_block_number: BlockNumber) {
        // Remove votes in the range [, latestBlockNumber - LOWER_LIMIT_OF_VOTE_BLOCK_NUMBER]
        while let Some(vote_data) = self.cur_votes_pq.peek() {
            // Saturating: the admission window bounds `target_number` to
            // `head + 11`, but it fails open while the head is unknown at
            // startup, so an extreme value can still reach the pool. A plain add
            // would then wrap in release and trap in debug.
            if vote_data.target_number.saturating_add(LOWER_LIMIT_OF_VOTE_BLOCK_NUMBER)
                <= latest_block_number
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
        // Future entries below the lower bound can never be promoted usefully.
        while let Some(vd) = self.future_votes_pq.peek() {
            if vd.target_number.saturating_add(LOWER_LIMIT_OF_VOTE_BLOCK_NUMBER)
                > latest_block_number
            {
                break;
            }
            let hash = vd.target_hash;
            self.future_votes_pq.pop();
            if let Some(box_) = self.future_votes.remove(&hash) {
                self.total_votes = self.total_votes.saturating_sub(box_.vote_messages.len());
                for entry in box_.vote_messages {
                    self.received_votes.remove(&entry.hash);
                }
            }
        }
        metrics::gauge!("futureVotesPq.local").set(self.future_votes_pq.heap.len() as f64);

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

/// Justified (source) pair recorded in a header's snapshot.
///
/// Shared so the vote pool and `BscForkChoiceEngine` derive it one way. The
/// Luban gate stays with the caller, which is where the chain spec lives.
///
/// Before the chain justifies anything the recorded pair is all zeroes, which
/// resolves to genesis — see `justified_pair_of`.
pub fn justified_pair_for_hash(header_hash: &B256) -> Option<(BlockNumber, B256)> {
    let sp = shared::get_snapshot_provider()?;
    let snap = sp.snapshot_by_hash(header_hash)?;
    justified_pair_of(&snap.vote_data, || {
        let genesis = shared::get_canonical_header_by_number(0)?;
        Some((genesis.number, genesis.hash_slow()))
    })
}

/// Which block a vote must cite as its source, given the attestation recorded in
/// a header's snapshot.
///
/// A zero target hash is the *absence* of an attestation, not a justified block
/// living at the zero hash. Nothing can ever cite that hash: our own producers
/// substitute genesis in this state (`vote_producer.rs`, and `Parlia::assemble_
/// vote_attestation`), and go-bsc's `GetJustifiedNumberAndHash` returns
/// `chain.GetHeaderByNumber(0).Hash()` whenever `snap.Attestation == nil`.
///
/// Reporting the zero hash here rejects every vote for source mismatch, and the
/// rejection is self-locking: leaving the state needs an attestation, and an
/// attestation can only be assembled from the votes being rejected. A fresh
/// all-reth network therefore never reaches finality at all. Raised by
/// will-2012 on #491.
///
/// `genesis` is lazy so a chain that has justified something never pays for the
/// lookup, and so the branch is unit-testable without a registered header
/// provider.
fn justified_pair_of(
    vote_data: &VoteData,
    genesis: impl FnOnce() -> Option<(BlockNumber, B256)>,
) -> Option<(BlockNumber, B256)> {
    if vote_data.target_hash == B256::ZERO {
        return genesis();
    }
    Some((vote_data.target_number, vote_data.target_hash))
}

/// Whether a *future* vote's sender is a validator, judged against the snapshot
/// at our own head.
///
/// `verify_vote_origin` cannot run on a future vote: it resolves membership from
/// the target's parent snapshot, and we do not hold the target. But the
/// validator set only changes at one block per epoch, and the admission window
/// caps a future target at `head + 11`, so the set at our head is the set that
/// will govern the target — unless that block falls in between.
///
/// Returns:
/// - `Some(true)`  sender is a validator in the current set
/// - `Some(false)` sender is not, and cannot become one inside the window
/// - `None` undecidable: no snapshot, or a validator-set swap lies in
///   `(head, target]` so the governing set may differ. Callers admit these
///   uncapped rather than guess.
fn future_vote_sender_is_validator(vote: &VoteEnvelope) -> Option<bool> {
    let head_number = shared::get_best_canonical_block_number()?;
    let head = shared::get_canonical_header_by_number(head_number)?;
    let snap = shared::get_snapshot_provider()?.snapshot_by_hash(&head.hash_slow())?;
    if snap.validators_map.is_empty() {
        return None;
    }

    // A validator-set swap between head and target changes the set out from
    // under us; decline to judge rather than risk rejecting an incoming
    // validator.
    if validator_set_swaps_within(
        head_number,
        vote.data.target_number,
        snap.epoch_num,
        snap.miner_history_check_len(),
    ) {
        return None;
    }

    Some(snap.validators_map.values().any(|v| v.vote_addr == vote.vote_address))
}

/// Whether a validator-set swap falls in `(head, target]`.
///
/// The set does **not** change at the epoch multiple: it changes `offset`
/// blocks later, where `offset` is `Snapshot::miner_history_check_len()`. That
/// is the `header.number % epoch_num == miner_check_len` test in
/// `SnapshotProvider::try_rebuild`, and it is the only place the set is
/// swapped. With 21 validators at `turn_length` 4 the offset is 43, so on a
/// 1000-block epoch the set turns over at `…043`, not `…000`.
///
/// Counting the swap points at or below each height and comparing the counts
/// catches a boundary wherever it sits in the window, and needs no special case
/// for a window that spans an epoch multiple.
fn validator_set_swaps_within(head: u64, target: u64, epoch: u64, offset: u64) -> bool {
    let epoch = epoch.max(1);
    // `n % epoch == offset` is unsatisfiable here, so the provider never treats
    // any block as a boundary. Mirror it rather than invent one.
    if offset >= epoch {
        return false;
    }
    let swaps_upto = |n: u64| if n >= offset { (n - offset) / epoch + 1 } else { 0 };
    swaps_upto(target) > swaps_upto(head)
}

/// Whether a vote plausibly originates from a validator of its target block and
/// cites the correct source, mirroring go-bsc's `Parlia.VerifyVote`.
///
/// Only meaningful once the target block is known; callers apply it to current
/// votes at admission and to future votes at promotion, exactly as upstream
/// does. Returns false when the target header or either snapshot is missing —
/// the same outcome go-bsc reaches by returning an error — but logs the two
/// cases separately, because "snapshot not available yet" and "vote is not from
/// a validator" have very different operational meanings.
fn verify_vote_origin(vote: &VoteEnvelope) -> OriginVerdict {
    let Some(header) = shared::get_canonical_header_by_hash_from_provider(&vote.data.target_hash)
    else {
        tracing::debug!(
            target: "bsc::vote_pool",
            target_number = vote.data.target_number,
            "vote origin unverifiable: target header not found",
        );
        return OriginVerdict::Unverifiable;
    };
    if header.number != vote.data.target_number {
        return OriginVerdict::Rejected;
    }

    match justified_pair_for_hash(&vote.data.target_hash) {
        Some((justified_number, justified_hash)) => {
            if vote.data.source_number != justified_number
                || vote.data.source_hash != justified_hash
            {
                metrics::counter!("votes.rejected.source_mismatch").increment(1);
                return OriginVerdict::Rejected;
            }
        }
        None => {
            tracing::debug!(
                target: "bsc::vote_pool",
                target_number = vote.data.target_number,
                "vote origin unverifiable: no snapshot for target",
            );
            return OriginVerdict::Unverifiable;
        }
    }

    let Some(sp) = shared::get_snapshot_provider() else {
        return OriginVerdict::Unverifiable;
    };
    let Some(parent_snap) = sp.snapshot_by_hash(&header.parent_hash) else {
        tracing::debug!(
            target: "bsc::vote_pool",
            target_number = vote.data.target_number,
            "vote origin unverifiable: no snapshot for target's parent",
        );
        return OriginVerdict::Unverifiable;
    };

    if parent_snap.validators_map.values().any(|v| v.vote_addr == vote.vote_address) {
        OriginVerdict::Ok
    } else {
        metrics::counter!("votes.rejected.not_a_validator").increment(1);
        OriginVerdict::Rejected
    }
}

/// Remember an envelope that can never become valid, so a replay is dropped at
/// `put_vote`'s cache check instead of paying for another BLS verification.
///
/// Only for verdicts that are properties of the envelope itself. Anything we
/// merely could not judge yet must stay out of here.
fn remember_rejected(vote_hash: B256) {
    REJECTED_VOTES.write().expect("rejected vote cache poisoned").put(vote_hash, ());
}

/// Outcome of an origin check.
///
/// `Rejected` and `Unverifiable` both keep a vote out, but they must not be
/// treated alike at admission: a rejection is a property of the envelope and
/// cannot change, while "unverifiable" means only that we lack the snapshot to
/// judge it yet. Caching the latter as rejected would blacklist a vote that a
/// later copy could have proven valid.
#[derive(Clone, Copy, PartialEq, Eq)]
enum OriginVerdict {
    Ok,
    /// The envelope contradicts the chain: wrong target height, wrong source
    /// pair, or a sender absent from the set that governs its target.
    Rejected,
    /// We cannot tell yet — the target's snapshot or the provider is missing.
    Unverifiable,
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

    put_vote_inner(vote, vote_hash);
}

/// Test-only ingress that skips signature verification and places the vote
/// directly into the current pool, for tests exercising pool and finality
/// bookkeeping with synthetic vote addresses.
///
/// Bypasses classification deliberately: no unit test can register the header
/// provider, so the real path would route everything to the future pool.
#[cfg(test)]
pub fn put_vote_unchecked(vote: VoteEnvelope) {
    let target_hash = vote.data.target_hash;
    let mut pool = VOTE_POOL.write().expect("vote pool poisoned");
    if pool.is_at_capacity(&target_hash, false, false) {
        return;
    }
    let votes_for_block = pool.insert(vote, 0, false);
    drop(pool);
    if votes_for_block > 0 {
        maybe_notify_finality(target_hash, votes_for_block);
    }
}

fn put_vote_inner(vote: VoteEnvelope, vote_hash: B256) {
    let target_hash = vote.data.target_hash;
    let target_number = vote.data.target_number;
    let pending_block_number = shared::get_best_canonical_block_number().unwrap_or(0);

    // Classify: a vote for a block we do not hold cannot have its origin checked
    // yet, because membership is resolved against the target's parent snapshot.
    // go-bsc splits `curVotes`/`futureVotes` on exactly this condition.
    //
    // We test canonical presence where go-bsc tests *verified* presence, which is
    // the stricter reading: a valid but not-yet-canonical target is treated as
    // future here. That defers its origin check to promotion rather than skipping
    // it, so the effect is conservative.
    //
    // Until the header provider is registered we cannot classify at all. The
    // network starts accepting peers in `build_network` while the provider is
    // registered later, in `build_consensus`, so that window is reachable by a
    // connected peer. Treat unclassifiable votes as future: they are then
    // uncapped (so they cannot crowd out validator votes), they do not reach
    // `maybe_notify_finality` (so they cannot manufacture quorum), and they are
    // fully origin-checked at promotion once the provider appears. Reported by
    // Hashdit Bot on #491.
    let can_classify = shared::has_header_by_hash_provider();
    let is_future = !can_classify
        || shared::get_canonical_header_by_hash_from_provider(&target_hash).is_none();

    // Future votes cannot be origin-checked (membership lives in the target's
    // parent snapshot, which we do not hold), but we can still ask whether the
    // sender is a validator *at all*, against our own head. An attacker's minted
    // key is in no validator set, so this refuses the junk before a bucket is
    // ever created. `None` means undecidable — admitted, but left uncapped.
    let future_sender_authenticated = if is_future {
        match future_vote_sender_is_validator(&vote) {
            Some(false) => {
                metrics::counter!("votes.rejected.future_not_a_validator").increment(1);
                tracing::debug!(
                    target: "bsc::vote_pool",
                    vote_address = %vote.vote_address,
                    target_number,
                    "rejecting future vote from a non-validator",
                );
                // The sender is in no validator set, and `future_vote_sender_is_
                // validator` only answers `Some` when no set change lies between
                // our head and the target — so this envelope cannot start
                // passing, and a replay must not cost another BLS verification.
                remember_rejected(vote_hash);
                return;
            }
            Some(true) => true,
            None => false,
        }
    } else {
        false
    };

    // Current votes are origin-checked at admission; future votes at promotion.
    if !is_future {
        match verify_vote_origin(&vote) {
            OriginVerdict::Ok => {}
            verdict => {
                tracing::debug!(
                    target: "bsc::vote_pool",
                    vote_address = %vote.vote_address,
                    target_number,
                    "rejecting vote that failed the origin check",
                );
                // Only a verdict about the envelope is cacheable. "Unverifiable"
                // means a snapshot was missing, which a later copy may not hit.
                if verdict == OriginVerdict::Rejected {
                    remember_rejected(vote_hash);
                }
                return;
            }
        }
    }

    // Lazy prune, once per observed head advance. Promotion is driven by block
    // import (`promote_future_votes`); the ingest path keeps calling it as a
    // backstop for the case where the fork-choice engine is not yet wired, but
    // *before* taking the write lock, since it does provider and snapshot reads.
    let need_head_work = pending_block_number > LAST_PRUNED_BLOCK.load(Ordering::Relaxed);
    if need_head_work {
        promote_future_votes(pending_block_number);
    }

    let mut pool = VOTE_POOL.write().expect("vote pool poisoned");

    if pool.is_at_capacity(&target_hash, is_future, future_sender_authenticated) {
        drop(pool);
        metrics::counter!("votes.rejected.block_at_capacity").increment(1);
        tracing::debug!(
            target: "bsc::vote_pool",
            target_number,
            is_future,
            "rejecting vote: target already at its per-block vote cap",
        );
        return;
    }

    let votes_for_block = pool.insert(vote, pending_block_number, is_future);

    if need_head_work {
        pool.prune(pending_block_number);
        LAST_PRUNED_BLOCK.fetch_max(pending_block_number, Ordering::Relaxed);
    }

    // Force prune if the pool is oversized.
    //
    // This used to prune relative to the *incoming* vote's target, which frees
    // nothing when the flood targets recent heights: pruning below
    // `target - 256` only evicts votes already far behind the window. Prune
    // relative to our head instead, and if that reclaims too little, shed
    // future votes.
    //
    // Shedding future votes is the right response because current votes cannot
    // be the cause: they are origin-checked and capped per target, so with the
    // 267-block admission window they are bounded at roughly
    // `267 * MAX_CUR_VOTE_AMOUNT_PER_BLOCK` entries. Any overflow is future
    // votes, which are the less trustworthy half by construction.
    if pool.len() > MAX_VOTES_IN_POOL {
        pool.prune(pending_block_number);
        let after_prune = pool.len();
        if after_prune > MAX_VOTES_IN_POOL {
            let shed = pool.shed_future_votes(after_prune - MAX_VOTES_IN_POOL);
            metrics::counter!("votes.shed.future_oversized").increment(shed as u64);
            tracing::warn!(
                target: "bsc::vote_pool",
                pool_size = pool.len(),
                shed,
                "vote pool oversized after pruning; shed future votes",
            );
        } else {
            tracing::debug!(
                target: "bsc::vote_pool",
                pool_size = after_prune,
                "vote pool oversized, pruned to head",
            );
        }
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

/// Promote future votes whose target block we now hold, and judge those that
/// have aged past the admission window.
///
/// Driven by **block import**, not by vote arrival. Promotion needs a *block*,
/// and the vote that would trigger it may never come: `put_vote` returns at its
/// dedup check, so a re-relayed copy is not an event, and a node that already
/// received every vote for `N` before importing `N` has nothing left to arrive
/// until a vote for `N+1` — which does not exist yet if we are the next
/// proposer. The votes then sit unpromoted through exactly the window where the
/// split was supposed to help: attestation assembly reads `cur_votes`, and so
/// does `get_finalized_number_and_hash`, so a full node loses its one-block
/// finalized lead the same way a validator loses the attestation.
///
/// go-bsc drives this off its `highestVerifiedBlock` event. The equivalent choke
/// point here is `BscForkChoiceEngine::update_forkchoice`, which every node
/// reaches on every import path. Raised by will-2012 on #491.
///
/// The provider and snapshot lookups run between the two locks, never under the
/// write lock that every incoming vote contends for.
pub fn promote_future_votes(head_number: BlockNumber) {
    // Phase 1 — read lock: what is eligible, and the envelopes to judge.
    let candidates = {
        let pool = VOTE_POOL.read().expect("vote pool poisoned");
        pool.promotion_candidates(head_number)
    };
    if candidates.is_empty() {
        return;
    }

    // Phase 2 — no lock held: the provider and snapshot reads.
    let mut resolved: HashMap<B256, HashMap<B256, bool>> = HashMap::new();
    for candidate in candidates {
        let target_hash = candidate.data.target_hash;
        if !candidate.expired
            && shared::get_canonical_header_by_hash_from_provider(&target_hash).is_none()
        {
            continue; // still future
        }
        resolved.insert(
            target_hash,
            candidate
                .votes
                .iter()
                .map(|entry| (entry.hash, verify_vote_origin(&entry.envelope) == OriginVerdict::Ok))
                .collect(),
        );
    }
    if resolved.is_empty() {
        return;
    }

    // Phase 3 — write lock: apply the verdicts.
    let promoted_counts: Vec<(B256, usize)> = {
        let mut pool = VOTE_POOL.write().expect("vote pool poisoned");
        let promoted = pool.apply_promotion(head_number, &resolved);
        promoted.into_iter().map(|hash| (hash, pool.len_for_block(&hash))).collect()
    };

    // A target may have crossed quorum while it sat in the future pool. Report the
    // delay stats too: `on_vote_received` is keyed on the current total for a
    // block, and promotion is the only path by which a future vote reaches that
    // total, so skipping it under-reports first- and majority-vote delay on
    // exactly the nodes that classify the most votes as future.
    for (hash, count) in promoted_counts {
        if count > 0 {
            block_stats::on_vote_received(hash, count);
            maybe_notify_finality(hash, count);
        }
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

/// Test-only: votes for a target in either pool.
///
/// No unit test can register the header provider, so the real ingest path
/// classifies everything as future. Tests whose subject is admission — dedup,
/// signature rejection, the height window — need to see both pools; production
/// callers deliberately see only current votes, which are origin-checked.
#[cfg(test)]
pub fn fetch_any_vote_by_block_hash(block_hash: B256) -> Vec<VoteEnvelope> {
    let pool = VOTE_POOL.read().expect("vote pool poisoned");
    let mut out = pool.fetch_vote_by_block_hash(block_hash);
    if let Some(vm) = pool.future_votes.get(&block_hash) {
        out.extend(vm.vote_messages.iter().map(|e| e.envelope.clone()));
    }
    out
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
        assert_eq!(fetch_any_vote_by_block_hash(data.target_hash).len(), 1, "genuine vote rejected");

        // Undecodable signature under a real validator's address. Before
        // verification existed this reached attestation assembly and panicked
        // in `Signature::from_bytes(..).unwrap()`.
        put_vote(VoteEnvelope {
            signature: VoteSignature::from([0x42u8; 96]),
            ..genuine.clone()
        });
        assert_eq!(
            fetch_any_vote_by_block_hash(data.target_hash).len(),
            1,
            "vote with undecodable signature was admitted",
        );

        // Well-formed signature by the same key, but over different vote data:
        // decodes cleanly, so only real verification catches it.
        let other = VoteData { target_number: 101, ..data };
        let mismatched = signer.sign_vote(other).expect("sign vote").signature;
        put_vote(VoteEnvelope { signature: mismatched, ..genuine.clone() });
        assert_eq!(
            fetch_any_vote_by_block_hash(data.target_hash).len(),
            1,
            "vote signed over different data was admitted",
        );

        // Signature valid in isolation, but attributed to another validator.
        put_vote(VoteEnvelope {
            vote_address: VoteAddress::from([0x07u8; 48]),
            ..genuine
        });
        assert_eq!(
            fetch_any_vote_by_block_hash(data.target_hash).len(),
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
        assert_eq!(fetch_any_vote_by_block_hash(data.target_hash).len(), 1);
        for _ in 0..10 {
            put_vote(genuine.clone());
        }
        assert_eq!(
            fetch_any_vote_by_block_hash(data.target_hash).len(),
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
            fetch_any_vote_by_block_hash(data.target_hash).len(),
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
            fetch_any_vote_by_block_hash(data.target_hash).len(),
            1,
            "an unknown head must not cause votes to be discarded",
        );

        let _ = drain();
    }




    // === D1: per-target vote caps ===

    /// The future-vote cap applies only once the sender is authenticated.
    ///
    /// Unauthenticated future votes are uncapped, because a cap over contents we
    /// cannot vouch for refuses genuine votes as readily as forged ones and
    /// whichever arrives second loses. Authenticated ones are capped, because
    /// then it can only ever refuse surplus from real validators. Current votes
    /// are always capped, having passed the origin check.
    #[test]
    fn future_cap_applies_only_to_authenticated_senders() {
        let mut pool = VotePool::new();

        let envelope = |target: B256, i: usize| {
            let mut address = VoteAddress::default();
            address[0] = (i & 0xff) as u8;
            address[1] = ((i >> 8) & 0xff) as u8;
            VoteEnvelope {
                vote_address: address,
                signature: VoteSignature::default(),
                data: VoteData {
                    source_number: 10,
                    source_hash: B256::from([0x71; 32]),
                    target_number: 11,
                    target_hash: target,
                },
            }
        };

        // Unauthenticated future sender: well past the cap, never refused.
        let unauth = B256::from([0x77; 32]);
        for i in 0..(MAX_FUTURE_VOTE_AMOUNT_PER_BLOCK * 4) {
            assert!(
                !pool.is_at_capacity(&unauth, true, false),
                "an unauthenticated future bucket must never refuse (i={i})",
            );
            pool.insert(envelope(unauth, i), 0, true);
        }
        assert_eq!(
            pool.future_votes.get(&unauth).map(|vm| vm.vote_messages.len()),
            Some(MAX_FUTURE_VOTE_AMOUNT_PER_BLOCK * 4),
            "every unauthenticated future vote is retained",
        );

        // Authenticated future sender: capped.
        let auth = B256::from([0x79; 32]);
        for i in 0..MAX_FUTURE_VOTE_AMOUNT_PER_BLOCK {
            assert!(!pool.is_at_capacity(&auth, true, true), "below the future cap (i={i})");
            pool.insert(envelope(auth, 5_000 + i), 0, true);
        }
        assert!(
            pool.is_at_capacity(&auth, true, true),
            "an authenticated future bucket stops at MAX_FUTURE_VOTE_AMOUNT_PER_BLOCK",
        );

        // Current votes stop at the validator-count cap.
        let cur_target = B256::from([0x78; 32]);
        for i in 0..MAX_CUR_VOTE_AMOUNT_PER_BLOCK {
            assert!(!pool.is_at_capacity(&cur_target, false, false), "below the cap (i={i})");
            pool.insert(envelope(cur_target, 1_000 + i), 0, false);
        }
        assert!(
            pool.is_at_capacity(&cur_target, false, false),
            "current votes must stop at MAX_CUR_VOTE_AMOUNT_PER_BLOCK",
        );
    }

    /// Shedding releases future votes when pruning cannot, furthest-ahead first.
    ///
    /// Answers the "what if there are too many bad votes" case: a flood that
    /// targets recent heights sits entirely inside the admission window, so
    /// pruning by head frees nothing and the pool needs another way down.
    #[test]
    fn shedding_releases_future_votes_furthest_ahead_first() {
        let mut pool = VotePool::new();

        // Three future targets at increasing heights, two votes each.
        for (n, byte) in [(100u64, 0xb1u8), (200, 0xb2), (300, 0xb3)] {
            let target = B256::from([byte; 32]);
            for i in 0..2usize {
                let mut address = VoteAddress::default();
                address[0] = byte;
                address[1] = i as u8;
                pool.insert(
                    VoteEnvelope {
                        vote_address: address,
                        signature: VoteSignature::default(),
                        data: VoteData {
                            source_number: n - 1,
                            source_hash: B256::from([0x01; 32]),
                            target_number: n,
                            target_hash: target,
                        },
                    },
                    0,
                    true,
                );
            }
        }
        assert_eq!(pool.len(), 6);

        // Ask for 1; the furthest-ahead bucket (300) goes, releasing both of its
        // votes. Buckets are released whole, so shedding can overshoot.
        let shed = pool.shed_future_votes(1);
        assert_eq!(shed, 2, "the whole furthest-ahead bucket is released");
        assert!(
            !pool.future_votes.contains_key(&B256::from([0xb3; 32])),
            "height 300 shed first",
        );
        assert!(
            pool.future_votes.contains_key(&B256::from([0xb1; 32])),
            "height 100 retained: nearest to promotion",
        );
        assert_eq!(pool.len(), 4, "accounting follows the shed votes");
    }

    /// One target hash cannot be made to hold unbounded votes, however many
    /// distinct validators sign for it. Mirrors go-bsc's cap in `basicVerify`.
    #[test]
    fn put_vote_caps_votes_per_target() {
        let _ = drain();

        let data = VoteData {
            source_number: 900,
            source_hash: B256::from([0x91; 32]),
            target_number: 901,
            target_hash: B256::from([0x92; 32]),
        };

        // Distinct signers so nothing is rejected as a duplicate.
        let over = MAX_CUR_VOTE_AMOUNT_PER_BLOCK + 8;
        for _ in 1..=over {
            // A fresh signer each time, so nothing is rejected as a duplicate.
            put_vote_unchecked(random_test_signer().sign_vote(data).expect("sign vote"));
        }

        assert_eq!(
            fetch_vote_by_block_hash(data.target_hash).len(),
            MAX_CUR_VOTE_AMOUNT_PER_BLOCK,
            "votes for one target must stop at the cap",
        );

        let _ = drain();
    }


    /// An extreme `target_number` must not be able to empty the pool.
    ///
    /// The oversize path used to derive its prune height from the *incoming*
    /// vote's target: `prune(target_number - 256)`. `target_number` is supplied
    /// by whoever sent the vote, and before the admission window existed it was
    /// unbounded — so one vote claiming a target near `u64::MAX` produced an
    /// astronomically large prune height, and `prune` then evicted every vote
    /// below it, which is all of them. Votes are never re-sent, so the node lost
    /// local quorum until fresh ones accumulated.
    ///
    /// No panic accompanied it: `[profile.release]` sets no `overflow-checks`,
    /// so `target_number + 255` inside `prune` wraps rather than trapping.
    ///
    /// The height now comes from our own head, so the sender cannot steer it.
    /// Uses `put_vote_unchecked` to reach the oversize path without paying a
    /// pairing per vote.
    #[test]
    fn extreme_target_number_cannot_wipe_the_pool() {
        let _ = drain();

        let survivor_target = B256::from([0xd1; 32]);
        let vote = |target: B256, number: u64, i: usize| {
            let mut address = VoteAddress::default();
            address[0] = (i & 0xff) as u8;
            address[1] = ((i >> 8) & 0xff) as u8;
            address[2] = ((i >> 16) & 0xff) as u8;
            VoteEnvelope {
                vote_address: address,
                signature: VoteSignature::default(),
                data: VoteData {
                    source_number: number.saturating_sub(1),
                    source_hash: B256::from([0xd0; 32]),
                    target_number: number,
                    target_hash: target,
                },
            }
        };

        // One vote we will look for afterwards, at an ordinary height.
        put_vote_unchecked(vote(survivor_target, 5_000, 0));
        assert_eq!(fetch_vote_by_block_hash(survivor_target).len(), 1);

        // Push past MAX_VOTES_IN_POOL so the oversize path engages. Spread over
        // many targets because the per-target cap bounds each one.
        let mut i = 1usize;
        let mut target_seed = 0u64;
        while len() <= MAX_VOTES_IN_POOL {
            target_seed += 1;
            let mut bytes = [0u8; 32];
            bytes[..8].copy_from_slice(&target_seed.to_le_bytes());
            let filler = B256::from(bytes);
            for _ in 0..MAX_CUR_VOTE_AMOUNT_PER_BLOCK {
                put_vote_unchecked(vote(filler, 5_000, i));
                i += 1;
            }
        }
        assert!(len() > MAX_VOTES_IN_POOL, "precondition: pool is oversized");

        // The payload: a target claiming to be near the end of the number space.
        put_vote_unchecked(vote(B256::from([0xff; 32]), u64::MAX - 300, i));

        assert_eq!(
            fetch_vote_by_block_hash(survivor_target).len(),
            1,
            "a vote at an ordinary height must survive an extreme target_number",
        );
        assert!(len() > MAX_CUR_VOTE_AMOUNT_PER_BLOCK, "the pool must not have been emptied");

        let _ = drain();
    }


    /// A vote that cannot be classified must not be treated as current.
    ///
    /// The network begins accepting peers in `build_network`, while the header
    /// provider is registered later in `build_consensus`, so votes can arrive
    /// while classification is impossible. Treating them as current would put
    /// un-origin-checked votes where attestation assembly and finality
    /// notification read from, let them consume the 21-per-target cap and so
    /// crowd out real validator votes, and never revalidate them afterwards.
    ///
    /// Routing them to the future pool instead means they are uncapped, invisible
    /// to finality counting, and fully origin-checked at promotion once the
    /// provider appears. Reported by Hashdit Bot on #491.
    ///
    /// Unit tests cannot register the provider, so this is the state under test.
    #[test]
    fn unclassifiable_votes_are_held_as_future_not_current() {
        let _ = drain();
        assert!(
            !shared::has_header_by_hash_provider(),
            "precondition: unit tests have no header provider",
        );

        let signer = random_test_signer();
        let data = VoteData {
            source_number: 7_000,
            source_hash: B256::from([0xe1; 32]),
            target_number: 7_001,
            target_hash: B256::from([0xe2; 32]),
        };
        put_vote(signer.sign_vote(data).expect("sign vote"));

        assert!(
            fetch_vote_by_block_hash(data.target_hash).is_empty(),
            "an unclassifiable vote must not enter the current pool, which feeds \
             attestation assembly and finality counting",
        );
        assert_eq!(
            fetch_any_vote_by_block_hash(data.target_hash).len(),
            1,
            "it is retained as a future vote, to be origin-checked at promotion",
        );

        // Uncapped, so it cannot be used to crowd out validator votes.
        {
            let pool = VOTE_POOL.read().expect("vote pool poisoned");
            assert!(!pool.is_at_capacity(&data.target_hash, true, false));
        }

        let _ = drain();
    }

    /// A recorded attestation is reported as-is, and the genesis lookup is never
    /// performed — a chain that has justified something must not pay for it.
    #[test]
    fn justified_pair_uses_the_recorded_attestation() {
        let vote_data = VoteData {
            source_number: 8,
            source_hash: B256::from([0x08; 32]),
            target_number: 9,
            target_hash: B256::from([0x09; 32]),
        };
        let pair = justified_pair_of(&vote_data, || panic!("genesis must not be consulted"));
        assert_eq!(pair, Some((9, B256::from([0x09; 32]))));
    }

    /// No attestation resolves to genesis, matching go-bsc's
    /// `GetJustifiedNumberAndHash` and the substitution our own vote producers
    /// already make. Reported by will-2012 on #491; verified on a fresh 10-node
    /// all-reth devnet, where every vote was rejected for source mismatch and
    /// `finalized` never left genesis.
    #[test]
    fn justified_pair_without_an_attestation_resolves_to_genesis() {
        let genesis_hash = B256::from([0x9e; 32]);
        let vote_data = VoteData {
            source_number: 0,
            source_hash: B256::ZERO,
            target_number: 0,
            target_hash: B256::ZERO,
        };
        let pair = justified_pair_of(&vote_data, || Some((0, genesis_hash)));
        assert_eq!(pair, Some((0, genesis_hash)));
    }

    /// A future vote for `target_number`, distinguished by `unique`.
    fn future_vote(target_hash: B256, target_number: u64, unique: u8) -> VoteEnvelope {
        let mut address = VoteAddress::default();
        address[0] = unique;
        let mut signature = VoteSignature::default();
        signature[0] = unique;
        VoteEnvelope {
            vote_address: address,
            signature,
            data: VoteData {
                source_number: target_number - 1,
                source_hash: B256::from([0x01; 32]),
                target_number,
                target_hash,
            },
        }
    }

    fn verdicts(target: B256, entries: &[(B256, bool)]) -> HashMap<B256, HashMap<B256, bool>> {
        let inner: HashMap<B256, bool> = entries.iter().copied().collect();
        [(target, inner)].into_iter().collect()
    }

    /// Promotion applies verdicts computed outside the lock: accepted votes reach
    /// the current pool, rejected ones are dropped along with their dedup entry.
    #[test]
    fn apply_promotion_moves_accepted_votes_and_drops_rejected() {
        let target = B256::from([0x77; 32]);
        let mut pool = VotePool::new();
        let accepted = future_vote(target, 100, 1);
        let rejected = future_vote(target, 100, 2);
        pool.insert(accepted.clone(), 0, true);
        pool.insert(rejected.clone(), 0, true);

        let promoted = pool.apply_promotion(
            100,
            &verdicts(target, &[(accepted.hash(), true), (rejected.hash(), false)]),
        );

        assert_eq!(promoted, vec![target]);
        assert_eq!(pool.fetch_vote_by_block_hash(target).len(), 1, "accepted vote is current");
        assert!(!pool.future_votes.contains_key(&target), "nothing left in the future pool");
        assert!(
            !pool.received_votes.contains(&rejected.hash()),
            "a rejected vote leaves the dedup set, so a legitimate copy can still arrive",
        );
    }

    /// A vote that lands between the verdicts being taken and the write has no
    /// verdict of its own. It must stay future and keep its target queued, rather
    /// than being guessed either way — and re-queueing must not let the same
    /// entry be popped again on this pass, which would never terminate.
    #[test]
    fn apply_promotion_requeues_votes_that_arrived_after_the_verdicts() {
        let target = B256::from([0x88; 32]);
        let mut pool = VotePool::new();
        let judged = future_vote(target, 100, 3);
        let latecomer = future_vote(target, 100, 4);
        pool.insert(judged.clone(), 0, true);
        pool.insert(latecomer.clone(), 0, true);

        // Verdicts were taken before `latecomer` arrived.
        let promoted = pool.apply_promotion(100, &verdicts(target, &[(judged.hash(), true)]));

        assert_eq!(promoted, vec![target]);
        assert_eq!(pool.fetch_vote_by_block_hash(target).len(), 1, "judged vote promoted");
        assert_eq!(
            pool.future_votes.get(&target).map(|vm| vm.vote_messages.len()),
            Some(1),
            "the unjudged vote stays future",
        );
        assert_eq!(
            pool.future_votes_pq.heap.len(),
            1,
            "and its target stays queued so the next pass judges it",
        );
    }

    /// Candidate selection is the read-lock half: everything at or below the head
    /// is a candidate, anything more than `head - 11` behind is flagged so
    /// promotion judges it instead of holding it forever — the same sweep go-bsc
    /// runs in `transferVotesFromFutureToCur` — and targets above the head are
    /// left alone.
    #[test]
    fn promotion_candidates_selects_reached_targets_and_flags_expired() {
        let stale = B256::from([0xa1; 32]);
        let current = B256::from([0xa2; 32]);
        let ahead = B256::from([0xa3; 32]);
        let mut pool = VotePool::new();
        pool.insert(future_vote(stale, 50, 5), 0, true);
        pool.insert(future_vote(current, 100, 6), 0, true);
        pool.insert(future_vote(ahead, 105, 7), 0, true);

        let mut got: Vec<(u64, bool, usize)> = pool
            .promotion_candidates(100)
            .into_iter()
            .map(|c| (c.data.target_number, c.expired, c.votes.len()))
            .collect();
        got.sort();

        assert_eq!(
            got,
            vec![(50, true, 1), (100, false, 1)],
            "target 105 is still ahead of the head; 50 is past head-11 so it expires",
        );
    }

    /// One pass judges at most `MAX_PROMOTION_VOTES_PER_PASS` votes, so an
    /// uncapped future bucket cannot put unbounded provider and snapshot reads on
    /// the import path. The remainder is not lost: it stays future, its target
    /// stays queued, and the next pass continues.
    #[test]
    fn promotion_candidates_bounds_the_votes_judged_per_pass() {
        let target = B256::from([0xb1; 32]);
        let mut pool = VotePool::new();
        let oversized = MAX_PROMOTION_VOTES_PER_PASS + 40;
        for i in 0..oversized {
            let mut vote = future_vote(target, 100, 0);
            // Distinct envelopes: `insert` dedups by hash.
            vote.vote_address[1..3].copy_from_slice(&(i as u16).to_be_bytes());
            pool.insert(vote, 0, true);
        }

        let candidates = pool.promotion_candidates(100);
        let judged: usize = candidates.iter().map(|c| c.votes.len()).sum();
        assert_eq!(judged, MAX_PROMOTION_VOTES_PER_PASS, "pass budget is enforced");

        // Judge exactly what the pass captured; the rest must survive as future.
        let verdicts: HashMap<B256, bool> =
            candidates[0].votes.iter().map(|e| (e.hash, true)).collect();
        let promoted = pool.apply_promotion(100, &[(target, verdicts)].into_iter().collect());

        assert_eq!(promoted, vec![target]);
        assert_eq!(
            pool.future_votes.get(&target).map(|vm| vm.vote_messages.len()),
            Some(oversized - MAX_PROMOTION_VOTES_PER_PASS),
            "the unjudged remainder stays future",
        );
        assert_eq!(pool.future_votes_pq.heap.len(), 1, "and its target stays queued");
    }

    /// At most `MAX_PROMOTION_TARGETS_PER_PASS` targets are examined per pass.
    #[test]
    fn promotion_candidates_bounds_the_targets_per_pass() {
        let mut pool = VotePool::new();
        for i in 0..(MAX_PROMOTION_TARGETS_PER_PASS + 10) {
            let mut hash = [0xc0u8; 32];
            hash[0..2].copy_from_slice(&(i as u16).to_be_bytes());
            pool.insert(future_vote(B256::from(hash), 100, 9), 0, true);
        }

        assert_eq!(pool.promotion_candidates(100).len(), MAX_PROMOTION_TARGETS_PER_PASS);
    }

    /// Registers a snapshot provider for tests, reusing whichever one another
    /// test already installed — `SNAPSHOT_PROVIDER` is a `OnceLock`.
    fn test_snapshot_provider() -> &'static std::sync::Arc<
        dyn crate::consensus::parlia::provider::SnapshotProvider + Send + Sync,
    > {
        #[derive(Default)]
        struct MapProvider {
            snaps: std::sync::RwLock<
                std::collections::HashMap<B256, crate::consensus::parlia::snapshot::Snapshot>,
            >,
        }
        impl crate::consensus::parlia::provider::SnapshotProvider for MapProvider {
            fn snapshot_by_hash(
                &self,
                block_hash: &B256,
            ) -> Option<crate::consensus::parlia::snapshot::Snapshot> {
                self.snaps.read().ok().and_then(|m| m.get(block_hash).cloned())
            }
            fn insert(&self, snapshot: crate::consensus::parlia::snapshot::Snapshot) {
                if let Ok(mut m) = self.snaps.write() {
                    m.insert(snapshot.block_hash, snapshot);
                }
            }
        }

        if shared::get_snapshot_provider().is_none() {
            let p: std::sync::Arc<
                dyn crate::consensus::parlia::provider::SnapshotProvider + Send + Sync,
            > = std::sync::Arc::new(MapProvider::default());
            let _ = shared::set_snapshot_provider(p);
        }
        shared::get_snapshot_provider().expect("snapshot provider registered")
    }

    /// Before the chain has justified anything, `vote_data` is all zeroes — the
    /// *absence* of a justified block, not a justified block whose hash is zero.
    /// Vote producers know this and substitute genesis (`vote_producer.rs:192`,
    /// `consensus.rs:785`), and go-bsc's `GetJustifiedNumberAndHash` returns
    /// `chain.GetHeaderByNumber(0).Hash()` when `snap.Attestation == nil`.
    ///
    /// Reporting the zero hash here makes `verify_vote_origin` reject every vote
    /// on such a chain for source mismatch, and the rejection is self-locking:
    /// leaving the state needs an attestation, which can only be assembled from
    /// the votes being rejected. Raised by will-2012 on #491.
    #[test]
    fn justified_pair_never_reports_the_zero_hash() {
        use crate::consensus::parlia::snapshot::{Snapshot, DEFAULT_EPOCH_LENGTH};

        let head_hash = B256::from([0x5a; 32]);
        let snap = Snapshot::new(
            vec![alloy_primitives::Address::ZERO],
            10,
            head_hash,
            DEFAULT_EPOCH_LENGTH,
            None,
        );
        assert_eq!(
            snap.vote_data.target_hash,
            B256::ZERO,
            "a snapshot with no attestation records the zero hash",
        );
        test_snapshot_provider().insert(snap);

        let pair = justified_pair_for_hash(&head_hash);
        assert!(
            !matches!(pair, Some((_, B256::ZERO))),
            "no attestation must not be reported as a justified block at the zero hash, \
             which no vote can ever cite; got {pair:?}",
        );
    }

    /// The validator set swaps `miner_history_check_len()` blocks *after* the
    /// epoch multiple, not at it. Judging a future vote against our own head is
    /// only sound when no swap sits in `(head, target]`, and watching the
    /// multiple instead misses the real boundary by that offset — which rejects
    /// a joining validator's votes outright, since `Some(false)` drops them and
    /// votes are never re-sent. Raised by will-2012 on #491.
    #[test]
    fn validator_set_swap_window_tracks_the_real_boundary() {
        // Mainnet post-Maxwell: 1000-block epoch, 21 validators, turn_length 4.
        const EPOCH: u64 = 1000;
        const OFFSET: u64 = 43; // (21 / 2 + 1) * 4 - 1

        // The swap at 43_043 lies in the window, so membership is undecidable...
        assert!(validator_set_swaps_within(43_035, 43_043, EPOCH, OFFSET));
        // ...and both heights share an epoch multiple, so the multiple-based
        // test saw no boundary at all and judged against the outgoing set.
        assert_eq!(43_035 / EPOCH, 43_043 / EPOCH);

        // Crossing the multiple without reaching the swap: the set is unchanged...
        assert!(!validator_set_swaps_within(42_995, 43_000, EPOCH, OFFSET));
        // ...where the multiple-based test declined to judge. Harmless, but it
        // gave up the per-block cap for no reason.
        assert_ne!(42_995 / EPOCH, 43_000 / EPOCH);

        // Half-open window: a swap at `head` is behind us, one at `target` is not.
        assert!(!validator_set_swaps_within(43_043, 43_050, EPOCH, OFFSET));
        assert!(validator_set_swaps_within(43_042, 43_043, EPOCH, OFFSET));
        // Mid-epoch window, nothing near a boundary.
        assert!(!validator_set_swaps_within(43_100, 43_111, EPOCH, OFFSET));
    }

    /// Differential check against the predicate that actually swaps the set in
    /// `SnapshotProvider::try_rebuild`, over every window the admission bound
    /// permits.
    #[test]
    fn validator_set_swap_window_matches_the_provider_predicate() {
        const EPOCH: u64 = 200;
        const OFFSET: u64 = 10;

        for head in 0..(EPOCH * 3) {
            for target in head..=(head + UPPER_LIMIT_OF_VOTE_BLOCK_NUMBER) {
                // `is_epoch_boundary` in provider.rs, applied block by block.
                let expected = ((head + 1)..=target).any(|n| n > 0 && n % EPOCH == OFFSET);
                assert_eq!(
                    validator_set_swaps_within(head, target, EPOCH, OFFSET),
                    expected,
                    "head {head}, target {target}",
                );
            }
        }
    }

    /// Degenerate configurations must not invent a boundary the provider can
    /// never reach, and must not divide by zero.
    #[test]
    fn validator_set_swap_window_handles_degenerate_epochs() {
        // offset >= epoch: `n % epoch == offset` never holds, so the provider
        // never swaps the set and neither may we.
        assert!(!validator_set_swaps_within(0, 10_000, 200, 200));
        // A zero epoch is coerced to 1 rather than panicking.
        assert!(validator_set_swaps_within(5, 6, 0, 0));
        // Single-validator devnet: offset 0, so the swap sits on the multiple.
        assert!(validator_set_swaps_within(199, 200, 200, 0));
        assert!(!validator_set_swaps_within(200, 205, 200, 0));
    }
}
