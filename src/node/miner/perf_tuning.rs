//! Startup-only tuning and bounded-cardinality critical-path observations.
use std::{sync::OnceLock, time::Instant};

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
pub(crate) const fn depth_label(depth: usize) -> &'static str {
    match depth {
        0 => "0",
        1 => "1",
        2 => "2",
        3 => "3",
        4 => "4",
        5 => "5",
        6 => "6",
        7 => "7",
        _ => "8",
    }
}
/// One entered phase; early return/unwind is distinct from a completed phase.
/// Outer totals and inner phases overlap and must not be added together.
pub(crate) struct Phase {
    kind: &'static str,
    turn: &'static str,
    depth: &'static str,
    phase: &'static str,
    start: Instant,
    completed: bool,
}
impl Phase {
    pub(crate) fn new(kind: &'static str, inturn: bool, depth: usize, phase: &'static str) -> Self {
        Self {
            kind,
            turn: if inturn { "inturn" } else { "outofturn" },
            depth: depth_label(depth),
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
        metrics::histogram!("bsc.miner_build_phase_seconds", "kind" => self.kind, "turn" => self.turn, "depth" => self.depth, "phase" => self.phase, "outcome" => if self.completed { "completed" } else { "early_exit" }).record(self.start.elapsed().as_secs_f64());
    }
}

const DEFAULT_GAS_LIMIT_SKIP_LIMIT: usize = 5_000;

/// Bounds consecutive gas-limit rejections without changing transaction validity.
/// A zero limit preserves the previous unbounded scan. Any fitting candidate, or
/// rejection for another reason, starts a new streak.
pub(crate) struct GasLimitSkips {
    limit: usize,
    consecutive: usize,
    peak: usize,
}

impl GasLimitSkips {
    pub(crate) fn configured() -> Self {
        static LIMIT: OnceLock<usize> = OnceLock::new();
        let limit = *LIMIT.get_or_init(|| {
            let value = std::env::var("BSC_MINING_GAS_LIMIT_SKIP_LIMIT").ok();
            let limit = match parse_gas_limit_skip_limit(value.as_deref()) {
                Ok(limit) => limit,
                Err(reason) => {
                    tracing::warn!(target: "bsc::miner", ?value, reason,
                        default = DEFAULT_GAS_LIMIT_SKIP_LIMIT,
                        "Invalid gas-limit skip limit; using default");
                    DEFAULT_GAS_LIMIT_SKIP_LIMIT
                }
            };
            metrics::gauge!("bsc.miner_configured_gas_limit_skip_limit").set(limit as f64);
            tracing::info!(target: "bsc::miner", limit, "Miner gas-limit skip limit");
            limit
        });
        Self::new(limit)
    }

    const fn new(limit: usize) -> Self {
        Self { limit, consecutive: 0, peak: 0 }
    }

    pub(crate) fn rejected(&mut self) -> bool {
        self.consecutive = self.consecutive.saturating_add(1);
        self.peak = self.peak.max(self.consecutive);
        self.limit != 0 && self.consecutive >= self.limit
    }

    pub(crate) const fn reset(&mut self) {
        self.consecutive = 0;
    }

    pub(crate) const fn peak(&self) -> usize {
        self.peak
    }
}

fn parse_gas_limit_skip_limit(value: Option<&str>) -> Result<usize, &'static str> {
    value.map_or(Ok(DEFAULT_GAS_LIMIT_SKIP_LIMIT), |value| {
        value.trim().parse().map_err(|_| "expected a nonnegative integer")
    })
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
    fn phase_metrics_keep_depth_and_early_exit_separate() {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            Phase::new("pipeline", true, 1, "root_setup").complete();
            drop(Phase::new("pipeline", true, 2, "root_setup"));
        });
        let text = handle.render();
        assert!(text.lines().any(|line| line.contains("miner_build_phase_seconds_count") &&
            line.contains("depth=\"1\"") &&
            line.contains("outcome=\"completed\"") &&
            line.ends_with(" 1")));
        assert!(text.lines().any(|line| line.contains("miner_build_phase_seconds_count") &&
            line.contains("depth=\"2\"") &&
            line.contains("outcome=\"early_exit\"") &&
            line.ends_with(" 1")));
        assert!(!text.contains("block_hash="));
    }

    #[test]
    fn gas_limit_scan_stops_at_the_limit_and_resets_between_streaks() {
        assert_eq!(parse_gas_limit_skip_limit(None), Ok(5_000));
        assert_eq!(parse_gas_limit_skip_limit(Some(" 0 ")), Ok(0));
        assert_eq!(parse_gas_limit_skip_limit(Some("7")), Ok(7));
        for value in ["", "-1", "abc", "999999999999999999999999"] {
            assert_eq!(
                parse_gas_limit_skip_limit(Some(value)),
                Err("expected a nonnegative integer")
            );
        }
        let mut skips = GasLimitSkips::new(3);
        assert!(!skips.rejected());
        assert!(!skips.rejected());
        skips.reset();
        assert!(!skips.rejected());
        assert!(!skips.rejected());
        assert!(skips.rejected());
        assert_eq!(skips.peak(), 3);

        let mut unlimited = GasLimitSkips::new(0);
        for _ in 0..10_000 {
            assert!(!unlimited.rejected());
        }
        assert_eq!(unlimited.peak(), 10_000);
    }
}
