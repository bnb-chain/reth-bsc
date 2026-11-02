//! Startup-only tuning and bounded-cardinality critical-path observations.
use std::time::Instant;

fn parse_count(
    value: Option<&str>,
    default: usize,
    allow_zero: bool,
) -> Result<usize, &'static str> {
    let Some(value) = value else { return Ok(default) };
    let count = value.trim().parse::<usize>().map_err(|_| "expected an integer")?;
    if count > 64 || (!allow_zero && count == 0) {
        return Err("out of range")
    }
    Ok(count)
}
fn configured(name: &'static str, default: usize, allow_zero: bool) -> usize {
    let value = std::env::var(name).ok();
    match parse_count(value.as_deref(), default, allow_zero) {
        Ok(count) => {
            tracing::info!(target: "bsc::miner", name, count, default, "Miner performance thread configuration");
            metrics::gauge!("bsc.miner_configured_threads", "pool" => name).set(count as f64);
            count
        }
        Err(reason) => {
            tracing::warn!(target: "bsc::miner", name, ?value, reason, default, "Invalid miner thread configuration; using default");
            metrics::gauge!("bsc.miner_configured_threads", "pool" => name).set(default as f64);
            default
        }
    }
}
pub(crate) fn prewarm_threads(default: usize) -> usize {
    configured("BSC_MINING_PREWARM_THREADS", default, true)
}
pub(crate) fn storage_proof_threads(default: usize) -> usize {
    configured("BSC_MINING_STORAGE_PROOF_THREADS", default, false)
}
/// One entered phase; early return/unwind is distinct from a completed phase.
/// Outer totals and inner phases overlap and must not be added together.
pub(crate) struct Phase {
    kind: &'static str,
    turn: &'static str,
    phase: &'static str,
    start: Instant,
    completed: bool,
}
impl Phase {
    pub(crate) fn new(kind: &'static str, inturn: bool, phase: &'static str) -> Self {
        Self {
            kind,
            turn: if inturn { "inturn" } else { "outofturn" },
            phase,
            start: Instant::now(),
            completed: false,
        }
    }
    pub(crate) fn complete(mut self) {
        self.completed = true;
    }
}
impl Drop for Phase {
    fn drop(&mut self) {
        metrics::histogram!("bsc.miner_build_phase_seconds", "kind" => self.kind, "turn" => self.turn, "phase" => self.phase, "outcome" => if self.completed { "completed" } else { "early_exit" }).record(self.start.elapsed().as_secs_f64());
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn thread_settings_preserve_defaults_and_bound_explicit_counts() {
        assert_eq!(parse_count(None, 9, true), Ok(9));
        assert_eq!(parse_count(None, 4, false), Ok(4));
        assert_eq!(parse_count(Some("0"), 9, true), Ok(0));
        assert!(parse_count(Some("0"), 4, false).is_err());
        assert_eq!(parse_count(Some("8"), 4, false), Ok(8));
        for bad in ["", "-1", "abc", "65", "999999999999999999999999"] {
            assert!(parse_count(Some(bad), 4, false).is_err());
        }
    }
    #[test]
    fn phase_metrics_keep_completion_and_early_exit_separate() {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            Phase::new("local", true, "root_setup").complete();
            drop(Phase::new("local", true, "root_setup"));
        });
        let text = handle.render();
        assert!(text.lines().any(|line| line.contains("miner_build_phase_seconds_count") &&
            line.contains("outcome=\"completed\"") &&
            line.ends_with(" 1")));
        assert!(text.lines().any(|line| line.contains("miner_build_phase_seconds_count") &&
            line.contains("outcome=\"early_exit\"") &&
            line.ends_with(" 1")));
        assert!(!text.contains("block_hash="));
    }
}
