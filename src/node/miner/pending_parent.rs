//! Bounded early-parent notifications and execution-validity gates for speculative mining.

use alloy_consensus::Header;
use alloy_primitives::{Bytes, B256};
use reth_primitives_traits::SealedHeader;
use reth_tasks::ManualCancel;
use std::{
    sync::{
        atomic::{AtomicU8, Ordering},
        Arc, LazyLock, Weak,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::broadcast;

const PENDING: u8 = 0;
const IMPORTED: u8 = 1;
const REJECTED: u8 = 2;
static PARENTS: LazyLock<broadcast::Sender<Arc<PendingParent>>> =
    LazyLock::new(|| broadcast::channel(1).0);

/// An unverified parent is never authorization to sign its child.
#[derive(Debug)]
pub(crate) struct PendingParent {
    pub(crate) header: SealedHeader,
    pub(crate) raw: Bytes,
    status: AtomicU8,
}

impl PendingParent {
    pub(crate) fn rejected(&self) -> bool {
        self.status.load(Ordering::Acquire) == REJECTED
    }

    /// Permit computation only while the canonical tip is the base or this imported parent.
    pub(crate) fn viable(&self) -> bool {
        canonical_tip().is_some_and(|(number, hash)| self.matches_tip(number, hash))
    }

    fn matches_tip(&self, number: u64, hash: B256) -> bool {
        !self.rejected() &&
            ((number.checked_add(1) == Some(self.header.number) &&
                hash == self.header.parent_hash) ||
                (number == self.header.number && hash == self.header.hash()))
    }

    fn ready_at(&self, number: u64, hash: B256) -> bool {
        self.status.load(Ordering::Acquire) == IMPORTED &&
            number == self.header.number &&
            hash == self.header.hash()
    }

    /// Must be called again immediately before signing; a root match alone is insufficient.
    pub(crate) fn ready(&self) -> bool {
        canonical_tip().is_some_and(|(number, hash)| self.ready_at(number, hash))
    }

    /// Wait on a blocking-capable runtime thread, bounded by the job's remaining slot and cancel.
    pub(crate) fn wait(&self, cancel: &ManualCancel, deadline_ms: u128) -> bool {
        let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH) else { return false };
        let remaining = deadline_ms.saturating_sub(now.as_millis());
        let end = Instant::now() + Duration::from_millis(remaining.min(2000) as u64);
        tokio::task::block_in_place(|| self.wait_until(cancel, end, canonical_tip))
    }

    fn wait_until(
        &self,
        cancel: &ManualCancel,
        end: Instant,
        mut tip: impl FnMut() -> Option<(u64, B256)>,
    ) -> bool {
        loop {
            if cancel.is_cancelled() || Instant::now() >= end {
                return false;
            }
            let Some((number, hash)) = tip() else { return false };
            if !self.matches_tip(number, hash) {
                return false;
            }
            if self.ready_at(number, hash) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

/// Dropping an unfinished import invalidates every dependent candidate, including on cancellation.
pub(crate) struct ImportGuard(Weak<PendingParent>);

impl ImportGuard {
    pub(crate) fn imported(&self) {
        if let Some(parent) = self.0.upgrade() {
            parent.status.store(IMPORTED, Ordering::Release);
        }
    }
}

impl Drop for ImportGuard {
    fn drop(&mut self) {
        if let Some(parent) = self.0.upgrade() {
            if parent
                .status
                .compare_exchange(PENDING, REJECTED, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                metrics::counter!("bsc_parallel_parent_total", "result" => "import_rejected")
                    .increment(1);
            }
        }
    }
}

pub(crate) fn subscribe() -> broadcast::Receiver<Arc<PendingParent>> {
    PARENTS.subscribe()
}

pub(crate) fn announce(header: &Header) -> Option<ImportGuard> {
    if !matches!(std::env::var("BSC_BEP732_PARALLEL").as_deref(), Ok("1" | "true")) {
        return None;
    }
    let raw = crate::node::network::consensus_bal::get(header.hash_slow())?;
    let parent = Arc::new(PendingParent {
        header: SealedHeader::seal_slow(header.clone()),
        raw,
        status: AtomicU8::new(PENDING),
    });
    // An import future must not pin an evicted/unselected attachment outside the bounded queue.
    let guard = ImportGuard(Arc::downgrade(&parent));
    if PARENTS.send(parent).is_err() {
        return None;
    }
    Some(guard)
}

fn canonical_tip() -> Option<(u64, B256)> {
    let number = crate::shared::get_best_canonical_block_number()?;
    let header = crate::shared::get_canonical_header_by_number(number)?;
    Some((number, header.hash_slow()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parent() -> Arc<PendingParent> {
        Arc::new(PendingParent {
            header: SealedHeader::seal_slow(Header {
                number: 10,
                parent_hash: B256::repeat_byte(1),
                ..Default::default()
            }),
            raw: Bytes::new(),
            status: AtomicU8::new(PENDING),
        })
    }

    #[test]
    fn import_guard_does_not_retain_unselected_bal() {
        let parent = parent();
        let guard = ImportGuard(Arc::downgrade(&parent));
        drop(parent);
        assert!(guard.0.upgrade().is_none());
        guard.imported();
        drop(guard);
    }

    #[test]
    fn dropping_pending_import_cancels_children() {
        let parent = parent();
        let guard = ImportGuard(Arc::downgrade(&parent));
        assert!(parent.matches_tip(9, parent.header.parent_hash));
        assert!(!parent.ready_at(10, parent.header.hash()));
        drop(guard);
        assert!(parent.rejected());
        assert!(!parent.matches_tip(9, parent.header.parent_hash));
        assert!(!parent.ready_at(10, parent.header.hash()));
    }

    #[test]
    fn successful_import_still_requires_exact_canonical_tip() {
        let parent = parent();
        let guard = ImportGuard(Arc::downgrade(&parent));
        guard.imported();
        drop(guard);
        assert!(!parent.rejected());
        assert!(parent.ready_at(10, parent.header.hash()));
        assert!(!parent.ready_at(9, parent.header.parent_hash));
        assert!(!parent.ready_at(10, B256::repeat_byte(2)));
        assert!(!parent.ready_at(11, parent.header.hash()));
        assert!(!parent.matches_tip(10, B256::repeat_byte(2)));
        assert!(!parent.matches_tip(9, B256::repeat_byte(2)));
    }

    #[test]
    fn waiting_requires_import_not_just_a_matching_tip() {
        let parent = parent();
        let guard = ImportGuard(Arc::downgrade(&parent));
        let mut polls = 0;
        assert!(parent.wait_until(
            &ManualCancel::default(),
            Instant::now() + Duration::from_secs(1),
            || {
                polls += 1;
                if polls == 2 {
                    guard.imported();
                }
                Some((10, parent.header.hash()))
            }
        ));
        assert_eq!(polls, 2);
    }

    #[test]
    fn waits_end_on_reorg_cancellation_deadline_or_missing_provider() {
        let parent = parent();
        let deadline = Instant::now() + Duration::from_secs(1);
        let cancel = ManualCancel::default();
        assert!(!parent.wait_until(&cancel, deadline, || Some((10, B256::repeat_byte(2)))));
        assert!(!parent.wait_until(&cancel, deadline, || None));
        assert!(!parent
            .wait_until(&cancel, Instant::now(), || panic!("expired wait must not read provider")));
        cancel.clone().cancel();
        assert!(!parent
            .wait_until(&cancel, deadline, || panic!("cancelled wait must not read provider")));
    }

    #[test]
    fn invalidation_interrupts_an_active_wait() {
        let parent = parent();
        let mut guard = Some(ImportGuard(Arc::downgrade(&parent)));
        let mut polls = 0;
        assert!(!parent.wait_until(
            &ManualCancel::default(),
            Instant::now() + Duration::from_secs(1),
            || {
                polls += 1;
                if polls == 2 {
                    drop(guard.take());
                }
                Some((9, parent.header.parent_hash))
            }
        ));
        assert_eq!(polls, 2);
    }
}
