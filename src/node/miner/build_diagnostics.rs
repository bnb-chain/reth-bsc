//! Aggregate attempt accounting. No per-transaction logging or high-cardinality labels.

use super::super::engine::BscBuiltPayload;
use alloy_consensus::BlockHeader;
use reth_primitives_traits::BlockBody;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Instant,
};

/// Work source, independent of consensus turn and of the eventually chosen payload.
#[derive(Debug, Clone, Copy)]
pub(crate) enum AttemptKind {
    Speculative,
    Regular,
    Rebuild,
    Empty,
}
impl AttemptKind {
    const fn label(self) -> &'static str {
        match self {
            Self::Speculative => "speculative",
            Self::Regular => "regular",
            Self::Rebuild => "rebuild",
            Self::Empty => "empty",
        }
    }
}

/// Metadata follows a candidate through cloning and finalization; it retains no task resources.
#[derive(Debug)]
pub(crate) struct CandidateDiagnostics {
    pub(crate) id: u64,
    pub(crate) kind: &'static str,
    turn: &'static str,
    selected: AtomicBool,
    execution_seconds: f64,
    gas: u64,
    transactions: usize,
}
impl CandidateDiagnostics {
    /// Selection is counted once even if a candidate is cloned. This is not chain inclusion.
    pub(crate) fn selected(&self) {
        if !self.selected.swap(true, Ordering::Relaxed) {
            metrics::counter!("bsc.build_diagnostics.selected_total", "kind" => self.kind, "turn" => self.turn).increment(1);
        }
    }
}
impl Drop for CandidateDiagnostics {
    fn drop(&mut self) {
        if !self.selected.load(Ordering::Relaxed) {
            metrics::counter!("bsc.build_diagnostics.unselected_completed_total", "kind" => self.kind, "turn" => self.turn).increment(1);
            metrics::histogram!("bsc.build_diagnostics.unselected_execution_seconds", "kind" => self.kind, "turn" => self.turn).record(self.execution_seconds);
            metrics::histogram!("bsc.build_diagnostics.unselected_gas", "kind" => self.kind, "turn" => self.turn).record(self.gas as f64);
            metrics::histogram!("bsc.build_diagnostics.unselected_transactions", "kind" => self.kind, "turn" => self.turn).record(self.transactions as f64);
        }
    }
}

/// Tracks queueing and execution even when a future is dropped before it is polled.
pub(crate) struct AttemptObservation {
    kind: &'static str,
    turn: &'static str,
    queued: Instant,
    started: Option<Instant>,
    finished: bool,
}
impl AttemptObservation {
    pub(crate) fn new(kind: AttemptKind, inturn: bool) -> Self {
        let this = Self {
            kind: kind.label(),
            turn: if inturn { "inturn" } else { "outofturn" },
            queued: Instant::now(),
            started: None,
            finished: false,
        };
        metrics::counter!("bsc.build_diagnostics.submitted_total", "kind" => this.kind, "turn" => this.turn).increment(1);
        metrics::gauge!("bsc.build_diagnostics.queued", "kind" => this.kind, "turn" => this.turn)
            .increment(1.0);
        this
    }
    pub(crate) fn start(&mut self) {
        self.started = Some(Instant::now());
        metrics::gauge!("bsc.build_diagnostics.queued", "kind" => self.kind, "turn" => self.turn)
            .decrement(1.0);
        metrics::gauge!("bsc.build_diagnostics.active", "kind" => self.kind, "turn" => self.turn)
            .increment(1.0);
        metrics::histogram!("bsc.build_diagnostics.queue_seconds", "kind" => self.kind, "turn" => self.turn).record(self.queued.elapsed().as_secs_f64());
    }
    pub(crate) fn finish(&mut self, payload: Option<&mut BscBuiltPayload>, cancel_requested: bool) {
        self.finished = true;
        let outcome = match (payload.is_some(), cancel_requested) {
            (true, false) => "completed",
            (true, true) => "completed_after_cancel",
            (false, false) => "error",
            (false, true) => "error_after_cancel",
        };
        metrics::counter!("bsc.build_diagnostics.finished_total", "kind" => self.kind, "turn" => self.turn, "outcome" => outcome).increment(1);
        if let Some(payload) = payload {
            static NEXT_ID: AtomicU64 = AtomicU64::new(1);
            metrics::histogram!("bsc.build_diagnostics.execution_seconds", "kind" => self.kind, "turn" => self.turn).record(payload.exec_duration.as_secs_f64());
            metrics::histogram!("bsc.build_diagnostics.root_seconds", "kind" => self.kind, "turn" => self.turn).record(payload.trie_root_duration.as_secs_f64());
            metrics::histogram!("bsc.build_diagnostics.transactions", "kind" => self.kind, "turn" => self.turn).record(payload.block.body().transaction_count() as f64);
            metrics::histogram!("bsc.build_diagnostics.gas", "kind" => self.kind, "turn" => self.turn).record(payload.block.header().gas_used() as f64);
            payload.diagnostics = Some(Arc::new(CandidateDiagnostics {
                id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
                kind: self.kind,
                turn: self.turn,
                execution_seconds: payload.exec_duration.as_secs_f64(),
                gas: payload.block.header().gas_used(),
                transactions: payload.block.body().transaction_count(),
                selected: AtomicBool::new(false),
            }));
        }
    }
}
impl Drop for AttemptObservation {
    fn drop(&mut self) {
        if let Some(started) = self.started {
            metrics::gauge!("bsc.build_diagnostics.active", "kind" => self.kind, "turn" => self.turn).decrement(1.0);
            metrics::histogram!("bsc.build_diagnostics.run_seconds", "kind" => self.kind, "turn" => self.turn).record(started.elapsed().as_secs_f64());
        } else {
            metrics::gauge!("bsc.build_diagnostics.queued", "kind" => self.kind, "turn" => self.turn).decrement(1.0);
        }
        if !self.finished {
            let outcome = if self.started.is_some() { "dropped_running" } else { "dropped_queued" };
            metrics::counter!("bsc.build_diagnostics.finished_total", "kind" => self.kind, "turn" => self.turn, "outcome" => outcome).increment(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use metrics_exporter_prometheus::PrometheusBuilder;

    #[test]
    fn queued_drop_and_running_error_balance_gauges() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            drop(AttemptObservation::new(AttemptKind::Speculative, false));
            let mut running = AttemptObservation::new(AttemptKind::Regular, true);
            running.start();
            running.finish(None, true);
        });
        let rendered = handle.render();
        assert!(rendered.contains("outcome=\"dropped_queued\""));
        assert!(rendered.contains("outcome=\"error_after_cancel\""));
        for line in rendered.lines().filter(|l| {
            l.starts_with("bsc_build_diagnostics_queued{") ||
                l.starts_with("bsc_build_diagnostics_active{")
        }) {
            assert_eq!(line.split_whitespace().last(), Some("0"));
        }
    }

    #[test]
    fn candidate_clones_count_selection_once_and_last_drop_counts_discard() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            let candidate = Arc::new(CandidateDiagnostics {
                id: 1,
                kind: "speculative",
                turn: "outofturn",
                selected: AtomicBool::new(false),
                execution_seconds: 0.5,
                gas: 100,
                transactions: 2,
            });
            let clone = candidate.clone();
            candidate.selected();
            clone.selected();
            drop(candidate);
            drop(clone);
            let candidate = Arc::new(CandidateDiagnostics {
                id: 2,
                kind: "speculative",
                turn: "outofturn",
                selected: AtomicBool::new(false),
                execution_seconds: 0.5,
                gas: 100,
                transactions: 2,
            });
            let clone = candidate.clone();
            drop(candidate);
            drop(clone);
        });
        let rendered = handle.render();
        for name in ["selected_total", "unselected_completed_total"] {
            let prefix = format!("bsc_build_diagnostics_{name}{{");
            let line = rendered.lines().find(|l| l.starts_with(&prefix)).unwrap();
            assert_eq!(line.split_whitespace().last(), Some("1"));
        }
    }
}
