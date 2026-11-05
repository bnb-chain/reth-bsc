//! Bounded single-block announcement fetches, independent of ancestor recovery.

use super::super::{bsc_protocol::registry, data_availability, BscNetworkPrimitives};
use super::config::{BlockSyncArgs, BlockSyncConfig};
use crate::{chainspec::BscChainSpec, consensus::parlia::EMPTY_WITHDRAWALS_HASH, BscBlock};
use alloy_primitives::{map::B256Map, B256};
use eyre::{ensure, WrapErr};
use futures::{
    future::{AbortHandle, Abortable, BoxFuture},
    stream::FuturesUnordered,
    FutureExt, StreamExt,
};
use rand::seq::SliceRandom;
use reth_eth_wire::{BlockHashNumber, GetBlockBodies, GetBlockHeaders, HeadersDirection};
use reth_network_api::{PeerId, PeerRequest};
use std::{
    future::Future,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    sync::oneshot,
    time::{Instant, MissedTickBehavior},
};

#[cfg(test)]
const MAX_HEAD_FETCHES: usize = 32;
#[cfg(test)]
const MAX_ETH_PEERS: usize = 3;
#[cfg(test)]
const ARRIVE_TIMEOUT: Duration = Duration::from_millis(200);
#[cfg(test)]
const QUICK_TIMEOUT: Duration = Duration::from_millis(400);
#[cfg(test)]
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

type HeadFetch = BoxFuture<'static, (B256, Option<eyre::Result<(BscBlock, PeerId)>>)>;

/// Dropping the service drops all requests; full-block arrival aborts a hash's fetch.
#[derive(Default)]
pub(crate) struct HeadFetches {
    config: Arc<BlockSyncConfig>,
    active: B256Map<AbortHandle>,
    pending: FuturesUnordered<HeadFetch>,
}

impl HeadFetches {
    pub(crate) fn new(config: Arc<BlockSyncConfig>) -> Self {
        Self { config, active: B256Map::default(), pending: FuturesUnordered::new() }
    }

    /// Start a single-block fetch if this hash is idle and capacity is available.
    pub(crate) fn start(&mut self, target: BlockHashNumber, peer: PeerId, spec: Arc<BscChainSpec>) {
        if self.active.contains_key(&target.hash)
            || self.pending.len() >= usize::from(self.config.args.max_head_fetches)
        {
            return;
        }
        let Some(network) = crate::shared::get_network_handle() else { return };
        let hash = target.hash;
        let config = self.config.args;
        self.insert(hash, async move {
            fetch_head(
                &target,
                peer,
                fetch_quick(&target, peer, config.quick_timeout),
                move |peer, request| network.send_request(peer, request),
                move || registry::list_eth_announcers_for(hash),
                &spec,
                config,
            )
            .await
        });
    }

    /// Retain a request under the same hash deduplication and capacity bounds.
    pub(super) fn insert(
        &mut self,
        hash: B256,
        future: impl Future<Output = eyre::Result<(BscBlock, PeerId)>> + Send + 'static,
    ) {
        if self.active.contains_key(&hash)
            || self.pending.len() >= usize::from(self.config.args.max_head_fetches)
        {
            return;
        }
        let (handle, registration) = AbortHandle::new_pair();
        self.active.insert(hash, handle);
        self.pending.push(Box::pin(async move {
            let result = match Abortable::new(future, registration).await {
                Ok(result) => Some(result),
                Err(_) => None,
            };
            (hash, result)
        }));
    }

    #[cfg(test)]
    pub(super) fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Abort this hash's request; the next poll reclaims its capacity.
    pub(crate) fn cancel(&self, hash: &B256) {
        if let Some(handle) = self.active.get(hash) {
            handle.abort();
        }
    }

    /// Drive requests, reclaiming completed and canceled entries before returning results.
    pub(crate) fn poll(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<(B256, eyre::Result<(BscBlock, PeerId)>)>> {
        loop {
            match self.pending.poll_next_unpin(cx) {
                Poll::Ready(Some((hash, result))) => {
                    self.active.remove(&hash);
                    if let Some(result) = result {
                        return Poll::Ready(Some((hash, result)));
                    }
                }
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

async fn fetch_head(
    target: &BlockHashNumber,
    preferred: PeerId,
    quick: impl Future<Output = eyre::Result<BscBlock>>,
    send: impl Fn(PeerId, PeerRequest<BscNetworkPrimitives>),
    announcers: impl Fn() -> Vec<PeerId>,
    spec: &BscChainSpec,
    config: BlockSyncArgs,
) -> eyre::Result<(BscBlock, PeerId)> {
    let started = Instant::now();
    let quick = tokio::time::timeout(config.quick_timeout, quick).fuse();
    tokio::pin!(quick);
    let deadline = tokio::time::sleep_until(started + config.head_timeout);
    tokio::pin!(deadline);
    // Re-read announcers while requests are outstanding: a second announcer can
    // rescue the fetch without waiting for the first peer's request timeout.
    let mut tick =
        tokio::time::interval_at(started + config.eth_fallback_delay, config.eth_fallback_delay);
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut attempted = Vec::with_capacity(usize::from(config.eth_peer_attempts));
    let mut requests = FuturesUnordered::new();
    let mut quick_finished = false;
    let mut last_error = eyre::eyre!("head fetch timed out");
    loop {
        tokio::select! {
            result = &mut quick, if !quick_finished => {
                quick_finished = true;
                match result {
                    Ok(Ok(mut block)) => match validate_block(&mut block, target, spec) {
                        Ok(()) => return Ok((block, preferred)),
                        Err(error) => last_error = error,
                    },
                    Ok(Err(error)) => last_error = error,
                    Err(error) => last_error = eyre::Report::new(error).wrap_err("quick fetch timed out"),
                }
            }
            _ = tick.tick(), if attempted.len() < usize::from(config.eth_peer_attempts) => {
                let mut peers = announcers();
                peers.retain(|peer| !attempted.contains(peer));
                peers.shuffle(&mut rand::rng());
                // Prefer another announcer over the peer already serving quick fetch.
                if let Some(position) = peers.iter().position(|peer| *peer != preferred) {
                    peers.swap(0, position);
                }
                if let Some(peer) = peers.first().copied() {
                    attempted.push(peer);
                    requests.push(fetch_eth(target, peer, &send, spec));
                }
            }
            Some(result) = requests.next(), if !requests.is_empty() => {
                match result {
                    Ok(block) => return Ok(block),
                    Err(error) => last_error = error,
                }
            }
            _ = &mut deadline => return Err(last_error.wrap_err("unable to fetch announced head")),
        }
    }
}

async fn fetch_eth(
    target: &BlockHashNumber,
    peer: PeerId,
    send: &impl Fn(PeerId, PeerRequest<BscNetworkPrimitives>),
    spec: &BscChainSpec,
) -> eyre::Result<(BscBlock, PeerId)> {
    let (response, receiver) = oneshot::channel();
    send(
        peer,
        PeerRequest::GetBlockHeaders {
            request: GetBlockHeaders {
                start_block: target.hash.into(),
                limit: 1,
                skip: 0,
                direction: HeadersDirection::Rising,
            },
            response,
        },
    );
    let mut headers =
        receiver.await.wrap_err("header request canceled")?.wrap_err("header request failed")?;
    ensure!(headers.0.len() == 1, "header fetch must return exactly one header");
    let header = headers.0.remove(0);
    ensure!(
        header.number == target.number && header.hash_slow() == target.hash,
        "announced header mismatch"
    );
    let (response, receiver) = oneshot::channel();
    send(
        peer,
        PeerRequest::GetBlockBodies { request: GetBlockBodies(vec![target.hash]), response },
    );
    let mut bodies =
        receiver.await.wrap_err("body request canceled")?.wrap_err("body request failed")?;
    ensure!(bodies.0.len() == 1, "body fetch must return exactly one body");
    let mut block = BscBlock { header, body: bodies.0.remove(0) };
    validate_block(&mut block, target, spec)?;
    Ok((block, peer))
}

fn validate_block(
    block: &mut BscBlock,
    target: &BlockHashNumber,
    spec: &BscChainSpec,
) -> eyre::Result<()> {
    ensure!(
        block.header.number == target.number && block.header.hash_slow() == target.hash,
        "announced block mismatch"
    );
    // BSC bodies can omit the empty Cancun withdrawals list while retaining its root.
    if block.header.withdrawals_root == Some(EMPTY_WITHDRAWALS_HASH)
        && block.body.inner.withdrawals.is_none()
    {
        block.body.inner.withdrawals = Some(Default::default());
    }
    reth::consensus_common::validation::validate_body_against_header(&block.body, &block.header)?;
    data_availability::validate_data_availability(block, spec)
}

async fn fetch_quick(
    target: &BlockHashNumber,
    peer: PeerId,
    timeout: Duration,
) -> eyre::Result<BscBlock> {
    let mut reply = registry::request_blocks_by_range(peer, target.number, target.hash, 1, timeout)
        .await
        .map_err(eyre::Report::msg)?;
    ensure!(reply.blocks.len() == 1, "quick fetch must return exactly one block");
    Ok(reply.blocks.remove(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        chainspec::bsc::bsc_mainnet,
        node::network::{blocks_by_range::BlocksByRangePacket, bsc_protocol::stream::BscCommand},
    };
    use alloy_consensus::Header;
    use futures::{future::pending, task::noop_waker_ref};
    use parking_lot::Mutex;
    use reth_eth_wire::{BlockBodies, BlockHeaders};
    use reth_primitives_traits::BlockBody;
    use std::pin::Pin;
    use tokio::sync::mpsc;

    fn block() -> BscBlock {
        BscBlock { header: Header { number: 10, ..Default::default() }, ..Default::default() }
    }

    fn target(block: &BscBlock) -> BlockHashNumber {
        BlockHashNumber { hash: block.header.hash_slow(), number: block.header.number }
    }

    fn assert_pending(future: Pin<&mut impl Future>) {
        assert!(future.poll(&mut Context::from_waker(noop_waker_ref())).is_pending());
    }

    #[tokio::test(start_paused = true)]
    async fn quick_fetch_requests_only_announced_block_and_skips_eth() {
        let spec = BscChainSpec::from(bsc_mainnet());
        let block = block();
        let target = target(&block);
        let peer = PeerId::random();
        let (tx, mut rx) = mpsc::unbounded_channel();
        registry::register_peer(peer, tx, 2);
        let fetch = fetch_head(
            &target,
            peer,
            fetch_quick(&target, peer, BlockSyncArgs::default().quick_timeout),
            |_, _| panic!("quick success must not request ETH"),
            || vec![peer],
            &spec,
            BlockSyncArgs::default(),
        );
        tokio::pin!(fetch);
        assert_pending(fetch.as_mut());
        let command = rx.try_recv().unwrap();
        let BscCommand::GetBlocksByRange(request, reply) = command else {
            panic!("range request expected")
        };
        assert_eq!(
            (request.start_block_height, request.start_block_hash, request.count),
            (target.number, target.hash, 1)
        );
        reply
            .send(Ok(BlocksByRangePacket {
                request_id: request.request_id,
                blocks: vec![block.clone()],
            }))
            .unwrap();
        assert_eq!(fetch.await.unwrap(), (block, peer));
        drop(rx);
    }

    #[tokio::test(start_paused = true)]
    async fn slow_quick_fetch_starts_eth_at_200ms_and_cancels_loser() {
        let spec = BscChainSpec::from(bsc_mainnet());
        let block = block();
        let target = target(&block);
        let preferred = PeerId::random();
        let other = PeerId::random();
        let (quick_tx, quick_rx) = oneshot::channel::<BscBlock>();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let started = Instant::now();
        let fetch = fetch_head(
            &target,
            preferred,
            async { Ok(quick_rx.await?) },
            |peer, request| tx.send((peer, request)).unwrap(),
            || vec![preferred, other],
            &spec,
            BlockSyncArgs::default(),
        );
        tokio::pin!(fetch);
        assert_pending(fetch.as_mut());
        tokio::time::advance(Duration::from_millis(199)).await;
        assert_pending(fetch.as_mut());
        assert!(rx.try_recv().is_err());
        tokio::time::advance(Duration::from_millis(1)).await;
        assert_pending(fetch.as_mut());
        let (peer, PeerRequest::GetBlockHeaders { request, response }) = rx.try_recv().unwrap()
        else {
            panic!("header request expected")
        };
        assert_eq!(peer, other);
        assert_eq!(request.start_block, target.hash.into());
        assert_eq!(request.limit, 1);
        assert_eq!(started.elapsed(), ARRIVE_TIMEOUT);
        response.send(Ok(BlockHeaders(vec![block.header.clone()]))).unwrap();
        assert_pending(fetch.as_mut());
        let (peer, PeerRequest::GetBlockBodies { request, response }) = rx.try_recv().unwrap()
        else {
            panic!("body request expected")
        };
        assert_eq!(peer, other);
        assert_eq!(request.0, vec![target.hash]);
        response.send(Ok(BlockBodies(vec![block.body.clone()]))).unwrap();
        assert_eq!(fetch.await.unwrap(), (block, other));
        assert!(quick_tx.is_closed());
    }

    #[tokio::test(start_paused = true)]
    async fn late_announcer_rescues_stalled_eth_and_quick_expires_at_400ms() {
        let spec = BscChainSpec::from(bsc_mainnet());
        let block = block();
        let target = target(&block);
        let preferred = PeerId::random();
        let other = PeerId::random();
        let peers = Mutex::new(vec![preferred]);
        let (quick_tx, quick_rx) = oneshot::channel::<BscBlock>();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let fetch = fetch_head(
            &target,
            preferred,
            async { Ok(quick_rx.await?) },
            |peer, request| tx.send((peer, request)).unwrap(),
            || peers.lock().clone(),
            &spec,
            BlockSyncArgs::default(),
        );
        tokio::pin!(fetch);
        assert_pending(fetch.as_mut());
        tokio::time::advance(ARRIVE_TIMEOUT).await;
        assert_pending(fetch.as_mut());
        let (peer, PeerRequest::GetBlockHeaders { response: stalled, .. }) = rx.try_recv().unwrap()
        else {
            panic!("header request expected")
        };
        assert_eq!(peer, preferred);
        assert!(!quick_tx.is_closed());
        peers.lock().push(other);
        tokio::time::advance(ARRIVE_TIMEOUT).await;
        assert_pending(fetch.as_mut());
        assert!(quick_tx.is_closed());
        let (peer, PeerRequest::GetBlockHeaders { response, .. }) = rx.try_recv().unwrap() else {
            panic!("header request expected")
        };
        assert_eq!(peer, other);
        response.send(Ok(BlockHeaders(vec![block.header.clone()]))).unwrap();
        assert_pending(fetch.as_mut());
        let (_, PeerRequest::GetBlockBodies { response, .. }) = rx.try_recv().unwrap() else {
            panic!("body request expected")
        };
        response.send(Ok(BlockBodies(vec![block.body.clone()]))).unwrap();
        assert_eq!(fetch.await.unwrap(), (block, other));
        assert!(stalled.is_closed());
    }

    #[tokio::test(start_paused = true)]
    async fn all_silent_peers_finish_within_five_seconds() {
        let spec = BscChainSpec::from(bsc_mainnet());
        let target = target(&block());
        let peer = PeerId::random();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let fetch = fetch_head(
            &target,
            peer,
            pending(),
            |peer, request| tx.send((peer, request)).unwrap(),
            || vec![peer],
            &spec,
            BlockSyncArgs::default(),
        );
        tokio::pin!(fetch);
        assert_pending(fetch.as_mut());
        tokio::time::advance(ARRIVE_TIMEOUT).await;
        assert_pending(fetch.as_mut());
        let (_, PeerRequest::GetBlockHeaders { response, .. }) = rx.try_recv().unwrap() else {
            panic!("header request expected")
        };
        tokio::time::advance(ARRIVE_TIMEOUT).await;
        assert_pending(fetch.as_mut());
        tokio::time::advance(FETCH_TIMEOUT - QUICK_TIMEOUT).await;
        assert_eq!(
            format!("{:#}", fetch.await.unwrap_err()),
            "unable to fetch announced head: quick fetch timed out: deadline has elapsed"
        );
        assert!(response.is_closed());
    }

    #[tokio::test]
    async fn cancellation_dedup_capacity_and_shutdown_drop_waiters() {
        let mut fetches = HeadFetches::default();
        let mut senders = Vec::new();
        for number in 0..MAX_HEAD_FETCHES {
            let hash = B256::with_last_byte(number as u8);
            let (tx, rx) = oneshot::channel::<(BscBlock, PeerId)>();
            senders.push(tx);
            fetches.insert(hash, async { Ok(rx.await?) });
            fetches.insert(hash, async { panic!("duplicate fetch polled") });
        }
        fetches.insert(B256::repeat_byte(255), async { panic!("over capacity fetch polled") });
        let mut cx = Context::from_waker(noop_waker_ref());
        assert!(fetches.poll(&mut cx).is_pending());
        assert_eq!(fetches.pending.len(), MAX_HEAD_FETCHES);
        assert_eq!(fetches.active.len(), MAX_HEAD_FETCHES);
        fetches.cancel(&B256::ZERO);
        assert!(fetches.poll(&mut cx).is_pending());
        assert!(senders[0].is_closed());
        assert_eq!(fetches.active.len(), MAX_HEAD_FETCHES - 1);
        drop(fetches);
        assert!(senders.iter().all(oneshot::Sender::is_closed));
    }

    #[test]
    fn responses_must_match_announcement_body_roots_and_blob_sidecars() {
        let spec = BscChainSpec::from(bsc_mainnet());
        let mut good = block();
        let target = target(&good);
        validate_block(&mut good, &target, &spec).unwrap();
        good.header.number += 1;
        assert_eq!(
            validate_block(&mut good, &target, &spec).unwrap_err().to_string(),
            "announced block mismatch"
        );
        let mut bad_body = block();
        bad_body.body.inner.ommers.push(Header::default());
        assert!(matches!(
            validate_block(&mut bad_body, &target, &spec)
                .unwrap_err()
                .downcast_ref::<reth::consensus::ConsensusError>(),
            Some(reth::consensus::ConsensusError::BodyOmmersHashDiff(_))
        ));

        let now =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        let mut blob = crate::node::network::data_availability::tests::blob_block(now);
        blob.header.transactions_root = blob.body.calculate_tx_root();
        blob.header.withdrawals_root = Some(EMPTY_WITHDRAWALS_HASH);
        assert!(blob.body.inner.withdrawals.is_none());
        let hash = blob.header.hash_slow();
        for sidecar in blob.body.sidecars.as_mut().unwrap() {
            sidecar.block_hash = hash;
        }
        let target = BlockHashNumber { hash, number: blob.header.number };
        let spec = BscChainSpec::from(
            reth_chainspec::ChainSpecBuilder::mainnet().cancun_activated().build(),
        );
        validate_block(&mut blob, &target, &spec).unwrap();
        assert!(blob.body.inner.withdrawals.as_ref().unwrap().is_empty());
        blob.body.sidecars = None;
        assert_eq!(
            validate_block(&mut blob, &target, &spec).unwrap_err().to_string(),
            "blob sidecar count mismatch: expected 2, got 0"
        );
    }

    #[tokio::test]
    async fn eth_response_errors_are_not_completed_blocks() {
        let spec = BscChainSpec::from(bsc_mainnet());
        let block = block();
        let target = target(&block);
        let peer = PeerId::random();
        for mode in 0..8 {
            let (tx, mut rx) = mpsc::unbounded_channel();
            let send = |peer, request| tx.send((peer, request)).unwrap();
            let fetch = fetch_eth(&target, peer, &send, &spec);
            tokio::pin!(fetch);
            assert_pending(fetch.as_mut());
            let (_, PeerRequest::GetBlockHeaders { response, .. }) = rx.try_recv().unwrap() else {
                panic!("header request expected")
            };
            let expected = match mode {
                0 => {
                    response.send(Ok(BlockHeaders(vec![]))).unwrap();
                    "header fetch must return exactly one header"
                }
                1 => {
                    response.send(Ok(BlockHeaders(vec![Header::default()]))).unwrap();
                    "announced header mismatch"
                }
                2 => {
                    response.send(Err(reth_network_p2p::error::RequestError::Timeout)).unwrap();
                    "header request failed"
                }
                3 => {
                    drop(response);
                    "header request canceled"
                }
                _ => {
                    response.send(Ok(BlockHeaders(vec![block.header.clone()]))).unwrap();
                    assert_pending(fetch.as_mut());
                    let (_, PeerRequest::GetBlockBodies { response, .. }) = rx.try_recv().unwrap()
                    else {
                        panic!("body request expected")
                    };
                    match mode {
                        4 => {
                            response.send(Ok(BlockBodies(vec![]))).unwrap();
                            "body fetch must return exactly one body"
                        }
                        5 => {
                            response
                                .send(Err(reth_network_p2p::error::RequestError::BadResponse))
                                .unwrap();
                            "body request failed"
                        }
                        6 => {
                            drop(response);
                            "body request canceled"
                        }
                        7 => {
                            response
                                .send(Ok(BlockBodies(vec![block.body.clone(), block.body.clone()])))
                                .unwrap();
                            "body fetch must return exactly one body"
                        }
                        _ => unreachable!(),
                    }
                }
            };
            assert_eq!(fetch.await.unwrap_err().to_string(), expected);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn fallback_never_requests_more_than_three_peers() {
        let spec = BscChainSpec::from(bsc_mainnet());
        let target = target(&block());
        let peers = (0..5).map(|_| PeerId::random()).collect::<Vec<_>>();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let fetch = fetch_head(
            &target,
            peers[0],
            pending(),
            |peer, request| tx.send((peer, request)).unwrap(),
            || peers.clone(),
            &spec,
            BlockSyncArgs::default(),
        );
        tokio::pin!(fetch);
        assert_pending(fetch.as_mut());
        let mut waiters = Vec::new();
        let mut requested = Vec::new();
        for _ in 0..10 {
            tokio::time::advance(ARRIVE_TIMEOUT).await;
            assert_pending(fetch.as_mut());
            if let Ok((peer, PeerRequest::GetBlockHeaders { response, .. })) = rx.try_recv() {
                assert!(!requested.contains(&peer));
                requested.push(peer);
                waiters.push(response);
            }
        }
        assert_eq!(requested.len(), MAX_ETH_PEERS);
        tokio::time::advance(FETCH_TIMEOUT).await;
        assert_eq!(fetch.await.unwrap_err().to_string(), "unable to fetch announced head");
        assert!(waiters.iter().all(oneshot::Sender::is_closed));
    }

    #[tokio::test(start_paused = true)]
    async fn configured_head_timers_and_peer_limit_change_actual_requests() {
        let config = BlockSyncArgs {
            quick_timeout: Duration::from_millis(50),
            eth_fallback_delay: Duration::from_millis(20),
            head_timeout: Duration::from_millis(100),
            eth_peer_attempts: 1,
            ..Default::default()
        };
        config.into_config().unwrap();
        let spec = BscChainSpec::from(bsc_mainnet());
        let target = target(&block());
        let peer = PeerId::random();
        let other = PeerId::random();
        let (quick_tx, quick_rx) = oneshot::channel::<BscBlock>();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let fetch = fetch_head(
            &target,
            peer,
            async { Ok(quick_rx.await?) },
            |peer, request| tx.send((peer, request)).unwrap(),
            || vec![peer, other],
            &spec,
            config,
        );
        tokio::pin!(fetch);
        assert_pending(fetch.as_mut());
        tokio::time::advance(Duration::from_millis(19)).await;
        assert_pending(fetch.as_mut());
        assert!(rx.try_recv().is_err());
        tokio::time::advance(Duration::from_millis(1)).await;
        assert_pending(fetch.as_mut());
        let (selected, PeerRequest::GetBlockHeaders { response, .. }) = rx.try_recv().unwrap()
        else {
            panic!("configured ETH fallback expected")
        };
        assert_eq!(selected, other);
        assert!(!quick_tx.is_closed());
        tokio::time::advance(Duration::from_millis(30)).await;
        assert_pending(fetch.as_mut());
        assert!(quick_tx.is_closed());
        assert!(rx.try_recv().is_err(), "configured single-peer limit must hold");
        tokio::time::advance(Duration::from_millis(50)).await;
        assert_eq!(fetch.await.unwrap_err().to_string(), "unable to fetch announced head");
        assert!(response.is_closed());
    }
}
