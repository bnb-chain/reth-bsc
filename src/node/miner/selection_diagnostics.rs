//! Selection evidence, not pool eviction or canonical-chain accounting.
//! Pool snapshots are instantaneous and not atomic with iterator contents.
use super::build_diagnostics::SKIP_REASONS;
use reth::transaction_pool::PoolSize;

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct PoolSnapshot {
    pending: usize,
    queued: usize,
    basefee: usize,
    blob: usize,
}
impl From<PoolSize> for PoolSnapshot {
    fn from(p: PoolSize) -> Self {
        Self { pending: p.pending, queued: p.queued, basefee: p.basefee, blob: p.blob }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum SelectionExit {
    Cancelled,
    Deadline,
    IteratorExhausted,
    GasSkipLimit,
}
impl SelectionExit {
    const fn label(self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::Deadline => "transaction_deadline",
            Self::IteratorExhausted => "iterator_exhausted",
            Self::GasSkipLimit => "gas_skip_limit",
        }
    }
}

/// Small immutable summary travels with the candidate; no tx hashes or provider resources.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SelectionDiagnostics {
    pub(crate) block_number: u64,
    pub(crate) start: PoolSnapshot,
    pub(crate) end: PoolSnapshot,
    pub(crate) exit: SelectionExit,
    pub(crate) yielded: u64,
    pub(crate) ancestor_excluded: u64,
    pub(crate) executed: u64,
    pub(crate) skipped: [u64; 8],
    pub(crate) gas_used: u64,
    pub(crate) gas_limit: u64,
    pub(crate) remaining_seconds: f64,
}

fn fill_bucket(gas: u64, limit: u64) -> &'static str {
    if limit == 0 {
        return "unknown";
    }
    if gas == 0 {
        return "empty";
    }
    let percent = u128::from(gas) * 100;
    let limit = u128::from(limit);
    if percent < limit * 50 {
        "lt50"
    } else if percent < limit * 90 {
        "50to90"
    } else if percent < limit * 95 {
        "90to95"
    } else if percent < limit * 99 {
        "95to99"
    } else {
        "ge99"
    }
}
/// Also covers explicitly empty candidates which never entered transaction selection.
pub(crate) fn record_fill(
    stage: &'static str,
    kind: &'static str,
    turn: &'static str,
    reason: &'static str,
    gas: u64,
    limit: u64,
) {
    let fill = fill_bucket(gas, limit);
    metrics::counter!("bsc.miner_selection_outcomes_total", "stage" => stage, "kind" => kind, "turn" => turn, "reason" => reason, "fill" => fill).increment(1);
    metrics::histogram!("bsc.miner_selection_gas_used", "stage" => stage, "kind" => kind, "turn" => turn).record(gas as f64);
    metrics::histogram!("bsc.miner_selection_gas_remaining", "stage" => stage, "kind" => kind, "turn" => turn).record(limit.saturating_sub(gas) as f64);
    if limit > 0 {
        metrics::histogram!("bsc.miner_selection_fill_ratio", "stage" => stage, "kind" => kind, "turn" => turn).record(gas as f64 / limit as f64);
    }
}
impl SelectionDiagnostics {
    fn supply(&self) -> &'static str {
        if self.end.pending > 0 {
            "pending_present"
        } else if self.end.queued > 0 || self.end.basefee > 0 || self.end.blob > 0 {
            "parked_only"
        } else {
            "pool_empty"
        }
    }

    /// `selection` counts loops that reached an exit, including later failed candidates.
    /// `selected` is deduplicated by CandidateDiagnostics, not canonical inclusion.
    pub(crate) fn record(
        &self,
        stage: &'static str,
        kind: &'static str,
        turn: &'static str,
        gas: u64,
        limit: u64,
    ) {
        let reason = self.exit.label();
        record_fill(stage, kind, turn, reason, gas, limit);
        let fill = fill_bucket(gas, limit);
        if matches!(self.exit, SelectionExit::IteratorExhausted) {
            metrics::counter!("bsc.miner_selection_exhausted_total", "stage" => stage, "kind" => kind, "turn" => turn, "supply" => self.supply(), "fill" => fill).increment(1);
        }
        for (reason, count) in [
            ("iterator_yielded", self.yielded),
            ("ancestor_excluded", self.ancestor_excluded),
            ("executed", self.executed),
        ] {
            metrics::counter!("bsc.miner_selection_transactions_total", "stage" => stage, "kind" => kind, "turn" => turn, "reason" => reason).increment(count);
        }
        for (reason, count) in SKIP_REASONS.iter().zip(self.skipped) {
            metrics::counter!("bsc.miner_selection_transactions_total", "stage" => stage, "kind" => kind, "turn" => turn, "reason" => *reason).increment(count);
            // Conditions overlap: this is evidence of filtering, not mutually exclusive causality.
            if count > 0 {
                metrics::counter!("bsc.miner_selection_filtered_attempts_total", "stage" => stage, "kind" => kind, "turn" => turn, "reason" => *reason, "fill" => fill).increment(1);
            }
        }
        metrics::histogram!("bsc.miner_selection_remaining_seconds", "stage" => stage, "kind" => kind, "turn" => turn).record(self.remaining_seconds);
        for (boundary, snapshot) in [("start", self.start), ("end", self.end)] {
            for (pool, count) in [
                ("pending", snapshot.pending),
                ("queued", snapshot.queued),
                ("basefee", snapshot.basefee),
                ("blob", snapshot.blob),
            ] {
                metrics::histogram!("bsc.miner_selection_pool_transactions", "stage" => stage, "kind" => kind, "turn" => turn, "boundary" => boundary, "pool" => pool).record(count as f64);
            }
        }
        if stage == "selected" && limit > 0 && u128::from(gas) * 100 < u128::from(limit) * 95 {
            tracing::info!(target: "bsc::miner::diagnostics", block_number = self.block_number,
                kind, turn, reason, fill, gas_used = gas, gas_limit = limit,
                pool_start = ?self.start, pool_end = ?self.end,
                iterator_yielded = self.yielded, ancestor_excluded = self.ancestor_excluded,
                executed = self.executed, skips = ?self.skipped,
                remaining_ms = self.remaining_seconds * 1000.0,
                "Selected underfilled candidate selection evidence");
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    pub(crate) fn sample() -> SelectionDiagnostics {
        SelectionDiagnostics {
            block_number: 1,
            start: PoolSnapshot::default(),
            end: PoolSnapshot { pending: 6000, ..Default::default() },
            exit: SelectionExit::IteratorExhausted,
            yielded: 6000,
            ancestor_excluded: 5000,
            executed: 1000,
            skipped: [0; 8],
            gas_used: 100,
            gas_limit: 200,
            remaining_seconds: 0.1,
        }
    }

    #[test]
    fn exhaustion_with_ancestor_exclusions_is_not_empty_pool_or_gas_cap() {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            sample().record("selection", "pipeline", "inturn", 100, 200);
        });
        let text = handle.render();
        assert!(text.lines().any(|l| l.starts_with("bsc_miner_selection_exhausted_total{") &&
            l.contains("pending_present") &&
            l.ends_with(" 1")));
        assert!(!text.contains("gas_skip_limit"));
        assert!(!text.contains("pool_empty"));
        assert!(text.lines().any(|l| l.contains("ancestor_excluded") && l.ends_with(" 5000")));
        let mut summary = sample();
        summary.end = PoolSnapshot { queued: 12, ..Default::default() };
        assert_eq!(summary.supply(), "parked_only");
        summary.end = PoolSnapshot::default();
        assert_eq!(summary.supply(), "pool_empty");
    }

    #[test]
    fn fill_boundaries_and_overflow() {
        for (gas, expected) in [
            (0, "empty"),
            (49, "lt50"),
            (50, "50to90"),
            (90, "90to95"),
            (95, "95to99"),
            (99, "ge99"),
            (100, "ge99"),
        ] {
            assert_eq!(fill_bucket(gas, 100), expected);
        }
        assert_eq!(fill_bucket(u64::MAX, u64::MAX), "ge99");
        assert_eq!(fill_bucket(0, 0), "unknown");
    }
}
