//! Bounded retries for payloads whose validation depends on the local clock.
use crate::node::engine_api::{payload::BscPayloadTypes, validator::BscExecutionData};
use alloy_consensus::InMemorySize;
use alloy_primitives::B256;
use alloy_rpc_types_engine::{PayloadStatus, PayloadStatusEnum};
use parking_lot::Mutex;
use reth_engine_primitives::ConsensusEngineHandle;
use std::{
    collections::HashMap,
    future::Future,
    sync::Arc,
    time::{Duration, SystemTime},
};

const MAX_BLOCKS: usize = 256;
const MAX_BYTES: usize = 256 * 1024 * 1024;
const MAX_WAIT: Duration = Duration::from_secs(30);
const RETRY_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug, Default)]
struct Budget {
    blocks: HashMap<B256, usize>,
    bytes: usize,
}

/// Retry capacity shared by one node's peer, recovery, and BidBlock imports.
#[derive(Debug, Clone, Default)]
pub(crate) struct Pending(Arc<Mutex<Budget>>);

impl Pending {
    fn reserve(&self, hash: B256, bytes: usize) -> eyre::Result<Reservation> {
        let mut budget = self.0.lock();
        eyre::ensure!(!budget.blocks.contains_key(&hash), "payload already awaiting retry: {hash}");
        eyre::ensure!(
            budget.blocks.len() < MAX_BLOCKS && bytes <= MAX_BYTES.saturating_sub(budget.bytes),
            "future payload retry capacity exhausted"
        );
        budget.blocks.insert(hash, bytes);
        budget.bytes += bytes;
        Ok(Reservation { pending: self.clone(), hash })
    }

    fn contains(&self, hash: B256) -> bool {
        self.0.lock().blocks.contains_key(&hash)
    }
}

struct Reservation {
    pending: Pending,
    hash: B256,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let mut budget = self.pending.0.lock();
        if let Some(bytes) = budget.blocks.remove(&self.hash) {
            budget.bytes -= bytes;
        }
    }
}

/// `ACCEPTED` is nonterminal: the engine has not established invalidity and the
/// caller must retry before advancing fork choice or attributing a fault to a builder.
pub(super) async fn submit(
    engine: &ConsensusEngineHandle<BscPayloadTypes>,
    payload: BscExecutionData,
    pending: &Pending,
) -> eyre::Result<PayloadStatus> {
    submit_with(
        payload,
        pending,
        || SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default(),
        |payload| async move { engine.new_payload(payload).await.map_err(Into::into) },
    )
    .await
}

async fn submit_with<F, Fut>(
    payload: BscExecutionData,
    pending: &Pending,
    now: impl Fn() -> Duration,
    mut send: F,
) -> eyre::Result<PayloadStatus>
where
    F: FnMut(BscExecutionData) -> Fut,
    Fut: Future<Output = eyre::Result<PayloadStatus>>,
{
    let timestamp = Duration::from_secs(payload.block.header.timestamp);
    let hash = payload.block_hash_cached();
    let parent = payload.block.header.parent_hash;
    // Charge both the retained payload and its Engine request clone. This is an
    // estimate of block storage, not a bound on the Engine's execution memory.
    let bytes = payload.block.size().saturating_mul(2);
    // Ordinary execution keeps its existing timeout policy. Only deferred work
    // is subject to the retry budget and deadline.
    if timestamp <= now() && !pending.contains(parent) {
        let status = send(payload.clone()).await?;
        if !matches!(status.status, PayloadStatusEnum::Accepted) {
            return Ok(status);
        }
    }
    eyre::ensure!(
        timestamp.saturating_sub(now()) <= MAX_WAIT,
        "payload timestamp exceeds future retry window"
    );
    let _reservation = pending.reserve(hash, bytes)?;
    tracing::debug!(target: "bsc::block_import", block_hash = %hash,
        timestamp = payload.block.header.timestamp, "Deferring payload for clock or engine validation");
    let deadline = tokio::time::Instant::now() + MAX_WAIT;
    tokio::time::timeout_at(deadline, async {
        loop {
            tokio::time::sleep(RETRY_INTERVAL).await;
            let delay = timestamp.saturating_sub(now());
            eyre::ensure!(delay <= MAX_WAIT, "payload timestamp exceeds future retry window");
            if !delay.is_zero() || pending.contains(parent) {
                // Recheck wall time, including backward clock adjustments. The
                // monotonic deadline prevents such adjustments retaining work forever.
                continue;
            }
            let status = send(payload.clone()).await?;
            if !matches!(status.status, PayloadStatusEnum::Accepted) {
                return Ok(status);
            }
        }
    })
    .await
    .map_err(|_| eyre::eyre!("future payload retry deadline exceeded"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    fn payload(timestamp: u64) -> BscExecutionData {
        let mut payload = BscExecutionData::default();
        payload.block.header.timestamp = timestamp;
        payload
    }

    #[tokio::test(start_paused = true)]
    async fn clock_boundary_and_accepted_are_retried_without_another_announcement() {
        let pending = Pending::default();
        let now = AtomicU64::new(100_982);
        let calls = AtomicUsize::new(0);
        let future = submit_with(
            payload(101),
            &pending,
            || Duration::from_millis(now.load(Ordering::SeqCst)),
            |_| {
                let attempt = calls.fetch_add(1, Ordering::SeqCst);
                async move {
                    Ok(PayloadStatus::from_status(if attempt == 0 {
                        PayloadStatusEnum::Accepted
                    } else {
                        PayloadStatusEnum::Valid
                    }))
                }
            },
        );
        tokio::pin!(future);
        assert!(futures::poll!(&mut future).is_pending());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        // A backward wall-clock adjustment must not cause premature submission.
        now.store(99_000, Ordering::SeqCst);
        tokio::time::advance(RETRY_INTERVAL).await;
        assert!(futures::poll!(&mut future).is_pending());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        now.store(101_000, Ordering::SeqCst);
        tokio::time::advance(RETRY_INTERVAL).await;
        assert!(futures::poll!(&mut future).is_pending());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        tokio::time::advance(RETRY_INTERVAL).await;
        assert!(future.await.unwrap().is_valid());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(pending.0.lock().blocks.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn genuine_invalid_and_syncing_responses_are_preserved() {
        for status in [
            PayloadStatusEnum::Invalid { validation_error: "bad root".into() },
            PayloadStatusEnum::Syncing,
        ] {
            let result = submit_with(
                payload(100),
                &Pending::default(),
                || Duration::from_secs(100),
                |_| std::future::ready(Ok(PayloadStatus::from_status(status.clone()))),
            )
            .await
            .unwrap();
            assert_eq!(result.status, status);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_clock_and_repeated_accepted_have_a_deadline() {
        for timestamp in [0, 101] {
            let pending = Pending::default();
            let result = submit_with(
                payload(timestamp),
                &pending,
                || Duration::from_secs(100),
                |_| std::future::ready(Ok(PayloadStatus::from_status(PayloadStatusEnum::Accepted))),
            )
            .await;
            assert!(result.unwrap_err().to_string().contains("deadline"));
            assert_eq!(pending.0.lock().bytes, 0);
        }
    }

    #[test]
    fn reservation_deduplicates_limits_memory_and_releases_on_drop() {
        let pending = Pending::default();
        let hash = B256::repeat_byte(1);
        let reservation = pending.reserve(hash, MAX_BYTES).unwrap();
        assert!(pending.reserve(hash, 1).is_err());
        assert!(pending.reserve(B256::repeat_byte(2), 1).is_err());
        drop(reservation);
        assert!(pending.reserve(hash, MAX_BYTES).is_ok());
        let reservations: Vec<_> = (0..MAX_BLOCKS)
            .map(|i| pending.reserve(B256::repeat_byte(i as u8), 1).unwrap())
            .collect();
        let mut extra = B256::ZERO;
        extra[0] = 1;
        assert!(pending.reserve(extra, 1).err().unwrap().to_string().contains("capacity"));
        drop(reservations);
        assert_eq!(pending.0.lock().bytes, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn descendants_wait_for_parent_and_cancellation_releases_capacity() {
        let pending = Pending::default();
        let parent = payload(101);
        let parent_hash = parent.block_hash_cached();
        let mut child = payload(102);
        child.block.header.parent_hash = parent_hash;
        let parent_reservation = pending.reserve(parent_hash, 1).unwrap();
        let mut future = Box::pin(submit_with(
            child.clone(),
            &pending,
            || Duration::from_secs(102),
            |_| async { Ok(PayloadStatus::from_status(PayloadStatusEnum::Valid)) },
        ));
        assert!(futures::poll!(&mut future).is_pending());
        tokio::time::advance(RETRY_INTERVAL).await;
        assert!(futures::poll!(&mut future).is_pending());
        assert_eq!(pending.0.lock().blocks.len(), 2);
        drop(parent_reservation);
        tokio::time::advance(RETRY_INTERVAL).await;
        assert!(future.await.unwrap().is_valid());
        assert!(pending.0.lock().blocks.is_empty());

        let mut future = Box::pin(submit_with(
            child,
            &pending,
            || Duration::from_secs(101),
            |_| async { panic!("future child reached engine") },
        ));
        assert!(futures::poll!(&mut future).is_pending());
        assert_eq!(pending.0.lock().blocks.len(), 1);
        drop(future);
        assert!(pending.0.lock().blocks.is_empty());
        assert_eq!(pending.0.lock().bytes, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn far_future_is_not_submitted() {
        let result = submit_with(
            payload(131),
            &Pending::default(),
            || Duration::from_secs(100),
            |_| async { panic!("far-future payload reached engine") },
        )
        .await;
        assert!(result.unwrap_err().to_string().contains("window"));
    }
}
