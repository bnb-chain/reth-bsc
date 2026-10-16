use super::metrics::Queue;
use crate::node::engine_api::payload::BscPayloadTypes;
use alloy_primitives::B256;
use alloy_rpc_types_engine::ForkchoiceState;
use reth_engine_primitives::ConsensusEngineHandle;
use reth_network_api::PeerId;
use std::{collections::VecDeque, future::Future, pin::Pin, task::Context};

const MAX_WAITING_PEERS: usize = 64;

type FcuFuture = Pin<Box<dyn Future<Output = ()> + Send + Sync>>;

/// Bounded, round-robin admission for speculative far-ahead sync targets.
/// Each peer keeps at most one waiting target. This only bounds FCU admission,
/// not Engine downloads: a FCU returning SYNCING is not download completion.
pub(crate) struct PipelineTargets {
    /// Submits speculative fork-choice targets to the Engine; does not download blocks itself.
    engine: ConsensusEngineHandle<BscPayloadTypes>,
    /// FIFO of (announcing peer, target hash), with at most MAX_WAITING_PEERS peers and one
    /// waiting hash per peer.
    waiting: VecDeque<(PeerId, B256)>,
    /// The single FCU awaiting an Engine reply. Completion may mean SYNCING, not a finished
    /// download.
    active: Option<(B256, FcuFuture)>,
}

impl PipelineTargets {
    pub(crate) fn new(engine: ConsensusEngineHandle<BscPayloadTypes>) -> Self {
        Self { engine, waiting: VecDeque::new(), active: None }
    }

    /// Returns false only when a new peer's waiting target cannot be admitted.
    /// Rejected targets are not cached; a later announcement can retry after a slot opens.
    pub(crate) fn enqueue(&mut self, peer: PeerId, hash: B256) -> bool {
        if self.active.as_ref().is_some_and(|(active_hash, _)| *active_hash == hash) ||
            self.waiting.iter().any(|(_, h)| *h == hash)
        {
            return true;
        }
        if let Some((_, target)) = self.waiting.iter_mut().find(|(p, _)| *p == peer) {
            // Preserve this peer's place and retain its most recently received announcement.
            // Arrival order does not establish height, ancestry, or canonical-chain status.
            *target = hash;
        } else if self.waiting.len() < MAX_WAITING_PEERS {
            self.waiting.push_back((peer, hash));
        } else {
            Queue::PipelineWaiting.full(MAX_WAITING_PEERS, "drop");
            return false;
        }
        Queue::PipelineWaiting.observe(self.waiting.len(), MAX_WAITING_PEERS);
        true
    }

    pub(crate) fn poll(&mut self, cx: &mut Context<'_>) {
        self.poll_inner(cx);
        Queue::PipelineWaiting.observe(self.waiting.len(), MAX_WAITING_PEERS);
        Queue::PipelineActive.observe(usize::from(self.active.is_some()), 1);
    }

    fn poll_inner(&mut self, cx: &mut Context<'_>) {
        if let Some((_, active)) = &mut self.active {
            if active.as_mut().poll(cx).is_pending() {
                return;
            }
            self.active = None;
        }
        let Some((_, hash)) = self.waiting.pop_front() else {
            return;
        };
        // Deduplicate only while the FCU is pending. A SYNCING response does not
        // prove its download was admitted, so later announcements must remain retryable.
        let engine = self.engine.clone();
        self.active = Some((
            hash,
            Box::pin(async move {
                let state = ForkchoiceState {
                    head_block_hash: hash,
                    safe_block_hash: B256::ZERO,
                    finalized_block_hash: B256::ZERO,
                };
                if let Err(error) = engine.fork_choice_updated(state, None).await {
                    tracing::debug!(target: "bsc::block_import", %hash, %error, "Pipeline FCU failed");
                }
            }),
        ));
        // Start the future and register the Engine response waker on the next poll.
        cx.waker().wake_by_ref();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rejected_waiting_target_can_retry_when_a_slot_opens() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut targets = PipelineTargets::new(ConsensusEngineHandle::new(tx));
        for n in 0..MAX_WAITING_PEERS as u8 {
            assert!(targets.enqueue(PeerId::random(), B256::repeat_byte(n)));
        }
        let peer = PeerId::random();
        let hash = B256::repeat_byte(0xff);
        assert!(!targets.enqueue(peer, hash));
        let _held_request = futures::future::poll_fn(|cx| {
            targets.poll(cx);
            rx.poll_recv(cx)
        })
        .await
        .unwrap();
        assert!(targets.enqueue(peer, hash));
        assert_eq!(targets.waiting.len(), MAX_WAITING_PEERS);
        assert_eq!(targets.waiting.back(), Some(&(peer, hash)));
    }

    #[tokio::test]
    async fn engine_error_does_not_permanently_suppress_the_target() {
        use reth_engine_primitives::BeaconEngineMessage;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut targets = PipelineTargets::new(ConsensusEngineHandle::new(tx));
        let peer = PeerId::random();
        let hash = B256::repeat_byte(1);
        targets.enqueue(peer, hash);
        let first = futures::future::poll_fn(|cx| {
            targets.poll(cx);
            rx.poll_recv(cx)
        })
        .await
        .unwrap();
        let BeaconEngineMessage::ForkchoiceUpdated { tx, .. } = first else {
            panic!("expected FCU");
        };
        // Dropping the response makes the Engine call fail.
        drop(tx);
        let waker = futures::task::noop_waker();
        targets.poll(&mut Context::from_waker(&waker));
        assert!(targets.active.is_none());
        assert!(targets.enqueue(peer, hash));
        let next = futures::future::poll_fn(|cx| {
            targets.poll(cx);
            rx.poll_recv(cx)
        })
        .await
        .unwrap();
        assert!(
            matches!(next, BeaconEngineMessage::ForkchoiceUpdated { state, .. } if state.head_block_hash == hash)
        );
    }

    #[tokio::test]
    async fn slow_engine_keeps_one_fcu_and_other_peer_gets_next_turn() {
        use alloy_rpc_types_engine::{PayloadStatus, PayloadStatusEnum};
        use reth_engine_primitives::{BeaconEngineMessage, OnForkChoiceUpdated};
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut targets = PipelineTargets::new(ConsensusEngineHandle::new(tx));
        let attacker = PeerId::random();
        let honest = PeerId::random();
        let hash = B256::repeat_byte(1);
        targets.enqueue(attacker, hash);
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        let first = futures::future::poll_fn(|cx| {
            targets.poll(cx);
            rx.poll_recv(cx)
        })
        .await
        .unwrap();
        let BeaconEngineMessage::ForkchoiceUpdated { tx, .. } = first else {
            panic!("expected FCU");
        };
        let honest_hash = B256::repeat_byte(2);
        targets.enqueue(honest, honest_hash);
        for n in 3..1024u64 {
            targets
                .enqueue(attacker, B256::from(alloy_primitives::U256::from(n).to_be_bytes::<32>()));
            targets.poll(&mut cx);
        }
        assert!(rx.try_recv().is_err());
        assert_eq!(targets.waiting.len(), 2);
        tx.send(Ok(OnForkChoiceUpdated::valid(PayloadStatus::new(
            PayloadStatusEnum::Syncing,
            None,
        ))))
        .unwrap();
        let next = futures::future::poll_fn(|cx| {
            targets.poll(cx);
            rx.poll_recv(cx)
        })
        .await
        .unwrap();
        let BeaconEngineMessage::ForkchoiceUpdated { state, .. } = next else {
            panic!("expected FCU");
        };
        assert_eq!(state.head_block_hash, honest_hash);
    }

    #[tokio::test]
    async fn flood_is_coalesced_per_peer_and_globally_bounded() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut targets = PipelineTargets::new(ConsensusEngineHandle::new(tx));
        let attacker = PeerId::random();
        for i in 0..1024u64 {
            targets
                .enqueue(attacker, B256::from(alloy_primitives::U256::from(i).to_be_bytes::<32>()));
        }
        assert_eq!(targets.waiting.len(), 1);
        let honest = PeerId::random();
        let head = B256::repeat_byte(0xff);
        targets.enqueue(honest, head);
        assert_eq!(targets.waiting[1], (honest, head));
        for i in 2000..3000u64 {
            targets.enqueue(
                PeerId::random(),
                B256::from(alloy_primitives::U256::from(i).to_be_bytes::<32>()),
            );
        }
        assert_eq!(targets.waiting.len(), MAX_WAITING_PEERS);
        assert_eq!(targets.waiting[1], (honest, head));
    }
}
