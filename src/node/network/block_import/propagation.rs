//! Separate local block propagation from best-effort remote import events.

use super::{
    metrics::Queue,
    service::{BlockMsg, ImportEvent},
};
use futures::{future::BoxFuture, FutureExt};
use reth_network::import::{BlockImportEvent, BlockValidation};
use std::task::{Context, Poll};
use tokio::sync::mpsc::{
    self,
    error::{SendError, TrySendError},
    OwnedPermit, Receiver, Sender,
};

/// Local ingress and propagation are bounded in blocks, not individual notifications.
pub(crate) const LOCAL_BLOCK_CAPACITY: usize = 32;
/// Serve a ready remote event after at most eight local blocks (sixteen notifications).
const LOCAL_BLOCK_BURST: usize = 8;

type Reservation = BoxFuture<'static, Result<OwnedPermit<BlockMsg>, SendError<()>>>;

pub(crate) struct PropagationSender {
    /// Best-effort remote announcements/outcomes. Full queues reject without waiting;
    /// this lane cannot consume any local propagation capacity.
    remote: Sender<ImportEvent>,
    /// Local blocks reserve a slot before leaving ingress, applying backpressure. Each
    /// block expands into an adjacent full-block/hash pair when the receiver polls it.
    local: Sender<BlockMsg>,
    reservation: Option<Reservation>,
}

/// One queued local block expands into an adjacent full-block/hash notification pair.
#[derive(Debug)]
pub(crate) struct PropagationReceiver {
    remote: Receiver<ImportEvent>,
    local: Receiver<BlockMsg>,
    pending_hash: Option<BlockMsg>,
    local_burst: usize,
}

pub(crate) fn channel() -> (PropagationSender, PropagationReceiver) {
    let (remote, remote_rx) = mpsc::channel(super::IMPORT_QUEUE_CAPACITY);
    let (local, local_rx) = mpsc::channel(LOCAL_BLOCK_CAPACITY);
    (
        PropagationSender { remote, local, reservation: None },
        PropagationReceiver {
            remote: remote_rx,
            local: local_rx,
            pending_hash: None,
            local_burst: 0,
        },
    )
}

impl PropagationSender {
    pub(crate) fn send_remote(&self, event: ImportEvent) {
        match self.remote.try_send(event) {
            Err(TrySendError::Full(_)) => {
                Queue::RemotePropagation.full(super::IMPORT_QUEUE_CAPACITY, "drop")
            }
            Err(TrySendError::Closed(_)) => {
                tracing::warn!(target: "bsc::block_import", "Remote propagation channel closed; event dropped")
            }
            Ok(()) => {}
        }
        self.observe();
    }

    pub(crate) fn observe(&self) {
        Queue::RemotePropagation.observe(
            self.remote.max_capacity() - self.remote.capacity(),
            self.remote.max_capacity(),
        );
        Queue::LocalPropagation
            .observe(self.local.max_capacity() - self.local.capacity(), self.local.max_capacity());
    }

    /// Reserve before receiving a local input. While full, keep just this reservation future
    /// and let the bounded ingress apply backpressure to the producer.
    pub(crate) fn poll_local_permit(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<OwnedPermit<BlockMsg>, SendError<()>>> {
        let reservation =
            self.reservation.get_or_insert_with(|| self.local.clone().reserve_owned().boxed());
        let result = std::task::ready!(reservation.as_mut().poll(cx));
        self.reservation = None;
        Poll::Ready(result)
    }
}

impl PropagationReceiver {
    pub(crate) fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Option<ImportEvent>> {
        let result = self.poll_recv_inner(cx);
        Queue::RemotePropagation.observe(
            self.remote.max_capacity() - self.remote.capacity(),
            self.remote.max_capacity(),
        );
        Queue::LocalPropagation
            .observe(self.local.max_capacity() - self.local.capacity(), self.local.max_capacity());
        result
    }

    fn poll_recv_inner(&mut self, cx: &mut Context<'_>) -> Poll<Option<ImportEvent>> {
        if let Some(block) = self.pending_hash.take() {
            return Poll::Ready(Some(BlockImportEvent::Announcement(BlockValidation::ValidBlock {
                block,
            })));
        }

        if self.local_burst >= LOCAL_BLOCK_BURST {
            if let Poll::Ready(Some(event)) = self.remote.poll_recv(cx) {
                self.local_burst = 0;
                return Poll::Ready(Some(event));
            }
        }

        let local_closed = match self.local.poll_recv(cx) {
            Poll::Ready(Some(block)) => {
                self.local_burst = (self.local_burst + 1).min(LOCAL_BLOCK_BURST);
                self.pending_hash = Some(block.clone());
                return Poll::Ready(Some(BlockImportEvent::Announcement(
                    BlockValidation::ValidHeader { block },
                )));
            }
            Poll::Ready(None) => true,
            Poll::Pending => false,
        };
        match self.remote.poll_recv(cx) {
            Poll::Ready(Some(event)) => {
                self.local_burst = 0;
                Poll::Ready(Some(event))
            }
            Poll::Ready(None) if local_closed => Poll::Ready(None),
            _ => Poll::Pending,
        }
    }

    #[cfg(test)]
    pub(crate) fn try_recv(&mut self) -> Result<ImportEvent, mpsc::error::TryRecvError> {
        let mut cx = Context::from_waker(futures::task::noop_waker_ref());
        match self.poll_recv(&mut cx) {
            Poll::Ready(Some(event)) => Ok(event),
            Poll::Ready(None) => Err(mpsc::error::TryRecvError::Disconnected),
            Poll::Pending => Err(mpsc::error::TryRecvError::Empty),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{node::network::BscNewBlock, BscBlock, BscBlockBody};
    use alloy_consensus::Header;
    use alloy_primitives::{U128, U256};
    use reth_eth_wire::NewBlock;
    use reth_network::message::NewBlockMessage;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    fn block(number: u64) -> BlockMsg {
        let block = BscBlock {
            header: Header { number, ..Default::default() },
            body: BscBlockBody::default(),
        };
        NewBlockMessage {
            hash: block.header.hash_slow(),
            td: Some(U256::ZERO),
            block: Arc::new(BscNewBlock(NewBlock { block, td: U128::ZERO })),
        }
    }

    fn assert_event(event: ImportEvent, number: u64, full_block: bool) {
        match event {
            BlockImportEvent::Announcement(BlockValidation::ValidHeader { block })
                if full_block =>
            {
                assert_eq!(block.block.0.block.header.number, number)
            }
            BlockImportEvent::Announcement(BlockValidation::ValidBlock { block })
                if !full_block =>
            {
                assert_eq!(block.block.0.block.header.number, number)
            }
            _ => panic!("unexpected propagation event"),
        }
    }

    #[tokio::test]
    async fn remote_flood_cannot_consume_local_capacity_and_pairs_stay_adjacent() {
        let (mut tx, mut rx) = channel();
        for _ in 0..super::super::IMPORT_QUEUE_CAPACITY + 10 {
            tx.send_remote(BlockImportEvent::Announcement(BlockValidation::ValidBlock {
                block: block(99),
            }));
        }
        assert_eq!(tx.remote.capacity(), 0);
        for n in 0..LOCAL_BLOCK_CAPACITY {
            futures::future::poll_fn(|cx| tx.poll_local_permit(cx))
                .await
                .unwrap()
                .send(block(n as u64));
        }
        for n in 0..LOCAL_BLOCK_CAPACITY {
            if n > 0 && n % LOCAL_BLOCK_BURST == 0 {
                assert_event(rx.try_recv().unwrap(), 99, false);
            }
            assert_event(rx.try_recv().unwrap(), n as u64, true);
            assert_event(rx.try_recv().unwrap(), n as u64, false);
        }
        // Continued local load still gives a waiting remote event its turn.
        futures::future::poll_fn(|cx| tx.poll_local_permit(cx)).await.unwrap().send(block(100));
        assert_event(rx.try_recv().unwrap(), 99, false);
        assert_event(rx.try_recv().unwrap(), 100, true);
        assert_event(rx.try_recv().unwrap(), 100, false);
    }

    #[test]
    fn propagation_metrics_count_drops_and_include_reserved_local_slots() {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let snapshot = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            let (tx, mut rx) = channel();
            for _ in 0..super::super::IMPORT_QUEUE_CAPACITY + 10 {
                tx.send_remote(BlockImportEvent::Announcement(BlockValidation::ValidBlock {
                    block: block(99),
                }));
            }
            let permit = tx.local.try_reserve().unwrap();
            tx.observe();
            let rendered = snapshot.render();
            assert!(rendered
                .contains("bsc_block_import_queue_full_total{queue=\"remote_propagation\"} 10"));
            assert!(rendered.contains("bsc_block_import_queue_used{queue=\"local_propagation\"} 1"));
            drop(permit);
            while rx.try_recv().is_ok() {}
            let rendered = snapshot.render();
            assert!(
                rendered.contains("bsc_block_import_queue_used{queue=\"remote_propagation\"} 0")
            );
            assert!(rendered.contains("bsc_block_import_queue_used{queue=\"local_propagation\"} 0"));
        });
    }

    #[derive(Default)]
    struct WakeCounter(AtomicUsize);
    impl futures::task::ArcWake for WakeCounter {
        fn wake_by_ref(this: &Arc<Self>) {
            this.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[tokio::test]
    async fn full_local_queue_wakes_reservation_when_capacity_returns() {
        let (mut tx, mut rx) = channel();
        for n in 0..LOCAL_BLOCK_CAPACITY {
            futures::future::poll_fn(|cx| tx.poll_local_permit(cx))
                .await
                .unwrap()
                .send(block(n as u64));
        }
        let wakes = Arc::new(WakeCounter::default());
        let waker = futures::task::waker(wakes.clone());
        let mut cx = Context::from_waker(&waker);
        assert!(tx.poll_local_permit(&mut cx).is_pending());
        assert_event(rx.try_recv().unwrap(), 0, true);
        assert!(wakes.0.load(Ordering::Relaxed) > 0);
        let Poll::Ready(Ok(permit)) = tx.poll_local_permit(&mut cx) else {
            panic!("capacity did not recover")
        };
        permit.send(block(100));
        assert_event(rx.try_recv().unwrap(), 0, false);
        for n in 1..LOCAL_BLOCK_CAPACITY {
            assert_event(rx.try_recv().unwrap(), n as u64, true);
            assert_event(rx.try_recv().unwrap(), n as u64, false);
        }
        assert_event(rx.try_recv().unwrap(), 100, true);
        assert_event(rx.try_recv().unwrap(), 100, false);
    }

    #[tokio::test]
    async fn receiver_closure_wakes_blocked_local_producer_with_error() {
        let (mut tx, rx) = channel();
        for n in 0..LOCAL_BLOCK_CAPACITY {
            futures::future::poll_fn(|cx| tx.poll_local_permit(cx))
                .await
                .unwrap()
                .send(block(n as u64));
        }
        let wakes = Arc::new(WakeCounter::default());
        let waker = futures::task::waker(wakes.clone());
        let mut cx = Context::from_waker(&waker);
        assert!(tx.poll_local_permit(&mut cx).is_pending());
        drop(rx);
        assert!(wakes.0.load(Ordering::Relaxed) > 0);
        assert!(matches!(tx.poll_local_permit(&mut cx), Poll::Ready(Err(_))));
    }

    #[tokio::test]
    async fn closing_one_lane_does_not_hide_events_in_the_other() {
        let (tx, mut rx) = channel();
        tx.local.send(block(1)).await.unwrap();
        drop(tx.local);
        assert_event(rx.try_recv().unwrap(), 1, true);
        assert_event(rx.try_recv().unwrap(), 1, false);
        assert!(matches!(rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)));
        tx.remote
            .send(BlockImportEvent::Announcement(BlockValidation::ValidBlock { block: block(2) }))
            .await
            .unwrap();
        drop(tx.remote);
        assert_event(rx.try_recv().unwrap(), 2, false);
        assert!(matches!(rx.try_recv(), Err(mpsc::error::TryRecvError::Disconnected)));
    }
}
