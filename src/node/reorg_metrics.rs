//! Record reorganizations only after the engine changes the canonical chain.

use crate::{metrics::BscBlockchainMetrics, BscPrimitives};
use alloy_consensus::BlockHeader;
use reth_provider::{CanonStateNotification, CanonStateNotifications};
use tokio::sync::broadcast::error::RecvError;

pub(super) async fn run(
    mut notifications: CanonStateNotifications<BscPrimitives>,
    metrics: BscBlockchainMetrics,
) {
    loop {
        match notifications.recv().await {
            Ok(notification) => record(&metrics, &notification),
            Err(RecvError::Lagged(skipped)) => {
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
}
