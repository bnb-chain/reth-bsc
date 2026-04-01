use crate::consensus::parlia::snapshot::Snapshot;
use crate::node::miner::bsc_miner::MiningContext;
use alloy_primitives::B256;
use reth_primitives::SealedHeader;
use reth_revm::cached::CachedReads;
use rust_eth_triedb_common::DiffLayers;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MiningContextSource {
    Canonical,
    Speculative,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingLocalHead {
    pub block_number: u64,
    pub block_hash: B256,
    pub parent_hash: B256,
    pub durable_base_hash: B256,
    pub child_spawned: bool,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PendingLocalHeadTracker {
    current: Option<PendingLocalHead>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconcileDecision {
    KeepPending,
    ClearPending,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextDecision {
    UseCanonical,
    KeepSpeculative,
    ClearAndAbortSpeculative,
}

impl PendingLocalHeadTracker {
    pub fn current(&self) -> Option<&PendingLocalHead> {
        self.current.as_ref()
    }

    pub fn record_submitted_head(
        &mut self,
        pending_head: PendingLocalHead,
    ) -> Option<PendingLocalHead> {
        self.current.replace(pending_head)
    }

    pub fn can_spawn_child(&self, child_block_number: u64) -> bool {
        self.current.as_ref().is_some_and(|head| {
            !head.child_spawned && child_block_number == head.block_number.saturating_add(1)
        })
    }

    pub fn mark_child_spawned(&mut self) -> bool {
        if let Some(head) = self.current.as_mut() {
            if !head.child_spawned {
                head.child_spawned = true;
                return true;
            }
        }

        false
    }

    pub fn clear(&mut self) -> Option<PendingLocalHead> {
        self.current.take()
    }

    pub fn reconcile_canonical_head(
        &mut self,
        canonical_hash: B256,
        canonical_number: u64,
    ) -> ReconcileDecision {
        let should_clear = self.current.as_ref().is_some_and(|head| {
            canonical_number >= head.block_number && canonical_hash != head.block_hash
        });

        if should_clear {
            self.current = None;
            ReconcileDecision::ClearPending
        } else {
            ReconcileDecision::KeepPending
        }
    }
}

impl From<PendingLocalHead> for PendingLocalHeadTracker {
    fn from(head: PendingLocalHead) -> Self {
        Self { current: Some(head) }
    }
}

pub fn derive_speculative_child_context(
    parent_header: SealedHeader,
    parent_snapshot: Arc<Snapshot>,
    is_inturn: bool,
    cached_reads: Option<CachedReads>,
    parent_difflayers: Option<DiffLayers>,
    durable_base_hash: B256,
    prev_bundle_state: Option<revm::database::BundleState>,
) -> MiningContext {
    MiningContext {
        header: None,
        parent_header,
        parent_snapshot,
        is_inturn,
        cached_reads,
        parent_difflayers,
        source: MiningContextSource::Speculative,
        state_base_hash: Some(durable_base_hash),
        prev_bundle_state,
    }
}

pub fn choose_next_context(
    canonical_ctx: &MiningContext,
    speculative_ctx: Option<&MiningContext>,
) -> ContextDecision {
    if canonical_ctx.source != MiningContextSource::Canonical {
        return ContextDecision::UseCanonical;
    }

    match speculative_ctx {
        Some(speculative_ctx)
            if speculative_ctx.source == MiningContextSource::Speculative
                && speculative_ctx.parent_header.hash() == canonical_ctx.parent_header.hash() =>
        {
            ContextDecision::KeepSpeculative
        }
        Some(_) | None => ContextDecision::UseCanonical,
    }
}

pub fn on_canonical_tip(
    tracker: &mut PendingLocalHeadTracker,
    canonical_hash: B256,
    canonical_number: u64,
) -> ContextDecision {
    match tracker.reconcile_canonical_head(canonical_hash, canonical_number) {
        ReconcileDecision::ClearPending => ContextDecision::ClearAndAbortSpeculative,
        ReconcileDecision::KeepPending => {
            tracker.current().map_or(ContextDecision::UseCanonical, |head| {
                if head.block_hash == canonical_hash
                    && head.block_number == canonical_number
                    && head.child_spawned
                {
                    ContextDecision::KeepSpeculative
                } else {
                    ContextDecision::UseCanonical
                }
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::Header;
    use alloy_primitives::Address;
    use alloy_primitives::B256;
    use reth_primitives::SealedHeader;
    use std::sync::Arc;

    use super::{
        choose_next_context, derive_speculative_child_context, on_canonical_tip, ContextDecision,
        MiningContextSource, PendingLocalHead, PendingLocalHeadTracker, ReconcileDecision,
    };
    use crate::consensus::parlia::Snapshot;
    use crate::node::miner::bsc_miner::MiningContext;

    fn example_hash(number: u64) -> B256 {
        B256::with_last_byte(number as u8)
    }

    fn example_pending_head(block_number: u64, durable_base_number: u64) -> PendingLocalHead {
        PendingLocalHead {
            block_number,
            block_hash: example_hash(block_number),
            parent_hash: example_hash(block_number.saturating_sub(1)),
            durable_base_hash: example_hash(durable_base_number),
            child_spawned: false,
        }
    }

    fn example_ctx(
        source: MiningContextSource,
        parent_number: u64,
        parent_hash_number: u64,
    ) -> MiningContext {
        let parent_hash = example_hash(parent_hash_number);
        let parent_header = Header {
            number: parent_number,
            parent_hash: example_hash(parent_number.saturating_sub(1)),
            beneficiary: Address::with_last_byte(1),
            ..Default::default()
        };
        let parent_snapshot =
            Snapshot::new(vec![Address::with_last_byte(1)], parent_number, parent_hash, 200, None);

        MiningContext {
            header: None,
            parent_header: SealedHeader::new(parent_header, parent_hash),
            parent_snapshot: Arc::new(parent_snapshot),
            is_inturn: true,
            cached_reads: None,
            parent_difflayers: None,
            source,
            state_base_hash: (source == MiningContextSource::Speculative)
                .then_some(example_hash(parent_number.saturating_sub(1))),
            prev_bundle_state: None,
        }
    }

    #[test]
    fn pending_local_head_allows_only_one_speculative_child() {
        let mut tracker = PendingLocalHeadTracker::default();
        assert!(tracker.record_submitted_head(example_pending_head(100, 99)).is_none());
        assert!(tracker.can_spawn_child(101));
        assert!(!tracker.can_spawn_child(102));
    }

    #[test]
    fn canonical_mismatch_clears_speculative_state() {
        let mut tracker = PendingLocalHeadTracker::from(example_pending_head(100, 99));
        let decision = tracker.reconcile_canonical_head(example_hash(200), 100);
        assert_eq!(decision, ReconcileDecision::ClearPending);
        assert!(tracker.current().is_none());
    }

    #[test]
    fn canonical_context_wins_over_stale_speculative_context() {
        let canonical_ctx = example_ctx(MiningContextSource::Canonical, 100, 100);
        let speculative_ctx = example_ctx(MiningContextSource::Speculative, 100, 200);

        let decision = choose_next_context(&canonical_ctx, Some(&speculative_ctx));

        assert_eq!(decision, ContextDecision::UseCanonical);
    }

    #[test]
    fn mismatched_canonical_tip_clears_pending_head_and_aborts_child() {
        let mut tracker = PendingLocalHeadTracker::from(example_pending_head(100, 99));

        assert_eq!(
            on_canonical_tip(&mut tracker, example_hash(500), 100),
            ContextDecision::ClearAndAbortSpeculative
        );
        assert!(tracker.current().is_none());
    }

    #[test]
    fn derive_speculative_child_context_keeps_durable_base_on_parent() {
        let parent_hash = example_hash(100);
        let parent_header = Header {
            number: 100,
            parent_hash: example_hash(99),
            beneficiary: Address::with_last_byte(1),
            ..Default::default()
        };
        let parent_snapshot =
            Snapshot::new(vec![Address::with_last_byte(1)], 100, parent_hash, 200, None);

        let ctx = derive_speculative_child_context(
            SealedHeader::new(parent_header, parent_hash),
            Arc::new(parent_snapshot),
            true,
            None,
            None,
            example_hash(99),
            None,
        );

        assert_eq!(ctx.parent_header.number, 100);
        assert_eq!(ctx.state_base_hash, Some(example_hash(99)));
        assert_eq!(ctx.source, MiningContextSource::Speculative);
    }
}
