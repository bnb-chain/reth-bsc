//! Malicious vote monitor.
//!
//! Detects voting rule violations for BSC Parlia fast-finality:
//! - **Rule 1**: A validator must not publish two distinct votes for the same height.
//! - **Rule 2**: A validator must not vote within the span of its other votes.
//!
//! Ported from geth `core/monitor/malicious_vote_monitor.go`.

use std::collections::HashMap;

use alloy_primitives::BlockNumber;
use lru::LruCache;
use std::num::NonZero;

use super::vote::{VoteAddress, VoteEnvelope};

/// Maximum number of recent votes to track per validator.
const MAX_SIZE_OF_RECENT_ENTRY: usize = 512;

/// Scope in blocks for malicious vote slashing eligibility.
const MALICIOUS_VOTE_SLASH_SCOPE: u64 = 256;

/// Upper limit of vote block number offset (matches geth's `upperLimitOfVoteBlockNumber`).
const UPPER_LIMIT_OF_VOTE_BLOCK_NUMBER: u64 = 11;

/// Monitors incoming votes for rule violations and increments geth-compatible metrics
/// (`monitor/maliciousVote/violateRule1`, `monitor/maliciousVote/violateRule2`).
pub struct MaliciousVoteMonitor {
    /// Per-validator LRU cache mapping target_number → VoteEnvelope.
    cur_votes: HashMap<VoteAddress, LruCache<u64, VoteEnvelope>>,
}

impl MaliciousVoteMonitor {
    pub fn new() -> Self {
        Self { cur_votes: HashMap::with_capacity(21) }
    }

    /// Check a new vote against previously seen votes for the same validator.
    /// Returns `true` if a conflict (malicious vote) is detected.
    pub fn conflict_detect(
        &mut self,
        new_vote: &VoteEnvelope,
        pending_block_number: BlockNumber,
    ) -> bool {
        let vote_address = new_vote.vote_address;

        // Ensure LRU cache exists for this validator
        let vote_buffer = self.cur_votes.entry(vote_address).or_insert_with(|| {
            LruCache::new(NonZero::new(MAX_SIZE_OF_RECENT_ENTRY).unwrap())
        });

        let source_number = new_vote.data.source_number;
        let target_number = new_vote.data.target_number;

        // Basic scope check
        if !(target_number + MALICIOUS_VOTE_SLASH_SCOPE > pending_block_number) {
            return false;
        }

        // Determine scan range
        let mut block_number = source_number + 1;
        if !(block_number + MALICIOUS_VOTE_SLASH_SCOPE > pending_block_number) {
            block_number = pending_block_number - MALICIOUS_VOTE_SLASH_SCOPE + 1;
        }

        let new_vote_hash = new_vote.data.hash();

        while block_number <= pending_block_number + UPPER_LIMIT_OF_VOTE_BLOCK_NUMBER {
            if let Some(existing_vote) = vote_buffer.get(&block_number) {
                let mut malicious = false;

                // Rule 1: same target_number, different vote hash
                if block_number == target_number && existing_vote.data.hash() != new_vote_hash {
                    metrics::counter!("monitor.maliciousVote.violateRule1").increment(1);
                    malicious = true;
                }
                // Rule 2: overlapping vote spans
                else if (block_number < target_number
                    && existing_vote.data.source_number > source_number)
                    || (block_number > target_number
                        && existing_vote.data.source_number < source_number)
                {
                    metrics::counter!("monitor.maliciousVote.violateRule2").increment(1);
                    malicious = true;
                }

                if malicious {
                    tracing::warn!(
                        target: "bsc::vote",
                        validator = ?vote_address,
                        existing_target = existing_vote.data.target_number,
                        existing_source = existing_vote.data.source_number,
                        new_target = target_number,
                        new_source = source_number,
                        "Malicious vote detected"
                    );
                    return true;
                }
            }
            block_number += 1;
        }

        // Store the new vote (overwrite if target_number already exists)
        vote_buffer.put(target_number, new_vote.clone());
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::consensus::parlia::vote::{VoteData, VoteSignature};
    use alloy_primitives::B256;

    fn make_vote(
        address_byte: u8,
        source_number: u64,
        target_number: u64,
        target_hash_byte: u8,
    ) -> VoteEnvelope {
        let mut address = VoteAddress::default();
        address[0] = address_byte;
        VoteEnvelope {
            vote_address: address,
            signature: VoteSignature::default(),
            data: VoteData {
                source_number,
                source_hash: B256::from([source_number as u8; 32]),
                target_number,
                target_hash: B256::from([target_hash_byte; 32]),
            },
        }
    }

    #[test]
    fn no_conflict_for_identical_vote() {
        let mut monitor = MaliciousVoteMonitor::new();
        let vote = make_vote(1, 10, 20, 0xAA);
        assert!(!monitor.conflict_detect(&vote, 20));
        // Same vote again (same hash) → no conflict
        assert!(!monitor.conflict_detect(&vote, 20));
    }

    #[test]
    fn detects_rule1_violation() {
        let mut monitor = MaliciousVoteMonitor::new();
        let vote1 = make_vote(1, 10, 20, 0xAA);
        let vote2 = make_vote(1, 10, 20, 0xBB); // same target_number, different hash
        assert!(!monitor.conflict_detect(&vote1, 20));
        assert!(monitor.conflict_detect(&vote2, 20));
    }

    #[test]
    fn detects_rule2_violation() {
        let mut monitor = MaliciousVoteMonitor::new();
        // vote1: source=10, target=20
        let vote1 = make_vote(1, 10, 20, 0xAA);
        // vote2: source=15, target=18 — spans overlap (18 < 20 and source 15 > 10)
        let vote2 = make_vote(1, 15, 18, 0xCC);
        assert!(!monitor.conflict_detect(&vote1, 20));
        assert!(monitor.conflict_detect(&vote2, 20));
    }

    #[test]
    fn different_validators_no_conflict() {
        let mut monitor = MaliciousVoteMonitor::new();
        let vote1 = make_vote(1, 10, 20, 0xAA);
        let vote2 = make_vote(2, 10, 20, 0xBB); // different validator
        assert!(!monitor.conflict_detect(&vote1, 20));
        assert!(!monitor.conflict_detect(&vote2, 20));
    }
}
