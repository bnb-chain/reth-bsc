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
        if LAST_WARN[self as usize]
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |last| {
                (last != second).then_some(second)
            })
            .is_ok()
        {
            tracing::warn!(
                target: "bsc::block_import",
                queue = self.name(), capacity, action,
                "Block import queue full; message rejected (WARN limited to once per second per queue)"
            );
        }
    }
}
