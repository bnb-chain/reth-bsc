//! Record reorganizations only after the engine changes the canonical chain.

use crate::{metrics::BscBlockchainMetrics, BscPrimitives};
use alloy_consensus::{BlockHeader, Header};
use alloy_primitives::U256;
use reth_provider::{
    BlockIdReader, CanonStateNotification, CanonStateNotifications, HeaderProvider,
};
use tokio::sync::broadcast::error::RecvError;

pub(super) async fn run(
    mut notifications: CanonStateNotifications<BscPrimitives>,
    metrics: BscBlockchainMetrics,
) {
    loop {
        match notifications.recv().await {
            Ok(notification) => record(&metrics, &notification),
            Err(RecvError::Lagged(skipped)) => {
                metrics::counter!("bsc.chain_diagnostics.missed_notifications_total")
                    .increment(skipped);
                // Do not silently discard gaps via canonical_state_stream(). The engine
                // counters remain authoritative if this lightweight observer falls behind.
                tracing::warn!(
                    target: "bsc::forkchoice",
                    skipped,
                    "Reorg metrics missed canonical notifications; BSC counters may undercount"
                );
            }
            Err(RecvError::Closed) => break,
        }
    }
}

fn record(metrics: &BscBlockchainMetrics, notification: &CanonStateNotification<BscPrimitives>) {
    match notification {
        CanonStateNotification::Commit { new } => {
            for block in new.blocks().values() {
                record_header(block.header(), "canonical_added");
            }
        }
        CanonStateNotification::Reorg { old, new } => {
            for block in old.blocks().values() {
                record_header(block.header(), "canonical_removed");
            }
            for block in new.blocks().values() {
                record_header(block.header(), "canonical_added");
            }
        }
    }
    let CanonStateNotification::Reorg { old, new } = notification else { return };
    if old.is_empty() {
        return;
    }

    let reorg_depth = old.len();
    metrics.reorg_executions_total.increment(1);
    metrics.reorg_blocks_added_total.increment(new.len() as u64);
    metrics.reorg_blocks_dropped_total.increment(reorg_depth as u64);
    metrics.latest_reorg_depth.set(reorg_depth as f64);

    // A pure revert has no new blocks; its resulting head is the fork point.
    let (incoming_number, incoming_hash) = if new.is_empty() {
        (old.first().number().saturating_sub(1), old.first().parent_hash())
    } else {
        (new.tip().number(), new.tip().hash())
    };
    tracing::info!(
        target: "bsc::forkchoice",
        incoming_number,
        ?incoming_hash,
        current_number = old.tip().number(),
        current_hash = ?old.tip().hash(),
        reorg_depth,
        added_blocks = new.len(),
        "Reorg detected and metrics recorded"
    );
}

/// Difficulty is assigned and verified by Parlia (2 in-turn, 1 out-of-turn).
/// Keep unknown values visible rather than silently treating every other value as out-of-turn.
fn turn(header: &Header) -> &'static str {
    if header.difficulty == U256::from(2) {
        "inturn"
    } else if header.difficulty == U256::from(1) {
        "outofturn"
    } else {
        "unknown"
    }
}

fn record_header(header: &Header, stage: &'static str) {
    metrics::counter!("bsc.chain_diagnostics.blocks_total", "stage" => stage, "turn" => turn(header)).increment(1);
    metrics::histogram!("bsc.chain_diagnostics.gas_used", "stage" => stage, "turn" => turn(header))
        .record(header.gas_used as f64);
}

/// Counts newly finalized headers after an explicit startup baseline. Reads are chunked and
/// performed off the async executor. Errors leave the cursor at the last successfully read block.
pub(super) async fn run_finalized<P>(provider: P)
where
    P: HeaderProvider<Header = Header> + BlockIdReader + Clone + Send + Sync + 'static,
{
    let mut cursor = None;
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
    loop {
        interval.tick().await;
        let provider = provider.clone();
        match tokio::task::spawn_blocking(move || scan_finalized(&provider, cursor)).await {
            Ok(next) => cursor = next,
            Err(_) => {
                metrics::counter!("bsc.chain_diagnostics.finalized_scan_errors_total").increment(1);
            }
        }
    }
}

fn scan_finalized<P>(provider: &P, cursor: Option<u64>) -> Option<u64>
where
    P: HeaderProvider<Header = Header> + BlockIdReader,
{
    let tip = match provider.finalized_block_number() {
        Ok(Some(tip)) => tip,
        Ok(None) => return cursor,
        Err(_) => {
            metrics::counter!("bsc.chain_diagnostics.finalized_scan_errors_total").increment(1);
            return cursor;
        }
    };
    advance_finalized(tip, cursor, |number| provider.header_by_number(number).ok().flatten())
}

fn advance_finalized(
    tip: u64,
    mut cursor: Option<u64>,
    mut header: impl FnMut(u64) -> Option<Header>,
) -> Option<u64> {
    let Some(last) = cursor else {
        metrics::gauge!("bsc.chain_diagnostics.finalized_baseline_height").set(tip as f64);
        metrics::gauge!("bsc.chain_diagnostics.finalized_scanned_height").set(tip as f64);
        return Some(tip);
    };
    if tip < last {
        metrics::counter!("bsc.chain_diagnostics.finality_regressions_total").increment(1);
        return cursor;
    }
    for number in last.saturating_add(1)..=tip.min(last.saturating_add(256)) {
        match header(number) {
            Some(header) if header.number == number => {
                record_header(&header, "finalized");
                cursor = Some(number);
            }
            _ => {
                metrics::counter!("bsc.chain_diagnostics.finalized_scan_errors_total").increment(1);
                break;
            }
        }
    }
    metrics::gauge!("bsc.chain_diagnostics.finalized_scanned_height")
        .set(cursor.unwrap_or(last) as f64);
    metrics::gauge!("bsc.chain_diagnostics.finalized_backlog_blocks")
        .set(tip.saturating_sub(cursor.unwrap_or(last)) as f64);
    cursor
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BscBlock, BscBlockBody};
    use alloy_consensus::Header;
    use alloy_primitives::B256;
    use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
    use reth_execution_types::Chain;
    use reth_primitives_traits::RecoveredBlock;
    use std::sync::Arc;

    fn chain(first: u64, len: u64, branch: u8) -> Arc<Chain<BscPrimitives>> {
        if len == 0 {
            return Arc::new(Chain::default());
        }
        let mut parent_hash = B256::ZERO;
        let blocks = (first..first + len).map(|number| {
            let block = BscBlock {
                header: Header {
                    number,
                    parent_hash,
                    extra_data: vec![branch].into(),
                    ..Default::default()
                },
                body: BscBlockBody::default(),
            };
            let block = RecoveredBlock::new_unhashed(block, Default::default());
            parent_hash = block.hash();
            block
        });
        Arc::new(Chain::new(blocks, Default::default(), Default::default()))
    }

    fn recorder() -> (BscBlockchainMetrics, PrometheusHandle) {
        let recorder = PrometheusBuilder::new().build_recorder();
        // Default caches handles process-wide; each test needs its own recorder.
        let metrics = metrics::with_local_recorder(&recorder, || {
            BscBlockchainMetrics::new_with_labels(Vec::<metrics::Label>::new())
        });
        (metrics, recorder.handle())
    }

    fn value(handle: &PrometheusHandle, name: &str) -> f64 {
        let prefix = format!("bsc_blockchain_{name} ");
        handle
            .render()
            .lines()
            .find_map(|line| line.strip_prefix(&prefix))
            .unwrap_or_else(|| panic!("missing metric {name}: {}", handle.render()))
            .parse()
            .unwrap()
    }

    fn assert_metrics(
        handle: &PrometheusHandle,
        executions: u64,
        added: u64,
        dropped: u64,
        depth: u64,
    ) {
        for (name, expected) in [
            ("reorg_executions_total", executions),
            ("reorg_blocks_added_total", added),
            ("reorg_blocks_dropped_total", dropped),
            ("latest_reorg_depth", depth),
        ] {
            assert_eq!(value(handle, name), expected as f64, "{name}");
        }
    }

    #[test]
    fn normal_extensions_do_not_count_as_reorgs() {
        let (metrics, handle) = recorder();
        // Includes the two-block extension that the former parent-only check mislabeled.
        for len in [1, 2, 8] {
            record(&metrics, &CanonStateNotification::Commit { new: chain(101, len, 0) });
        }
        assert_metrics(&handle, 0, 0, 0, 0);
    }

    #[test]
    fn reorg_depth_counts_displaced_blocks_instead_of_tip_height_difference() {
        // Equal-height, longer incoming and shorter incoming branches.
        for (old_len, new_len) in [(3, 3), (1, 3), (4, 1)] {
            let (metrics, handle) = recorder();
            record(
                &metrics,
                &CanonStateNotification::Reorg {
                    old: chain(101, old_len, 0),
                    new: chain(101, new_len, 1),
                },
            );
            assert_metrics(&handle, 1, new_len, old_len, old_len);
        }
    }

    #[test]
    fn pure_revert_counts_removed_blocks_without_a_new_tip() {
        let (metrics, handle) = recorder();
        record(
            &metrics,
            &CanonStateNotification::Reorg { old: chain(101, 2, 0), new: chain(101, 0, 1) },
        );
        assert_metrics(&handle, 1, 0, 2, 2);
    }

    #[test]
    fn each_canonical_reorg_counts_once_and_commits_preserve_latest_depth() {
        let (metrics, handle) = recorder();
        for (old_len, new_len) in [(3, 4), (1, 2)] {
            record(
                &metrics,
                &CanonStateNotification::Reorg {
                    old: chain(101, old_len, 0),
                    new: chain(101, new_len, 1),
                },
            );
        }
        record(&metrics, &CanonStateNotification::Commit { new: chain(103, 2, 1) });
        assert_metrics(&handle, 2, 6, 4, 1);
    }

    #[tokio::test]
    async fn observer_continues_after_lag_and_exits_on_channel_close() {
        let (metrics, handle) = recorder();
        let (sender, receiver) = tokio::sync::broadcast::channel(1);
        sender.send(CanonStateNotification::Commit { new: chain(101, 2, 0) }).unwrap();
        sender
            .send(CanonStateNotification::Reorg { old: chain(101, 2, 0), new: chain(101, 3, 1) })
            .unwrap();
        drop(sender);

        // Both the lag error and the retained reorg must be handled before Closed.
        run(receiver, metrics).await;
        assert_metrics(&handle, 1, 3, 2, 2);
    }
    #[test]
    fn finalized_cursor_baselines_retries_gaps_and_does_not_double_count() {
        let baseline = advance_finalized(100, None, |_| panic!("must not scan history"));
        assert_eq!(baseline, Some(100));
        let mut reads = Vec::new();
        let cursor = advance_finalized(104, baseline, |number| {
            reads.push(number);
            (number != 103).then_some(Header {
                number,
                difficulty: U256::from(1),
                ..Default::default()
            })
        });
        assert_eq!(cursor, Some(102));
        assert_eq!(reads, [101, 102, 103]);
        reads.clear();
        let cursor = advance_finalized(104, cursor, |number| {
            reads.push(number);
            Some(Header { number, difficulty: U256::from(2), ..Default::default() })
        });
        assert_eq!(reads, [103, 104]);
        assert_eq!(cursor, Some(104));
        assert_eq!(advance_finalized(104, cursor, |_| panic!("already counted")), cursor);
        assert_eq!(advance_finalized(103, cursor, |_| panic!("regressed")), cursor);
        assert_eq!(
            advance_finalized(1000, cursor, |number| Some(Header { number, ..Default::default() })),
            Some(360)
        );
    }

    #[test]
    fn turn_is_independent_of_gas_limit_and_unknown_is_not_outofturn() {
        for gas_limit in [100_000_000, 120_000_000, 500_000_000] {
            for (difficulty, expected) in
                [(1, "outofturn"), (2, "inturn"), (0, "unknown"), (3, "unknown")]
            {
                assert_eq!(
                    turn(&Header {
                        difficulty: U256::from(difficulty),
                        gas_limit,
                        ..Default::default()
                    }),
                    expected
                );
            }
        }
    }

    #[test]
    fn canonical_additions_and_removals_are_separate_from_finalized_counts() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            let metrics = BscBlockchainMetrics::new_with_labels(Vec::<metrics::Label>::new());
            record(&metrics, &CanonStateNotification::Commit { new: chain(101, 2, 0) });
            record(
                &metrics,
                &CanonStateNotification::Reorg { old: chain(101, 2, 0), new: chain(101, 3, 1) },
            );
        });
        let rendered = handle.render();
        for (stage, expected) in [("canonical_added", 5), ("canonical_removed", 2)] {
            let line = rendered
                .lines()
                .find(|l| l.starts_with("bsc_chain_diagnostics_blocks_total{") && l.contains(stage))
                .unwrap();
            assert_eq!(line.split_whitespace().last().unwrap().parse::<u64>().unwrap(), expected);
        }
        assert!(!rendered.contains("stage=\"finalized\""));
    }
}
