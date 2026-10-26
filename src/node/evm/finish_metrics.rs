//! Miner finish() wall-clock accounting. No transaction/hash metric labels. Slow root waits have
//! rate-limited logs.
//!
//! `bsc.build_diagnostics.finish_stage_seconds{kind,turn,stage,outcome}` records each
//! entered phase, including its elapsed time on early error/unwind. `completed` means the
//! phase returned, not that the whole candidate succeeded; `interrupted` means finish exited
//! inside it. Untaken phases have no sample (in particular root_wait and sync_root).
//!
//! Stages are sequential, not nested: entry (initial budget check), system_transactions
//! (executor.finish + EVM extraction), close_state_stream (close hook + budget check),
//! root_wait (handle/receiver wait, including deadline checks), root_result (unwrap/clone
//! delivered trie updates and store result), merge_state (extract system txs + merge transitions
//! and budget check), hash_state (hashed_post_state), root_select (retrieve precomputed root),
//! sync_root (fallback guards + synchronous calculation + final budget check), assemble
//! (transaction/sender preparation, body assembly, cache sinks and recovered block).
//! The root_select phase also includes the post-root budget check on the precomputed path.
//!
//! `finish_total_seconds{kind,turn,outcome}` includes failed attempts, unlike root_seconds.
//! `finish_exits_total{kind,turn,stage,outcome,budget}` attributes the final exit. Budget is
//! the status at exit (not proof of the error cause): none, active, deadline_elapsed, cancelled.
//! These are foreground wall times, not CPU times or total background root task lifetimes.
//! No final header signing, candidate picking or submission is included here.

use crate::node::miner::build_control::BuildControl;
use std::time::Instant;

pub(super) struct FinishObservation {
    labels: Option<(&'static str, &'static str)>,
    control: Option<BuildControl>,
    started: Instant,
    phase_started: Instant,
    stage: &'static str,
    completed: bool,
}

impl FinishObservation {
    pub(super) fn new(
        labels: Option<(&'static str, &'static str)>,
        control: Option<BuildControl>,
    ) -> Self {
        let now = Instant::now();
        Self { labels, control, started: now, phase_started: now, stage: "entry", completed: false }
    }

    fn record_phase(&self, now: Instant, outcome: &'static str) {
        if let Some((kind, turn)) = self.labels {
            metrics::histogram!("bsc.build_diagnostics.finish_stage_seconds",
                "kind" => kind, "turn" => turn, "stage" => self.stage, "outcome" => outcome)
            .record(now.duration_since(self.phase_started).as_secs_f64());
        }
    }

    pub(super) fn enter(&mut self, stage: &'static str) {
        let now = Instant::now();
        self.record_phase(now, "completed");
        self.stage = stage;
        self.phase_started = now;
    }

    pub(super) fn complete(&mut self) {
        self.completed = true;
    }
}

impl Drop for FinishObservation {
    fn drop(&mut self) {
        let Some((kind, turn)) = self.labels else { return };
        let now = Instant::now();
        let outcome = if self.completed {
            "completed"
        } else if std::thread::panicking() {
            "panic"
        } else {
            "error"
        };
        self.record_phase(now, if self.completed { "completed" } else { "interrupted" });
        let budget =
            self.control.as_ref().map_or("none", |control| match control.check_active(now) {
                Ok(()) => "active",
                Err("payload build canceled") => "cancelled",
                Err(_) => "deadline_elapsed",
            });
        metrics::histogram!("bsc.build_diagnostics.finish_total_seconds",
            "kind" => kind, "turn" => turn, "outcome" => outcome)
        .record(now.duration_since(self.started).as_secs_f64());
        metrics::counter!("bsc.build_diagnostics.finish_exits_total",
            "kind" => kind, "turn" => turn, "stage" => self.stage,
            "outcome" => outcome, "budget" => budget)
        .increment(1);
    }
}

/// Foreground receive attribution only. The opaque Reth payload handle does not expose its
/// proof-session snapshot; do not infer a per-build background breakdown from these samples.
pub(super) struct RootWaitObservation {
    labels: Option<(&'static str, &'static str)>,
    control: Option<BuildControl>,
    block_number: u64,
    parent_hash: alloy_primitives::B256,
    root_task_id: u64,
    started: Instant,
    outcome: &'static str,
}

impl RootWaitObservation {
    pub(super) fn new(
        labels: Option<(&'static str, &'static str)>,
        control: Option<BuildControl>,
        block_number: u64,
        parent_hash: alloy_primitives::B256,
        root_task_id: u64,
    ) -> Self {
        Self {
            labels,
            control,
            block_number,
            parent_hash,
            root_task_id,
            started: Instant::now(),
            outcome: "interrupted",
        }
    }

    pub(super) fn returned(&mut self, delivered: bool) {
        self.outcome = if delivered { "delivered" } else { "unavailable" };
    }
}

fn wait_bucket(elapsed: std::time::Duration) -> &'static str {
    match elapsed.as_millis() {
        0..=49 => "lt50ms",
        50..=99 => "50to100ms",
        100..=199 => "100to200ms",
        200..=399 => "200to400ms",
        _ => "ge400ms",
    }
}

impl Drop for RootWaitObservation {
    fn drop(&mut self) {
        let Some((kind, turn)) = self.labels else { return };
        let elapsed = self.started.elapsed();
        let outcome = if std::thread::panicking() { "panic" } else { self.outcome };
        metrics::counter!("bsc.build_diagnostics.root_waits_total",
            "kind" => kind, "turn" => turn, "outcome" => outcome,
            "bucket" => wait_bucket(elapsed))
        .increment(1);
        if elapsed < std::time::Duration::from_millis(100) {
            return;
        }
        // Metrics include all attempts. Bound INFO output to one slow wait per five seconds
        // across the process; no sleeps, extra RPCs, task cancellation or timeout marking.
        use std::sync::{
            atomic::{AtomicU64, Ordering},
            OnceLock,
        };
        static CLOCK: OnceLock<Instant> = OnceLock::new();
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let now = CLOCK.get_or_init(Instant::now).elapsed().as_millis() as u64;
        let next = NEXT.load(Ordering::Relaxed);
        if now < next ||
            NEXT.compare_exchange(
                next,
                now.saturating_add(5000),
                Ordering::Relaxed,
                Ordering::Relaxed,
            )
            .is_err()
        {
            metrics::counter!("bsc.build_diagnostics.root_wait_logs_suppressed_total").increment(1);
            return;
        }
        let budget = self.control.as_ref().map_or("none", |control| {
            match control.check_active(Instant::now()) {
                Ok(()) => "active",
                Err("payload build canceled") => "cancelled",
                Err(_) => "deadline_elapsed",
            }
        });
        tracing::info!(target: "bsc::miner::diagnostics", block_number = self.block_number,
            parent_hash = %self.parent_hash, kind, turn, root_task_id = self.root_task_id, outcome, budget,
            wait_ms = elapsed.as_secs_f64() * 1000.0, "Sampled slow miner root wait");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use metrics_exporter_prometheus::PrometheusBuilder;
    use reth_revm::cancelled::ManualCancel;
    use std::time::Duration;

    #[test]
    fn root_wait_outcomes_include_early_exit_and_exclude_non_miner() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            for outcome in [Some(true), Some(false), None] {
                let mut sample = RootWaitObservation::new(
                    Some(("regular", "inturn")),
                    None,
                    10,
                    alloy_primitives::B256::ZERO,
                    123,
                );
                sample.started = Instant::now() - Duration::from_millis(200);
                if let Some(delivered) = outcome {
                    sample.returned(delivered);
                }
                drop(sample);
            }
            let mut sample =
                RootWaitObservation::new(None, None, 10, alloy_primitives::B256::ZERO, 123);
            sample.started = Instant::now() - Duration::from_millis(500);
        });
        let text = handle.render();
        for outcome in ["delivered", "unavailable", "interrupted"] {
            let expected = format!("root_waits_total{{kind=\"regular\",turn=\"inturn\",outcome=\"{outcome}\",bucket=\"200to400ms\"}} 1");
            assert!(text.contains(&expected), "missing {expected}: {text}");
        }
        assert!(!text.contains("ge400ms"), "{text}");
        for (ms, expected) in [
            (49, "lt50ms"),
            (50, "50to100ms"),
            (99, "50to100ms"),
            (100, "100to200ms"),
            (199, "100to200ms"),
            (200, "200to400ms"),
            (399, "200to400ms"),
            (400, "ge400ms"),
        ] {
            assert_eq!(wait_bucket(Duration::from_millis(ms)), expected);
        }
    }

    #[test]
    fn records_success_and_early_exit_with_budget_without_polluting_non_miner_paths() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            let mut ok = FinishObservation::new(Some(("regular", "inturn")), None);
            ok.enter("system_transactions");
            ok.enter("assemble");
            ok.complete();
            drop(ok);

            // A failed receive still contributes root_wait time and an exit-stage sample.
            let control = BuildControl::new(
                Instant::now() - Duration::from_secs(1),
                Duration::ZERO,
                Duration::ZERO,
                Duration::ZERO,
                ManualCancel::default(),
            );
            let mut failed = FinishObservation::new(Some(("regular", "inturn")), Some(control));
            failed.enter("root_wait");
            drop(failed);

            let cancel = ManualCancel::default();
            cancel.clone().cancel();
            let control = BuildControl::new(
                Instant::now(),
                Duration::from_secs(1),
                Duration::ZERO,
                Duration::from_secs(1),
                cancel,
            );
            let mut cancelled = FinishObservation::new(Some(("empty", "outofturn")), Some(control));
            cancelled.enter("hash_state");
            drop(cancelled);
            let mut rpc = FinishObservation::new(None, None);
            rpc.enter("sync_root");
            rpc.complete();
        });
        let text = handle.render();
        for expected in [
            "finish_stage_seconds_count{kind=\"regular\",turn=\"inturn\",stage=\"root_wait\",outcome=\"interrupted\"} 1",
            "finish_exits_total{kind=\"regular\",turn=\"inturn\",stage=\"root_wait\",outcome=\"error\",budget=\"deadline_elapsed\"} 1",
            "finish_exits_total{kind=\"regular\",turn=\"inturn\",stage=\"assemble\",outcome=\"completed\",budget=\"none\"} 1",
            "finish_exits_total{kind=\"empty\",turn=\"outofturn\",stage=\"hash_state\",outcome=\"error\",budget=\"cancelled\"} 1",
        ] {
            assert!(text.contains(expected), "missing {expected}: {text}");
        }
        assert!(!text.contains("sync_root"));
    }
}
