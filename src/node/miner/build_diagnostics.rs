//! Aggregate attempt accounting. No per-transaction logging or high-cardinality labels.

use super::super::engine::BscBuiltPayload;
use alloy_consensus::BlockHeader;
use reth_primitives_traits::BlockBody;
use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
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
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Speculative => "speculative",
            Self::Regular => "regular",
            Self::Rebuild => "rebuild",
            Self::Empty => "empty",
        }
    }
}

/// Fixed skip categories. These describe this candidate's iterator, not pool eviction or
/// unique invalid transactions. Retries may encounter the same transaction more than once.
#[derive(Clone, Copy)]
pub(crate) enum SkipReason {
    NonceTooLow,
    InvalidTransaction,
    GasLimit,
    Underpriced,
    Blacklisted,
    BlobLimit,
    BlobIneligible,
    BlobSidecar,
}
const SKIP_REASONS: [&str; 8] = [
    "nonce_too_low",
    "invalid_transaction",
    "gas_limit",
    "underpriced",
    "blacklisted",
    "blob_limit",
    "blob_ineligible",
    "blob_sidecar",
];

/// Per-attempt wall-clock accounting, including failed/cancelled attempts via Drop.
/// Only normal transaction-building attempts enter here; empty fallback bypasses this path.
/// All histograms use seconds. `kind` and `turn` match AttemptObservation's labels; IDs are
/// log fields only. Per-transaction work updates local integers, not metric handles/logs.
pub(crate) struct BuildWorkObservation {
    kind: &'static str,
    turn: &'static str,
    trace_id: u64,
    block_number: u64,
    started: Instant,
    acquired: Option<Instant>,
    executing: Option<Instant>,
    stopped: Option<Instant>,
    transaction_deadline: Option<Instant>,
    finish_deadline: Option<Instant>,
    reason: &'static str,
    completed: bool,
    pub(crate) scanned: u64,
    pub(crate) executed: u64,
    skipped: [u64; 8],
    skipped_time: Duration,
    first_invalid_error: Option<String>,
}
impl BuildWorkObservation {
    pub(crate) fn new(
        kind: AttemptKind,
        inturn: bool,
        trace_id: u64,
        block_number: u64,
        control: Option<&super::build_control::BuildControl>,
    ) -> Self {
        let started = Instant::now();
        // In-flight acquisition count, so a still-blocked task is visible before its
        // duration sample is emitted. No-control callers immediately mark acquisition.
        metrics::gauge!("bsc.build_diagnostics.permit_waiters", "kind" => kind.label(),
            "turn" => if inturn { "inturn" } else { "outofturn" })
        .increment(1.0);
        if let Some(control) = control {
            // Selection time left at attempt entry, before semaphore acquisition/setup.
            // Rebuilds share the job's deadline, so they may enter with a smaller window.
            metrics::histogram!("bsc.build_diagnostics.attempt_transaction_window_seconds", "kind" => kind.label(),
                "turn" => if inturn { "inturn" } else { "outofturn" })
                .record(control.transactions_until.saturating_duration_since(started).as_secs_f64());
        }
        Self {
            kind: kind.label(),
            turn: if inturn { "inturn" } else { "outofturn" },
            trace_id,
            block_number,
            started,
            acquired: None,
            executing: None,
            stopped: None,
            transaction_deadline: control.map(|c| c.transactions_until),
            finish_deadline: control.map(|c| c.finish_until),
            reason: "incomplete",
            completed: false,
            scanned: 0,
            executed: 0,
            skipped: [0; 8],
            skipped_time: Duration::ZERO,
            first_invalid_error: None,
        }
    }

    pub(crate) fn permit_acquired(&mut self) {
        self.acquired = Some(Instant::now());
        metrics::gauge!("bsc.build_diagnostics.permit_waiters", "kind" => self.kind, "turn" => self.turn).decrement(1.0);
    }

    pub(crate) fn execution_started(&mut self, now: Instant) {
        self.executing = Some(now);
        if let Some(deadline) = self.transaction_deadline {
            // Remaining selection window at the actual loop entry, not configured reserve.
            // Expired windows record zero; attempts that never reach the loop have no sample.
            metrics::histogram!("bsc.build_diagnostics.execution_window_seconds", "kind" => self.kind, "turn" => self.turn)
                .record(deadline.saturating_duration_since(now).as_secs_f64());
        }
    }

    pub(crate) fn skip(&mut self, reason: SkipReason, iteration_started: Instant) {
        self.skipped[reason as usize] += 1;
        // Includes iterator selection, validation, existing logging and invalidation for
        // the explicitly skipped transaction; this is a subset of selection_wall_seconds.
        // Excludes descendants silently removed by mark_invalid, and is NOT pure EVM CPU time.
        self.skipped_time += iteration_started.elapsed();
    }

    /// Keep one non-nonce-low validation error per attempt, never one allocation per tx.
    /// This is a log field only; arbitrary error messages must never become metric labels.
    pub(crate) fn invalid_error(&mut self, error: &impl std::fmt::Display) {
        if self.first_invalid_error.is_none() {
            self.first_invalid_error = Some(error.to_string());
        }
    }

    pub(crate) fn selection_stopped(&mut self, reason: &'static str) {
        self.stopped = Some(Instant::now());
        self.reason = reason;
    }

    pub(crate) fn completed(&mut self) {
        self.completed = true;
    }
}

// One diagnostic line per process per 10 seconds, across all attempt kinds/turns.
// Monotonic time avoids clock adjustments; metric counts are never rate limited.
fn claim_slow_log(last: &AtomicU64, now_ms: u64) -> bool {
    let previous = last.load(Ordering::Relaxed);
    if previous != 0 && now_ms.saturating_sub(previous) < 10_000 {
        return false;
    }
    last.compare_exchange(previous, now_ms, Ordering::Relaxed, Ordering::Relaxed).is_ok()
}

impl Drop for BuildWorkObservation {
    fn drop(&mut self) {
        let now = Instant::now();
        if self.acquired.is_none() {
            metrics::gauge!("bsc.build_diagnostics.permit_waiters", "kind" => self.kind, "turn" => self.turn).decrement(1.0);
        }
        let permit = self.acquired.unwrap_or(now).duration_since(self.started);
        let prepare = self.acquired.map(|a| self.executing.unwrap_or(now).duration_since(a));
        let execution = self.executing.map(|e| self.stopped.unwrap_or(now).duration_since(e));
        let finish = self.stopped.map(|s| now.duration_since(s));
        // Unlike queue_seconds (spawn -> first poll), permit_wait measures the semaphore
        // acquire await. `interrupted` samples end on drop before acquisition; not successes.
        metrics::histogram!("bsc.build_diagnostics.permit_wait_seconds", "kind" => self.kind, "turn" => self.turn,
            "status" => if self.acquired.is_some() { "acquired" } else { "interrupted" })
            .record(permit.as_secs_f64());
        if let Some(prepare) = prepare {
            // Permit acquisition -> transaction loop entry, excluding permit wait. Includes
            // state/provider/root-hook setup. Interrupted setup records partial elapsed time.
            metrics::histogram!("bsc.build_diagnostics.prepare_seconds", "kind" => self.kind, "turn" => self.turn,
                "status" => if self.executing.is_some() { "complete" } else { "interrupted" })
                .record(prepare.as_secs_f64());
        }
        if let Some(execution) = execution {
            // Loop wall time INCLUDING invalid scans and interrupted/failed attempts.
            // Existing execution_seconds only samples successfully completed payloads.
            metrics::histogram!("bsc.build_diagnostics.selection_wall_seconds", "kind" => self.kind, "turn" => self.turn,
                "status" => if self.stopped.is_some() { "complete" } else { "interrupted" })
                .record(execution.as_secs_f64());
        }
        // Aggregate once per attempt, including early errors and dropped futures. Scanned
        // counts iterator yields (not unique hashes); executed counts valid user executions,
        // including EVM reverts. A fatal execution error can leave scanned > executed+skipped.
        metrics::counter!("bsc.build_diagnostics.scanned_transactions_total", "kind" => self.kind, "turn" => self.turn).increment(self.scanned);
        metrics::counter!("bsc.build_diagnostics.executed_transactions_total", "kind" => self.kind, "turn" => self.turn).increment(self.executed);
        for (reason, count) in SKIP_REASONS.iter().zip(self.skipped) {
            metrics::counter!("bsc.build_diagnostics.skipped_transactions_total", "kind" => self.kind, "turn" => self.turn, "reason" => *reason).increment(count);
        }
        // Wall time is fractional; a histogram's sum exposes total skipped time without
        // rounding it to integer counter units. Zero-skip attempts also record zero.
        metrics::histogram!("bsc.build_diagnostics.skipped_transaction_seconds", "kind" => self.kind, "turn" => self.turn).record(self.skipped_time.as_secs_f64());
        let window = self
            .executing
            .zip(self.transaction_deadline)
            .map(|(e, d)| d.saturating_duration_since(e));
        let completion_deadline_elapsed =
            !self.completed && self.finish_deadline.is_some_and(|d| now >= d);
        let abnormal = permit >= Duration::from_millis(20) ||
            prepare.is_some_and(|p| p >= Duration::from_millis(20)) ||
            window.is_some_and(|w| w < Duration::from_millis(50)) ||
            self.skipped_time >= Duration::from_millis(20) ||
            completion_deadline_elapsed;
        if abnormal {
            static EPOCH: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
            static LAST_LOG: AtomicU64 = AtomicU64::new(0);
            let tick = EPOCH.get_or_init(Instant::now).elapsed().as_millis() as u64 + 1;
            if claim_slow_log(&LAST_LOG, tick) {
                tracing::info!(target: "bsc::miner::diagnostics",
                    trace_id = self.trace_id, block_number = self.block_number,
                    kind = self.kind, turn = self.turn, completed = self.completed, completion_deadline_elapsed,
                    stop_reason = self.reason, permit_acquired = self.acquired.is_some(),
                    permit_wait_ms = permit.as_secs_f64() * 1000.0,
                    prepare_ms = ?prepare.map(|d| d.as_secs_f64() * 1000.0),
                    initial_transaction_window_ms = ?self.transaction_deadline.map(|d| d.saturating_duration_since(self.started).as_secs_f64() * 1000.0),
                    execution_window_ms = ?window.map(|d| d.as_secs_f64() * 1000.0),
                    execution_ms = ?execution.map(|d| d.as_secs_f64() * 1000.0),
                    post_selection_ms = ?finish.map(|d| d.as_secs_f64() * 1000.0),
                    scanned = self.scanned, executed = self.executed,
                    nonce_too_low = self.skipped[SkipReason::NonceTooLow as usize],
                    invalid_transaction = self.skipped[SkipReason::InvalidTransaction as usize],
                    first_invalid_error = ?self.first_invalid_error,
                    gas_limit_skips = self.skipped[SkipReason::GasLimit as usize],
                    other_skips = self.skipped[3..].iter().sum::<u64>(),
                    skipped_ms = self.skipped_time.as_secs_f64() * 1000.0,
                    "Sampled mining build budget breakdown");
            }
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
    fn interrupted_builds_keep_work_counts_and_distinguish_permit_from_prepare() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            // Dropped while waiting for a permit: no preparation/selection samples.
            drop(BuildWorkObservation::new(AttemptKind::Speculative, false, 1, 1, None));
            let mut failed = BuildWorkObservation::new(AttemptKind::Regular, true, 2, 1, None);
            failed.permit_acquired();
            failed.execution_started(Instant::now());
            failed.scanned = 3;
            failed.executed = 1;
            failed.skip(SkipReason::NonceTooLow, Instant::now());
            failed.skip(SkipReason::InvalidTransaction, Instant::now());
            // Simulate an early return/future drop, without selection_stopped/completed.
            drop(failed);
        });
        let rendered = handle.render();
        let line = |name: &str, labels: &[&str]| -> String {
            rendered
                .lines()
                .find(|l| l.starts_with(name) && labels.iter().all(|v| l.contains(v)))
                .unwrap_or_else(|| panic!("missing {name}: {rendered}"))
                .to_owned()
        };
        assert!(line(
            "bsc_build_diagnostics_permit_wait_seconds_count",
            &["speculative", "interrupted"]
        )
        .ends_with(" 1"));
        assert!(!rendered
            .lines()
            .any(|l| l.starts_with("bsc_build_diagnostics_prepare_seconds") &&
                l.contains("speculative")));
        assert!(line("bsc_build_diagnostics_prepare_seconds_count", &["regular", "complete"])
            .ends_with(" 1"));
        assert!(line(
            "bsc_build_diagnostics_selection_wall_seconds_count",
            &["regular", "interrupted"]
        )
        .ends_with(" 1"));
        assert!(
            line("bsc_build_diagnostics_scanned_transactions_total", &["regular"]).ends_with(" 3")
        );
        assert!(
            line("bsc_build_diagnostics_executed_transactions_total", &["regular"]).ends_with(" 1")
        );
        assert!(line(
            "bsc_build_diagnostics_skipped_transactions_total",
            &["regular", "nonce_too_low"]
        )
        .ends_with(" 1"));
        assert!(line(
            "bsc_build_diagnostics_skipped_transactions_total",
            &["regular", "invalid_transaction"]
        )
        .ends_with(" 1"));
        for l in rendered.lines().filter(|l| l.starts_with("bsc_build_diagnostics_permit_waiters{"))
        {
            assert!(l.ends_with(" 0"), "{l}");
        }
        assert!(!rendered.contains("trace_id="));
        assert!(!rendered.contains("block_number="));
    }

    #[test]
    fn execution_window_excludes_time_spent_waiting_and_expired_windows_are_zero() {
        use super::super::build_control::BuildControl;
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            let now = Instant::now();
            let control = BuildControl::new(
                now,
                Duration::from_millis(450),
                Duration::from_millis(120),
                Duration::from_secs(1),
                Default::default(),
            );
            let mut work =
                BuildWorkObservation::new(AttemptKind::Regular, true, 1, 1, Some(&control));
            work.permit_acquired();
            work.execution_started(control.transactions_until - Duration::from_millis(80));
            // Only the observation boundary is synthetic; reset before Drop's elapsed timing.
            work.executing = Some(Instant::now());
            work.selection_stopped("transaction_deadline");
            work.completed();
            drop(work);
            let mut expired =
                BuildWorkObservation::new(AttemptKind::Rebuild, true, 1, 1, Some(&control));
            expired.permit_acquired();
            expired.execution_started(control.transactions_until + Duration::from_millis(1));
            expired.executing = Some(Instant::now());
        });
        let rendered = handle.render();
        assert!(rendered
            .lines()
            .any(|l| l.starts_with("bsc_build_diagnostics_execution_window_seconds_sum") &&
                l.contains("regular") &&
                l.ends_with(" 0.08")));
        assert!(rendered
            .lines()
            .any(|l| l.starts_with("bsc_build_diagnostics_execution_window_seconds_sum") &&
                l.contains("rebuild") &&
                l.ends_with(" 0")));
    }

    #[test]
    fn phase_timings_do_not_double_count_permit_wait_as_preparation() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            let now = Instant::now();
            let mut work = BuildWorkObservation::new(AttemptKind::Regular, true, 1, 1, None);
            work.permit_acquired();
            work.started = now - Duration::from_millis(100);
            work.acquired = Some(now - Duration::from_millis(80));
            work.executing = Some(now - Duration::from_millis(30));
            work.stopped = Some(now - Duration::from_millis(10));
            work.completed();
        });
        let rendered = handle.render();
        for (metric, value) in
            [("permit_wait", "0.02"), ("prepare", "0.05"), ("selection_wall", "0.02")]
        {
            assert!(
                rendered.lines().any(|l| l
                    .starts_with(&format!("bsc_build_diagnostics_{metric}_seconds_sum")) &&
                    l.ends_with(&format!(" {value}"))),
                "{rendered}"
            );
        }
    }

    #[test]
    fn slow_log_limiter_is_shared_and_allows_one_claim_per_interval() {
        let last = Arc::new(AtomicU64::new(0));
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let last = last.clone();
                std::thread::spawn(move || claim_slow_log(&last, 1))
            })
            .collect();
        assert_eq!(workers.into_iter().map(|w| usize::from(w.join().unwrap())).sum::<usize>(), 1);
        assert!(!claim_slow_log(&last, 10_000));
        assert!(claim_slow_log(&last, 10_001));
        assert!(!claim_slow_log(&last, 10_001));
    }

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
