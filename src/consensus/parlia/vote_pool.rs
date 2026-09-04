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
    fn transfer_future_votes(&mut self, latest: BlockNumber) -> Vec<B256> {
        let mut promoted = Vec::new();

        // Phase 1: too old to still be considered future.
        while let Some(vd) = self.future_votes_pq.peek() {
            if vd.target_number.saturating_add(UPPER_LIMIT_OF_VOTE_BLOCK_NUMBER) >= latest {
                break;
            }
            let hash = vd.target_hash;
            self.future_votes_pq.pop();
            if self.promote(hash) {
                promoted.push(hash);
            }
        }

        // Phase 2: promote only what we can now resolve; retain the rest.
        let mut deferred = Vec::new();
        while let Some(vd) = self.future_votes_pq.peek() {
            if vd.target_number > latest {
                break;
            }
            let vd = *vd;
            self.future_votes_pq.pop();
            if shared::get_canonical_header_by_hash_from_provider(&vd.target_hash).is_none() {
                deferred.push(vd);
                continue;
            }
            if self.promote(vd.target_hash) {
                promoted.push(vd.target_hash);
            }
        }
        for vd in deferred {
            self.future_votes_pq.push(vd);
        }

        metrics::gauge!("futureVotesPq.local").set(self.future_votes_pq.heap.len() as f64);
        promoted
    }

    /// Moves one target's future votes into the current pool, dropping any that
    /// fail the origin check. Returns whether any vote survived.
    ///
    /// The caller has already popped this hash from the future queue.
    fn promote(&mut self, block_hash: B256) -> bool {
        let Some(box_) = self.future_votes.remove(&block_hash) else {
            return false;
        };

        let mut valid = Vec::with_capacity(box_.vote_messages.len());
        for entry in box_.vote_messages {
            if verify_vote_origin(&entry.envelope) {
                valid.push(entry);
            } else {
                // Drop from the dedup set too, so a later legitimate copy is not
                // mistaken for a duplicate.
                self.received_votes.remove(&entry.hash);
                self.total_votes = self.total_votes.saturating_sub(1);
                metrics::counter!("votes.rejected.origin_on_promote").increment(1);
            }
        }
        if valid.is_empty() {
            return false;
        }

        let data = valid[0].envelope.data;
        if !self.cur_votes.contains_key(&block_hash) {
            self.cur_votes_pq.push(data);
        }
        self.cur_votes.entry(block_hash).or_default().vote_messages.extend(valid);
        metrics::gauge!("curVotesPq.local").set(self.cur_votes_pq.heap.len() as f64);
        true
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
pub fn justified_pair_for_hash(header_hash: &B256) -> Option<(BlockNumber, B256)> {
    let sp = shared::get_snapshot_provider()?;
    let snap = sp.snapshot_by_hash(header_hash)?;
    Some((snap.vote_data.target_number, snap.vote_data.target_hash))
}

/// Whether a *future* vote's sender is a validator, judged against the snapshot
/// at our own head.
///
/// `verify_vote_origin` cannot run on a future vote: it resolves membership from
/// the target's parent snapshot, and we do not hold the target. But the
/// validator set only changes on epoch boundaries, and the admission window caps
/// a future target at `head + 11`, so the set at our head is the set that will
/// govern the target — unless an epoch boundary falls in between.
///
/// Returns:
/// - `Some(true)`  sender is a validator in the current set
/// - `Some(false)` sender is not, and cannot become one inside the window
/// - `None` undecidable: no snapshot, or an epoch boundary lies in
///   `(head, target]` so the governing set may differ. Callers admit these
///   uncapped rather than guess.
fn future_vote_sender_is_validator(vote: &VoteEnvelope) -> Option<bool> {
    let head_number = shared::get_best_canonical_block_number()?;
    let head = shared::get_canonical_header_by_number(head_number)?;
    let snap = shared::get_snapshot_provider()?.snapshot_by_hash(&head.hash_slow())?;
    if snap.validators_map.is_empty() {
        return None;
    }

    // An epoch boundary between head and target can swap the set out from under
    // us; decline to judge rather than risk rejecting an incoming validator.
    let epoch = snap.epoch_num.max(1);
    if vote.data.target_number / epoch > head_number / epoch {
        return None;
    }

    Some(snap.validators_map.values().any(|v| v.vote_addr == vote.vote_address))
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
fn verify_vote_origin(vote: &VoteEnvelope) -> bool {
    let Some(header) = shared::get_canonical_header_by_hash_from_provider(&vote.data.target_hash)
    else {
        tracing::debug!(
            target: "bsc::vote_pool",
            target_number = vote.data.target_number,
            "vote origin unverifiable: target header not found",
        );
        return false;
    };
    if header.number != vote.data.target_number {
        return false;
    }

    match justified_pair_for_hash(&vote.data.target_hash) {
        Some((justified_number, justified_hash)) => {
            if vote.data.source_number != justified_number
                || vote.data.source_hash != justified_hash
            {
                metrics::counter!("votes.rejected.source_mismatch").increment(1);
                return false;
            }
        }
        None => {
            tracing::debug!(
                target: "bsc::vote_pool",
                target_number = vote.data.target_number,
                "vote origin unverifiable: no snapshot for target",
            );
            return false;
        }
    }

    let Some(sp) = shared::get_snapshot_provider() else {
        return false;
    };
    let Some(parent_snap) = sp.snapshot_by_hash(&header.parent_hash) else {
        tracing::debug!(
            target: "bsc::vote_pool",
            target_number = vote.data.target_number,
            "vote origin unverifiable: no snapshot for target's parent",
        );
        return false;
    };

    let is_validator =
        parent_snap.validators_map.values().any(|v| v.vote_addr == vote.vote_address);
    if !is_validator {
        metrics::counter!("votes.rejected.not_a_validator").increment(1);
    }
    is_validator
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

fn put_vote_inner(vote: VoteEnvelope) {
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
                return;
            }
            Some(true) => true,
            None => false,
        }
    } else {
        false
    };

    // Current votes are origin-checked at admission; future votes at promotion.
    if !is_future && !verify_vote_origin(&vote) {
        tracing::debug!(
            target: "bsc::vote_pool",
            vote_address = %vote.vote_address,
            target_number,
            "rejecting vote that failed the origin check",
        );
        return;
    }

    // Lazy prune and promotion: run once per observed head advance. Replaces
    // geth-bsc's chain-head subscription by piggybacking on the vote ingest
    // path, which is the same cadence in practice since votes arrive per block.
    let need_head_work = pending_block_number > LAST_PRUNED_BLOCK.load(Ordering::Relaxed);

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

    let mut promoted = Vec::new();
    if need_head_work {
        promoted = pool.transfer_future_votes(pending_block_number);
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
    let promoted_counts: Vec<(B256, usize)> =
        promoted.iter().map(|h| (*h, pool.len_for_block(h))).collect();
    drop(pool);
    update_vote_pool_size_metric(size);

    // Report chain delay vote metrics
    if votes_for_block > 0 {
        block_stats::on_vote_received(target_hash, votes_for_block);
        maybe_notify_finality(target_hash, votes_for_block);
    }
    // Promoted targets may have crossed quorum while sitting in the future pool.
    for (hash, count) in promoted_counts {
        if count > 0 {
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

}
