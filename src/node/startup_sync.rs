//! Actively discover quiet peers' heads before enabling startup signing.
//!
//! ETH status is a sync hint, not a fork-choice decision. Download a hash-linked chain rooted
//! in executed local history, validate it in Engine, then apply ordinary Parlia fork choice.

use crate::{
    chainspec::BscChainSpec,
    node::{
        consensus::BscForkChoiceEngine,
        engine_api::payload::BscPayloadTypes,
        network::{
            block_import::{
                config::BlockSyncConfig,
                fork_recover::{
                    import_discovered, Discovery, DiscoveryOutcome, ForkRecoverError, RangeFetcher,
                    RecoverTarget, RecoveryGate,
                },
            },
            BscNetworkPrimitives,
        },
        startup_recovery,
    },
    BscBlock,
};
use alloy_consensus::Header;
use alloy_eips::BlockHashOrNumber;
use alloy_primitives::B256;
use futures::future::BoxFuture;
use reth_engine_primitives::ConsensusEngineHandle;
use reth_eth_wire::{GetBlockBodies, GetBlockHeaders, HeadersDirection};
use reth_network::NetworkHandle;
use reth_network_api::{PeerId, PeerRequest, Peers};
use reth_provider::{BlockHashReader, BlockIdReader, BlockNumReader, HeaderProvider};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};
use tokio::{sync::oneshot, time::Instant};

const INTERVAL: Duration = Duration::from_secs(3);
const UNAVAILABLE_GRACE: Duration = Duration::from_secs(30);
const MAX_PEERS: usize = 16;
const HEADER_BATCH: u64 = 128;
const ENGINE_TIMEOUT: Duration = Duration::from_secs(30);

/// Maintain active catch-up independently of unsolicited `NewBlock` announcements.
/// This standalone entrypoint uses its own default recovery gate. Library callers sharing
/// admission with live imports must pass the same config to `run_with_config` and the node.
/// Startup requires a responsive, assessed peer. Unauthenticated hints receive a bounded grace
/// period; once a valid preferred chain is discovered, signing waits for its execution.
/// The startup gate is monotonic. Active discovery continues after it opens, while ordinary
/// mining, finality, and double-signing guards remain responsible for steady-state signing.
pub async fn run<P>(
    provider: P,
    engine: ConsensusEngineHandle<BscPayloadTypes>,
    network: NetworkHandle<BscNetworkPrimitives>,
    chain_spec: Arc<BscChainSpec>,
) where
    P: BlockHashReader
        + BlockIdReader
        + BlockNumReader
        + HeaderProvider<Header = Header>
        + Clone
        + Send
        + Sync
        + 'static,
{
    run_with_config(provider, engine, network, chain_spec, Arc::new(BlockSyncConfig::default()))
        .await;
}

/// Run catch-up with the same per-node limits and recovery gate as live imports.
pub async fn run_with_config<P>(
    provider: P,
    engine: ConsensusEngineHandle<BscPayloadTypes>,
    network: NetworkHandle<BscNetworkPrimitives>,
    chain_spec: Arc<BscChainSpec>,
    config: Arc<BlockSyncConfig>,
) where
    P: BlockHashReader
        + BlockIdReader
        + BlockNumReader
        + HeaderProvider<Header = Header>
        + Clone
        + Send
        + Sync
        + 'static,
{
    let forkchoice = BscForkChoiceEngine::new(provider.clone(), engine.clone(), chain_spec.clone());
    let fetcher = EthRangeFetcher { network: network.clone(), chain_spec, config };
    let mut unavailable = HashMap::new();
    let mut pending_target: Option<Header> = None;
    let mut round = 0usize;
    let mut scanned = HashSet::new();
    loop {
        let mut assessed = false;
        let mut pending = false;
        // Validated competitors outlive their source connection. Only executed canonical state
        // or an ordinary Parlia decision can clear them, never a peer timeout/disconnect.
        if let Ok(info) = provider.chain_info() {
            if let Ok(Some(current)) = provider.header_by_number(info.best_number) {
                if let Some(target) = pending_target.take() {
                    match forkchoice.is_need_reorg(&target, &current).await {
                        Ok(false) => {}
                        _ if target.hash_slow() == current.hash_slow() => {}
                        _ => pending_target = Some(target),
                    }
                }
            }
        }
        pending |= pending_target.is_some();
        match network.get_all_peers().await {
            Ok(mut peers) => {
                if !peers.is_empty() {
                    let offset = round % peers.len();
                    peers.rotate_left(offset);
                    round = round.wrapping_add(MAX_PEERS);
                }
                unavailable.retain(|peer, _| peers.iter().any(|p| p.remote_id == *peer));
                scanned.retain(|peer| peers.iter().any(|p| p.remote_id == *peer));
                let peer_ids = peers.iter().map(|p| p.remote_id).collect::<Vec<_>>();
                if let (Some(saved), Some(peer)) =
                    (startup_recovery::unresolved_persisted_head(), peers.first())
                {
                    if let Err(error) = assess_peer(
                        peer.remote_id,
                        saved.hash,
                        &provider,
                        &engine,
                        &forkchoice,
                        &fetcher,
                    )
                    .await
                    {
                        tracing::debug!(target: "bsc::startup_sync", %error,
                            "Persisted unfinalized head candidate will be retried");
                    }
                }
                // Retry a validated target through a rotating connected source, including when
                // the original source disconnected. It remains pending until state resolves it.
                if let (Some(target), Some(peer)) = (pending_target.as_ref(), peers.first()) {
                    if let Err(error) = assess_peer(
                        peer.remote_id,
                        target.hash_slow(),
                        &provider,
                        &engine,
                        &forkchoice,
                        &fetcher,
                    )
                    .await
                    {
                        tracing::debug!(target: "bsc::startup_sync", %error,
                            "Validated pending head recovery will be retried");
                    }
                }
                for peer in peers.into_iter().take(MAX_PEERS) {
                    let result = assess_peer(
                        peer.remote_id,
                        peer.status.blockhash,
                        &provider,
                        &engine,
                        &forkchoice,
                        &fetcher,
                    )
                    .await;
                    match result {
                        Ok(Assessment::Complete) => {
                            assessed = true;
                            scanned.insert(peer.remote_id);
                            unavailable.remove(&peer.remote_id);
                        }
                        Ok(Assessment::Retry) => pending = true,
                        Ok(Assessment::Pending(target)) => {
                            let replace = match &pending_target {
                                Some(previous) => forkchoice.is_need_reorg(&target, previous).await,
                                None => Ok(true),
                            };
                            match replace {
                                Ok(true) => pending_target = Some(target),
                                Ok(false) => {}
                                Err(error) => tracing::error!(target: "bsc::startup_sync", %error,
                                    "Unable to compare validated pending heads"),
                            }
                            pending = true;
                        }
                        Err(error) => {
                            // A claimed hash/height alone cannot suppress signing forever. Keep
                            // retrying quiet peers after the initial connection grace expires.
                            let first =
                                unavailable.entry(peer.remote_id).or_insert_with(Instant::now);
                            pending |= first.elapsed() < UNAVAILABLE_GRACE;
                            if first.elapsed() >= UNAVAILABLE_GRACE {
                                scanned.insert(peer.remote_id);
                            }
                            tracing::debug!(target: "bsc::startup_sync", peer = %peer.remote_id,
                                %error, "Peer head assessment will be retried");
                        }
                    }
                }
                pending |= peer_ids.iter().any(|peer| !scanned.contains(peer));
            }
            Err(error) => tracing::debug!(target: "bsc::startup_sync", %error,
                "Peer discovery will be retried"),
        }
        if assessed && !pending {
            if let Ok(head) = provider.chain_info() {
                startup_recovery::network_ready(alloy_eips::BlockNumHash {
                    number: head.best_number,
                    hash: head.best_hash,
                });
            }
        }
        tokio::time::sleep(INTERVAL).await;
    }
}

enum Assessment {
    Complete,
    Retry,
    /// Every payload was VALID, but the preferred head has not become canonical yet.
    Pending(Header),
}

async fn assess_peer<P>(
    peer: PeerId,
    advertised: B256,
    provider: &P,
    engine: &ConsensusEngineHandle<BscPayloadTypes>,
    forkchoice: &BscForkChoiceEngine<P>,
    fetcher: &EthRangeFetcher,
) -> eyre::Result<Assessment>
where
    P: BlockHashReader
        + BlockIdReader
        + BlockNumReader
        + HeaderProvider<Header = Header>
        + Clone
        + Send
        + Sync
        + 'static,
{
    let mut headers = fetcher
        .headers(peer, BlockHashOrNumber::Hash(advertised), 1, HeadersDirection::Falling)
        .await?;
    eyre::ensure!(
        headers.len() == 1 && headers[0].hash_slow() == advertised,
        "peer did not return its advertised head"
    );
    let target = headers.remove(0);
    // Probe beyond the greater known height first, then fall back to canonical numeric
    // discovery so a taller local fork cannot hide a shorter, more justified peer chain.
    let (target, tip_confirmed) =
        probe_forward(target, provider.chain_info()?.best_number, |start, limit| {
            fetcher.headers(peer, BlockHashOrNumber::Number(start), limit, HeadersDirection::Rising)
        })
        .await?;
    match assess_target(
        peer,
        target,
        provider,
        engine,
        forkchoice,
        fetcher,
        &fetcher.config.recovery_gate,
    )
    .await?
    {
        Assessment::Complete if !tip_confirmed => Ok(Assessment::Retry),
        assessment => Ok(assessment),
    }
}

async fn probe_forward<F, Fut>(
    advertised: Header,
    local: u64,
    mut fetch: F,
) -> eyre::Result<(Header, bool)>
where
    F: FnMut(u64, u64) -> Fut,
    Fut: std::future::Future<Output = eyre::Result<Vec<Header>>>,
{
    // The fast path resumes at execution progress even when ETH status never changes.
    let upper = advertised.number.max(local);
    let start = upper.checked_add(1).ok_or_else(|| eyre::eyre!("peer head number overflow"))?;
    let headers = fetch(start, HEADER_BATCH).await?;
    eyre::ensure!(headers.len() <= HEADER_BATCH as usize, "oversized peer header response");
    for (offset, header) in headers.iter().enumerate() {
        eyre::ensure!(
            header.number == start + offset as u64,
            "non-contiguous forward header response"
        );
    }
    if let Some(last) = headers.last() {
        // ETH may truncate a response before its tip. Only a later empty numeric probe
        // confirms the tip; execution progress supplies the next round's starting point.
        return Ok((last.clone(), false));
    }
    // A taller local fork must not hide a shorter, more justified peer chain. Re-read numeric
    // origins so a reorg since handshake cannot leave us following its old noncanonical hash.
    let at_upper = fetch(upper, 1).await?;
    if let Some(header) = at_upper.first() {
        eyre::ensure!(
            at_upper.len() == 1 && header.number == upper,
            "invalid peer numeric header response"
        );
        return Ok((header.clone(), true));
    }
    let mut high = upper.saturating_sub(1);
    let at_advertised = fetch(advertised.number, 1).await?;
    let mut best = if let Some(header) = at_advertised.first() {
        eyre::ensure!(
            at_advertised.len() == 1 && header.number == advertised.number,
            "invalid peer numeric header response"
        );
        header.clone()
    } else {
        let genesis = fetch(0, 1).await?;
        eyre::ensure!(
            genesis.len() == 1 && genesis[0].number == 0,
            "peer canonical genesis unavailable"
        );
        genesis[0].clone()
    };
    let mut low = best.number;
    while low < high {
        let middle = low + (high - low).div_ceil(2);
        let response = fetch(middle, 1).await?;
        if let Some(header) = response.first() {
            eyre::ensure!(
                response.len() == 1 && header.number == middle,
                "invalid peer numeric header response"
            );
            best = header.clone();
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    Ok((best, true))
}

async fn assess_target<P>(
    peer: PeerId,
    target: Header,
    provider: &P,
    engine: &ConsensusEngineHandle<BscPayloadTypes>,
    forkchoice: &BscForkChoiceEngine<P>,
    fetcher: &dyn RangeFetcher,
    gate: &RecoveryGate,
) -> eyre::Result<Assessment>
where
    P: BlockHashReader
        + BlockIdReader
        + BlockNumReader
        + HeaderProvider<Header = Header>
        + Clone
        + Send
        + Sync
        + 'static,
{
    let permit = match gate.try_acquire(target.number) {
        Ok(permit) => permit,
        Err(_) => return Ok(Assessment::Retry),
    };
    let blocks = discover_chain(peer, &target, provider, fetcher, &permit).await?;
    if blocks.is_empty() && provider.chain_info()?.best_hash == target.hash_slow() {
        consider_persisted_head(&target, provider, forkchoice).await?;
        return Ok(Assessment::Complete);
    }
    let discovery = Discovery { fork_blocks: blocks, outcome: DiscoveryOutcome::AncestorFound };
    let target_pair = RecoverTarget::single_pair(target.hash_slow(), target.number);
    match tokio::time::timeout(
        ENGINE_TIMEOUT,
        import_discovered(
            target_pair,
            discovery,
            provider.clone(),
            engine.clone(),
            forkchoice.clone(),
            &permit,
        ),
    )
    .await?
    {
        Ok(()) => {}
        Err(ForkRecoverError::ImportHalted { .. }) => return Ok(Assessment::Retry),
        Err(error) => return Err(error.into()),
    }
    let current = provider.chain_info()?;
    let current = provider
        .header_by_number(current.best_number)?
        .ok_or_else(|| eyre::eyre!("executed canonical head missing"))?;
    // A successful FCU may merely mean SYNCING. Check actual canonical state and compare the
    // validated competitor again; VALID alone does not establish catch-up completion.
    consider_persisted_head(&current, provider, forkchoice).await?;
    if current.hash_slow() == target.hash_slow()
        || !forkchoice.is_need_reorg(&target, &current).await?
    {
        Ok(Assessment::Complete)
    } else {
        Ok(Assessment::Pending(target))
    }
}

async fn consider_persisted_head<P>(
    header: &Header,
    provider: &P,
    forkchoice: &BscForkChoiceEngine<P>,
) -> eyre::Result<()>
where
    P: BlockHashReader
        + BlockNumReader
        + HeaderProvider<Header = Header>
        + Clone
        + Send
        + Sync
        + 'static,
{
    startup_recovery::confirm_persisted_ancestor(provider)?;
    let justified = crate::shared::get_snapshot_provider()
        .and_then(|snapshots| snapshots.snapshot_by_hash(&header.hash_slow()))
        .map_or(0, |snapshot| snapshot.vote_data.target_number);
    startup_recovery::supersede_persisted_head(
        header,
        provider.header_td(&header.hash_slow())?,
        justified,
        forkchoice.chain_spec().clone(),
    )
    .await?;
    Ok(())
}

/// Walk only to executed canonical history. Header-only staged downloads cannot terminate this
/// walk, and a competing chain must meet the persisted finalized history before any import.
async fn discover_chain<P>(
    peer: PeerId,
    target: &Header,
    provider: &P,
    fetcher: &dyn RangeFetcher,
    progress: &dyn crate::node::network::block_import::fork_recover::ProgressSink,
) -> eyre::Result<Vec<BscBlock>>
where
    P: BlockHashReader + BlockIdReader + BlockNumReader + HeaderProvider<Header = Header>,
{
    let executed = provider.chain_info()?.best_number;
    let finalized =
        [provider.finalized_block_num_hash()?, startup_recovery::finalized_checkpoint()]
            .into_iter()
            .flatten()
            .max_by_key(|checkpoint| checkpoint.number);
    if let Some(checkpoint) = finalized.filter(|checkpoint| checkpoint.number <= executed) {
        eyre::ensure!(
            provider.block_hash(checkpoint.number)? == Some(checkpoint.hash),
            "executed canonical history conflicts with finalized checkpoint"
        );
    }
    let mut number = target.number;
    let mut hash = target.hash_slow();
    let mut blocks = Vec::new();
    loop {
        if number <= executed && provider.block_hash(number)? == Some(hash) {
            return Ok(blocks);
        }
        eyre::ensure!(
            finalized.is_none_or(|f| number > f.number),
            "peer chain conflicts with finalized history"
        );
        eyre::ensure!(
            blocks.len() < fetcher.max_depth() as usize,
            "peer chain exceeds startup ancestor max_depth={}",
            fetcher.max_depth()
        );
        let count = (fetcher.max_depth() - blocks.len() as u64).min(fetcher.hop_count());
        let response = fetcher.fetch(peer, number, hash, count).await.map_err(eyre::Error::msg)?;
        progress.record_progress();
        eyre::ensure!(
            !response.is_empty() && response.len() <= count as usize,
            "invalid peer ancestor response length"
        );
        for block in response {
            eyre::ensure!(
                block.header.number == number && block.header.hash_slow() == hash,
                "peer ancestor response is not hash-linked"
            );
            if number <= executed && provider.block_hash(number)? == Some(hash) {
                return Ok(blocks);
            }
            eyre::ensure!(
                finalized.is_none_or(|f| number > f.number),
                "peer chain conflicts with finalized history"
            );
            number = number.checked_sub(1).ok_or_else(|| eyre::eyre!("no common genesis"))?;
            hash = block.header.parent_hash;
            blocks.push(block);
        }
    }
}

struct EthRangeFetcher {
    network: NetworkHandle<BscNetworkPrimitives>,
    chain_spec: Arc<BscChainSpec>,
    config: Arc<BlockSyncConfig>,
}

impl EthRangeFetcher {
    async fn headers(
        &self,
        peer: PeerId,
        start_block: BlockHashOrNumber,
        limit: u64,
        direction: HeadersDirection,
    ) -> eyre::Result<Vec<Header>> {
        let (response, rx) = oneshot::channel();
        self.network.send_request(
            peer,
            PeerRequest::GetBlockHeaders {
                request: GetBlockHeaders { start_block, limit, skip: 0, direction },
                response,
            },
        );
        let headers = tokio::time::timeout(self.config.args.recovery_timeout, rx).await???;
        eyre::ensure!(headers.0.len() <= limit as usize, "oversized peer header response");
        Ok(headers.0)
    }
}

impl RangeFetcher for EthRangeFetcher {
    fn max_depth(&self) -> u64 {
        self.config.args.max_depth
    }
    fn hop_count(&self) -> u64 {
        self.config.args.recovery_hop_count
    }

    fn fetch<'a>(
        &'a self,
        peer: PeerId,
        _start_num: u64,
        start_hash: B256,
        count: u64,
    ) -> BoxFuture<'a, Result<Vec<BscBlock>, String>> {
        Box::pin(async move {
            async {
                let headers = self
                    .headers(
                        peer,
                        BlockHashOrNumber::Hash(start_hash),
                        count,
                        HeadersDirection::Falling,
                    )
                    .await?;
                let (response, rx) = oneshot::channel();
                self.network.send_request(
                    peer,
                    PeerRequest::GetBlockBodies {
                        request: GetBlockBodies(headers.iter().map(Header::hash_slow).collect()),
                        response,
                    },
                );
                let bodies =
                    tokio::time::timeout(self.config.args.recovery_timeout, rx).await???;
                eyre::ensure!(bodies.0.len() == headers.len(), "peer omitted requested bodies");
                headers
                    .into_iter()
                    .zip(bodies.0)
                    .map(|(header, body)| {
                        let mut block = BscBlock { header, body };
                        crate::node::network::data_availability::validate_data_availability(
                            &mut block,
                            &self.chain_spec,
                        )?;
                        Ok(block)
                    })
                    .collect::<eyre::Result<Vec<_>>>()
            }
            .await
            .map_err(|error| format!("{error:#}"))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::network::block_import::fork_recover::MAX_FORK_DEPTH;
    use alloy_primitives::B256;
    use alloy_rpc_types_engine::{PayloadStatus, PayloadStatusEnum};
    use reth_engine_primitives::{BeaconEngineMessage, OnForkChoiceUpdated};
    use reth_primitives_traits::SealedHeader;
    use reth_provider::{
        providers::BlockchainProvider,
        test_utils::{create_test_provider_factory, insert_headers},
        CanonChainTracker,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Fetcher {
        blocks: Vec<BscBlock>,
        calls: AtomicUsize,
    }

    impl RangeFetcher for Fetcher {
        fn fetch<'a>(
            &'a self,
            _peer: PeerId,
            start_num: u64,
            _hash: B256,
            count: u64,
        ) -> BoxFuture<'a, Result<Vec<BscBlock>, String>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Box::pin(async move {
                Ok(self
                    .blocks
                    .iter()
                    .filter(|b| b.header.number <= start_num)
                    .rev()
                    .take(count as usize)
                    .cloned()
                    .collect())
            })
        }
    }

    fn blocks(count: u64) -> Vec<BscBlock> {
        let mut parent_hash = B256::ZERO;
        (0..=count)
            .map(|number| {
                let header = Header { number, parent_hash, ..Default::default() };
                parent_hash = header.hash_slow();
                BscBlock { header, body: Default::default() }
            })
            .collect()
    }

    #[tokio::test]
    async fn header_table_ahead_is_not_an_executed_common_ancestor() {
        let blocks = blocks(5);
        let factory = create_test_provider_factory();
        let headers =
            blocks.iter().map(|b| SealedHeader::seal_slow(b.header.clone())).collect::<Vec<_>>();
        insert_headers(&factory, &headers);
        let provider = BlockchainProvider::with_latest(factory, headers[2].clone()).unwrap();
        provider.set_finalized(headers[1].clone());
        let fetcher = Fetcher { blocks: blocks.clone(), calls: AtomicUsize::new(0) };
        let discovered =
            discover_chain(PeerId::random(), &blocks[5].header, &provider, &fetcher, &())
                .await
                .unwrap();
        assert_eq!(discovered.iter().map(|b| b.header.number).collect::<Vec<_>>(), vec![5, 4, 3]);
        assert_eq!(fetcher.calls.load(Ordering::Relaxed), 1);
        assert!(discover_chain(PeerId::random(), &blocks[2].header, &provider, &fetcher, &())
            .await
            .unwrap()
            .is_empty());
        assert_eq!(fetcher.calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn refuses_unlinked_or_empty_ancestor_responses() {
        let blocks = blocks(5);
        let factory = create_test_provider_factory();
        let headers = blocks[..=2]
            .iter()
            .map(|b| SealedHeader::seal_slow(b.header.clone()))
            .collect::<Vec<_>>();
        insert_headers(&factory, &headers);
        let provider = BlockchainProvider::with_latest(factory, headers[2].clone()).unwrap();
        let mut forged = blocks.clone();
        forged[4].header.timestamp = 1;
        let fetcher = Fetcher { blocks: forged, calls: AtomicUsize::new(0) };
        assert_eq!(
            discover_chain(PeerId::random(), &blocks[5].header, &provider, &fetcher, &())
                .await
                .unwrap_err()
                .to_string(),
            "peer ancestor response is not hash-linked"
        );
        let empty = Fetcher { blocks: vec![], calls: AtomicUsize::new(0) };
        assert_eq!(
            discover_chain(PeerId::random(), &blocks[5].header, &provider, &empty, &())
                .await
                .unwrap_err()
                .to_string(),
            "invalid peer ancestor response length"
        );
    }

    #[tokio::test]
    async fn refuses_competing_history_at_finalized_height() {
        let local = blocks(3);
        let factory = create_test_provider_factory();
        let headers =
            local.iter().map(|b| SealedHeader::seal_slow(b.header.clone())).collect::<Vec<_>>();
        insert_headers(&factory, &headers);
        let provider = BlockchainProvider::with_latest(factory, headers[3].clone()).unwrap();
        provider.set_finalized(headers[2].clone());
        let mut fork = local;
        fork[2].header.timestamp = 1;
        fork[3].header.parent_hash = fork[2].header.hash_slow();
        let target = fork[3].header.clone();
        let fetcher = Fetcher { blocks: fork, calls: AtomicUsize::new(0) };
        assert_eq!(
            discover_chain(PeerId::random(), &target, &provider, &fetcher, &())
                .await
                .unwrap_err()
                .to_string(),
            "peer chain conflicts with finalized history"
        );
    }
    #[tokio::test]
    async fn valid_payloads_with_syncing_fcu_do_not_complete_assessment() {
        let blocks = blocks(5);
        let factory = create_test_provider_factory();
        let headers =
            blocks.iter().map(|b| SealedHeader::seal_slow(b.header.clone())).collect::<Vec<_>>();
        insert_headers(&factory, &headers);
        let provider = BlockchainProvider::with_latest(factory, headers[2].clone()).unwrap();
        let fetcher = Fetcher { blocks: blocks.clone(), calls: AtomicUsize::new(0) };
        let (tx, mut requests) = tokio::sync::mpsc::unbounded_channel();
        let engine = ConsensusEngineHandle::new(tx);
        let choice = BscForkChoiceEngine::new(
            provider.clone(),
            engine.clone(),
            Arc::new(BscChainSpec::from(crate::chainspec::bsc::bsc_mainnet())),
        );
        let gate = BlockSyncConfig::default().recovery_gate;
        let work = assess_target(
            PeerId::random(),
            blocks[5].header.clone(),
            &provider,
            &engine,
            &choice,
            &fetcher,
            &gate,
        );
        let responder = async {
            let mut payloads = 0;
            let mut fcus = 0;
            for _ in 0..7 {
                match requests.recv().await.unwrap() {
                    BeaconEngineMessage::NewPayload { tx, .. } => {
                        payloads += 1;
                        tx.send(Ok(PayloadStatus::new(PayloadStatusEnum::Valid, None))).unwrap();
                    }
                    BeaconEngineMessage::ForkchoiceUpdated { tx, .. } => {
                        fcus += 1;
                        tx.send(Ok(OnForkChoiceUpdated::valid(PayloadStatus::new(
                            PayloadStatusEnum::Syncing,
                            None,
                        ))))
                        .unwrap();
                    }
                    _ => panic!("unexpected Engine message"),
                }
            }
            assert_eq!((payloads, fcus), (3, 4));
        };
        let (result, ()) = tokio::join!(work, responder);
        let Assessment::Pending(target) = result.unwrap() else {
            panic!("SYNCING FCU must retain validated target")
        };
        assert_eq!(target.hash_slow(), headers[5].hash());
        assert_eq!(provider.chain_info().unwrap().best_number, 2);
    }
    #[tokio::test]
    async fn stale_status_discovers_shorter_peer_head_above_its_advertised_height() {
        let chain = blocks(126);
        let mut requests = Vec::new();
        let (tip, complete) = probe_forward(chain[95].header.clone(), 130, |start, limit| {
            requests.push((start, limit));
            std::future::ready(Ok(chain
                .iter()
                .filter(|b| b.header.number >= start)
                .take(limit as usize)
                .map(|b| b.header.clone())
                .collect()))
        })
        .await
        .unwrap();
        assert_eq!(
            requests,
            vec![
                (131, HEADER_BATCH),
                (130, 1),
                (95, 1),
                (112, 1),
                (121, 1),
                (125, 1),
                (127, 1),
                (126, 1)
            ]
        );
        assert_eq!(tip.number, 126);
        assert!(complete);
        // The local branch may be at 130; it never determines the remote probe origin.
        assert!(tip.number < 130);
    }
    #[tokio::test]
    async fn unchanged_status_does_not_repeat_the_first_download_window() {
        let chain = blocks(2300);
        let mut local = 130;
        let mut origins = Vec::new();
        loop {
            let (target, complete) =
                probe_forward(chain[95].header.clone(), local, |start, limit| {
                    origins.push(start);
                    std::future::ready(Ok(chain
                        .iter()
                        .filter(|b| b.header.number >= start)
                        .take(limit as usize)
                        .map(|b| b.header.clone())
                        .collect()))
                })
                .await
                .unwrap();
            assert!(target.number > local || (complete && target.number == local));
            local = target.number;
            if complete {
                break;
            }
        }
        assert_eq!(local, 2300);
        assert!(origins.iter().any(|origin| *origin > 95 + MAX_FORK_DEPTH));
        assert_eq!(origins[0], 131);
        assert_eq!(&origins[origins.len() - 2..], &[2301, 2300]);
    }
}
