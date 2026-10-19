//! Fixed-cardinality diagnostics for bounded import queues and work sets.

use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Copy)]
pub(crate) enum Queue {
    RemoteBlocks,
    RemoteHashes,
    RemotePropagation,
    LocalIngress,
    LocalPropagation,
    RemoteImports,
    PipelineWaiting,
    PipelineActive,
}

impl Queue {
    fn name(self) -> &'static str {
        match self {
            Self::RemoteBlocks => "remote_blocks",
            Self::RemoteHashes => "remote_hashes",
            Self::RemotePropagation => "remote_propagation",
            Self::LocalIngress => "local_ingress",
            Self::LocalPropagation => "local_propagation",
            Self::RemoteImports => "remote_imports",
            Self::PipelineWaiting => "pipeline_waiting",
            Self::PipelineActive => "pipeline_active",
        }
    }

    /// Sample occupied slots, including channel reservations, at producer/consumer polls.
    /// These gauges can miss short peaks; the full counter records every rejected admission.
    pub(crate) fn observe(self, used: usize, capacity: usize) {
        metrics::gauge!("bsc.block_import.queue_used", "queue" => self.name()).set(used as f64);
        metrics::gauge!("bsc.block_import.queue_capacity", "queue" => self.name())
            .set(capacity as f64);
    }

    pub(crate) fn full(self, capacity: usize, action: &'static str) {
        metrics::counter!("bsc.block_import.queue_full_total", "queue" => self.name()).increment(1);
        self.observe(capacity, capacity);

        // Log the first rejection and at most once per second per queue under a flood.
        // Every rejection is still counted, including those whose WARN is suppressed.
        static LAST_WARN: [AtomicU64; 8] = [const { AtomicU64::new(0) }; 8];
        let second = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
        if claim_warn_slot(&LAST_WARN[self as usize], second) {
            tracing::warn!(
                target: "bsc::block_import",
                queue = self.name(), capacity, action,
                "Block import queue full; message rejected (WARN limited to once per second per queue)"
            );
        }
    }
}

/// Claim a changed wall-clock second without requiring newer atomic update APIs.
fn claim_warn_slot(last_warn: &AtomicU64, second: u64) -> bool {
    let mut last = last_warn.load(Ordering::Relaxed);
    while last != second {
        match last_warn.compare_exchange_weak(last, second, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return true,
            Err(current) => last = current,
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warn_slot_tracks_wall_clock_changes() {
        let last_warn = AtomicU64::new(0);
        assert!(!claim_warn_slot(&last_warn, 0));
        assert!(claim_warn_slot(&last_warn, 10));
        assert!(!claim_warn_slot(&last_warn, 10));
        assert!(claim_warn_slot(&last_warn, 11));
        assert!(!claim_warn_slot(&last_warn, 11));
        // Preserve the previous behavior when the wall clock moves backwards.
        assert!(claim_warn_slot(&last_warn, 9));
        assert!(!claim_warn_slot(&last_warn, 9));
    }

    #[test]
    fn concurrent_rejections_claim_one_warn_slot() {
        let last_warn = AtomicU64::new(0);
        let barrier = std::sync::Barrier::new(16);
        std::thread::scope(|scope| {
            let workers: Vec<_> = (0..16)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        claim_warn_slot(&last_warn, 10)
                    })
                })
                .collect();
            let claimed = workers
                .into_iter()
                .map(|worker| usize::from(worker.join().unwrap()))
                .sum::<usize>();
            assert_eq!(claimed, 1);
        });
    }
}
