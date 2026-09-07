//! Cross-validator bad-BidBlock evidence for BEP-675.
//!
//! A local validator revokes a builder immediately when its own blind-signed BidBlock fails
//! post-broadcast verification. This module also observes execution-invalid BidBlocks sealed by
//! other validators. Once distinct sealers reach the cabinet or total-validator threshold within
//! 24 hours, the builder is revoked locally before it can burn this validator's slot.

use crate::{
    chainspec::BscChainSpec,
    metrics::BscBidBlockEvidenceMetrics,
    node::{
        evm::pre_execution::{validator_role_at_parent, ValidatorRole},
        miner::block_mev_info::{decode_block_mev_info, BlockMevInfoVersion},
        primitives::{BscBlock, BscPrimitives},
    },
    shared,
};
use alloy_consensus::{BlockHeader, Header};
use alloy_primitives::{Address, B256};
use lru::LruCache;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use reth_engine_primitives::InvalidBlockHook;
use reth_ethereum_primitives::Receipt;
use reth_execution_types::BlockExecutionOutput;
use reth_primitives_traits::{RecoveredBlock, SealedBlock, SealedHeader};
use reth_provider::StateProviderFactory;
use reth_tasks::TaskExecutor;
use reth_trie_common::updates::TrieUpdates;
use std::{
    collections::HashMap,
    num::NonZeroUsize,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::mpsc;

const EVIDENCE_WINDOW: Duration = Duration::from_secs(24 * 60 * 60);
const EVIDENCE_QUEUE_SIZE: usize = 64;
const EVIDENCED_BLOCK_CACHE_SIZE: usize = 1_000;

const fn majority_threshold(validators: usize) -> usize {
    validators / 2 + 1
}

static EVIDENCE_METRICS: Lazy<BscBidBlockEvidenceMetrics> =
    Lazy::new(BscBidBlockEvidenceMetrics::default);

#[derive(Debug)]
struct BadBidBlockEvidence {
    builder: Address,
    sealer: Address,
    hash: B256,
    number: u64,
    parent: SealedHeader<Header>,
}

/// A lightweight engine-tree hook. Contract reads and threshold accounting happen on the
/// background consumer so invalid-block handling never waits for them.
#[derive(Clone)]
pub(crate) struct BadBidBlockEvidenceReporter {
    sender: mpsc::Sender<BadBidBlockEvidence>,
    evidenced: Arc<Mutex<LruCache<B256, ()>>>,
}

impl BadBidBlockEvidenceReporter {
    pub(crate) fn spawn<P>(
        provider: P,
        chain_spec: Arc<BscChainSpec>,
        task_executor: &TaskExecutor,
    ) -> Self
    where
        P: StateProviderFactory + Clone + Send + Sync + 'static,
    {
        let (sender, receiver) = mpsc::channel(EVIDENCE_QUEUE_SIZE);
        task_executor.spawn_critical_task(
            "bad bid block evidence",
            run_evidence_service(receiver, provider, chain_spec),
        );

        Self::new(sender)
    }

    fn new(sender: mpsc::Sender<BadBidBlockEvidence>) -> Self {
        Self {
            sender,
            evidenced: Arc::new(Mutex::new(LruCache::new(
                NonZeroUsize::new(EVIDENCED_BLOCK_CACHE_SIZE).unwrap(),
            ))),
        }
    }

    fn evidence_for(
        parent_header: &SealedHeader<Header>,
        block: &SealedBlock<BscBlock>,
    ) -> Option<BadBidBlockEvidence> {
        let tag = block.header().requests_hash?;
        let (version, builder) = decode_block_mev_info(tag)?;
        if version != BlockMevInfoVersion::BidBlock {
            return None;
        }

        Some(BadBidBlockEvidence {
            builder,
            sealer: block.header().beneficiary,
            hash: block.hash(),
            number: block.number(),
            // The hook is invoked only after pre-execution validation has succeeded. This parent
            // therefore anchors both the execution state and the validator set that authorized
            // the sealer.
            parent: parent_header.clone(),
        })
    }

    pub(crate) fn report(
        &self,
        parent_header: &SealedHeader<Header>,
        block: &SealedBlock<BscBlock>,
    ) {
        let Some(evidence) = Self::evidence_for(parent_header, block) else { return };

        // Keep check-and-insert atomic across hook/reporter clones. As in BSC, even a dropped
        // event is remembered so repeated announcements cannot crowd the queue.
        {
            let mut evidenced = self.evidenced.lock();
            if evidenced.contains(&evidence.hash) {
                return;
            }
            evidenced.put(evidence.hash, ());
        }
        match self.sender.try_send(evidence) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(evidence)) => {
                EVIDENCE_METRICS.dropped_total.increment(1);
                tracing::warn!(
                    target: "bsc::bid_block_evidence",
                    hash = %evidence.hash,
                    "Bad BidBlock evidence dropped, queue full"
                );
            }
            Err(mpsc::error::TrySendError::Closed(evidence)) => {
                tracing::warn!(
                    target: "bsc::bid_block_evidence",
                    builder = %evidence.builder,
                    number = evidence.number,
                    hash = %evidence.hash,
                    "Bad BidBlock evidence service is unavailable"
                );
            }
        }
    }
}

impl InvalidBlockHook<BscPrimitives> for BadBidBlockEvidenceReporter {
    fn on_invalid_block(
        &self,
        parent_header: &SealedHeader<Header>,
        block: &RecoveredBlock<BscBlock>,
        _output: &BlockExecutionOutput<Receipt>,
        _trie_updates: Option<(&TrieUpdates, B256)>,
    ) {
        self.report(parent_header, block.sealed_block())
    }
}

#[derive(Debug, Clone, Copy)]
struct Sighting {
    at: Instant,
    cabinet: bool,
}

#[derive(Debug, Default)]
struct BadBidBlockTracker {
    seen: HashMap<Address, HashMap<Address, Sighting>>,
}

impl BadBidBlockTracker {
    /// Repeat sightings from one sealer refresh its timestamp without adding another vote. Cabinet
    /// membership is sticky inside the window so a later demotion cannot reduce collected proof.
    fn add(
        &mut self,
        builder: Address,
        sealer: Address,
        cabinet: bool,
        now: Instant,
    ) -> (usize, usize) {
        let sightings = self.seen.entry(builder).or_default();
        let previous = sightings.get(&sealer).copied();
        sightings.insert(
            sealer,
            Sighting { at: now, cabinet: previous.is_some_and(|s| s.cabinet) || cabinet },
        );

        sightings.retain(|_, sighting| now.duration_since(sighting.at) < EVIDENCE_WINDOW);
        let total = sightings.len();
        let cabinet = sightings.values().filter(|sighting| sighting.cabinet).count();
        (cabinet, total)
    }

    fn clear(&mut self, builder: Address) {
        self.seen.remove(&builder);
    }
}

async fn run_evidence_service<P>(
    mut receiver: mpsc::Receiver<BadBidBlockEvidence>,
    provider: P,
    chain_spec: Arc<BscChainSpec>,
) where
    P: StateProviderFactory + Clone + Send + Sync + 'static,
{
    let mut tracker = BadBidBlockTracker::default();

    while let Some(evidence) = receiver.recv().await {
        let Some(config) = crate::node::miner::config::get_global_mining_config() else { continue };
        if !config.enabled || !config.bid_block_enabled {
            continue;
        }

        let permissions = shared::get_bid_block_permission_manager();
        if !permissions.is_allowed(evidence.builder) {
            continue;
        }

        let state = match provider.state_by_block_hash(evidence.parent.hash()) {
            Ok(state) => state,
            Err(err) => {
                tracing::warn!(
                    target: "bsc::bid_block_evidence",
                    builder = %evidence.builder,
                    sealer = %evidence.sealer,
                    parent_hash = %evidence.parent.hash(),
                    %err,
                    "Failed to open parent state for bad BidBlock evidence"
                );
                continue;
            }
        };
        let (role, cabinet_count, total_count) = match validator_role_at_parent(
            state,
            chain_spec.as_ref().clone(),
            &evidence.parent,
            evidence.sealer,
        ) {
            Ok(result) => result,
            Err(err) => {
                tracing::warn!(
                    target: "bsc::bid_block_evidence",
                    builder = %evidence.builder,
                    sealer = %evidence.sealer,
                    %err,
                    "Validator lookup failed for bad BidBlock evidence"
                );
                continue;
            }
        };
        if role == ValidatorRole::None {
            tracing::debug!(
                target: "bsc::bid_block_evidence",
                builder = %evidence.builder,
                sealer = %evidence.sealer,
                number = evidence.number,
                "Bad BidBlock sealer is not in the parent validator set"
            );
            continue;
        }

        let (cabinet_votes, total_votes) = tracker.add(
            evidence.builder,
            evidence.sealer,
            role == ValidatorRole::Cabinet,
            Instant::now(),
        );
        let cabinet_threshold = majority_threshold(cabinet_count);
        let total_threshold = majority_threshold(total_count);
        if cabinet_votes < cabinet_threshold && total_votes < total_threshold {
            tracing::info!(
                target: "bsc::bid_block_evidence",
                builder = %evidence.builder,
                sealer = %evidence.sealer,
                role = %role,
                number = evidence.number,
                hash = %evidence.hash,
                cabinet_votes,
                cabinet_threshold,
                total_votes,
                total_threshold,
                "Recorded bad BidBlock evidence"
            );
            continue;
        }

        let reason =
            format!("bad BidBlocks from {cabinet_votes}/{cabinet_threshold} cabinet and {total_votes}/{total_threshold} total validators");
        tracing::error!(
            target: "bsc::bid_block_evidence",
            builder = %evidence.builder,
            cabinet_votes,
            cabinet_threshold,
            total_votes,
            total_threshold,
            last_sealer = %evidence.sealer,
            number = evidence.number,
            hash = %evidence.hash,
            "Revoking builder based on cross-validator bad BidBlock evidence"
        );
        permissions.revoke(evidence.builder, reason, evidence.hash, evidence.number);
        EVIDENCE_METRICS.revokes_total.increment(1);
        tracker.clear(evidence.builder);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::{miner::block_mev_info::encode_block_mev_info, primitives::BscBlockBody};
    use reth_ethereum_primitives::BlockBody;

    fn address(n: u8) -> Address {
        Address::with_last_byte(n)
    }

    fn recovered_block(version: BlockMevInfoVersion) -> RecoveredBlock<BscBlock> {
        let header = Header {
            beneficiary: address(9),
            number: 7,
            requests_hash: Some(encode_block_mev_info(version, address(0xb0))),
            ..Default::default()
        };
        RecoveredBlock::new_unhashed(
            BscBlock {
                header,
                body: BscBlockBody {
                    inner: BlockBody {
                        transactions: Vec::new(),
                        ommers: Vec::new(),
                        withdrawals: None,
                    },
                    sidecars: None,
                },
            },
            Vec::new(),
        )
    }

    #[test]
    fn evidence_only_accepts_bid_block_tags() {
        let parent_header = Header::default();
        let parent = SealedHeader::new(parent_header.clone(), parent_header.hash_slow());

        let evidence = BadBidBlockEvidenceReporter::evidence_for(
            &parent,
            recovered_block(BlockMevInfoVersion::BidBlock).sealed_block(),
        )
        .unwrap();
        assert_eq!(evidence.builder, address(0xb0));
        assert_eq!(evidence.sealer, address(9));
        assert_eq!(evidence.number, 7);

        assert!(BadBidBlockEvidenceReporter::evidence_for(
            &parent,
            recovered_block(BlockMevInfoVersion::Bid).sealed_block(),
        )
        .is_none());
    }

    #[test]
    fn tracker_counts_distinct_sealers_and_expires_old_sightings() {
        let mut tracker = BadBidBlockTracker::default();
        let start = Instant::now();
        let builder = address(0xb0);

        tracker.add(builder, address(1), true, start);
        tracker.add(builder, address(1), true, start);
        assert_eq!(tracker.add(builder, address(2), false, start), (1, 2));

        let later = start + EVIDENCE_WINDOW + Duration::from_secs(1);
        assert_eq!(tracker.add(builder, address(3), false, later), (0, 1));
    }

    #[test]
    fn cabinet_membership_is_sticky_inside_the_window() {
        let mut tracker = BadBidBlockTracker::default();
        let now = Instant::now();
        let builder = address(0xb0);
        tracker.add(builder, address(1), true, now);
        assert_eq!(tracker.add(builder, address(1), false, now), (1, 1));
    }

    #[test]
    fn revoke_thresholds_match_bsc() {
        assert_eq!(majority_threshold(21), 11);
        assert_eq!(majority_threshold(45), 23);
        assert_eq!(majority_threshold(3), 2);
        assert_eq!(majority_threshold(4), 3);
    }

    #[test]
    fn queue_is_bounded_and_clones_deduplicate_before_enqueue() {
        let (sender, mut receiver) = mpsc::channel(1);
        let reporter = BadBidBlockEvidenceReporter::new(sender);
        let parent = SealedHeader::seal_slow(Header::default());
        let first = recovered_block(BlockMevInfoVersion::BidBlock);
        reporter.report(&parent, first.sealed_block());
        reporter.clone().report(&parent, first.sealed_block());
        assert_eq!(receiver.len(), 1);
        let mut second = first.clone().into_sealed_block().into_block();
        second.header.number += 1;
        let second = SealedBlock::seal_slow(second);
        reporter.report(&parent, &second); // Full: dropped without blocking.
        assert_eq!(receiver.len(), 1);
        assert_eq!(receiver.try_recv().unwrap().hash, first.hash());
        reporter.report(&parent, &second); // A dropped hash is also deduplicated.
        assert!(receiver.try_recv().is_err());
    }
}
