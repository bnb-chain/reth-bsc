//! Miner finish() wall-clock accounting. No transaction/hash labels or per-block log output.
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

#[cfg(test)]
mod tests {
    use super::*;
    use metrics_exporter_prometheus::PrometheusBuilder;
    use reth_tasks::ManualCancel;
    use std::time::Duration;

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
