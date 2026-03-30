use alloy_primitives::B256;

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

#[cfg(test)]
mod tests {
    use alloy_primitives::B256;

    use super::{PendingLocalHead, PendingLocalHeadTracker, ReconcileDecision};

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
}
