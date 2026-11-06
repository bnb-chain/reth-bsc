use super::{
    config::BlockSyncConfig,
    handle::ImportHandle,
    metrics::Queue,
    propagation::{self, PropagationSender},
    IMPORT_QUEUE_CAPACITY,
};
use crate::{
    chainspec::BscChainSpec,
    consensus::{parlia::vote_pool, ParliaConsensusErr},
    node::{
        consensus::{BscConsensus, BscForkChoiceEngine},
        engine::BscBuiltPayload,
        engine_api::payload::BscPayloadTypes,
        evm::util::insert_header_to_cache_with_hash,
        miner::startup_diagnostics,
        network::BscNewBlock,
    },
    BscBlock, BscBlockBody,
};
use alloy_consensus::{BlockBody, Header};
use alloy_eips::BlockNumberOrTag;
use alloy_primitives::{Address, B256, U128, U256};
use alloy_rpc_types_engine::{ForkchoiceState, PayloadStatusEnum};
use futures::{future::Either, stream::FuturesUnordered, FutureExt, StreamExt};
use parking_lot::RwLock;
use reth::{consensus::HeaderValidator, network::cache::LruCache};
use reth_engine_primitives::{ConsensusEngineHandle, EngineTypes};
use reth_engine_tree::engine::EngineApiRequest;
use reth_eth_wire::{BlockHashNumber, GetBlockHeaders, NewBlock};
use reth_eth_wire_types::broadcast::NewBlockHashes;
use reth_network::{
    import::{BlockImportError, BlockImportEvent, BlockImportOutcome, BlockValidation},
    message::{BlockRequest, NewBlockMessage, PeerMessage, PeerResponse},
    FetchClient, NetworkHandle,
};
use reth_network_api::{PeerId, Peers, ReputationChangeKind};
use reth_node_ethereum::EthEngineTypes;
use reth_payload_builder_primitives::Events;
use reth_payload_primitives::{BuiltPayload, PayloadTypes};
use reth_primitives_traits::{AlloyBlockHeader, Block, NodePrimitives, SealedBlock, SealedHeader};
use reth_provider::{
    BlockHashReader, BlockNumReader, BlockReaderIdExt, HeaderProvider, ReceiptProvider,
};
use std::{
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    task::{Context, Poll},
};
use tokio::sync::mpsc::{self, OwnedPermit, Receiver};

/// Network message containing a new block
pub(crate) type BlockMsg = NewBlockMessage<BscNewBlock>;

/// Import outcome for a block
pub(crate) type Outcome = BlockImportOutcome<BscNewBlock>;

/// Import event for a block
pub(crate) type ImportEvent = BlockImportEvent<BscNewBlock>;

/// Future that processes a block import and returns its outcome
type ImportFut = Pin<Box<dyn Future<Output = Option<Outcome>> + Send>>;
type RemoteImportFut = Pin<Box<dyn Future<Output = RemoteImportResult> + Send>>;

/// Completion metadata stays with its future so only its own dedup entry is released.
struct RemoteImportResult {
    hash: B256,
    outcome: Option<Outcome>,
}

/// Channel message type for incoming blocks
pub(crate) type IncomingBlock = (BlockMsg, PeerId);

/// Channel message type for incoming mined blocks
pub(crate) type IncomingMinedBlock = (BscBuiltPayload, BlockMsg);

/// Channel message for a selected (sealed, unexecuted) BEP-675 BidBlock:
/// `(sealed, builder, bid_hash, gas_fee, system_tx_start)`.
/// Unlike [`IncomingMinedBlock`] there is no executed payload: under zero-simulate the validator
/// broadcasts first and executes on import.
pub(crate) type IncomingBidBlock = (SealedBlock<BscBlock>, Address, B256, U256, usize);

/// Locally produced blocks share a bounded ingress, isolated from remote announcements.
/// The variants retain separate handlers: sharing capacity does not make a BidBlock an
/// executed payload. BidBlocks broadcast before validation; fallback blocks validate first.
#[derive(Debug)]
pub(crate) enum IncomingLocalBlock {
    Mined(Box<IncomingMinedBlock>),
    Bid(Box<IncomingBidBlock>),
    /// The submit_built_payload=false fallback: ask Engine to validate before propagation.
    Unexecuted(BlockMsg),
}

/// Channel message type for incoming block hashes
pub(crate) type IncomingHashes = (NewBlockHashes, PeerId);

/// Size of the LRU cache for processed blocks.
const LRU_PROCESSED_BLOCKS_SIZE: u32 = 100;
/// Remote new-payload/fork-choice futures retained while waiting on the Engine.
/// For example, with 32 unfinished remote imports, the next near-head block is rejected
/// (and counted/logged) until a slot frees. Excludes local imports and staged-sync downloads.
const MAX_PENDING_IMPORTS: usize = 32;
/// Maximum items processed per source in one poll before yielding to other work.
/// For example, 100 queued network messages get at most 32 turns before local/hash work.
/// This is per source, not a total concurrency cap; one hashes message may contain many hashes.
const POLL_BATCH: usize = 32;
/// Geth's default backward window, retained for default-policy regression assertions.
#[cfg(test)]
const MAX_UNCLE_DISTANCE: u64 = 11;
/// Default boundary used by existing pipeline regression tests. Runtime uses max-depth.
#[cfg(test)]
const PIPELINE_TRIGGER_DELTA: u64 =
    crate::node::network::block_import::fork_recover::MAX_FORK_DEPTH;

/// A service that handles bidirectional block import communication with the network.
/// It receives new blocks from the network via `from_network` channel and sends back
/// import outcomes via `to_network` channel.
pub struct ImportService<Provider>
where
    Provider: BlockNumReader + HeaderProvider + Clone,
{
    /// The handle to communicate with the engine service
    engine: ConsensusEngineHandle<BscPayloadTypes>,
    /// The fork choice engine for BSC
    forkchoice_engine: BscForkChoiceEngine<Provider>,
    /// Cheap header/parent checks before diffusion, without waiting for child execution.
    propagation_consensus: BscConsensus<BscChainSpec>,
    /// Receive the new block from the network
    from_network: Receiver<IncomingBlock>,
    /// Local blocks, isolated from remote ingress and backpressured by propagation.
    from_local: Receiver<IncomingLocalBlock>,
    /// Receive block hashes from the network for downloading
    from_hashes: Receiver<IncomingHashes>,
    /// Send the event of the import to the network
    to_network: PropagationSender,
    /// Pending block imports.
    pending_imports: FuturesUnordered<RemoteImportFut>,
    /// Each local fallback holds a propagation permit until validation finishes, bounding
    /// these futures by the local output capacity independently of remote imports.
    pending_local_imports: FuturesUnordered<ImportFut>,
    sync_config: Arc<BlockSyncConfig>,
    /// Near-head single-block downloads do not occupy ancestor recovery slots.
    head_fetches: super::head_fetch::HeadFetches,
    /// Cache of processed block hashes to avoid reprocessing the same block.
    processed_blocks: LruCache<B256>,
    /// Cache of queued block hashes to avoid processing the same block.
    queued_blocks: LruCache<B256>,
    /// Heads currently being fork-recovered. Prevents duplicate spawned tasks
    /// when the same head is announced repeatedly.
    recovering_heads: crate::node::network::block_import::fork_recover::RecoveringHeads,
    /// Heads whose most recent recovery attempt failed. Suppresses
    /// re-spawning recovery until the cooldown elapses. Prevents storm
    /// behaviour when the 3s head-announce tick re-announces the same
    /// unreachable head.
    failed_heads: crate::node::network::block_import::fork_recover::FailedHeadsCooler,
    /// Admits one ancestor recovery at a time. Concurrent recoveries walk
    /// near-identical ranges, so the extra ones are pure duplicated work — see
    /// [`fork_recover::RecoveryGate`] and bnb-chain/reth-bsc#456.
    ///
    /// [`fork_recover::RecoveryGate`]: crate::node::network::block_import::fork_recover::RecoveryGate
    recovery_gate: crate::node::network::block_import::fork_recover::RecoveryGate,
    /// Periodic timer for head announcement.
    announce_interval: tokio::time::Interval,
    pipeline_targets: super::pipeline_targets::PipelineTargets,
}

/// Log a `recover_ancestors` error at a level matching its severity.
///
/// `ImportHalted` with committed progress is the expected outcome whenever the
/// engine wants ancestors deeper than this walk reached: the attempt still
/// advanced the canonical tip, so the next one starts closer. Logging it at
/// `warn` alongside genuine failures is what made bnb-chain/reth-bsc#456 look
/// like thousands of failed recoveries rather than incremental progress.
fn log_recovery_error(
    err: &crate::node::network::block_import::fork_recover::ForkRecoverError,
    head_hash: B256,
    head_num: u64,
    source: &'static str,
) {
    use crate::node::network::block_import::fork_recover::ForkRecoverError;
    match err {
        ForkRecoverError::ImportHalted { num, committed: Some(committed) } => {
            tracing::info!(
                target: "bsc::block_import",
                %head_hash,
                head_num,
                halted_at = num,
                committed,
                source,
                "Fork recovery committed partial progress"
            );
        }
        _ => {
            tracing::warn!(
                target: "bsc::block_import",
                %head_hash,
                head_num,
                error = %err,
                source,
                "Fork recovery failed"
            );
        }
    }
}

/// Pick a peer to route `GetBlocksByRange` to. Only bsc/2 peers qualify —
/// bsc/1 peers don't speak `GetBlocksByRange` and would kick us with
/// `SubprotocolSpecific`.
///
/// Only the announcing peer is eligible: it is the one peer known to have
/// the block. Routing the request to an arbitrary bsc/2 peer asks a node
/// that may have never seen the hash; geth answers `GetBlocksByRange` for
/// an unknown start block with an error and disconnects the requester
/// (`DiscSubprotocolError`), so every misrouted request costs us a peer.
/// If the announcer doesn't speak bsc/2, give up — the caller's cooldown
/// and the far-behind pipeline trigger handle the gap instead.
fn resolve_bsc_peer_static(announcer: PeerId) -> Option<PeerId> {
    crate::node::network::bsc_protocol::registry::is_v2_peer(announcer).then_some(announcer)
}

/// Recheck the complete target while independent ancestor requests are pending.
/// Header availability is only a prompt to ask Engine, never execution evidence.
async fn validate_target_during_recovery<P: HeaderProvider<Header = Header>>(
    block: BscBlock,
    provider: &P,
    engine: ConsensusEngineHandle<BscPayloadTypes>,
    recovery: impl Future<Output = Result<(), super::fork_recover::ForkRecoverError>>,
) -> Result<(), super::fork_recover::ForkRecoverError> {
    let header = &block.header;
    let block_hash = header.hash_slow();
    let block_number = header.number;
    tokio::pin!(recovery);
    let mut recovery_result = None;
    let mut probes = FuturesUnordered::new();
    let mut ticks = tokio::time::interval_at(
        tokio::time::Instant::now() + std::time::Duration::from_millis(50),
        std::time::Duration::from_millis(50),
    );
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        // Keep at most one target request in flight, including when
        // recovery finishes while Engine is still processing a probe.
        if recovery_result.is_some() && probes.is_empty() {
            let engine = engine.clone();
            let payload =
                BscPayloadTypes::block_to_payload(block.clone().seal_unchecked(block_hash), None);
            probes.push(async move { engine.new_payload(payload).await }.boxed());
        }
        tokio::select! {
            result = &mut recovery, if recovery_result.is_none() => {
                recovery_result = Some(result);
            }
            status = probes.next(), if !probes.is_empty() => {
                match status.expect("one target probe") {
                    Ok(status) if matches!(status.status, PayloadStatusEnum::Valid) => {
                        if recovery_result.is_none() {
                            tracing::info!(target: "bsc::fork_recover", %block_hash, block_number,
                                "Target became Valid; canceling pending ancestor recovery");
                        }
                        break Ok(());
                    },
                    Ok(status) if matches!(status.status, PayloadStatusEnum::Invalid { .. }) => {
                        break Err(super::fork_recover::ForkRecoverError::ImportInvalid {
                            num: block_number, reason: format!("{:?}", status.status),
                        });
                    }
                    _ => {
                        if let Some(result) = recovery_result.take() {
                            break result.and_then(|_| Err(super::fork_recover::ForkRecoverError::ImportHalted {
                                num: block_number, committed: None,
                            }));
                        }
                    }
                }
            }
            _ = ticks.tick(), if probes.is_empty() && recovery_result.is_none() => {
                if provider.header(header.parent_hash).ok().flatten().is_some() {
                    let engine = engine.clone();
                    let payload = BscPayloadTypes::block_to_payload(
                        block.clone().seal_unchecked(block_hash), None,
                    );
                    probes.push(async move { engine.new_payload(payload).await }.boxed());
                }
            }
        }
    }
}

impl<Provider> ImportService<Provider>
where
    Provider: BlockNumReader
        + BlockHashReader
        + HeaderProvider<Header = Header>
        + ReceiptProvider
        + Clone
        + Send
        + Sync
        + 'static,
{
    /// Create a new block import service
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        provider: Provider,
        chain_spec: Arc<BscChainSpec>,
        engine: ConsensusEngineHandle<BscPayloadTypes>,
        from_network: Receiver<IncomingBlock>,
        from_local: Receiver<IncomingLocalBlock>,
        from_hashes: Receiver<IncomingHashes>,
        to_network: PropagationSender,
    ) -> Self {
        let propagation_consensus = BscConsensus::new(chain_spec.clone());
        let forkchoice_engine = BscForkChoiceEngine::new(provider, engine.clone(), chain_spec);

        if let Err(e) = crate::shared::set_fork_choice_engine(forkchoice_engine.clone()) {
            tracing::warn!(target: "bsc::block_import", error = %e, "Fork choice engine already initialized; skipping global set");
        }

        let sync_config = Arc::new(BlockSyncConfig::default());
        Self {
            engine: engine.clone(),
            forkchoice_engine,
            propagation_consensus,
            from_network,
            from_local,
            from_hashes,
            to_network,
            pending_imports: FuturesUnordered::new(),
            pending_local_imports: FuturesUnordered::new(),
            head_fetches: super::head_fetch::HeadFetches::new(sync_config.clone()),
            sync_config: sync_config.clone(),
            processed_blocks: LruCache::new(LRU_PROCESSED_BLOCKS_SIZE),
            queued_blocks: LruCache::new(LRU_PROCESSED_BLOCKS_SIZE),
            recovering_heads:
                crate::node::network::block_import::fork_recover::new_recovering_heads(
                    LRU_PROCESSED_BLOCKS_SIZE,
                ),
            failed_heads: crate::node::network::block_import::fork_recover::new_failed_heads_cooler(
                LRU_PROCESSED_BLOCKS_SIZE,
            ),
            recovery_gate: sync_config.recovery_gate.clone(),
            pipeline_targets: super::pipeline_targets::PipelineTargets::new(engine.clone()),
            announce_interval: {
                // 3s ≈ 6-7 BSC slots (450ms each). Fast enough to break fork
                // livelocks, slow enough to be negligible overhead.
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(3));
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                interval
            },
        }
    }

    pub(crate) fn with_sync_config(mut self, config: Arc<BlockSyncConfig>) -> Self {
        self.recovery_gate = config.recovery_gate.clone();
        self.head_fetches = super::head_fetch::HeadFetches::new(config.clone());
        self.sync_config = config;
        self
    }

    fn send_remote_event(&self, event: ImportEvent) {
        self.to_network.send_remote(event);
    }

    /// Process a new payload and return the outcome
    fn new_payload(&self, block: BlockMsg, peer_id: PeerId) -> ImportFut {
        self.new_payload_inner(block, peer_id, false)
    }

    fn new_payload_inner(&self, mut block: BlockMsg, peer_id: PeerId, fetched: bool) -> ImportFut {
        let engine = self.engine.clone();
        let forkchoice_engine = self.forkchoice_engine.clone();
        let recovering_heads = self.recovering_heads.clone();
        let failed_heads = self.failed_heads.clone();
        let recovery_gate = self.recovery_gate.clone();
        let sync_config = self.sync_config.clone();

        let announced_hash = block.hash;
        let block_hash = block.block.0.block.header.hash_slow();
        tracing::debug!(target: "bsc::block_import", "New payload: block = ({:?}, {:?}), peer_id = {:?}", block.block.0.block.header.number, block_hash, peer_id);
        let admitted_at = std::time::Instant::now();
        Box::pin(async move {
            startup_diagnostics::import_started(block_hash);
            if announced_hash != block_hash {
                tracing::warn!(
                    target: "bsc::block_import",
                    number = block.block.0.block.header.number,
                    announced_hash = %announced_hash,
                    computed_hash = %block_hash,
                    peer_id = %peer_id,
                    "Rejecting new payload with mismatched announced hash"
                );
                return Outcome {
                    peer: peer_id,
                    result: Err(BlockImportError::Other(
                        std::io::Error::other(format!(
                            "announced block hash {announced_hash} does not match computed header hash {block_hash}"
                        ))
                        .into(),
                    )),
                }
                .into();
            }

            let sealed_block = block.block.0.block.clone().seal_unchecked(block_hash);
            let header = sealed_block.header().clone();
            let payload = BscPayloadTypes::block_to_payload(sealed_block, None);
            let new_payload_started = std::time::Instant::now();
            let result = engine.new_payload(payload).await;
            startup_diagnostics::new_payload_finished(block_hash, new_payload_started.elapsed());
            if admitted_at.elapsed() >= std::time::Duration::from_millis(500) {
                tracing::info!(target: "bsc::block_import", %block_hash, block_number = header.number,
                    %peer_id, fetched, queue_ms = new_payload_started.duration_since(admitted_at).as_millis() as u64,
                    engine_ms = new_payload_started.elapsed().as_millis() as u64, result = ?result,
                    "Slow remote payload validation");
            }
            match result {
                Ok(payload_status) => {
                    match payload_status.status {
                        PayloadStatusEnum::Valid => {
                            tracing::debug!(target: "bsc::block_import", "New payload is valid, block_hash = {:?}, block_number = {}, peer_id = {:?}", block.hash, header.number, peer_id);
                            // handle fork choice update with valid payload
                            if let Err(e) = forkchoice_engine.update_forkchoice(&header).await {
                                tracing::warn!(target: "bsc::block_import", "Failed to update fork choice: {}", e);
                                if fetched {
                                    return None;
                                }
                            } else {
                                tracing::debug!(target: "bsc::block_import", "Succeed to update fork choice for new payload: number = {:?}, hash = {:?}", header.number, block_hash);
                            }
                            if fetched {
                                match fetched_block_td(&forkchoice_engine, &header) {
                                    Ok(Some(td)) => Arc::make_mut(&mut block.block).0.td = td,
                                    Ok(None) => {
                                        tracing::debug!(target: "bsc::block_import", %block_hash, "Imported fetched block without local parent TD; suppressing propagation");
                                        return None;
                                    }
                                    Err(error) => {
                                        tracing::warn!(target: "bsc::block_import", %block_hash, %error, "Cannot derive fetched block TD; suppressing propagation");
                                        return None;
                                    }
                                }
                            }
                            let validation = if fetched {
                                // The fetched header is only eligible for full diffusion after
                                // Engine has checked the seal and execution, and TD is known.
                                BlockValidation::ValidHeader { block }
                            } else {
                                BlockValidation::ValidBlock { block }
                            };
                            Outcome { peer: peer_id, result: Ok(validation) }.into()
                        }
                        PayloadStatusEnum::Invalid { validation_error } => {
                            // Do NOT penalize the peer for Invalid blocks.
                            //
                            // In BSC's PoSA with devp2p block propagation, Invalid
                            // frequently results from timing issues during concurrent
                            // reorgs — the block itself is legitimate but was executed
                            // against the wrong state. Penalizing the peer with BadBlock
                            // (-16384 reputation) for this drains peers rapidly,
                            // especially under BSC's fast block time (0.45s), where
                            // concurrent forks are routine.
                            //
                            // This aligns with geth's behavior: geth's fetcher only
                            // drops a peer when header verification fails
                            // (verifyHeader), never when block execution fails
                            // (insertChain). Truly malicious peers are still caught by
                            // the network layer's BadMessage / BadProtocol penalties.
                            tracing::debug!(
                                target: "bsc::block_import",
                                block_hash = %header.hash_slow(),
                                block_number = header.number,
                                %validation_error,
                                peer = %peer_id,
                                "New payload returned Invalid - not penalizing peer"
                            );
                            None
                        }
                        PayloadStatusEnum::Syncing => {
                            // Parent block is missing. Launch fork-aware ancestor
                            // recovery rather than a naive range fetch + premature
                            // FCU. The recovery task also owns the final FCU.
                            let block_number = header.number;
                            tracing::info!(
                                target: "bsc::block_import",
                                %block_hash,
                                block_number,
                                parent_hash = %header.parent_hash,
                                peer = %peer_id,
                                "New payload returned Syncing - recovering missing ancestors"
                            );

                            if failed_heads.is_cooling(&block_hash) {
                                tracing::debug!(
                                    target: "bsc::block_import",
                                    %block_hash,
                                    block_number,
                                    "Skipping fork recovery: head is in cooldown after recent failure"
                                );
                                return None;
                            }

                            // Single-flight: an in-flight recovery already walks back
                            // to the same common ancestor, so a second one for a head
                            // a few blocks higher is duplicated work, not progress.
                            let permit = match recovery_gate.try_acquire(block_number) {
                                Ok(permit) => permit,
                                Err(in_flight_head) => {
                                    tracing::debug!(
                                        target: "bsc::block_import",
                                        %block_hash,
                                        block_number,
                                        in_flight_head,
                                        "Skipping fork recovery: all recovery slots in flight"
                                    );
                                    return None;
                                }
                            };

                            // Recovery remains owned by the bounded import future,
                            // so dropping the service also releases its slot.
                            {
                                let mut guard = recovering_heads.lock();
                                if guard.contains(&block_hash) {
                                    return None;
                                }
                                guard.insert(block_hash);
                            }
                            let _guard = crate::node::network::block_import::fork_recover::RecoveringHeadGuard::new(
                            block_hash, recovering_heads.clone(),
                        );
                            let local_tip =
                                forkchoice_engine.provider.best_block_number().unwrap_or(0);
                            let near_head = block_number.saturating_sub(local_tip)
                                <= sync_config.args.near_head_distance;
                            let peer = if fetched {
                                let peers = crate::node::network::bsc_protocol::registry::list_live_v2_announcers_for(block_hash);
                                peers
                                    .iter()
                                    .find(|peer| **peer == peer_id)
                                    .copied()
                                    .or_else(|| peers.first().copied())
                            } else {
                                resolve_bsc_peer_static(peer_id)
                            };
                            let Some(peer) = peer.or_else(|| near_head.then_some(peer_id)) else {
                                tracing::debug!(target: "bsc::block_import", %block_hash, fetched,
                                "No announcing bsc/2 peer for parent recovery; awaiting another announcement");
                                return None;
                            };
                            let fetcher = crate::node::network::block_import::fork_recover::ParentRangeFetcher {
                                range: crate::node::network::block_import::fork_recover::BscRangeFetcher(
                                    forkchoice_engine.chain_spec().clone(), sync_config.clone(),
                                ),
                                eth_peer: peer_id,
                                single_block_floor: near_head.then_some(local_tip.saturating_sub(sync_config.args.near_head_distance)),
                            };
                            // Recover only the parent. A known parent header is a reason to
                            // recheck execution, never proof that this target is Valid.
                            let recover_target = crate::node::network::block_import::fork_recover::RecoverTarget::single_pair(
                            header.parent_hash, block_number.saturating_sub(1),
                        );
                            let recovery =
                                crate::node::network::block_import::fork_recover::recover_ancestors(
                                    peer,
                                    recover_target,
                                    forkchoice_engine.provider.clone(),
                                    engine.clone(),
                                    forkchoice_engine.clone(),
                                    &fetcher,
                                    &permit,
                                );
                            let validation = validate_target_during_recovery(
                                block.block.0.block.clone(),
                                &forkchoice_engine.provider,
                                engine.clone(),
                                recovery,
                            )
                            .await;
                            // Release stale range waiters before advancing fork choice. Late
                            // responses cannot restart imports or retain recovery admission.
                            if let Err(err) = validation {
                                log_recovery_error(&err, block_hash, block_number, "Syncing path");
                                failed_heads.mark_failed(block_hash);
                                return None;
                            }
                            if let Err(error) = forkchoice_engine.update_forkchoice(&header).await {
                                tracing::warn!(target: "bsc::block_import", %block_hash, %error, "Target fork choice failed after parent recovery");
                                failed_heads.mark_failed(block_hash);
                                return None;
                            }
                            if fetched {
                                match fetched_block_td(&forkchoice_engine, &header) {
                                    Ok(Some(td)) => Arc::make_mut(&mut block.block).0.td = td,
                                    result => {
                                        tracing::debug!(target: "bsc::block_import", %block_hash, ?result, "Fetched target TD unavailable after recovery; suppressing propagation");
                                        return None;
                                    }
                                }
                                return Some(Outcome {
                                    peer: peer_id,
                                    result: Ok(BlockValidation::ValidHeader { block }),
                                });
                            }
                            Some(Outcome {
                                peer: peer_id,
                                result: Ok(BlockValidation::ValidBlock { block }),
                            })
                        }
                        _ => None,
                    }
                }
                Err(err) => {
                    tracing::debug!(
                        target: "bsc::block_import",
                        block_number = header.number,
                        block_hash = %block_hash,
                        peer = %peer_id,
                        error = %err,
                        "engine.new_payload returned error"
                    );
                    None
                }
            }
        })
    }

    /// Add a new block import task to the pending imports
    fn on_new_mined_block(
        &mut self,
        payload: BscBuiltPayload,
        block_msg: NewBlockMessage<BscNewBlock>,
        propagation: OwnedPermit<BlockMsg>,
    ) {
        startup_diagnostics::import_started(block_msg.hash);
        let block = &block_msg.block.0.block;
        // insert header to cache
        {
            let _stage = startup_diagnostics::HandoffStage::new("header_cache", "local");
            insert_header_to_cache_with_hash(block.header.clone(), Some(block_msg.hash));
        }
        // Cache the full block body for later range responses.
        {
            let _stage = startup_diagnostics::HandoffStage::new("body_clone_cache", "local");
            crate::shared::cache_full_block(block.clone());
        }
        let block_hash = block_msg.hash;
        // Clone header for FCU update
        let header_for_fcu = block.header.clone();

        // Register block stats so vote-delay metrics can still be computed when votes arrive for
        // this self-mined block. We deliberately do NOT call `on_block_received` here — that
        // records `chain.delay.block_recv`, which is meant to measure pure network propagation
        // delay; for a block we just produced locally the sample would actually reflect local
        // mining/finalize latency and would pollute cross-region diagnosis. Mirrors geth-bsc,
        // which only sets `RecvNewBlockTime` inside `handleBlockBroadcast` (the network path).
        {
            let _stage = startup_diagnostics::HandoffStage::new("stats_register", "local");
            crate::consensus::parlia::block_stats::register_self_mined_block(
                block_hash,
                &block.header,
            );
        }

        // This scope measures enqueue work, not delivery to the remote peer.
        {
            let _stage = startup_diagnostics::HandoffStage::new("evn_enqueue", "local");
            // send to EVN peers first
            if let Err(e) = self.transfer_to_evn_peers(block_msg.clone()) {
                tracing::warn!(target: "bsc::block_import", "Failed to transfer block to EVN peers: number = {:?}, hash = {:?}, error = {}", block.header.number, block_hash, e);
            }
        }
        // One reserved slot owns both full-block and hash propagation notifications.
        {
            let _stage = startup_diagnostics::HandoffStage::new("propagation_enqueue", "local");
            propagation.send(block_msg);
        }

        // Insert the executed block into the engine tree, then update fork choice.
        //
        // Ordering guarantee: InsertExecutedBlock MUST be processed by the engine before FCU.
        // Both messages travel through separate channels (engine_api_tx vs consensus_engine_tx)
        // that feed into the same engine service via a tokio::select! loop. Without explicit
        // ordering, the engine may process FCU before the block is indexed, causing it to be
        // rejected or ignored.
        //
        // Fix: run both in a single spawned task. After sending InsertExecutedBlock, call
        // yield_now() so the engine service task can pick up and process the insert from
        // engine_api_rx before we send FCU through the separate consensus channel.
        {
            let engine_tx_opt = crate::shared::get_engine_api_tx();
            let executed_block = payload.executed_block.clone();
            let forkchoice_engine = self.forkchoice_engine.clone();
            let submitted = std::time::Instant::now();
            tokio::spawn(async move {
                metrics::histogram!("bsc.block_handoff.stage_seconds", "phase" => "submit_task_schedule", "source" => "local").record(submitted.elapsed().as_secs_f64());
                if let Some(engine_tx) = engine_tx_opt {
                    tracing::debug!(
                        target: "bsc::block_import",
                        block_number = %header_for_fcu.number,
                        block_hash = %block_hash,
                        "Inserting mined block into engine tree"
                    );
                    if let Err(e) =
                        engine_tx.send(EngineApiRequest::InsertExecutedBlock(executed_block))
                    {
                        tracing::warn!(
                            target: "bsc::block_import",
                            block_number = %header_for_fcu.number,
                            block_hash = %block_hash,
                            error = %e,
                            "Failed to insert executed block into engine tree, block will be dropped"
                        );
                        return;
                    }
                    // Yield to the tokio runtime so the engine service loop processes
                    // InsertExecutedBlock from engine_api_rx before FCU is enqueued in
                    // the separate consensus_engine channel. This closes the race window
                    // where FCU could arrive at the engine tree before the block is indexed.
                    tokio::task::yield_now().await;
                } else {
                    tracing::warn!(
                        target: "bsc::block_import",
                        block_number = %header_for_fcu.number,
                        block_hash = %block_hash,
                        "engine_api_tx not initialized, skipping InsertExecutedBlock"
                    );
                }

                tracing::debug!(
                    target: "bsc::block_import",
                    block_number = %header_for_fcu.number,
                    block_hash = %block_hash,
                    "Updating fork choice for mined block"
                );
                if let Err(e) = forkchoice_engine.update_forkchoice(&header_for_fcu).await {
                    tracing::warn!(
                        target: "bsc::block_import",
                        block_number = %header_for_fcu.number,
                        block_hash = %block_hash,
                        error = %e,
                        "Failed to update fork choice for mined block"
                    );
                } else {
                    tracing::debug!(
                        target: "bsc::block_import",
                        block_number = %header_for_fcu.number,
                        block_hash = %block_hash,
                        "Succeeded to update fork choice for mined block"
                    );
                }
            });
        }
        // Cache the block hash to avoid re-processing the same block.
        self.processed_blocks.insert(block_hash);
        self.head_fetches.cancel(&block_hash);
    }

    /// Broadcast a selected BEP-675 BidBlock, then validate it through `new_payload`.
    /// `Valid` advances fork choice; `Invalid` revokes the builder unless validation failed
    /// locally.
    fn on_new_bid_block(
        &mut self,
        sealed: SealedBlock<BscBlock>,
        builder: Address,
        bid_hash: B256,
        gas_fee: U256,
        system_tx_start: usize,
        propagation: OwnedPermit<BlockMsg>,
    ) {
        let block_hash = sealed.hash();
        let header = sealed.header().clone();
        let block_number = header.number;

        // Total difficulty for the wire message: parent TD + this block's difficulty.
        let parent_td = self
            .forkchoice_engine
            .provider
            .header_td_by_number(block_number.saturating_sub(1))
            .ok()
            .flatten()
            .unwrap_or_default();
        let new_td = parent_td + header.difficulty;

        let new_block = BscNewBlock(NewBlock {
            block: sealed.clone_block(),
            td: U128::from(new_td.to::<u128>()),
        });
        let block_msg = NewBlockMessage { hash: block_hash, block: Arc::new(new_block) };

        // Cache + register stats like a self-mined block so range responses and vote-delay metrics
        // work for it (mirrors `on_new_mined_block`).
        insert_header_to_cache_with_hash(header.clone(), Some(block_hash));
        crate::shared::cache_full_block(block_msg.block.0.block.clone());
        crate::consensus::parlia::block_stats::register_self_mined_block(block_hash, &header);

        // 1. Broadcast first — before verification. go-bsc posts the sealed block, then runs
        //    InsertChain. Announce header + full block so peers diffuse it immediately.
        if let Err(e) = self.transfer_to_evn_peers(block_msg.clone()) {
            tracing::warn!(target: "bsc::block_import", number = block_number, hash = %block_hash, error = %e, "BidBlock: failed to transfer to EVN peers");
        }
        propagation.send(block_msg);

        // 2. Verify after broadcast.
        let engine = self.engine.clone();
        let forkchoice_engine = self.forkchoice_engine.clone();
        let mut payload = BscPayloadTypes::block_to_payload(sealed, None);
        let local_fault = Arc::new(AtomicBool::new(false));
        payload.local_fault = Some(local_fault.clone());
        tokio::spawn(async move {
            match engine.new_payload(payload).await {
                Ok(status) => match status.status {
                    PayloadStatusEnum::Valid => {
                        tracing::info!(target: "bsc::block_import", number = block_number, hash = %block_hash, %bid_hash, "[BID BLOCK VERIFIED] advancing fork choice");

                        if let Err(e) = forkchoice_engine.update_forkchoice(&header).await {
                            tracing::warn!(target: "bsc::block_import", number = block_number, hash = %block_hash, error = %e, "BidBlock: failed to update fork choice");
                        }

                        // Post-import average-gas-price floor check (go-bsc
                        // `validateBidBlockAverageGasPrice`), run now that the block is confirmed
                        // valid. This does not reject the (already-canonical) block — it only
                        // affects the builder's future SendBidBlock permission, since the
                        // deposit-derived `gas_fee` is the sole source of the fee ranking and a
                        // builder could otherwise pad it while underpaying for user-tx gas.
                        //
                        // Must run AFTER the fork-choice update (go-bsc runs it after
                        // `InsertChain`): the receipts of a just-inserted payload only become
                        // visible through the provider once the block is canonical in the
                        // in-memory tree. Retry briefly to absorb canonicalization lag.
                        let mut receipts_lookup =
                            forkchoice_engine.provider.receipts_by_block(block_hash.into());
                        for _ in 0..10 {
                            if matches!(receipts_lookup, Ok(Some(_))) {
                                break;
                            }
                            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                            receipts_lookup =
                                forkchoice_engine.provider.receipts_by_block(block_hash.into());
                        }
                        match receipts_lookup {
                            Ok(Some(receipts)) => {
                                // Mirrors the fallback `MevApiImpl::new` uses when the CLI/env
                                // hasn't published a global config yet: fall through to env vars
                                // (and ultimately `DEFAULT_MIN_GAS_TIP`) rather than treating
                                // "unset" as "no floor at all".
                                let min_gas_price = U256::from(
                                    crate::node::miner::config::get_global_mining_config()
                                        .map(|cfg| cfg.get_min_gas_tip())
                                        .unwrap_or_else(|| {
                                            crate::node::miner::config::MiningConfig::from_env()
                                                .get_min_gas_tip()
                                        }),
                                );
                                if let Err(avg_gas_price) =
                                    crate::node::miner::bid_block::validate_bid_block_average_gas_price(
                                        gas_fee,
                                        &receipts,
                                        system_tx_start,
                                        min_gas_price,
                                    )
                                {
                                    let revoke_duration_secs = crate::node::miner::bid_block_permission::BID_BLOCK_GAS_PRICE_LOW_REVOKE_DURATION_SECS;
                                    tracing::error!(target: "bsc::block_import", number = block_number, hash = %block_hash, %bid_hash, %avg_gas_price, %min_gas_price, revoke_duration_secs, "[BID BLOCK GASPRICE LOW] revoking builder");
                                    crate::shared::get_bid_block_permission_manager().revoke_for(
                                        builder,
                                        format!(
                                            "BidBlock average gas price too low: avg={avg_gas_price}, min={min_gas_price}"
                                        ),
                                        block_hash,
                                        block_number,
                                        revoke_duration_secs,
                                    );
                                }
                            }
                            Ok(None) => {
                                tracing::warn!(target: "bsc::block_import", number = block_number, hash = %block_hash, "BidBlock: receipts not found post-import; skipping gas-price check");
                            }
                            Err(e) => {
                                tracing::warn!(target: "bsc::block_import", number = block_number, hash = %block_hash, error = %e, "BidBlock: failed to fetch receipts for gas-price check");
                            }
                        }
                    }
                    // The engine also returns Invalid when local validation could not complete.
                    PayloadStatusEnum::Invalid { validation_error }
                        if local_fault.load(Ordering::Acquire) =>
                    {
                        tracing::error!(target: "bsc::block_import", number = block_number, hash = %block_hash, %bid_hash, %validation_error, "[BID BLOCK VERIFY FAILED] local fault, not revoking builder");
                    }
                    PayloadStatusEnum::Invalid { validation_error } => {
                        let violation_count = crate::shared::get_bid_block_permission_manager()
                            .revoke_for_violation(
                                builder,
                                format!("BidBlock invalid on import: {validation_error}"),
                                block_hash,
                                block_number,
                            );
                        tracing::error!(target: "bsc::block_import", number = block_number, hash = %block_hash, %bid_hash, %validation_error, violation_count, "[BID BLOCK VERIFY FAILED] revoking builder");
                    }
                    other => {
                        // Parent is canonical (we just built on top of it), so Syncing/Accepted is
                        // unexpected; log without revoking — not provable dishonesty.
                        tracing::warn!(target: "bsc::block_import", number = block_number, hash = %block_hash, ?other, "BidBlock: unexpected non-terminal payload status");
                    }
                },
                Err(err) => {
                    tracing::error!(target: "bsc::block_import", number = block_number, hash = %block_hash, %bid_hash, error = %err, "BidBlock: engine.new_payload errored");
                }
            }
        });

        self.processed_blocks.insert(block_hash);
        self.head_fetches.cancel(&block_hash);
    }

    /// Geth treats a NewBlock sender as having at least its parent. Only diffuse
    /// early when that exact parent is available through the same provider used
    /// by ETH header requests. Seeing the child bytes is not sufficient.
    fn can_propagate_before_import(&self, block: &BlockMsg) -> bool {
        let header = SealedHeader::seal_slow(block.block.0.block.header.clone());
        if header.hash() != block.hash {
            return false;
        }
        let parent = match self.forkchoice_engine.provider.header(header.parent_hash) {
            Ok(Some(parent)) => SealedHeader::seal_slow(parent),
            Ok(None) => return false,
            Err(error) => {
                tracing::debug!(target: "bsc::block_import", block_hash = %block.hash,
                    parent_hash = %header.parent_hash, %error,
                    "Deferring block propagation: parent header lookup failed");
                return false;
            }
        };
        if let Err(error) = self.propagation_consensus.validate_header(&header).and_then(|()| {
            self.propagation_consensus.validate_header_against_parent(&header, &parent)
        }) {
            tracing::debug!(target: "bsc::block_import", block_hash = %block.hash, %error,
                "Deferring block propagation until Engine validation");
            return false;
        }
        true
    }

    /// Add a new block import task to the pending imports
    fn on_new_block(&mut self, block: BlockMsg, peer_id: PeerId) {
        self.on_new_block_inner(block, peer_id, false);
    }

    /// Fetched blocks already passed body and blob validation before winning the race.
    fn on_new_block_inner(&mut self, mut block: BlockMsg, peer_id: PeerId, fetched: bool) {
        // Some internal senders bypass ImportHandle; preserve an earlier local/remote ingress.
        startup_diagnostics::ingress(
            block.hash,
            crate::consensus::parlia::util::calculate_millisecond_timestamp(
                &block.block.0.block.header,
            ),
            "network",
        );
        tracing::debug!(target: "bsc::block_import", "Receiving new block from network: number = {:?}, hash = {:?}, peer = {:?}", block.block.0.block.header.number, block.hash, peer_id);

        // Record before stale-block / dedup checks: announcer remains a
        // valid `GetBlocksByRange` target even if we skip processing here.
        crate::node::network::bsc_protocol::registry::record_announcer(block.hash, peer_id);

        // Complete bytes can satisfy an existing parent/hash download even if
        // this entrance's Engine import is already queued or processed.
        self.sync_config.downloader.supply(
            block.hash,
            &block.block.0.block,
            peer_id,
            self.forkchoice_engine.chain_spec(),
        );

        // Drop blocks that are far behind the canonical head early to avoid wasting
        // resources on stale blocks from misbehaving or out-of-sync peers. Without this
        // guard, proof workers open read transactions against cold historical trie pages,
        // blocking until db.read-transaction-timeout (5 min default) is hit.
        const MAX_STALE_BLOCK_DISTANCE: u64 = 64;
        if let Ok(info) = self.forkchoice_engine.provider.chain_info() {
            let block_number = block.block.0.block.header.number;
            if block_number + MAX_STALE_BLOCK_DISTANCE < info.best_number {
                let gap = info.best_number - block_number;
                tracing::debug!(
                    target: "bsc::block_import",
                    block_number,
                    block_hash = %block.hash,
                    canonical_head = info.best_number,
                    gap,
                    peer_id = %peer_id,
                    "Dropping stale block far behind canonical head"
                );
                // Apply a lightweight reputation penalty so peers that repeatedly send
                // stale blocks are gradually deprioritized (BadAnnouncement = -1024,
                // needs ~50 hits to reach ban threshold).
                //
                // Surface this at INFO under `bsc::peers` so #312-style peer-loss
                // investigations can attribute drift to this guard without DEBUG logs.
                if let Some(net) = crate::shared::get_network_handle() {
                    tracing::debug!(
                        target: "bsc::peers",
                        peer = %peer_id, gap, threshold = MAX_STALE_BLOCK_DISTANCE,
                        "applying BadAnnouncement: stale-block guard"
                    );
                    net.reputation_change(peer_id, ReputationChangeKind::BadAnnouncement);
                }
                return;
            }
        }

        if self.processed_blocks.contains(&block.hash) {
            tracing::trace!(target: "bsc::block_import", "Block already processed when receiving new block: number = {:?}, hash = {:?}", block.block.0.block.header.number, block.hash);
            return;
        }
        if self.queued_blocks.contains(&block.hash) {
            tracing::trace!(target: "bsc::block_import", "Block already queued when receiving new block: number = {:?}, hash = {:?}", block.block.0.block.header.number, block.hash);
            return;
        }

        let local_tip = self.forkchoice_engine.provider.best_block_number().unwrap_or(0);
        let block_number = block.block.0.block.header.number;
        let delta = block_number.saturating_sub(local_tip);
        if fetched {
            let finalized = match self.finalized_number(local_tip) {
                Ok(number) => number,
                Err(error) => {
                    tracing::warn!(target: "bsc::block_import", %error, "Failed to recheck fetched head finality");
                    return;
                }
            };
            if obsolete_announcement(
                block_number,
                local_tip,
                finalized,
                self.sync_config.args.stale_distance,
            ) {
                return;
            }
            match self.forkchoice_engine.provider.header(block.hash) {
                Ok(Some(_)) => return,
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(target: "bsc::block_import", %error, "Failed to recheck fetched head");
                    return;
                }
            }
        }
        // Reject overload before cloning or validating the body. A rejected block
        // remains retryable. Far-ahead targets have their own bounded admission.
        if delta <= self.sync_config.args.max_depth
            && self.pending_imports.len() >= MAX_PENDING_IMPORTS
        {
            Queue::RemoteImports.full(MAX_PENDING_IMPORTS, "drop");
            return;
        }

        if !fetched {
            if let Err(error) = super::super::data_availability::validate_data_availability(
                &mut Arc::make_mut(&mut block.block).0.block,
                self.forkchoice_engine.chain_spec(),
            ) {
                tracing::warn!(
                    target: "bsc::block_import",
                    peer = %peer_id,
                    block_number = block.block.0.block.header.number,
                    block_hash = %block.hash,
                    error = %format_args!("{error:#}"),
                    "Rejecting NewBlock with unavailable blob data"
                );
                return;
            }
        }

        if delta > self.sync_config.args.max_depth {
            tracing::debug!(
                target: "bsc::block_import",
                block_hash = %block.hash,
                block_number,
                local_tip,
                delta,
                peer = %peer_id,
                "NewBlock far ahead of local tip; routing to pipeline instead of fork_recover"
            );
            self.pipeline_targets.enqueue(peer_id, block.hash);
            return;
        }

        self.queued_blocks.insert(block.hash);
        self.head_fetches.cancel(&block.hash);

        // Record chain delay metrics: time from block creation to first network reception
        crate::consensus::parlia::block_stats::on_block_received(
            block.hash,
            &block.block.0.block.header,
        );

        let propagate_early = !fetched && self.can_propagate_before_import(&block);
        if propagate_early {
            // Send to EVN peers before child execution, once its parent is queryable.
            if let Err(e) = self.transfer_to_evn_peers(block.clone()) {
                tracing::warn!(target: "bsc::block_import", "Failed to transfer block to EVN peers: number = {:?}, hash = {:?}, error = {}", block.block.0.block.header.number, block.hash, e);
            }
            // Send ValidHeader announcement to trigger NewBlock diffusion from few peers
            self.send_remote_event(BlockImportEvent::Announcement(BlockValidation::ValidHeader {
                block: block.clone(),
            }));
        }

        tracing::debug!(target: "bsc::block_import", "Sending new block to import service: number = {:?}, hash = {:?}", block.block.0.block.header.number, block.hash);
        let hash = block.hash;
        let payload_fut = self.new_payload_inner(block, peer_id, fetched);
        let post_validation_full_propagation = !propagate_early;
        let provider = self.forkchoice_engine.provider.clone();
        // Keep the slot until the actual Engine response. Dropping a timed-out
        // waiter would not cancel the payload already in the Engine queue.
        self.pending_imports.push(Box::pin(async move {
            let mut outcome = payload_fut.await;
            if post_validation_full_propagation {
                if let Some(Outcome {
                    result:
                        Ok(
                            BlockValidation::ValidBlock { block } |
                            BlockValidation::ValidHeader { block },
                        ),
                    ..
                }) = &mut outcome
                {
                    // Recovery/validation can finish while the parent is still only in the
                    // Engine's non-canonical tree. Do not advertise it as ETH-queryable yet.
                    let parent_hash = block.block.0.block.header.parent_hash;
                    match provider.header(parent_hash) {
                        // Genesis has no parent to serve.
                        _ if block.block.0.block.header.number == 0 => {}
                        Ok(Some(parent)) if parent.hash_slow() == parent_hash => {}
                        result => {
                            tracing::debug!(target: "bsc::block_import", %hash, %parent_hash,
                                ?result, "Suppressing deferred block propagation: parent is not queryable");
                            return RemoteImportResult { hash, outcome: None };
                        }
                    }
                    // Share the post-validation full-block path with fetched blocks.
                    // A ValidBlock outcome alone would announce only the child's hash.
                    let block = block.clone();
                    outcome.as_mut().expect("outcome was matched").result =
                        Ok(BlockValidation::ValidHeader { block });
                }
            }
            RemoteImportResult { hash, outcome }
        }));
    }

    /// Fetch near-head announcements as single blocks; recover ancestors only
    /// when import discovers a missing parent. Announcements whose height exceeds
    /// the local tip by more than the configured recovery depth are instead routed
    /// to the staged backfill pipeline via a synthesized FCU — `fork_recover`
    /// cannot close gaps that deep.
    fn on_new_block_hashes(&mut self, hashes: NewBlockHashes, peer_id: PeerId) {
        let local_tip = match self.forkchoice_engine.provider.best_block_number() {
            Ok(tip) => tip,
            Err(err) => {
                tracing::warn!(
                    target: "bsc::block_import",
                    error = %err,
                    "Failed to read local best_block_number; skipping hash announcements"
                );
                return;
            }
        };

        let finalized = match self.finalized_number(local_tip) {
            Ok(number) => number,
            Err(error) => {
                tracing::warn!(target: "bsc::block_import", %error, "Failed to read local head for hash admission");
                return;
            }
        };

        for hash_number in hashes.0 {
            // Record before dedup checks (see `on_new_block` above).
            crate::node::network::bsc_protocol::registry::record_announcer(
                hash_number.hash,
                peer_id,
            );

            if self.processed_blocks.contains(&hash_number.hash) {
                continue;
            }
            if self.queued_blocks.contains(&hash_number.hash) {
                continue;
            }
            // Match geth's near-head admission. This also prevents an old hash
            // outside the small processed LRU from triggering cold execution.
            if obsolete_announcement(
                hash_number.number,
                local_tip,
                finalized,
                self.sync_config.args.stale_distance,
            ) {
                continue;
            }
            match self.forkchoice_engine.provider.header(hash_number.hash) {
                Ok(Some(_)) => continue,
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(target: "bsc::block_import", %error, "Failed to check announced hash");
                    continue;
                }
            }
            if self.failed_heads.is_cooling(&hash_number.hash) {
                tracing::debug!(
                    target: "bsc::block_import",
                    block_hash = %hash_number.hash,
                    block_number = hash_number.number,
                    "Skipping fork recovery: head is in cooldown after recent failure"
                );
                continue;
            }

            let delta = hash_number.number.saturating_sub(local_tip);
            if delta > self.sync_config.args.max_depth {
                // Beyond the ancestor-walk limit, ask Engine's optimistic-sync path to
                // backfill instead. BSC has no CL finalized target; the FCU uses head only.
                // Coalesce far-ahead targets before sending FCUs. Admission is
                // bounded and retryable; it does not mark a block as processed.
                self.pipeline_targets.enqueue(peer_id, hash_number.hash);
                continue;
            }

            if delta <= self.sync_config.args.near_head_distance {
                self.head_fetches.start(
                    hash_number,
                    peer_id,
                    self.forkchoice_engine.chain_spec().clone(),
                );
                continue;
            }

            // Single-flight across all heads: see the `Syncing` path in
            // `new_payload`. Skipped heads are deliberately not marked
            // processed/queued, so the next announcement retries them once the
            // in-flight recovery — which has meanwhile advanced the tip — ends.
            let permit = match self.recovery_gate.try_acquire(hash_number.number) {
                Ok(permit) => permit,
                Err(in_flight_head) => {
                    tracing::trace!(
                        target: "bsc::block_import",
                        block_hash = %hash_number.hash,
                        block_number = hash_number.number,
                        in_flight_head,
                        "Skipping fork recovery: all recovery slots in flight"
                    );
                    continue;
                }
            };

            // Concurrent-dedup: one recovery per head at a time.
            {
                let mut guard = self.recovering_heads.lock();
                if guard.contains(&hash_number.hash) {
                    continue;
                }
                guard.insert(hash_number.hash);
            }

            tracing::debug!(
                target: "bsc::block_import",
                %peer_id,
                block_hash = %hash_number.hash,
                block_number = hash_number.number,
                "Spawning fork recovery for announced head"
            );

            let peer = self.resolve_bsc_peer(peer_id);
            let provider = self.forkchoice_engine.provider.clone();
            let engine = self.engine.clone();
            let forkchoice_engine = self.forkchoice_engine.clone();
            let recovering = self.recovering_heads.clone();
            let failed_heads = self.failed_heads.clone();
            let sync_config = self.sync_config.clone();
            let head_hash = hash_number.hash;
            let head_num = hash_number.number;

            tokio::spawn(async move {
                // Slot holder and progress sink; see the `Syncing` path above.
                let permit = permit;
                let _guard =
                    crate::node::network::block_import::fork_recover::RecoveringHeadGuard::new(
                        head_hash, recovering,
                    );
                let fetcher = crate::node::network::block_import::fork_recover::BscRangeFetcher(
                    forkchoice_engine.chain_spec().clone(),
                    sync_config,
                );
                let Some(target) = peer else {
                    tracing::debug!(
                        target: "bsc::block_import",
                        %head_hash,
                        "No BSC protocol peer available for fork recovery"
                    );
                    return;
                };
                // Header-only path: no parent_hash, so fetch and FCU collapse
                // to the same hash. See `RecoverTarget::single_pair`.
                let recover_target =
                    crate::node::network::block_import::fork_recover::RecoverTarget::single_pair(
                        head_hash, head_num,
                    );
                if let Err(err) =
                    crate::node::network::block_import::fork_recover::recover_ancestors(
                        target,
                        recover_target,
                        provider,
                        engine,
                        forkchoice_engine,
                        &fetcher,
                        &permit,
                    )
                    .await
                {
                    log_recovery_error(&err, head_hash, head_num, "announced head");
                    failed_heads.mark_failed(head_hash);
                }
            });
        }
    }

    fn finalized_number(&self, local_tip: u64) -> Result<u64, reth_provider::ProviderError> {
        Ok(self
            .forkchoice_engine
            .provider
            .header_by_number(local_tip)?
            .and_then(|header| self.forkchoice_engine.get_finalized_number_and_hash(&header))
            .map_or(0, |(number, _)| number))
    }

    /// See [`resolve_bsc_peer_static`].
    fn resolve_bsc_peer(&self, announcer: PeerId) -> Option<PeerId> {
        resolve_bsc_peer_static(announcer)
    }

    /// Transfer the block to EVN peers if from proxied validators or validator address.
    fn transfer_to_evn_peers(&self, block: BlockMsg) -> Result<(), Box<dyn std::error::Error>> {
        let mining_config = crate::node::miner::config::get_global_mining_config()
            .ok_or("Mining config is not set")?;
        let cfg =
            crate::node::network::evn::get_global_evn_config().ok_or("EVN config is not set")?;
        if !cfg.enabled {
            return Ok(());
        }
        let header_ref = &block.block.0.block.header;
        let coinbase = header_ref.beneficiary;
        // If from proxied validators or validator address, target EVN peers with ETH
        // NewBlockHashes.
        if cfg.proxyed_validators.contains(&coinbase)
            || (mining_config.enabled
                && mining_config.validator_address.unwrap_or_default() == coinbase)
        {
            if let Some(net) = crate::shared::get_network_handle() {
                let peers = crate::node::network::evn_peers::snapshot();
                for (peer_id, info) in peers {
                    // Send to EVN peers or proxyed peers
                    let is_proxyed =
                        crate::node::network::bsc_protocol::registry::is_proxyed_peer(&peer_id);
                    if info.is_evn || is_proxyed {
                        // Send full NewBlock to EVN/proxyed peers to avoid re-fetching.
                        net.send_eth_message(peer_id, PeerMessage::NewBlock(block.clone()));
                        tracing::debug!(target: "bsc::block_import", "Sent full NewBlock to EVN/proxyed peer: number = {:?}, hash = {:?}, peer = {:?}", block.block.0.block.header.number, block.hash, peer_id);
                    }
                }
            }
        }
        Ok(())
    }

    /// Read local head and spawn a detached task that announces it to every
    /// connected peer that is not more than 64 blocks ahead of us.
    ///
    /// Runs on every `announce_interval` tick. This is the livelock-breaking
    /// mechanism for the case where two validators are forked and both are
    /// blocked from producing new blocks: without this, neither learns of the
    /// other's head after the initial handshake.
    fn spawn_head_announcement(&self) {
        let provider = self.forkchoice_engine.provider.clone();

        tokio::spawn(async move {
            // Resolve local head.
            let num = match provider.best_block_number() {
                Ok(n) if n > 0 => n,
                Ok(_) => {
                    tracing::debug!(target: "bsc::block_import", "Skip head announce: local best_block_number is 0");
                    return;
                }
                Err(e) => {
                    tracing::debug!(target: "bsc::block_import", error = %e, "Skip head announce: failed to read best_block_number");
                    return;
                }
            };
            let hash = match provider.block_hash(num) {
                Ok(Some(h)) => h,
                Ok(None) => {
                    tracing::debug!(target: "bsc::block_import", num, "Skip head announce: no hash for best_block_number");
                    return;
                }
                Err(e) => {
                    tracing::debug!(target: "bsc::block_import", num, error = %e, "Skip head announce: block_hash lookup failed");
                    return;
                }
            };

            // Resolve network handle.
            let net = match crate::shared::get_network_handle() {
                Some(n) => n,
                None => {
                    tracing::debug!(target: "bsc::block_import", "Skip head announce: network handle not yet initialized");
                    return;
                }
            };

            // Query peers.
            let peers = match net.get_all_peers().await {
                Ok(p) => p,
                Err(e) => {
                    tracing::debug!(target: "bsc::block_import", error = %e, "Skip head announce: get_all_peers failed");
                    return;
                }
            };
            if peers.is_empty() {
                return;
            }

            let peer_tuples: Vec<(PeerId, Option<u64>)> =
                peers.iter().map(|p| (p.remote_id, p.best_number)).collect();
            let targets = plan_head_announcements(num, &peer_tuples);

            if targets.is_empty() {
                return;
            }

            let hashes = NewBlockHashes(vec![BlockHashNumber { hash, number: num }]);
            let target_count = targets.len();
            for peer_id in targets {
                tracing::debug!(
                    target: "bsc::block_import",
                    %peer_id,
                    local_num = num,
                    %hash,
                    "Sending head announce to peer"
                );
                net.send_eth_message(peer_id, PeerMessage::NewBlockHashes(hashes.clone()));
            }
            tracing::debug!(
                target: "bsc::block_import",
                local_num = num,
                sent_to = target_count,
                total_peers = peers.len(),
                "Announced head to peers"
            );
        });
    }
}

/// Decide which peers to send `NewBlockHashes(local_head)` to.
///
/// A peer is skipped when its known `best_number` is more than
/// `MAX_STALE_BLOCK_DISTANCE` (64) blocks ahead of the local head: announcing a
/// stale hash to such a peer would be dropped and trigger a `BadAnnouncement`
/// reputation penalty on us.
///
/// A peer with `best_number = None` (head not yet observed) is announced to:
/// there's no evidence it is ahead, and the worst case is the peer ignores the
/// hint.
fn plan_head_announcements(local_head: u64, peers: &[(PeerId, Option<u64>)]) -> Vec<PeerId> {
    const MAX_STALE_BLOCK_DISTANCE: u64 = 64;
    peers
        .iter()
        .filter_map(|(peer_id, peer_best)| match peer_best {
            Some(peer_best) if local_head + MAX_STALE_BLOCK_DISTANCE < *peer_best => None,
            _ => Some(*peer_id),
        })
        .collect()
}

impl<Provider> Future for ImportService<Provider>
where
    Provider: BlockNumReader
        + BlockHashReader
        + HeaderProvider<Header = Header>
        + ReceiptProvider
        + Clone
        + Send
        + Sync
        + 'static
        + Unpin,
{
    type Output = Result<(), Box<dyn std::error::Error>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        // Receive new blocks from network
        for n in 0..POLL_BATCH {
            if n + 1 == POLL_BATCH {
                cx.waker().wake_by_ref();
            }
            let Poll::Ready(Some((block, peer_id))) = this.from_network.poll_recv(cx) else {
                break;
            };
            this.on_new_block(block, peer_id);
        }

        // Complete local fallback imports before admitting more local work. Local slots are
        // independent of the remote import cap. Each future owns a propagation reservation.
        for _ in 0..POLL_BATCH {
            match this.pending_local_imports.poll_next_unpin(cx) {
                Poll::Ready(Some(Some(outcome))) => {
                    if let Ok(BlockValidation::ValidBlock { block }) = outcome.result {
                        this.processed_blocks.insert(block.hash);
                        if let Err(error) = this.transfer_to_evn_peers(block) {
                            tracing::warn!(target: "bsc::block_import", %error, "Failed to transfer local fallback block to EVN peers");
                        }
                    }
                }
                Poll::Ready(Some(None)) => {}
                _ => break,
            }
        }
        for n in 0..POLL_BATCH {
            // Do not dequeue a local block until its propagation slot is reserved. A single
            // retained reservation future registers the wakeup; no detached send tasks.
            let permit = match this.to_network.poll_local_permit(cx) {
                Poll::Ready(Ok(permit)) => permit,
                Poll::Ready(Err(error)) => {
                    tracing::error!(target: "bsc::block_import", %error, "Local propagation channel closed");
                    return Poll::Ready(Err(Box::new(error)));
                }
                Poll::Pending => break,
            };
            let Poll::Ready(Some(local)) = this.from_local.poll_recv(cx) else {
                // Dropping the unused permit returns capacity to the local queue.
                break;
            };
            match local {
                IncomingLocalBlock::Mined(block) => {
                    let (payload, message) = *block;
                    this.on_new_mined_block(payload, message, permit);
                }
                IncomingLocalBlock::Bid(block) => {
                    let (sealed, builder, hash, fee, system_tx_start) = *block;
                    this.on_new_bid_block(sealed, builder, hash, fee, system_tx_start, permit);
                }
                IncomingLocalBlock::Unexecuted(block) => {
                    this.head_fetches.cancel(&block.hash);
                    let future =
                        this.new_payload(block, crate::shared::get_local_peer_id_or_default());
                    this.pending_local_imports.push(Box::pin(async move {
                        let outcome = future.await;
                        // The fallback requests Engine validation. Only propagate the pair once
                        // validation succeeds; the reserved slot cannot be stolen meanwhile.
                        if let Some(Outcome {
                            result: Ok(BlockValidation::ValidBlock { block }),
                            ..
                        }) = &outcome
                        {
                            insert_header_to_cache_with_hash(
                                block.block.0.block.header.clone(),
                                Some(block.hash),
                            );
                            crate::shared::cache_full_block(block.block.0.block.clone());
                            permit.send(block.clone());
                        }
                        outcome
                    }));
                    // Newly inserted futures must be polled even without another input event.
                    cx.waker().wake_by_ref();
                }
            }
            if n + 1 == POLL_BATCH {
                cx.waker().wake_by_ref();
            }
        }

        // Receive new block hashes from network
        for n in 0..POLL_BATCH {
            if n + 1 == POLL_BATCH {
                cx.waker().wake_by_ref();
            }
            let Poll::Ready(Some((hashes, peer_id))) = this.from_hashes.poll_recv(cx) else {
                break;
            };
            this.on_new_block_hashes(hashes, peer_id);
        }

        // Process completed imports and send events to network
        for n in 0..POLL_BATCH {
            if n + 1 == POLL_BATCH {
                cx.waker().wake_by_ref();
            }
            let Poll::Ready(Some((hash, result))) = this.head_fetches.poll(cx) else { break };
            match result {
                Ok((block, peer)) => this.on_new_block_inner(
                    NewBlockMessage {
                        hash,
                        block: Arc::new(BscNewBlock(NewBlock { block, td: U128::ZERO })),
                    },
                    peer,
                    true,
                ),
                Err(error) => {
                    tracing::debug!(target: "bsc::block_import", %hash, %error, "Announced head fetch failed")
                }
            }
        }

        for n in 0..POLL_BATCH {
            if n + 1 == POLL_BATCH {
                cx.waker().wake_by_ref();
            }
            let Poll::Ready(Some(RemoteImportResult { hash, outcome })) =
                this.pending_imports.poll_next_unpin(cx)
            else {
                break;
            };
            // Every completion releases only its own queue entry. Failed full
            // NewBlock imports must remain retryable after recovery cooldown.
            this.queued_blocks.remove(&hash);
            if let Some(mut outcome) = outcome {
                if let Ok(BlockValidation::ValidHeader { block }) = &outcome.result {
                    // Fetched and deferred blocks reach full diffusion after Engine validation.
                    this.send_remote_event(BlockImportEvent::Announcement(
                        BlockValidation::ValidHeader { block: block.clone() },
                    ));
                    outcome.result = Ok(BlockValidation::ValidBlock { block: block.clone() });
                }
                if let Ok(BlockValidation::ValidBlock { block }) = &outcome.result {
                    this.processed_blocks.insert(block.hash);
                    // Cache the full block body for later range responses.
                    crate::shared::cache_full_block(block.block.0.block.clone());
                    if let Err(e) = this.transfer_to_evn_peers(block.clone()) {
                        tracing::warn!(target: "bsc::block_import", "Failed to transfer block to EVN peers: number = {:?}, hash = {:?}, error = {}", block.block.0.block.header.number, block.hash, e);
                    }
                }

                // The network may be busy. Do not move bounded imports into an
                // unbounded outcome queue or stop driving the service on overload.
                this.send_remote_event(BlockImportEvent::Outcome(outcome));
            }
        }

        this.pipeline_targets.poll(cx);
        Queue::RemoteBlocks.observe(
            this.from_network.max_capacity() - this.from_network.capacity(),
            IMPORT_QUEUE_CAPACITY,
        );
        Queue::RemoteHashes.observe(
            this.from_hashes.max_capacity() - this.from_hashes.capacity(),
            IMPORT_QUEUE_CAPACITY,
        );
        Queue::LocalIngress.observe(
            this.from_local.max_capacity() - this.from_local.capacity(),
            propagation::LOCAL_BLOCK_CAPACITY,
        );
        Queue::RemoteImports.observe(this.pending_imports.len(), MAX_PENDING_IMPORTS);
        this.to_network.observe();

        // Drive periodic head announcement to break forked-validator livelocks.
        // Each tick spawns a detached task so we never block the poll loop on
        // the async `get_all_peers()` query.
        while this.announce_interval.poll_tick(cx).is_ready() {
            this.spawn_head_announcement();
        }

        Poll::Pending
    }
}

fn obsolete_announcement(number: u64, local_tip: u64, finalized: u64, stale_distance: u64) -> bool {
    number <= finalized || local_tip.saturating_sub(number) > stale_distance
}

/// Wire TD is derived from the exact parent, including competing branches.
fn fetched_block_td<P>(
    forkchoice: &BscForkChoiceEngine<P>,
    header: &Header,
) -> eyre::Result<Option<U128>>
where
    P: BlockNumReader + HeaderProvider<Header = Header> + Clone + Send + Sync,
{
    let Some(parent_td) = forkchoice.header_td_by_hash(header.parent_hash)? else {
        return Ok(None);
    };
    let total = parent_td
        .checked_add(header.difficulty)
        .ok_or_else(|| eyre::eyre!("fetched block total difficulty overflow"))?;
    eyre::ensure!(
        total <= U256::from(u128::MAX),
        "fetched block total difficulty exceeds wire u128"
    );
    Ok(Some(U128::from(total.to::<u128>())))
}

#[cfg(test)]
mod tests {
    use crate::chainspec::bsc::bsc_mainnet;

    use super::*;
    use alloy_primitives::{BlockHash, BlockNumber, B256, U128, U256};
    use alloy_rpc_types::engine::PayloadStatus;
    use reth_chainspec::ChainInfo;
    use reth_engine_primitives::{BeaconEngineMessage, OnForkChoiceUpdated};
    use reth_eth_wire::NewBlock;
    use reth_ethereum_primitives::{Block, Receipt};
    use reth_node_ethereum::EthEngineTypes;
    use reth_primitives_traits::SealedHeader;
    use reth_provider::ProviderError;
    use schnellru::{ByLength, LruMap};
    use std::{
        collections::HashMap,
        sync::Arc,
        task::{Context, Poll},
    };

    #[tokio::test]
    async fn can_handle_valid_block() {
        let mut fixture = TestFixture::new(EngineResponses::both_valid()).await;
        fixture
            .assert_block_import(|outcome| {
                matches!(
                    outcome,
                    BlockImportEvent::Outcome(BlockImportOutcome {
                        peer: _,
                        result: Ok(BlockValidation::ValidBlock { .. })
                    })
                )
            })
            .await;
    }

    fn propagation_test_parent() -> Header {
        Header { number: 10, timestamp: 1, gas_limit: 200_000_000, ..Default::default() }
    }

    fn propagation_test_child(parent: &Header) -> BlockMsg {
        let block = BscBlock {
            header: Header {
                number: parent.number + 1,
                timestamp: parent.timestamp + 1,
                parent_hash: parent.hash_slow(),
                gas_limit: parent.gas_limit,
                difficulty: U256::from(2),
                extra_data: vec![0; 97].into(),
                ..Default::default()
            },
            ..Default::default()
        };
        NewBlockMessage {
            hash: block.header.hash_slow(),
            block: Arc::new(BscNewBlock(NewBlock { block, td: U128::from(102) })),
        }
    }

    #[tokio::test]
    async fn full_block_with_queryable_parent_propagates_before_engine_execution() {
        let parent = propagation_test_parent();
        let mut provider = MockProvider::new();
        provider.insert(parent.clone(), U256::from(100));
        let (mut service, mut engine, mut events) = fetched_fixture(provider);
        let message = propagation_test_child(&parent);
        assert!(service.can_propagate_before_import(&message));
        service.on_new_block(message.clone(), PeerId::random());
        let BlockImportEvent::Announcement(BlockValidation::ValidHeader { block }) =
            events.try_recv().expect("full block must not wait for child execution")
        else {
            panic!("expected full-block propagation")
        };
        assert_eq!(block.hash, message.hash);
        assert!(engine.try_recv().is_err(), "Engine future has not even been polled");
        assert_eq!(service.pending_imports.len(), 1);
    }

    #[tokio::test]
    async fn full_block_does_not_propagate_with_missing_or_invalid_parent_header() {
        let parent = propagation_test_parent();
        for has_parent in [false, true] {
            let mut provider = MockProvider::new();
            if has_parent {
                provider.insert(parent.clone(), U256::from(100));
            }
            let (mut service, mut engine, mut events) = fetched_fixture(provider);
            let mut message = propagation_test_child(&parent);
            if has_parent {
                // Parent exists, but this child does not follow its height.
                Arc::make_mut(&mut message.block).0.block.header.number += 1;
                message.hash = message.block.0.block.header.hash_slow();
            }
            service.on_new_block(message, PeerId::random());
            assert!(events.try_recv().is_err(), "unsafe NewBlock must not escape");
            assert_eq!(service.pending_imports.len(), 1, "import/recovery must still run");
            let mut cx = Context::from_waker(futures::task::noop_waker_ref());
            assert!(Pin::new(&mut service).poll(&mut cx).is_pending());
            let BeaconEngineMessage::NewPayload { tx, .. } = engine.try_recv().unwrap() else {
                panic!("Engine import must still be admitted")
            };
            tx.send(Ok(PayloadStatus::new(
                PayloadStatusEnum::Invalid { validation_error: "invalid child".into() },
                None,
            )))
            .unwrap();
            assert!(Pin::new(&mut service).poll(&mut cx).is_pending());
            assert!(events.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn deferred_full_block_propagates_after_parent_becomes_queryable_and_import_is_valid() {
        let parent = propagation_test_parent();
        let mut provider = MockProvider::new();
        provider.insert(Header { number: 9, ..Default::default() }, U256::from(98));
        provider.td_by_hash.insert(parent.hash_slow(), U256::from(100));
        let shared_headers = provider.concurrent_headers.clone();
        let (mut service, mut engine, mut events) = fetched_fixture(provider);
        let message = propagation_test_child(&parent);
        service.on_new_block(message.clone(), PeerId::random());
        assert!(events.try_recv().is_err());
        let mut cx = Context::from_waker(futures::task::noop_waker_ref());
        assert!(Pin::new(&mut service).poll(&mut cx).is_pending());
        let BeaconEngineMessage::NewPayload { tx, .. } = engine.try_recv().unwrap() else {
            panic!("new payload expected")
        };
        // Simulate the parent being imported by the concurrent shared recovery path.
        shared_headers.write().insert(parent.hash_slow(), parent);
        assert!(events.try_recv().is_err(), "parent alone does not validate the child");
        tx.send(Ok(PayloadStatus::new(PayloadStatusEnum::Valid, None))).unwrap();
        assert!(Pin::new(&mut service).poll(&mut cx).is_pending());
        assert!(events.try_recv().is_err(), "forkchoice must complete before deferred diffusion");
        let BeaconEngineMessage::ForkchoiceUpdated { tx, .. } = engine.try_recv().unwrap() else {
            panic!("forkchoice expected")
        };
        tx.send(Ok(OnForkChoiceUpdated::valid(PayloadStatus::new(PayloadStatusEnum::Valid, None))))
            .unwrap();
        assert!(Pin::new(&mut service).poll(&mut cx).is_pending());
        let BlockImportEvent::Announcement(BlockValidation::ValidHeader { block }) =
            events.try_recv().expect("deferred full block must eventually be forwarded")
        else {
            panic!("expected deferred full-block diffusion")
        };
        assert_eq!(block.hash, message.hash);
        assert!(matches!(events.try_recv().unwrap(), BlockImportEvent::Outcome(_)));
        assert!(events.try_recv().is_err());
    }

    #[tokio::test]
    async fn deferred_full_block_stays_retryable_if_engine_valid_parent_is_not_queryable() {
        let parent = propagation_test_parent();
        let mut provider = MockProvider::new();
        provider.insert(Header { number: 9, ..Default::default() }, U256::from(98));
        provider.td_by_hash.insert(parent.hash_slow(), U256::from(100));
        let shared_headers = provider.concurrent_headers.clone();
        let (mut service, mut engine, mut events) = fetched_fixture(provider);
        let message = propagation_test_child(&parent);
        let peer = PeerId::random();
        service.on_new_block(message.clone(), peer);
        let mut cx = Context::from_waker(futures::task::noop_waker_ref());
        assert!(Pin::new(&mut service).poll(&mut cx).is_pending());
        let BeaconEngineMessage::NewPayload { tx, .. } = engine.try_recv().unwrap() else {
            panic!("new payload expected")
        };
        tx.send(Ok(PayloadStatus::new(PayloadStatusEnum::Valid, None))).unwrap();
        assert!(Pin::new(&mut service).poll(&mut cx).is_pending());
        let BeaconEngineMessage::ForkchoiceUpdated { tx, .. } = engine.try_recv().unwrap() else {
            panic!("forkchoice expected")
        };
        tx.send(Ok(OnForkChoiceUpdated::valid(PayloadStatus::new(PayloadStatusEnum::Valid, None))))
            .unwrap();
        assert!(Pin::new(&mut service).poll(&mut cx).is_pending());
        assert!(events.try_recv().is_err(), "Engine validation alone must not authorize diffusion");
        assert!(!service.processed_blocks.contains(&message.hash));
        assert!(!service.queued_blocks.contains(&message.hash));
        assert!(service.pending_imports.is_empty());

        shared_headers.write().insert(parent.hash_slow(), parent);
        service.on_new_block(message.clone(), peer);
        let BlockImportEvent::Announcement(BlockValidation::ValidHeader { block }) =
            events.try_recv().expect("a safe retry must not be lost to deduplication")
        else {
            panic!("full-block propagation expected")
        };
        assert_eq!(block.hash, message.hash);
        assert_eq!(service.pending_imports.len(), 1);
    }

    #[tokio::test]
    async fn missing_blob_does_not_queue_or_forward_and_can_be_retried() {
        use crate::node::network::data_availability::tests::blob_block;
        let spec = Arc::new(BscChainSpec::from(
            reth_chainspec::ChainSpecBuilder::mainnet().cancun_activated().build(),
        ));
        let (to_engine, mut from_engine) = mpsc::unbounded_channel();
        let (_, from_network) = mpsc::channel(IMPORT_QUEUE_CAPACITY);
        let (_, from_local) = mpsc::channel(propagation::LOCAL_BLOCK_CAPACITY);
        let (_, from_hashes) = mpsc::channel(IMPORT_QUEUE_CAPACITY);
        let (to_network, mut events) = propagation::channel();
        let mut service = ImportService::new(
            MockProvider::new(),
            spec,
            ConsensusEngineHandle::new(to_engine),
            from_network,
            from_local,
            from_hashes,
            to_network,
        );
        let now =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        let block = blob_block(now);
        let hash = block.header.hash_slow();
        let good = NewBlockMessage {
            hash,
            block: Arc::new(BscNewBlock(NewBlock { block, td: U128::ZERO })),
        };
        let mut missing = good.clone();
        Arc::make_mut(&mut missing.block).0.block.body.sidecars = None;
        service.on_new_block(missing, PeerId::random());
        assert!(!service.queued_blocks.contains(&hash));
        assert!(!service.processed_blocks.contains(&hash));
        assert!(service.pending_imports.is_empty());
        assert!(events.try_recv().is_err());
        assert!(from_engine.try_recv().is_err());

        service.on_new_block(good, PeerId::random());
        assert!(service.queued_blocks.contains(&hash));
        assert_eq!(service.pending_imports.len(), 1);
        // Body availability permits import, but its unknown parent still forbids early diffusion.
        assert!(events.try_recv().is_err());
    }

    #[tokio::test]
    async fn local_backpressure_reaches_producer_without_blocking_remote_imports() {
        let (engine_tx, mut engine_rx) = mpsc::unbounded_channel();
        let (network_tx, network_rx) = mpsc::channel(IMPORT_QUEUE_CAPACITY);
        let (local_tx, local_rx) = mpsc::channel(propagation::LOCAL_BLOCK_CAPACITY);
        let (_, hashes_rx) = mpsc::channel(IMPORT_QUEUE_CAPACITY);
        let (mut events_tx, mut events_rx) = propagation::channel();
        let block = create_test_block();
        for _ in 0..propagation::LOCAL_BLOCK_CAPACITY {
            futures::future::poll_fn(|cx| events_tx.poll_local_permit(cx))
                .await
                .unwrap()
                .send(block.clone());
            local_tx.try_send(IncomingLocalBlock::Unexecuted(block.clone())).unwrap();
        }
        let mut service = ImportService::new(
            MockProvider::new(),
            Arc::new(BscChainSpec::from(bsc_mainnet())),
            ConsensusEngineHandle::new(engine_tx),
            network_rx,
            local_rx,
            hashes_rx,
            events_tx,
        );
        let send = local_tx.send(IncomingLocalBlock::Unexecuted(block.clone()));
        tokio::pin!(send);
        let mut cx = Context::from_waker(futures::task::noop_waker_ref());
        assert!(send.as_mut().poll(&mut cx).is_pending());
        network_tx.try_send((block, PeerId::random())).unwrap();
        assert!(Pin::new(&mut service).poll(&mut cx).is_pending());
        assert_eq!(service.from_local.len(), propagation::LOCAL_BLOCK_CAPACITY);
        assert!(service.pending_local_imports.is_empty());
        assert!(matches!(engine_rx.try_recv(), Ok(BeaconEngineMessage::NewPayload { .. })));
        assert!(send.as_mut().poll(&mut cx).is_pending());

        // Taking one grouped local block releases one slot and resumes local admission.
        assert!(matches!(
            events_rx.try_recv(),
            Ok(BlockImportEvent::Announcement(BlockValidation::ValidHeader { .. }))
        ));
        assert!(Pin::new(&mut service).poll(&mut cx).is_pending());
        assert_eq!(service.pending_local_imports.len(), 1);
        assert!(matches!(send.as_mut().poll(&mut cx), Poll::Ready(Ok(()))));
    }

    #[tokio::test]
    async fn local_fallback_waits_for_validation_and_uses_local_propagation() {
        let (engine_tx, mut engine_rx) = mpsc::unbounded_channel();
        let (_, network_rx) = mpsc::channel(IMPORT_QUEUE_CAPACITY);
        let (local_tx, local_rx) = mpsc::channel(propagation::LOCAL_BLOCK_CAPACITY);
        let (_, hashes_rx) = mpsc::channel(IMPORT_QUEUE_CAPACITY);
        let (events_tx, mut events_rx) = propagation::channel();
        let mut service = ImportService::new(
            MockProvider::new(),
            Arc::new(BscChainSpec::from(bsc_mainnet())),
            ConsensusEngineHandle::new(engine_tx),
            network_rx,
            local_rx,
            hashes_rx,
            events_tx,
        );
        // Remote saturation must not reject a local fallback block.
        for _ in 0..MAX_PENDING_IMPORTS {
            service.pending_imports.push(Box::pin(futures::future::pending()));
        }
        local_tx.send(IncomingLocalBlock::Unexecuted(create_test_block())).await.unwrap();
        let mut cx = Context::from_waker(futures::task::noop_waker_ref());
        assert!(Pin::new(&mut service).poll(&mut cx).is_pending());
        assert!(Pin::new(&mut service).poll(&mut cx).is_pending());
        assert!(events_rx.try_recv().is_err());
        let BeaconEngineMessage::NewPayload { tx, .. } = engine_rx.try_recv().unwrap() else {
            panic!("local fallback did not reach Engine");
        };
        tx.send(Ok(PayloadStatus::new(
            PayloadStatusEnum::Invalid { validation_error: "test".into() },
            None,
        )))
        .unwrap();
        assert!(Pin::new(&mut service).poll(&mut cx).is_pending());
        assert!(events_rx.try_recv().is_err());
        assert!(service.pending_local_imports.is_empty());
        // Invalid work releases its reserved slot; a subsequent valid submission can proceed.
        local_tx.send(IncomingLocalBlock::Unexecuted(create_test_block())).await.unwrap();
        assert!(Pin::new(&mut service).poll(&mut cx).is_pending());
        assert!(Pin::new(&mut service).poll(&mut cx).is_pending());
        let BeaconEngineMessage::NewPayload { tx, .. } = engine_rx.try_recv().unwrap() else {
            panic!("local capacity was not released");
        };
        tx.send(Ok(PayloadStatus::new(PayloadStatusEnum::Valid, None))).unwrap();
        // Fork-choice may require a reply, depending on the fixture's canonical head.
        assert!(Pin::new(&mut service).poll(&mut cx).is_pending());
        if let Ok(BeaconEngineMessage::ForkchoiceUpdated { tx, .. }) = engine_rx.try_recv() {
            tx.send(Ok(OnForkChoiceUpdated::valid(PayloadStatus::new(
                PayloadStatusEnum::Valid,
                None,
            ))))
            .unwrap();
            assert!(Pin::new(&mut service).poll(&mut cx).is_pending());
        }
        assert!(matches!(
            events_rx.try_recv(),
            Ok(BlockImportEvent::Announcement(BlockValidation::ValidHeader { .. }))
        ));
        assert!(matches!(
            events_rx.try_recv(),
            Ok(BlockImportEvent::Announcement(BlockValidation::ValidBlock { .. }))
        ));
        assert!(service.pending_local_imports.is_empty());
    }

    #[tokio::test]
    async fn closed_propagation_stops_import_service_with_error() {
        let (engine_tx, _engine_rx) = mpsc::unbounded_channel();
        let (_, network_rx) = mpsc::channel(IMPORT_QUEUE_CAPACITY);
        let (_, local_rx) = mpsc::channel(propagation::LOCAL_BLOCK_CAPACITY);
        let (_, hashes_rx) = mpsc::channel(IMPORT_QUEUE_CAPACITY);
        let (events_tx, events_rx) = propagation::channel();
        drop(events_rx);
        let mut service = ImportService::new(
            MockProvider::new(),
            Arc::new(BscChainSpec::from(bsc_mainnet())),
            ConsensusEngineHandle::new(engine_tx),
            network_rx,
            local_rx,
            hashes_rx,
            events_tx,
        );
        let mut cx = Context::from_waker(futures::task::noop_waker_ref());
        assert!(matches!(Pin::new(&mut service).poll(&mut cx), Poll::Ready(Err(_))));
    }

    #[tokio::test]
    async fn slow_engine_bounds_imports_and_completion_allows_retry() {
        let (tx, mut engine_rx) = mpsc::unbounded_channel();
        let (_, network_rx) = mpsc::channel(IMPORT_QUEUE_CAPACITY);
        let (_, local_rx) = mpsc::channel(propagation::LOCAL_BLOCK_CAPACITY);
        let (_, hashes_rx) = mpsc::channel(IMPORT_QUEUE_CAPACITY);
        let (events_tx, mut events_rx) = propagation::channel();
        let mut service = ImportService::new(
            MockProvider::new(),
            Arc::new(BscChainSpec::from(bsc_mainnet())),
            ConsensusEngineHandle::new(tx),
            network_rx,
            local_rx,
            hashes_rx,
            events_tx,
        );
        let peer = PeerId::random();
        let make_block = |n: u64| {
            let block = BscBlock {
                header: Header {
                    number: 1,
                    extra_data: n.to_be_bytes().to_vec().into(),
                    ..Default::default()
                },
                body: BscBlockBody::default(),
            };
            NewBlockMessage {
                hash: block.header.hash_slow(),
                block: Arc::new(BscNewBlock(NewBlock { block, td: U128::ZERO })),
            }
        };
        for n in 0..1024 {
            service.on_new_block(make_block(n), peer);
        }
        assert_eq!(service.pending_imports.len(), MAX_PENDING_IMPORTS);
        let rejected = make_block(1023);
        assert!(!service.queued_blocks.contains(&rejected.hash));
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        assert!(Pin::new(&mut service).poll(&mut cx).is_pending());
        let mut replies = Vec::new();
        while let Ok(BeaconEngineMessage::NewPayload { tx, .. }) = engine_rx.try_recv() {
            replies.push(tx);
        }
        assert_eq!(replies.len(), MAX_PENDING_IMPORTS);
        for _ in 0..100 {
            service.on_new_block(make_block(2000), peer);
        }
        assert_eq!(service.pending_imports.len(), MAX_PENDING_IMPORTS);
        for tx in replies {
            let _ = tx.send(Ok(PayloadStatus::new(
                PayloadStatusEnum::Invalid { validation_error: "test".into() },
                None,
            )));
        }
        while events_rx.try_recv().is_ok() {}
        assert!(Pin::new(&mut service).poll(&mut cx).is_pending());
        assert!(service.pending_imports.is_empty());
        service.on_new_block(rejected.clone(), peer);
        assert_eq!(service.pending_imports.len(), 1);
        assert!(service.queued_blocks.contains(&rejected.hash));
    }

    #[tokio::test]
    async fn can_handle_invalid_new_payload() {
        // When new_payload returns Invalid, the peer should NOT be penalized.
        // A header-valid block with a queryable parent may be announced early;
        // an Engine execution rejection must still not penalize its sender.
        let parent = propagation_test_parent();
        let mut provider = MockProvider::new();
        provider.insert(parent.clone(), U256::from(100));
        let mut fixture =
            TestFixture::new_with_provider(EngineResponses::invalid_new_payload(), provider).await;
        fixture
            .assert_block_import_with_block(propagation_test_child(&parent), |outcome| {
                matches!(
                    outcome,
                    BlockImportEvent::Announcement(BlockValidation::ValidHeader { .. })
                )
            })
            .await;

        // Verify no error outcome was emitted
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        let mut extra = Vec::new();
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_millis(200);
        loop {
            match fixture.handle.poll_outcome(&mut cx) {
                Poll::Ready(Some(event)) => extra.push(event),
                Poll::Ready(None) => break,
                Poll::Pending => {
                    if tokio::time::Instant::now() >= deadline {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            }
        }
        assert!(
            !extra.iter().any(|e| matches!(
                e,
                BlockImportEvent::Outcome(BlockImportOutcome { result: Err(_), .. })
            )),
            "Should not penalize peer for Invalid new_payload. Extra events: {extra:?}"
        );
    }

    #[tokio::test]
    async fn rejects_new_payload_with_mismatched_announced_hash() {
        let mut fixture = TestFixture::new(EngineResponses::both_valid()).await;
        let mut block_msg = create_test_block();
        block_msg.hash = B256::random();

        fixture
            .assert_block_import_with_block(block_msg, |outcome| {
                matches!(
                    outcome,
                    BlockImportEvent::Outcome(BlockImportOutcome {
                        peer: _,
                        result: Err(BlockImportError::Other(_))
                    })
                )
            })
            .await;
    }

    #[tokio::test]
    async fn deduplicates_blocks() {
        let mut fixture = TestFixture::new(EngineResponses::both_valid()).await;

        // Send the same block twice from different peers
        let block_msg = create_test_block();
        let peer1 = PeerId::random();
        let peer2 = PeerId::random();

        // First block should be processed
        fixture.handle.send_block(block_msg.clone(), peer1).unwrap();

        // Wait for the first block to be processed
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);

        // Wait for the first block to be processed
        loop {
            match fixture.handle.poll_outcome(&mut cx) {
                Poll::Ready(Some(event)) => {
                    if matches!(event, BlockImportEvent::Outcome(_)) {
                        break;
                    }
                }
                Poll::Ready(None) => break,
                Poll::Pending => tokio::task::yield_now().await,
            }
        }

        // Second block with same hash should be deduplicated
        fixture.handle.send_block(block_msg, peer2).unwrap();

        // Wait a bit and check that no additional outcomes are generated
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Should not have any additional outcomes
        match fixture.handle.poll_outcome(&mut cx) {
            Poll::Ready(Some(_)) => {
                panic!("Duplicate block should not generate additional outcomes")
            }
            Poll::Ready(None) | Poll::Pending => {
                // This is expected - no additional outcomes
            }
        }
    }

    /// Build a minimal sealed BidBlock (no txs) at the given height for the BidBlock import path.
    fn create_bid_sealed_block(number: u64) -> SealedBlock<BscBlock> {
        use reth_primitives_traits::Block as _;
        let block = BscBlock {
            header: Header { number, ..Default::default() },
            body: BscBlockBody {
                inner: BlockBody {
                    transactions: Vec::new(),
                    ommers: Vec::new(),
                    withdrawals: None,
                },
                sidecars: None,
            },
        };
        let hash = block.header.hash_slow();
        block.seal_unchecked(hash)
    }

    #[tokio::test(start_paused = true)]
    async fn bid_block_verdict_controls_revoke_and_forkchoice() {
        for (valid, local_fault, builder, block_number) in [
            (false, false, 0x7e, 900_102),
            (false, true, 0x7d, 900_103),
            (true, false, 0x7b, 900_104),
        ] {
            let sealed = create_bid_sealed_block(block_number);
            let sealed_hash = sealed.hash();
            let bid_hash = B256::repeat_byte(0xb1);
            let parent_hash = sealed.header().parent_hash;
            let mut provider = MockProvider::new();
            provider.insert(sealed.header().clone(), U256::from(1));
            provider.insert_receipts(sealed_hash, Vec::new());
            let (observed_tx, mut new_payload_observed) = mpsc::unbounded_channel();
            let mut responses = if valid {
                EngineResponses::both_valid()
            } else {
                EngineResponses::invalid_new_payload()
            };
            responses.new_payload_local_fault = local_fault;
            responses.new_payload_observed = Some(observed_tx);
            let mut fixture = TestFixture::new_with_provider(responses, provider).await;
            let builder = Address::repeat_byte(builder);
            let pm = crate::shared::get_bid_block_permission_manager();
            assert!(pm.is_allowed(builder), "builder should start allowed");

            // The miner claims the slot before handing the signed block to this service.
            assert!(crate::shared::check_and_record_mined_block(block_number, parent_hash));
            fixture
                .bid_tx
                .send(IncomingLocalBlock::Bid(Box::new((sealed, builder, bid_hash, U256::ZERO, 0))))
                .await
                .unwrap();

            tokio::time::timeout(tokio::time::Duration::from_secs(2), async {
                loop {
                    let event = futures::future::poll_fn(|cx| fixture.handle.poll_outcome(cx))
                        .await
                        .expect("import service closed before broadcast");
                    if matches!(
                        event,
                        BlockImportEvent::Announcement(BlockValidation::ValidBlock { .. })
                    ) {
                        break;
                    }
                }
            })
            .await
            .expect("BidBlock was not broadcast");
            tokio::time::timeout(tokio::time::Duration::from_secs(2), new_payload_observed.recv())
                .await
                .expect("engine.new_payload was never called")
                .expect("engine mock closed without observing new_payload");
            // The paused clock advances after ready verification work has run.
            tokio::time::sleep(tokio::time::Duration::from_millis(1)).await;

            assert_eq!(pm.is_allowed(builder), valid || local_fault);
            assert_eq!(fixture.forkchoice_updates.try_recv().is_ok(), valid);
            if !valid && !local_fault {
                let revoked = pm.get_status(builder).block_hash;
                assert_eq!(revoked, sealed_hash);
                assert_ne!(revoked, bid_hash);
            }
            assert!(
                !crate::shared::check_and_record_mined_block(block_number, parent_hash),
                "a broadcast block must retain its double-sign reservation"
            );
        }
    }

    #[tokio::test]
    async fn bid_block_engine_err_leaves_double_sign_slot_claimed() {
        // An engine failure proves no builder fault, but cannot undo an already-broadcast
        // signature.
        let (responses, mut new_payload_observed) = EngineResponses::unavailable_new_payload();
        let mut fixture = TestFixture::new(responses).await;

        // High, test-local block number: `RECENT_MINED_BLOCKS` is process-global.
        let block_number = 900_101_u64;
        let sealed = create_bid_sealed_block(block_number);
        let parent_hash = sealed.header().parent_hash;

        let builder = Address::repeat_byte(0x7c);
        let bid_hash = B256::repeat_byte(0xb2);
        let pm = crate::shared::get_bid_block_permission_manager();
        assert!(pm.is_allowed(builder), "builder should start allowed");

        // Stand in for the miner, which claims the slot before handing the block to this service
        // (`pick_best_payload_and_finalize` → `check_and_record_mined_block`).
        assert!(
            crate::shared::check_and_record_mined_block(block_number, parent_hash),
            "slot must start unclaimed"
        );

        fixture
            .bid_tx
            .send(IncomingLocalBlock::Bid(Box::new((sealed, builder, bid_hash, U256::ZERO, 0))))
            .await
            .unwrap();

        // 1. Broadcast still happens, before (and despite) the failed verification.
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        let mut saw_broadcast = false;
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(2);
        while !saw_broadcast && tokio::time::Instant::now() < deadline {
            match fixture.handle.poll_outcome(&mut cx) {
                Poll::Ready(Some(event)) => {
                    if matches!(
                        event,
                        BlockImportEvent::Announcement(BlockValidation::ValidBlock { .. })
                    ) {
                        saw_broadcast = true;
                    }
                }
                Poll::Ready(None) => break,
                Poll::Pending => tokio::task::yield_now().await,
            }
        }
        assert!(saw_broadcast, "BidBlock must be broadcast before verification");

        // 2. Wait for positive proof the engine call was made and failed, rather than assuming it
        //    from a bare timeout — otherwise the negative assertions below could pass vacuously.
        tokio::time::timeout(tokio::time::Duration::from_secs(2), new_payload_observed.recv())
            .await
            .expect("engine.new_payload was never called")
            .expect("engine mock closed without observing new_payload");
        // The responder was already dropped when the observation fired, so the handle's `Err` is
        // determined; yield briefly to let the spawned verify task reach its `Err` arm.
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // 3. The slot is still claimed: a second call for the same pair is refused.
        assert!(
            !crate::shared::check_and_record_mined_block(block_number, parent_hash),
            "double-sign slot must remain claimed after an engine error — the block was already \
             broadcast, so releasing it would permit a second signature at this height"
        );

        // 4. The builder keeps its permission: `Err` is not provable dishonesty.
        assert!(pm.is_allowed(builder), "an engine-side error must not revoke the builder");
    }

    #[tokio::test]
    async fn bid_block_low_average_gas_price_revokes() {
        // Post-import average-gas-price floor check (go-bsc `validateBidBlockAverageGasPrice`):
        // a Valid BidBlock whose deposit-derived gas_fee implies an average price below the
        // validator's floor must still get canonicalized (it already passed InsertChain-equivalent
        // verification) but must revoke the builder's *future* SendBidBlock permission.
        let sealed = create_bid_sealed_block(1);
        let block_hash = sealed.hash();

        let mut provider = MockProvider::new();
        // gas_fee = 0 over 21_000 non-system gas => avg = 0, below any positive floor (including
        // the compiled-in DEFAULT_MIN_GAS_TIP the test process falls back to).
        provider.insert_receipts(
            block_hash,
            vec![Receipt {
                tx_type: alloy_consensus::TxType::Legacy,
                success: true,
                cumulative_gas_used: 21_000,
                logs: Vec::new(),
            }],
        );

        let mut fixture =
            TestFixture::new_with_provider(EngineResponses::both_valid(), provider).await;

        let builder = Address::repeat_byte(0x7f);
        let bid_hash = B256::repeat_byte(0xb2);
        let pm = crate::shared::get_bid_block_permission_manager();
        assert!(pm.is_allowed(builder), "builder should start allowed");

        // system_tx_start = 1: the single receipt above is the entire non-system-tx region.
        fixture
            .bid_tx
            .send(IncomingLocalBlock::Bid(Box::new((sealed, builder, bid_hash, U256::ZERO, 1))))
            .await
            .unwrap();

        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(2);
        while pm.is_allowed(builder) && tokio::time::Instant::now() < deadline {
            let _ = fixture.handle.poll_outcome(&mut cx);
            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        }
        assert!(!pm.is_allowed(builder), "builder must be revoked for a too-low average gas price");
        let revoked = pm.get_status(builder).block_hash;
        assert_eq!(revoked, block_hash);
        assert_ne!(revoked, bid_hash);
    }

    #[tokio::test]
    async fn bid_block_sufficient_average_gas_price_does_not_revoke() {
        // Regression guard for the check above: a BidBlock whose average gas price clears the
        // floor must not be revoked, even though it goes through the exact same code path.
        let sealed = create_bid_sealed_block(1);
        let block_hash = sealed.hash();

        let mut provider = MockProvider::new();
        provider.insert_receipts(
            block_hash,
            vec![Receipt {
                tx_type: alloy_consensus::TxType::Legacy,
                success: true,
                cumulative_gas_used: 21_000,
                logs: Vec::new(),
            }],
        );

        let mut fixture =
            TestFixture::new_with_provider(EngineResponses::both_valid(), provider).await;

        let builder = Address::repeat_byte(0x80);
        let bid_hash = B256::repeat_byte(0xb3);
        let pm = crate::shared::get_bid_block_permission_manager();
        assert!(pm.is_allowed(builder), "builder should start allowed");

        // gas_fee well above 21_000 * DEFAULT_MIN_GAS_TIP (21_000 * 50_000_000) clears the floor
        // with room to spare regardless of whatever the test process's ambient config resolves to.
        let gas_fee = U256::from(21_000u64) * U256::from(1_000_000_000_000u64);
        fixture
            .bid_tx
            .send(IncomingLocalBlock::Bid(Box::new((sealed, builder, bid_hash, gas_fee, 1))))
            .await
            .unwrap();

        // There's no state change to poll for in the non-revoked case; give the spawned
        // verify-then-check task ample time to run, then assert nothing happened.
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_millis(500);
        while tokio::time::Instant::now() < deadline {
            let _ = fixture.handle.poll_outcome(&mut cx);
            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        }
        assert!(
            pm.is_allowed(builder),
            "builder must not be revoked for a sufficient average gas price"
        );
    }

    #[derive(Clone)]
    struct MockProvider {
        headers_by_number: HashMap<BlockNumber, Header>,
        concurrent_headers: Arc<RwLock<HashMap<BlockHash, Header>>>,
        concurrent_canonical: Arc<RwLock<HashMap<BlockNumber, BlockHash>>>,
        headers_by_hash: HashMap<BlockHash, Header>,
        td_by_hash: HashMap<BlockHash, U256>,
        head_number: BlockNumber,
        head_hash: BlockHash,
        /// Configurable receipts for the post-import average-gas-price check
        /// (`validate_bid_block_average_gas_price`); empty unless a test opts in via
        /// [`MockProvider::insert_receipts`].
        receipts_by_hash: HashMap<BlockHash, Vec<Receipt>>,
    }

    impl MockProvider {
        fn new() -> Self {
            let headers_by_number = HashMap::new();
            let headers_by_hash = HashMap::new();
            let td_by_hash = HashMap::new();
            Self {
                headers_by_number,
                concurrent_headers: Default::default(),
                concurrent_canonical: Default::default(),
                headers_by_hash,
                td_by_hash,
                head_number: 0,
                head_hash: BlockHash::ZERO,
                receipts_by_hash: HashMap::new(),
            }
        }

        fn insert(&mut self, header: Header, td: U256) {
            self.headers_by_number.insert(header.number, header.clone());
            self.headers_by_hash.insert(header.hash_slow(), header.clone());
            self.td_by_hash.insert(header.hash_slow(), td);
            if header.number > self.head_number {
                self.head_number = header.number;
                self.head_hash = header.hash_slow();
            }
        }

        fn insert_receipts(&mut self, block_hash: BlockHash, receipts: Vec<Receipt>) {
            self.receipts_by_hash.insert(block_hash, receipts);
        }
    }

    impl ReceiptProvider for MockProvider {
        type Receipt = Receipt;

        fn receipt(&self, _id: u64) -> Result<Option<Self::Receipt>, ProviderError> {
            Ok(None)
        }

        fn receipt_by_hash(&self, _hash: B256) -> Result<Option<Self::Receipt>, ProviderError> {
            Ok(None)
        }

        fn receipts_by_block(
            &self,
            block: alloy_eips::BlockHashOrNumber,
        ) -> Result<Option<Vec<Self::Receipt>>, ProviderError> {
            let hash = match block {
                alloy_eips::BlockHashOrNumber::Hash(hash) => hash,
                alloy_eips::BlockHashOrNumber::Number(number) => {
                    match self.headers_by_number.get(&number) {
                        Some(header) => header.hash_slow(),
                        None => return Ok(None),
                    }
                }
            };
            Ok(self.receipts_by_hash.get(&hash).cloned())
        }

        fn receipts_by_tx_range(
            &self,
            _range: impl core::ops::RangeBounds<u64>,
        ) -> Result<Vec<Self::Receipt>, ProviderError> {
            Ok(vec![])
        }

        fn receipts_by_block_range(
            &self,
            _block_range: core::ops::RangeInclusive<BlockNumber>,
        ) -> Result<Vec<Vec<Self::Receipt>>, ProviderError> {
            Ok(vec![])
        }
    }

    impl BlockHashReader for MockProvider {
        fn block_hash(&self, number: BlockNumber) -> Result<Option<B256>, ProviderError> {
            Ok(self
                .concurrent_canonical
                .read()
                .get(&number)
                .copied()
                .or_else(|| self.headers_by_number.get(&number).map(|h| h.hash_slow())))
        }

        fn canonical_hashes_range(
            &self,
            _start: BlockNumber,
            _end: BlockNumber,
        ) -> Result<Vec<B256>, ProviderError> {
            Ok(vec![])
        }
    }

    impl BlockNumReader for MockProvider {
        fn chain_info(&self) -> Result<ChainInfo, ProviderError> {
            Ok(ChainInfo { best_hash: self.head_hash, best_number: self.head_number })
        }

        fn best_block_number(&self) -> Result<BlockNumber, ProviderError> {
            Ok(self.head_number)
        }

        fn last_block_number(&self) -> Result<BlockNumber, ProviderError> {
            Ok(self.head_number)
        }

        fn block_number(&self, hash: B256) -> Result<Option<BlockNumber>, ProviderError> {
            Ok(self.headers_by_hash.get(&hash).map(|h| h.number))
        }
    }

    impl HeaderProvider for MockProvider {
        type Header = Header;

        fn header(&self, block_hash: B256) -> Result<Option<Self::Header>, ProviderError> {
            Ok(self
                .concurrent_headers
                .read()
                .get(&block_hash)
                .cloned()
                .or_else(|| self.headers_by_hash.get(&block_hash).cloned()))
        }

        fn header_by_number(&self, num: u64) -> Result<Option<Self::Header>, ProviderError> {
            Ok(self.headers_by_number.get(&num).cloned())
        }

        fn header_td(&self, hash: &B256) -> Result<Option<U256>, ProviderError> {
            Ok(self.td_by_hash.get(hash).copied())
        }

        fn header_td_by_number(&self, number: BlockNumber) -> Result<Option<U256>, ProviderError> {
            if let Some(h) = self.headers_by_number.get(&number) {
                Ok(self.td_by_hash.get(&h.hash_slow()).cloned())
            } else {
                Ok(None)
            }
        }

        fn headers_range(
            &self,
            range: impl core::ops::RangeBounds<BlockNumber>,
        ) -> Result<Vec<Self::Header>, ProviderError> {
            use std::ops::Bound::*;
            let start = match range.start_bound() {
                Included(&s) => s,
                Excluded(&s) => s + 1,
                Unbounded => 0,
            };
            let end = match range.end_bound() {
                Included(&e) => e,
                Excluded(&e) => e - 1,
                Unbounded => self.head_number,
            };
            let mut out = Vec::new();
            for n in start..=end {
                if let Some(h) = self.headers_by_number.get(&n) {
                    out.push(h.clone());
                }
            }
            Ok(out)
        }

        fn sealed_header(
            &self,
            number: BlockNumber,
        ) -> Result<Option<SealedHeader<Self::Header>>, ProviderError> {
            Ok(self.headers_by_number.get(&number).cloned().map(SealedHeader::seal_slow))
        }

        fn sealed_headers_while(
            &self,
            range: impl core::ops::RangeBounds<BlockNumber>,
            mut predicate: impl FnMut(&SealedHeader<Self::Header>) -> bool,
        ) -> Result<Vec<SealedHeader<Self::Header>>, ProviderError> {
            let hs = self.headers_range(range)?;
            let mut out = Vec::new();
            for h in hs {
                let sh = SealedHeader::seal_slow(h);
                if !predicate(&sh) {
                    break;
                }
                out.push(sh);
            }
            Ok(out)
        }
    }
    /// Response configuration for engine messages
    struct EngineResponses {
        new_payload: PayloadStatusEnum,
        new_payload_local_fault: bool,
        fcu: PayloadStatusEnum,
        /// Drop the responder so `new_payload` returns `EngineUnavailable`.
        new_payload_unavailable: bool,
        /// Signals that the mock has replied to or dropped a `NewPayload` request.
        new_payload_observed: Option<mpsc::UnboundedSender<()>>,
    }

    impl EngineResponses {
        fn both_valid() -> Self {
            Self {
                new_payload: PayloadStatusEnum::Valid,
                new_payload_local_fault: false,
                fcu: PayloadStatusEnum::Valid,
                new_payload_unavailable: false,
                new_payload_observed: None,
            }
        }

        fn invalid_new_payload() -> Self {
            Self {
                new_payload: PayloadStatusEnum::Invalid { validation_error: "test error".into() },
                ..Self::both_valid()
            }
        }

        fn invalid_fcu() -> Self {
            Self {
                fcu: PayloadStatusEnum::Invalid { validation_error: "fcu error".into() },
                ..Self::both_valid()
            }
        }

        fn unavailable_new_payload() -> (Self, mpsc::UnboundedReceiver<()>) {
            let (observed_tx, observed_rx) = mpsc::unbounded_channel();
            (
                Self {
                    new_payload_unavailable: true,
                    new_payload_observed: Some(observed_tx),
                    ..Self::both_valid()
                },
                observed_rx,
            )
        }
    }

    /// Test fixture for block import tests
    struct TestFixture {
        handle: ImportHandle,
        forkchoice_updates: mpsc::UnboundedReceiver<ForkchoiceState>,
        /// Sender feeding the bounded local ingress with selected BidBlocks.
        bid_tx: mpsc::Sender<IncomingLocalBlock>,
    }

    impl TestFixture {
        /// Create a new test fixture with the given engine responses
        async fn new(responses: EngineResponses) -> Self {
            Self::new_with_provider(responses, MockProvider::new()).await
        }

        /// Create a new test fixture with the given engine responses and a pre-configured
        /// provider (e.g. with receipts installed via [`MockProvider::insert_receipts`] for the
        /// post-import average-gas-price check).
        async fn new_with_provider(responses: EngineResponses, provider: MockProvider) -> Self {
            // Use mainnet chain spec for tests; it influences only fast-finality parsing.
            let chain_spec = Arc::new(crate::chainspec::BscChainSpec::from(
                crate::chainspec::bsc::bsc_mainnet(),
            ));

            let (to_engine, from_engine) = mpsc::unbounded_channel();
            let engine_handle = ConsensusEngineHandle::new(to_engine);

            let forkchoice_updates =
                handle_engine_msg(from_engine, responses, provider.clone()).await;

            let (to_import, from_network) = mpsc::channel(IMPORT_QUEUE_CAPACITY);
            let (to_import_bid, from_local) = mpsc::channel(propagation::LOCAL_BLOCK_CAPACITY);
            let (to_hashes, from_hashes) = mpsc::channel(IMPORT_QUEUE_CAPACITY);
            let (to_network, import_outcome) = propagation::channel();

            let handle = ImportHandle::new(to_import, to_hashes, import_outcome);

            let service = ImportService::new(
                provider,
                chain_spec,
                engine_handle,
                from_network,
                from_local,
                from_hashes,
                to_network,
            );
            tokio::spawn(Box::pin(async move {
                service.await.unwrap();
            }));

            Self { handle, bid_tx: to_import_bid, forkchoice_updates }
        }

        /// Run a block import test with the given event assertion
        async fn assert_block_import<F>(&mut self, assert_fn: F)
        where
            F: Fn(&BlockImportEvent<BscNewBlock>) -> bool,
        {
            let block_msg = create_test_block();
            self.assert_block_import_with_block(block_msg, assert_fn).await;
        }

        async fn assert_block_import_with_block<F>(
            &mut self,
            block_msg: NewBlockMessage<BscNewBlock>,
            assert_fn: F,
        ) where
            F: Fn(&BlockImportEvent<BscNewBlock>) -> bool,
        {
            self.handle.send_block(block_msg, PeerId::random()).unwrap();

            let waker = futures::task::noop_waker();
            let mut cx = Context::from_waker(&waker);
            let mut outcomes = Vec::new();

            // Wait for the first block to be processed
            let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(1);
            loop {
                match self.handle.poll_outcome(&mut cx) {
                    Poll::Ready(Some(event)) => {
                        outcomes.push(event);
                        if outcomes.iter().any(&assert_fn) {
                            break;
                        }
                    }
                    Poll::Ready(None) => break,
                    Poll::Pending => {
                        if tokio::time::Instant::now() >= deadline {
                            break;
                        }
                        tokio::task::yield_now().await;
                    }
                }
            }

            // Assert that at least one outcome matches our criteria
            assert!(
                outcomes.iter().any(assert_fn),
                "No outcome matched the expected criteria. Outcomes: {outcomes:?}"
            );
        }
    }

    /// Creates a test block message
    fn create_test_block() -> NewBlockMessage<BscNewBlock> {
        let block = BscBlock {
            header: Header::default(),
            body: BscBlockBody {
                inner: BlockBody {
                    transactions: Vec::new(),
                    ommers: Vec::new(),
                    withdrawals: None,
                },
                sidecars: None,
            },
        };
        let new_block = BscNewBlock(NewBlock { block, td: U128::from(1) });
        let hash = new_block.0.block.header.hash_slow();
        NewBlockMessage { hash, block: Arc::new(new_block) }
    }

    /// Helper function to handle engine messages with specified payload statuses
    async fn handle_engine_msg(
        mut from_engine: mpsc::UnboundedReceiver<BeaconEngineMessage<BscPayloadTypes>>,
        responses: EngineResponses,
        provider: MockProvider,
    ) -> mpsc::UnboundedReceiver<ForkchoiceState> {
        let (fcu_tx, fcu_rx) = mpsc::unbounded_channel();
        tokio::spawn(Box::pin(async move {
            while let Some(message) = from_engine.recv().await {
                match message {
                    BeaconEngineMessage::NewPayload { payload, tx, .. } => {
                        if responses.new_payload_local_fault {
                            let local_fault = payload.local_fault.as_ref().expect("request marker");
                            assert!(!local_fault.load(Ordering::Acquire));
                            local_fault.store(true, Ordering::Release);
                        }
                        if responses.new_payload_unavailable {
                            drop(tx);
                        } else {
                            tx.send(Ok(PayloadStatus::new(responses.new_payload.clone(), None)))
                                .unwrap();
                        }
                        if let Some(observed) = &responses.new_payload_observed {
                            let _ = observed.send(());
                        }
                    }
                    BeaconEngineMessage::ForkchoiceUpdated {
                        state, payload_attrs: _, tx, ..
                    } => {
                        let _ = fcu_tx.send(state);
                        tx.send(Ok(OnForkChoiceUpdated::valid(PayloadStatus::new(
                            responses.fcu.clone(),
                            None,
                        ))))
                        .unwrap();
                    }
                    _ => {}
                }
            }
        }));
        fcu_rx
    }

    /// Spawn an `ImportService` with `MockProvider::best_block_number = local_tip`
    /// and an engine handler that forwards every observed `ForkchoiceState`
    /// into the returned channel, and every `NewPayload` into the other.
    /// Replies Valid to both kinds of message so the service doesn't block.
    async fn spawn_service_with_tip(
        local_tip: u64,
    ) -> (ImportHandle, mpsc::UnboundedReceiver<ForkchoiceState>, mpsc::UnboundedReceiver<()>) {
        let mut provider = MockProvider::new();
        let header = Header { number: local_tip, ..Default::default() };
        provider.insert(header, U256::from(1));
        // "TD unknown" is produced by the provider, not the engine: since v2.5 fork choice resolves
        // TD through `header_td`, and this mock holds only `local_tip`, both the incoming fork
        // block's hash and its parent's answer `Ok(None)` — the case that used to wedge the head.
        let chain_spec =
            Arc::new(crate::chainspec::BscChainSpec::from(crate::chainspec::bsc::bsc_mainnet()));

        let (to_engine, mut from_engine) = mpsc::unbounded_channel();
        let engine_handle = ConsensusEngineHandle::new(to_engine);

        let (fcu_tx, fcu_rx) = mpsc::unbounded_channel();
        let (np_tx, np_rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(message) = from_engine.recv().await {
                match message {
                    BeaconEngineMessage::NewPayload { payload: _, tx, .. } => {
                        let _ = np_tx.send(());
                        let _ = tx.send(Ok(PayloadStatus::new(PayloadStatusEnum::Valid, None)));
                    }
                    BeaconEngineMessage::ForkchoiceUpdated {
                        state, payload_attrs: _, tx, ..
                    } => {
                        let _ = fcu_tx.send(state);
                        let _ = tx.send(Ok(OnForkChoiceUpdated::valid(PayloadStatus::new(
                            PayloadStatusEnum::Valid,
                            None,
                        ))));
                    }
                    _ => {}
                }
            }
        });

        let (to_import, from_network) = mpsc::channel(IMPORT_QUEUE_CAPACITY);
        let (_to_local, from_local) = mpsc::channel(propagation::LOCAL_BLOCK_CAPACITY);
        let (to_hashes, from_hashes) = mpsc::channel(IMPORT_QUEUE_CAPACITY);
        let (to_network, import_outcome) = propagation::channel();
        let handle = ImportHandle::new(to_import, to_hashes, import_outcome);

        let service = ImportService::new(
            provider,
            chain_spec,
            engine_handle,
            from_network,
            from_local,
            from_hashes,
            to_network,
        );
        tokio::spawn(Box::pin(async move {
            service.await.unwrap();
        }));

        (handle, fcu_rx, np_rx)
    }

    /// Try to receive a FCU from the observation channel within `timeout`.
    async fn recv_fcu_within(
        fcu_rx: &mut mpsc::UnboundedReceiver<ForkchoiceState>,
        timeout: tokio::time::Duration,
    ) -> Option<ForkchoiceState> {
        tokio::time::timeout(timeout, fcu_rx.recv()).await.ok().flatten()
    }

    /// Like [`spawn_service_with_tip`], but the provider cannot resolve the incoming block's TD.
    /// Since v2.5 fork choice reads TD via `header_td` rather than asking the engine, "TD unknown"
    /// is what a real node sees for a fork whose divergence point lies below the persisted tip.
    async fn spawn_service_with_td_unknown(
        local_tip: u64,
    ) -> (ImportHandle, mpsc::UnboundedReceiver<ForkchoiceState>, mpsc::UnboundedReceiver<()>) {
        let mut provider = MockProvider::new();
        let header = Header { number: local_tip, ..Default::default() };
        provider.insert(header, U256::from(1));
        let chain_spec =
            Arc::new(crate::chainspec::BscChainSpec::from(crate::chainspec::bsc::bsc_mainnet()));

        let (to_engine, mut from_engine) = mpsc::unbounded_channel();
        let engine_handle = ConsensusEngineHandle::new(to_engine);

        let (fcu_tx, fcu_rx) = mpsc::unbounded_channel();
        let (np_tx, np_rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(message) = from_engine.recv().await {
                match message {
                    BeaconEngineMessage::NewPayload { payload: _, tx, .. } => {
                        let _ = np_tx.send(());
                        let _ = tx.send(Ok(PayloadStatus::new(PayloadStatusEnum::Valid, None)));
                    }
                    BeaconEngineMessage::ForkchoiceUpdated {
                        state, payload_attrs: _, tx, ..
                    } => {
                        let _ = fcu_tx.send(state);
                        let _ = tx.send(Ok(OnForkChoiceUpdated::valid(PayloadStatus::new(
                            PayloadStatusEnum::Valid,
                            None,
                        ))));
                    }
                    _ => {}
                }
            }
        });

        let (to_import, from_network) = mpsc::channel(IMPORT_QUEUE_CAPACITY);
        let (_to_local, from_local) =
            mpsc::channel(super::super::propagation::LOCAL_BLOCK_CAPACITY);
        let (to_hashes, from_hashes) = mpsc::channel(IMPORT_QUEUE_CAPACITY);
        let (to_network, import_outcome) = super::super::propagation::channel();
        let handle = ImportHandle::new(to_import, to_hashes, import_outcome);

        let service = ImportService::new(
            provider,
            chain_spec,
            engine_handle,
            from_network,
            from_local,
            from_hashes,
            to_network,
        );
        tokio::spawn(Box::pin(async move {
            service.await.unwrap();
        }));

        (handle, fcu_rx, np_rx)
    }

    /// Builds a test block message at `number`.
    fn create_test_block_at(number: u64) -> NewBlockMessage<BscNewBlock> {
        let block = BscBlock {
            header: Header { number, ..Default::default() },
            body: BscBlockBody {
                inner: BlockBody {
                    transactions: Vec::new(),
                    ommers: Vec::new(),
                    withdrawals: None,
                },
                sidecars: None,
            },
        };
        let new_block = BscNewBlock(NewBlock { block, td: U128::from(1) });
        let hash = new_block.0.block.header.hash_slow();
        NewBlockMessage { hash, block: Arc::new(new_block) }
    }

    /// Regression test for the unrecoverable-fork wedge (QA net, 2026-07-28).
    ///
    /// A node whose persisted tip sits on a branch below the fork point cannot resolve the TD of
    /// any incoming majority-chain block. Before the fix, `head_choice_with_td` turned that into
    /// `UnknownTotalDifficulty`, `update_forkchoice` returned `Err`, and **no forkchoice update
    /// was ever sent** — freezing the canonical head, which kept the DB on the losing branch,
    /// which kept the TD unresolvable. Four validators stayed dead for an entire run while
    /// reporting themselves healthy.
    ///
    /// This asserts the loop-level property the unit tests cannot: an FCU is actually emitted,
    /// and it targets the incoming block rather than the stale local tip.
    #[tokio::test]
    async fn unresolvable_td_still_emits_forkchoice_update() {
        // Local tip 63 and incoming 653 mirror the production incident.
        let (handle, mut fcu_rx, _np_rx) = spawn_service_with_td_unknown(63).await;

        let block = create_test_block_at(653);
        let expected_head = block.hash;
        handle.send_block(block, PeerId::random()).unwrap();

        let state = recv_fcu_within(&mut fcu_rx, tokio::time::Duration::from_secs(5)).await.expect(
            "a forkchoice update must be emitted even when the incoming TD is unresolvable; \
                 without one the canonical head can never advance and the node is wedged",
        );
        assert_eq!(
            state.head_block_hash, expected_head,
            "the forkchoice update must target the incoming block, not the stale local tip"
        );
    }

    #[tokio::test]
    async fn far_ahead_102_hash_replay_is_coalesced_before_engine() {
        let (handle, mut fcu_rx, _np_rx) = spawn_service_with_tip(100).await;
        let peer = PeerId::random();
        let hashes = NewBlockHashes(
            (1..=102u64)
                .map(|n| BlockHashNumber {
                    hash: B256::from(U256::from(n).to_be_bytes::<32>()),
                    number: 100 + PIPELINE_TRIGGER_DELTA + n,
                })
                .collect(),
        );
        handle.send_hashes(hashes.clone(), peer).unwrap();
        handle.send_hashes(hashes, peer).unwrap();
        let fcu = recv_fcu_within(&mut fcu_rx, tokio::time::Duration::from_millis(500))
            .await
            .expect("one coalesced target should reach Engine");
        assert_eq!(fcu.head_block_hash, B256::from(U256::from(102).to_be_bytes::<32>()));
        assert!(recv_fcu_within(&mut fcu_rx, tokio::time::Duration::from_millis(100))
            .await
            .is_none());
    }

    #[tokio::test]
    async fn routes_far_behind_announcement_to_pipeline_fcu() {
        // Local tip = 100. Announce a head whose gap exceeds the threshold by
        // one. Expect the engine to see exactly one FCU with the announced
        // hash as `head_block_hash` and zeroed safe/finalized.
        let (handle, mut fcu_rx, _np_rx) = spawn_service_with_tip(100).await;

        let head_hash = B256::from([0xAA; 32]);
        let head_num = 100 + PIPELINE_TRIGGER_DELTA + 1;
        let hashes = NewBlockHashes(vec![BlockHashNumber { hash: head_hash, number: head_num }]);
        handle.send_hashes(hashes, PeerId::random()).unwrap();

        let fcu = recv_fcu_within(&mut fcu_rx, tokio::time::Duration::from_millis(500))
            .await
            .expect("expected a pipeline-trigger FCU");
        assert_eq!(fcu.head_block_hash, head_hash);
        assert_eq!(fcu.safe_block_hash, B256::ZERO);
        assert_eq!(fcu.finalized_block_hash, B256::ZERO);
    }

    #[tokio::test]
    async fn routes_at_threshold_to_fork_recover_not_pipeline() {
        // Equality case: gap == PIPELINE_TRIGGER_DELTA (2048). Threshold is a
        // strict `>`, so this announcement must NOT dispatch an FCU — it goes
        // to fork_recover (which will itself no-op here since the MockProvider
        // has no BSC peer and no ancestry, but we only assert absence of FCU).
        let (handle, mut fcu_rx, _np_rx) = spawn_service_with_tip(100).await;

        let head_hash = B256::from([0xBB; 32]);
        let head_num = 100 + PIPELINE_TRIGGER_DELTA;
        let hashes = NewBlockHashes(vec![BlockHashNumber { hash: head_hash, number: head_num }]);
        handle.send_hashes(hashes, PeerId::random()).unwrap();

        assert!(
            recv_fcu_within(&mut fcu_rx, tokio::time::Duration::from_millis(200)).await.is_none(),
            "announcement at the exact threshold must not dispatch an FCU"
        );
    }

    #[tokio::test]
    async fn pipeline_target_can_be_reannounced_after_fcu_completion() {
        let (handle, mut fcu_rx, _np_rx) = spawn_service_with_tip(100).await;

        let head_hash = B256::from([0xCC; 32]);
        let hashes = NewBlockHashes(vec![BlockHashNumber {
            hash: head_hash,
            number: 100 + PIPELINE_TRIGGER_DELTA + 10,
        }]);
        handle.send_hashes(hashes.clone(), PeerId::random()).unwrap();
        let first = recv_fcu_within(&mut fcu_rx, tokio::time::Duration::from_millis(500))
            .await
            .expect("expected one FCU");
        assert_eq!(first.head_block_hash, head_hash);
        // A response may still be in transit. Reannounce until the active slot is released;
        // there is no cooldown or permanent processed marker suppressing this target.
        let next = tokio::time::timeout(tokio::time::Duration::from_secs(1), async {
            loop {
                handle.send_hashes(hashes.clone(), PeerId::random()).unwrap();
                if let Some(fcu) =
                    recv_fcu_within(&mut fcu_rx, tokio::time::Duration::from_millis(10)).await
                {
                    break fcu;
                }
            }
        })
        .await
        .expect("completed FCU must leave the target eligible for retry");
        assert_eq!(next.head_block_hash, head_hash);
    }

    fn peer(tag: u8, best_number: Option<u64>) -> (PeerId, Option<u64>) {
        // `PeerId` is `alloy_primitives::B512`. Build a deterministic 64-byte
        // value from the tag via the `From<[u8; 64]>` impl.
        let mut bytes = [0u8; 64];
        bytes[0] = tag;
        (PeerId::from(bytes), best_number)
    }

    #[test]
    fn planner_announces_when_we_are_ahead() {
        let peers = vec![peer(1, Some(100))];
        let result = plan_head_announcements(200, &peers);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], peers[0].0);
    }

    #[test]
    fn planner_announces_at_exact_64_gap_boundary() {
        // Receiver drops only on strict `num + 64 < peer_best`, so gap == 64 is still fine.
        let local = 100;
        let peers = vec![peer(1, Some(local + 64))];
        let result = plan_head_announcements(local, &peers);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn planner_skips_peer_more_than_64_ahead() {
        let local = 100;
        let peers = vec![peer(1, Some(local + 65))];
        let result = plan_head_announcements(local, &peers);
        assert!(result.is_empty());
    }

    #[test]
    fn planner_announces_when_peer_best_number_unknown() {
        // best_number is None before any head info has been observed; announce is the right
        // default.
        let peers = vec![peer(1, None)];
        let result = plan_head_announcements(100, &peers);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn planner_mixes_skip_and_announce_across_peers() {
        let local = 1000;
        let p_ahead = peer(1, Some(local + 65)); // skipped
        let p_at_boundary = peer(2, Some(local + 64)); // announced
        let p_behind = peer(3, Some(local - 10)); // announced
        let p_unknown = peer(4, None); // announced
        let peers = vec![p_ahead, p_at_boundary, p_behind, p_unknown];
        let result = plan_head_announcements(local, &peers);
        assert_eq!(result.len(), 3);
        assert!(result.contains(&p_at_boundary.0));
        assert!(result.contains(&p_behind.0));
        assert!(result.contains(&p_unknown.0));
        assert!(!result.contains(&p_ahead.0));
    }

    #[test]
    fn planner_returns_empty_on_no_peers() {
        let result = plan_head_announcements(100, &[]);
        assert!(result.is_empty());
    }

    fn fetched_fixture(
        provider: MockProvider,
    ) -> (
        ImportService<MockProvider>,
        mpsc::UnboundedReceiver<BeaconEngineMessage<BscPayloadTypes>>,
        propagation::PropagationReceiver,
    ) {
        let (tx, rx) = mpsc::unbounded_channel();
        let (_, blocks) = mpsc::channel(IMPORT_QUEUE_CAPACITY);
        let (_, local) = mpsc::channel(propagation::LOCAL_BLOCK_CAPACITY);
        let (_, hashes) = mpsc::channel(IMPORT_QUEUE_CAPACITY);
        let (events, receiver) = propagation::channel();
        let mut service = ImportService::new(
            provider,
            Arc::new(BscChainSpec::from(bsc_mainnet())),
            ConsensusEngineHandle::new(tx),
            blocks,
            local,
            hashes,
            events,
        );
        service.recovery_gate = crate::node::network::block_import::fork_recover::RecoveryGate::new(
            1,
            std::time::Duration::from_secs(60),
        );
        (service, rx, receiver)
    }

    #[test]
    fn hash_admission_filters_finalized_stale_and_handles_u64_edges() {
        assert!(obsolete_announcement(99, 100, 99, MAX_UNCLE_DISTANCE));
        assert!(obsolete_announcement(88, 100, 80, MAX_UNCLE_DISTANCE));
        assert!(!obsolete_announcement(89, 100, 80, MAX_UNCLE_DISTANCE));
        assert!(!obsolete_announcement(101, 100, 80, MAX_UNCLE_DISTANCE));
        assert!(obsolete_announcement(0, 0, 0, MAX_UNCLE_DISTANCE));
        assert!(!obsolete_announcement(u64::MAX, u64::MAX, u64::MAX - 1, MAX_UNCLE_DISTANCE));
    }

    #[tokio::test]
    async fn known_parent_fetched_block_propagates_exact_branch_td_and_dedups() {
        let mut provider = MockProvider::new();
        let parent = Header { number: 10, ..Default::default() };
        provider.insert(parent.clone(), U256::from(100));
        let (mut service, mut engine, mut events) = fetched_fixture(provider);
        let block = BscBlock {
            header: Header {
                number: 11,
                parent_hash: parent.hash_slow(),
                difficulty: U256::from(2),
                ..Default::default()
            },
            ..Default::default()
        };
        let message = NewBlockMessage {
            hash: block.header.hash_slow(),
            block: Arc::new(BscNewBlock(NewBlock { block, td: U128::ZERO })),
        };
        service.on_new_block_inner(message.clone(), PeerId::random(), true);
        assert!(events.try_recv().is_err(), "fetched headers must await Engine validation");
        service.on_new_block_inner(message, PeerId::random(), true);
        assert_eq!(service.pending_imports.len(), 1);
        let mut cx = Context::from_waker(futures::task::noop_waker_ref());
        assert!(Pin::new(&mut service).poll(&mut cx).is_pending());
        let BeaconEngineMessage::NewPayload { tx, .. } = engine.try_recv().unwrap() else {
            panic!("new payload expected")
        };
        tx.send(Ok(PayloadStatus::new(PayloadStatusEnum::Valid, None))).unwrap();
        assert!(Pin::new(&mut service).poll(&mut cx).is_pending());
        assert!(events.try_recv().is_err());
        let BeaconEngineMessage::ForkchoiceUpdated { tx, .. } = engine.try_recv().unwrap() else {
            panic!("fork choice expected")
        };
        tx.send(Ok(OnForkChoiceUpdated::valid(PayloadStatus::new(PayloadStatusEnum::Valid, None))))
            .unwrap();
        assert!(Pin::new(&mut service).poll(&mut cx).is_pending());
        let BlockImportEvent::Announcement(BlockValidation::ValidHeader { block }) =
            events.try_recv().unwrap()
        else {
            panic!("full propagation expected")
        };
        assert_eq!(block.block.0.td, U128::from(102));
        let BlockImportEvent::Outcome(Outcome {
            result: Ok(BlockValidation::ValidBlock { block }),
            ..
        }) = events.try_recv().unwrap()
        else {
            panic!("hash propagation expected")
        };
        assert_eq!(block.block.0.td, U128::from(102));
        assert!(events.try_recv().is_err());
    }

    #[test]
    fn fetched_td_errors_never_wrap_or_truncate() {
        let mut provider = MockProvider::new();
        let parent = Header::default();
        provider.insert(parent.clone(), U256::MAX);
        let header = Header {
            parent_hash: parent.hash_slow(),
            difficulty: U256::from(1),
            ..Default::default()
        };
        let (tx, _) = mpsc::unbounded_channel();
        let engine = BscForkChoiceEngine::new(
            provider.clone(),
            ConsensusEngineHandle::new(tx),
            Arc::new(BscChainSpec::from(bsc_mainnet())),
        );
        assert_eq!(
            fetched_block_td(&engine, &header).unwrap_err().to_string(),
            "fetched block total difficulty overflow"
        );
        provider.insert(parent, U256::from(u128::MAX));
        let (tx, _) = mpsc::unbounded_channel();
        let engine = BscForkChoiceEngine::new(
            provider,
            ConsensusEngineHandle::new(tx),
            Arc::new(BscChainSpec::from(bsc_mainnet())),
        );
        assert_eq!(
            fetched_block_td(&engine, &header).unwrap_err().to_string(),
            "fetched block total difficulty exceeds wire u128"
        );
    }

    #[tokio::test]
    async fn fetched_unknown_parent_recovers_then_revalidates_target_before_propagation() {
        for (case, (target_status, fcu_valid)) in [
            (PayloadStatusEnum::Valid, true),
            (PayloadStatusEnum::Valid, false),
            (PayloadStatusEnum::Syncing, true),
            (PayloadStatusEnum::Invalid { validation_error: "target invalid".into() }, true),
        ]
        .into_iter()
        .enumerate()
        {
            let mut provider = MockProvider::new();
            let common = Header { number: 10, ..Default::default() };
            provider.insert(common.clone(), U256::from(100));
            let parent = BscBlock {
                header: Header {
                    number: 11,
                    parent_hash: common.hash_slow(),
                    difficulty: U256::from(1),
                    ..Default::default()
                },
                ..Default::default()
            };
            let head = BscBlock {
                header: Header {
                    number: 12,
                    extra_data: vec![u8::try_from(case).unwrap()].into(),
                    parent_hash: parent.header.hash_slow(),
                    difficulty: U256::from(1),
                    ..Default::default()
                },
                ..Default::default()
            };
            let (mut service, mut engine, mut events) = fetched_fixture(provider);
            let shared_headers = service.forkchoice_engine.provider.concurrent_headers.clone();
            let peer = PeerId::random();
            let recovery_peer = PeerId::random();
            let (tx, mut ranges) = mpsc::unbounded_channel();
            crate::node::network::bsc_protocol::registry::register_peer(recovery_peer, tx, 2);
            crate::node::network::bsc_protocol::registry::record_announcer(
                head.header.hash_slow(),
                recovery_peer,
            );
            crate::node::network::bsc_protocol::registry::record_announcer(
                parent.header.hash_slow(),
                recovery_peer,
            );
            assert!(!crate::node::network::bsc_protocol::registry::is_v2_peer(peer));
            let message = NewBlockMessage {
                hash: head.header.hash_slow(),
                block: Arc::new(BscNewBlock(NewBlock { block: head.clone(), td: U128::ZERO })),
            };
            service.on_new_block_inner(message, peer, true);
            assert!(events.try_recv().is_err(), "unknown TD must not be propagated");
            let mut cx = Context::from_waker(futures::task::noop_waker_ref());
            let mut payloads = Vec::new();
            let mut result = None;
            for _ in 0..32 {
                if let Poll::Ready(Some(outcome)) = service.pending_imports.poll_next_unpin(&mut cx)
                {
                    result = Some(outcome.outcome);
                    break;
                }
                if let Ok(
                    crate::node::network::bsc_protocol::stream::BscCommand::GetBlocksByRange(
                        request,
                        response,
                    ),
                ) = ranges.try_recv()
                {
                    assert_eq!(
                        (request.start_block_height, request.start_block_hash, request.count),
                        (11, parent.header.hash_slow(), 1)
                    );
                    response
                        .send(Ok(crate::node::network::blocks_by_range::BlocksByRangePacket {
                            request_id: request.request_id,
                            blocks: vec![parent.clone()],
                        }))
                        .unwrap();
                }
                if let Ok(message) = engine.try_recv() {
                    match message {
                        BeaconEngineMessage::NewPayload { payload, tx, .. } => {
                            payloads.push(payload.block.header.number);
                            let status = match payloads.len() {
                                1 => PayloadStatusEnum::Syncing,
                                2 => PayloadStatusEnum::Valid,
                                3 => target_status.clone(),
                                _ => panic!("unexpected duplicate import"),
                            };
                            tx.send(Ok(PayloadStatus::new(status, None))).unwrap();
                        }
                        BeaconEngineMessage::ForkchoiceUpdated { state, tx, .. } => {
                            if payloads.len() < 3 {
                                assert_eq!(state.head_block_hash, parent.header.hash_slow());
                                // Simulate canonicalization making the recovered parent
                                // available to the same provider that serves ETH headers.
                                shared_headers
                                    .write()
                                    .insert(parent.header.hash_slow(), parent.header.clone());
                            }
                            let status = if payloads.len() == 3 && !fcu_valid {
                                PayloadStatusEnum::Invalid {
                                    validation_error: "target FCU rejected".into(),
                                }
                            } else {
                                PayloadStatusEnum::Valid
                            };
                            tx.send(Ok(OnForkChoiceUpdated::valid(PayloadStatus::new(
                                status, None,
                            ))))
                            .unwrap();
                        }
                        _ => panic!("unexpected engine message"),
                    }
                }
                assert!(events.try_recv().is_err());
            }
            assert_eq!(payloads, vec![12, 11, 12]);
            if matches!(target_status, PayloadStatusEnum::Valid) && fcu_valid {
                let Some(Some(outcome)) = result else { panic!("verified head outcome expected") };
                let Ok(BlockValidation::ValidHeader { block }) = &outcome.result else {
                    panic!("verified full-block outcome expected")
                };
                assert_eq!(block.hash, head.header.hash_slow());
                assert_eq!(block.block.0.td, U128::from(102));
                let hash = head.header.hash_slow();
                service.pending_imports.push(Box::pin(async move {
                    RemoteImportResult { hash, outcome: Some(outcome) }
                }));
                assert!(Pin::new(&mut service).poll(&mut cx).is_pending());
                let BlockImportEvent::Announcement(BlockValidation::ValidHeader { block }) =
                    events.try_recv().unwrap()
                else {
                    panic!("full propagation expected")
                };
                assert_eq!(block.block.0.td, U128::from(102));
                let BlockImportEvent::Outcome(Outcome {
                    result: Ok(BlockValidation::ValidBlock { block }),
                    ..
                }) = events.try_recv().unwrap()
                else {
                    panic!("hash propagation expected")
                };
                assert_eq!(block.block.0.td, U128::from(102));
                assert!(events.try_recv().is_err());
            } else {
                assert!(matches!(result, Some(None)));
                assert!(service.failed_heads.is_cooling(&head.header.hash_slow()));
            }
            assert!(service.recovery_gate.try_acquire(13).is_ok());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn full_new_block_rechecks_target_while_parent_range_is_stalled() {
        for newer_head in [false, true] {
            let mut provider = MockProvider::new();
            let common = Header { number: 10, difficulty: U256::from(1), ..Default::default() };
            provider.insert(common.clone(), U256::from(100));
            let parent = BscBlock {
                header: Header {
                    number: 11,
                    parent_hash: common.hash_slow(),
                    difficulty: U256::from(1),
                    ..Default::default()
                },
                ..Default::default()
            };
            let head = BscBlock {
                header: Header {
                    number: 12,
                    parent_hash: parent.header.hash_slow(),
                    difficulty: U256::from(1),
                    ..Default::default()
                },
                ..Default::default()
            };
            let hash = head.header.hash_slow();
            provider.td_by_hash.insert(parent.header.hash_slow(), U256::from(101));
            let better = Header {
                number: 13,
                difficulty: U256::from(100),
                extra_data: vec![0x7f].into(),
                ..Default::default()
            };
            if newer_head {
                provider.insert(better.clone(), U256::from(200));
            }
            let shared_headers = provider.concurrent_headers.clone();
            let shared_canonical = provider.concurrent_canonical.clone();
            let (mut service, mut engine, mut events) = fetched_fixture(provider);
            service.sync_config = crate::node::network::block_import::config::BlockSyncArgs {
                quick_timeout: std::time::Duration::from_secs(5),
                ..Default::default()
            }
            .into_config()
            .unwrap();
            let peer = PeerId::random();
            let (tx, mut ranges) = mpsc::unbounded_channel();
            crate::node::network::bsc_protocol::registry::register_peer(peer, tx, 2);
            crate::node::network::bsc_protocol::registry::record_announcer(
                parent.header.hash_slow(),
                peer,
            );
            let message = |block: BscBlock| NewBlockMessage {
                hash: block.header.hash_slow(),
                block: Arc::new(BscNewBlock(NewBlock { block, td: U128::ZERO })),
            };
            service.on_new_block_inner(message(head.clone()), peer, false);
            let mut cx = Context::from_waker(futures::task::noop_waker_ref());
            assert!(Pin::new(&mut service).poll(&mut cx).is_pending());
            let BeaconEngineMessage::NewPayload { tx, .. } = engine.try_recv().unwrap() else {
                panic!("original target")
            };
            tx.send(Ok(PayloadStatus::new(PayloadStatusEnum::Syncing, None))).unwrap();
            assert!(Pin::new(&mut service).poll(&mut cx).is_pending());
            let crate::node::network::bsc_protocol::stream::BscCommand::GetBlocksByRange(
                _,
                stalled,
            ) = ranges.try_recv().unwrap()
            else {
                panic!("stalled ancestor request")
            };
            let mut stalled = Some(stalled);
            // Header sync alone does not authorize any target FCU.
            shared_headers.write().insert(parent.header.hash_slow(), parent.header.clone());
            tokio::time::advance(std::time::Duration::from_millis(50)).await;
            assert!(Pin::new(&mut service).poll(&mut cx).is_pending());
            let BeaconEngineMessage::NewPayload { tx: probe, .. } = engine.try_recv().unwrap()
            else {
                panic!("target execution recheck")
            };
            tokio::time::advance(std::time::Duration::from_millis(500)).await;
            assert!(Pin::new(&mut service).poll(&mut cx).is_pending());
            assert!(engine.try_recv().is_err(), "a busy Engine must not accumulate target probes");
            probe.send(Ok(PayloadStatus::new(PayloadStatusEnum::Syncing, None))).unwrap();
            assert!(Pin::new(&mut service).poll(&mut cx).is_pending());
            if let Ok(message) = engine.try_recv() {
                let BeaconEngineMessage::NewPayload { tx, .. } = message else {
                    panic!("cached parent header must not produce FCU")
                };
                tx.send(Ok(PayloadStatus::new(PayloadStatusEnum::Syncing, None))).unwrap();
                assert!(Pin::new(&mut service).poll(&mut cx).is_pending());
            }
            assert!(engine.try_recv().is_err());
            assert!(!stalled.as_ref().unwrap().is_closed());
            // A separate complete NewBlock entrance executes the missing parent.
            let parent_import = service.new_payload_inner(message(parent.clone()), peer, false);
            tokio::pin!(parent_import);
            assert!(parent_import.as_mut().poll(&mut cx).is_pending());
            let BeaconEngineMessage::NewPayload { payload, tx, .. } = engine.try_recv().unwrap()
            else {
                panic!("independent parent payload")
            };
            assert_eq!(payload.block.header.number, 11);
            tx.send(Ok(PayloadStatus::new(PayloadStatusEnum::Valid, None))).unwrap();
            shared_canonical.write().insert(11, parent.header.hash_slow());
            if newer_head {
                // Error arrival racing with local progress must not mark the head failed.
                stalled.take().unwrap().send(Err("range timed out concurrently".into())).unwrap();
            }
            assert!(parent_import.as_mut().poll(&mut cx).is_pending());
            let BeaconEngineMessage::ForkchoiceUpdated { tx, .. } = engine.try_recv().unwrap()
            else {
                panic!("parent forkchoice")
            };
            tx.send(Ok(OnForkChoiceUpdated::valid(PayloadStatus::new(
                PayloadStatusEnum::Valid,
                None,
            ))))
            .unwrap();
            assert!(parent_import.as_mut().poll(&mut cx).is_ready());
            tokio::time::advance(std::time::Duration::from_millis(50)).await;
            let mut target_valid = false;
            let mut target_fcu = false;
            for _ in 0..16 {
                assert!(Pin::new(&mut service).poll(&mut cx).is_pending());
                match engine.try_recv() {
                    Ok(BeaconEngineMessage::NewPayload { payload, tx, .. }) => {
                        assert_eq!(payload.block.header.number, 12);
                        target_valid = true;
                        tx.send(Ok(PayloadStatus::new(PayloadStatusEnum::Valid, None))).unwrap();
                    }
                    Ok(BeaconEngineMessage::ForkchoiceUpdated { state, tx, .. }) => {
                        if state.head_block_hash
                            == (if newer_head { better.hash_slow() } else { hash })
                        {
                            if target_valid {
                                target_fcu = true;
                            }
                        } else {
                            assert_eq!(
                                state.head_block_hash,
                                parent.header.hash_slow(),
                                "recovery may still finish a parent FCU already in flight"
                            );
                            assert!(
                                !newer_head,
                                "parent recovery must also preserve a better current head"
                            );
                        }
                        tx.send(Ok(OnForkChoiceUpdated::valid(PayloadStatus::new(
                            PayloadStatusEnum::Valid,
                            None,
                        ))))
                        .unwrap();
                    }
                    Ok(_) => panic!("unexpected engine message"),
                    Err(_) if service.pending_imports.is_empty() => break,
                    Err(_) => {}
                }
            }
            assert!(
                target_valid && target_fcu,
                "target must be revalidated and considered before the 5s stale request expires"
            );
            if let Some(stalled) = stalled {
                assert!(stalled.is_closed());
                assert!(stalled.send(Err("late response".into())).is_err());
            }
            assert!(service.pending_imports.is_empty());
            assert!(!service.queued_blocks.contains(&hash));
            assert!(!service.recovering_heads.lock().contains(&hash));
            assert!(!service.failed_heads.is_cooling(&hash));
            assert!(service.recovery_gate.try_acquire(13).is_ok());
            while events.try_recv().is_ok() {}
        }
    }

    #[tokio::test(start_paused = true)]
    async fn full_new_block_timeout_rechecks_execution_and_releases_dedup() {
        let (mut service, mut engine, _events) = fetched_fixture(MockProvider::new());
        service.sync_config = crate::node::network::block_import::config::BlockSyncArgs {
            recovery_timeout: std::time::Duration::from_millis(100),
            head_timeout: std::time::Duration::from_millis(100),
            quick_timeout: std::time::Duration::from_millis(50),
            eth_fallback_delay: std::time::Duration::from_millis(20),
            recovery_peer_attempts: 1,
            ..Default::default()
        }
        .into_config()
        .unwrap();
        let peer = PeerId::random();
        let (tx, mut ranges) = mpsc::unbounded_channel();
        crate::node::network::bsc_protocol::registry::register_peer(peer, tx, 2);
        let block = BscBlock {
            header: Header { number: 2, parent_hash: B256::random(), ..Default::default() },
            ..Default::default()
        };
        crate::node::network::bsc_protocol::registry::record_announcer(
            block.header.parent_hash,
            peer,
        );
        let hash = block.header.hash_slow();
        let message = NewBlockMessage {
            hash,
            block: Arc::new(BscNewBlock(NewBlock { block, td: U128::ZERO })),
        };
        service.on_new_block_inner(message.clone(), peer, false);
        service.on_new_block_inner(message.clone(), peer, false);
        assert_eq!(service.pending_imports.len(), 1);
        let mut cx = Context::from_waker(futures::task::noop_waker_ref());
        assert!(Pin::new(&mut service).poll(&mut cx).is_pending());
        let BeaconEngineMessage::NewPayload { tx, .. } = engine.try_recv().unwrap() else {
            panic!("initial target")
        };
        tx.send(Ok(PayloadStatus::new(PayloadStatusEnum::Syncing, None))).unwrap();
        assert!(Pin::new(&mut service).poll(&mut cx).is_pending());
        let crate::node::network::bsc_protocol::stream::BscCommand::GetBlocksByRange(_, stalled) =
            ranges.try_recv().unwrap()
        else {
            panic!("ancestor range")
        };
        tokio::time::advance(std::time::Duration::from_millis(100)).await;
        assert!(Pin::new(&mut service).poll(&mut cx).is_pending());
        let BeaconEngineMessage::NewPayload { tx, .. } = engine.try_recv().unwrap() else {
            panic!("final target recheck before accepting timeout")
        };
        tx.send(Ok(PayloadStatus::new(PayloadStatusEnum::Syncing, None))).unwrap();
        assert!(Pin::new(&mut service).poll(&mut cx).is_pending());
        assert!(stalled.is_closed());
        assert!(service.pending_imports.is_empty());
        assert!(!service.queued_blocks.contains(&hash));
        assert!(!service.recovering_heads.lock().contains(&hash));
        assert!(service.failed_heads.is_cooling(&hash));
        assert!(service.recovery_gate.try_acquire(3).is_ok());
        assert!(
            engine.try_recv().is_err(),
            "a genuinely missing parent must not produce target FCU"
        );
        // Model cooldown expiry: no permanent queue/dedup marker prevents a retry.
        service.failed_heads =
            crate::node::network::block_import::fork_recover::FailedHeadsCooler::new(
                16,
                std::time::Duration::from_secs(30),
            );
        service.on_new_block_inner(message, peer, false);
        assert_eq!(service.pending_imports.len(), 1);
        assert!(Pin::new(&mut service).poll(&mut cx).is_pending());
        assert!(matches!(engine.try_recv(), Ok(BeaconEngineMessage::NewPayload { .. })));
        let gate = service.recovery_gate.clone();
        drop(service);
        assert!(gate.try_acquire(3).is_ok());
    }

    #[tokio::test]
    async fn dropping_fetched_parent_recovery_releases_slot_and_request() {
        let (mut service, mut engine, mut events) = fetched_fixture(MockProvider::new());
        let peer = PeerId::random();
        let (tx, mut ranges) = mpsc::unbounded_channel();
        crate::node::network::bsc_protocol::registry::register_peer(peer, tx, 2);
        let block = BscBlock {
            header: Header { number: 2, parent_hash: B256::random(), ..Default::default() },
            ..Default::default()
        };
        crate::node::network::bsc_protocol::registry::record_announcer(
            block.header.parent_hash,
            peer,
        );
        let hash = block.header.hash_slow();
        service.on_new_block_inner(
            NewBlockMessage {
                hash,
                block: Arc::new(BscNewBlock(NewBlock { block, td: U128::ZERO })),
            },
            peer,
            true,
        );
        assert!(events.try_recv().is_err());
        let mut cx = Context::from_waker(futures::task::noop_waker_ref());
        assert!(service.pending_imports.poll_next_unpin(&mut cx).is_pending());
        let BeaconEngineMessage::NewPayload { tx, .. } = engine.try_recv().unwrap() else {
            panic!("new payload expected")
        };
        tx.send(Ok(PayloadStatus::new(PayloadStatusEnum::Syncing, None))).unwrap();
        assert!(service.pending_imports.poll_next_unpin(&mut cx).is_pending());
        let crate::node::network::bsc_protocol::stream::BscCommand::GetBlocksByRange(_, response) =
            ranges.try_recv().unwrap()
        else {
            panic!("parent recovery expected")
        };
        let gate = service.recovery_gate.clone();
        assert!(matches!(gate.try_acquire(3), Err(2)));
        drop(service);
        assert!(response.is_closed());
        assert!(gate.try_acquire(3).is_ok());
    }

    #[tokio::test]
    async fn known_and_stale_hashes_do_not_claim_recovery_or_import_slots() {
        let mut provider = MockProvider::new();
        provider.insert(Header { number: 100, ..Default::default() }, U256::from(100));
        let known = Header { number: 133, ..Default::default() };
        let known_hash = known.hash_slow();
        provider.headers_by_hash.insert(known_hash, known.clone());
        let (mut service, mut engine, mut events) = fetched_fixture(provider);
        service.on_new_block_hashes(
            NewBlockHashes(vec![
                BlockHashNumber { hash: known_hash, number: 133 },
                BlockHashNumber { hash: B256::random(), number: 88 },
            ]),
            PeerId::random(),
        );
        assert!(service.recovery_gate.try_acquire(134).is_ok());
        for header in [known, Header { number: 88, ..Default::default() }] {
            let block = BscBlock { header, ..Default::default() };
            let message = NewBlockMessage {
                hash: block.header.hash_slow(),
                block: Arc::new(BscNewBlock(NewBlock { block, td: U128::ZERO })),
            };
            service.on_new_block_inner(message, PeerId::random(), true);
        }
        assert!(service.pending_imports.is_empty());
        assert!(engine.try_recv().is_err());
        assert!(events.try_recv().is_err());
    }

    #[tokio::test]
    async fn full_block_arrival_cancels_fetch_before_a_second_import() {
        let (mut service, mut engine, _events) = fetched_fixture(MockProvider::new());
        let block =
            BscBlock { header: Header { number: 1, ..Default::default() }, ..Default::default() };
        let hash = block.header.hash_slow();
        let (sender, receiver) = tokio::sync::oneshot::channel::<(BscBlock, PeerId)>();
        service.head_fetches.insert(hash, async { Ok(receiver.await?) });
        let mut cx = Context::from_waker(futures::task::noop_waker_ref());
        assert!(service.head_fetches.poll(&mut cx).is_pending());
        let message = NewBlockMessage {
            hash,
            block: Arc::new(BscNewBlock(NewBlock { block, td: U128::from(1) })),
        };
        service.on_new_block(message.clone(), PeerId::random());
        assert!(matches!(service.head_fetches.poll(&mut cx), Poll::Ready(None)));
        assert!(sender.is_closed());
        service.on_new_block(message, PeerId::random());
        assert_eq!(service.pending_imports.len(), 1);
        assert!(service.pending_imports.poll_next_unpin(&mut cx).is_pending());
        assert!(matches!(engine.try_recv().unwrap(), BeaconEngineMessage::NewPayload { .. }));
        assert!(engine.try_recv().is_err());
    }

    #[tokio::test]
    async fn fetched_side_parent_td_uses_primed_hash_cache_not_canonical_height_td() {
        let common = Header { number: 10, ..Default::default() };
        let canonical = Header {
            number: 11,
            parent_hash: common.hash_slow(),
            difficulty: U256::from(1000),
            ..Default::default()
        };
        let side = Header {
            number: 11,
            parent_hash: common.hash_slow(),
            difficulty: U256::from(1),
            ..Default::default()
        };
        let mut provider = MockProvider::new();
        provider.insert(common, U256::from(100));
        provider.insert(canonical.clone(), U256::from(1100));
        provider.headers_by_hash.insert(side.hash_slow(), side.clone());
        // Model ConsistentProvider's hash -> number -> canonical TD lookup.
        provider.td_by_hash.insert(side.hash_slow(), U256::from(1100));
        let (mut service, mut engine, mut events) = fetched_fixture(provider);
        assert_eq!(service.forkchoice_engine.header_td_by_hash(side.hash_slow()).unwrap(), None);
        service.forkchoice_engine.prime_fork_chain_tds([&side]).unwrap();
        assert_eq!(
            service.forkchoice_engine.header_td_by_hash(side.hash_slow()).unwrap(),
            Some(U256::from(101))
        );
        let head = BscBlock {
            header: Header {
                number: 12,
                parent_hash: side.hash_slow(),
                difficulty: U256::from(1),
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(
            fetched_block_td(&service.forkchoice_engine, &head.header).unwrap(),
            Some(U128::from(102))
        );
        let hash = head.header.hash_slow();
        service.on_new_block_inner(
            NewBlockMessage {
                hash,
                block: Arc::new(BscNewBlock(NewBlock { block: head, td: U128::ZERO })),
            },
            PeerId::random(),
            true,
        );
        assert!(events.try_recv().is_err());
        let mut cx = Context::from_waker(futures::task::noop_waker_ref());
        assert!(Pin::new(&mut service).poll(&mut cx).is_pending());
        let BeaconEngineMessage::NewPayload { tx, .. } = engine.try_recv().unwrap() else {
            panic!("new payload expected")
        };
        tx.send(Ok(PayloadStatus::new(PayloadStatusEnum::Valid, None))).unwrap();
        for _ in 0..4 {
            assert!(Pin::new(&mut service).poll(&mut cx).is_pending());
            if let Ok(BeaconEngineMessage::ForkchoiceUpdated { tx, .. }) = engine.try_recv() {
                tx.send(Ok(OnForkChoiceUpdated::valid(PayloadStatus::new(
                    PayloadStatusEnum::Valid,
                    None,
                ))))
                .unwrap();
            }
        }
        let BlockImportEvent::Announcement(BlockValidation::ValidHeader { block }) =
            events.try_recv().unwrap()
        else {
            panic!("full side-branch propagation expected")
        };
        assert_eq!(block.block.0.td, U128::from(102));
        let BlockImportEvent::Outcome(Outcome {
            result: Ok(BlockValidation::ValidBlock { block }),
            ..
        }) = events.try_recv().unwrap()
        else {
            panic!("hash side-branch propagation expected")
        };
        assert_eq!(block.block.0.td, U128::from(102));
        assert!(events.try_recv().is_err());
    }

    #[tokio::test]
    async fn fetched_eth_only_parent_gap_stays_inflight_without_unqualified_bsc_requests() {
        let (mut service, mut engine, mut events) = fetched_fixture(MockProvider::new());
        let peer = PeerId::random();
        let block = BscBlock {
            header: Header { number: 2, parent_hash: B256::random(), ..Default::default() },
            ..Default::default()
        };
        let hash = block.header.hash_slow();
        let parent_hash = block.header.parent_hash;
        let message = NewBlockMessage {
            hash,
            block: Arc::new(BscNewBlock(NewBlock { block, td: U128::ZERO })),
        };
        // A connected bsc/2 peer that never announced this head must not be queried.
        let unrelated = PeerId::random();
        let (unrelated_tx, mut unrelated_rx) = mpsc::unbounded_channel();
        crate::node::network::bsc_protocol::registry::register_peer(unrelated, unrelated_tx, 2);
        let closed = PeerId::random();
        let (closed_tx, closed_rx) = mpsc::unbounded_channel();
        crate::node::network::bsc_protocol::registry::register_peer(closed, closed_tx, 2);
        drop(closed_rx);
        crate::node::network::bsc_protocol::registry::record_announcer(hash, closed);
        service.on_new_block_inner(message.clone(), peer, true);
        let mut cx = Context::from_waker(futures::task::noop_waker_ref());
        assert!(Pin::new(&mut service).poll(&mut cx).is_pending());
        let BeaconEngineMessage::NewPayload { tx, .. } = engine.try_recv().unwrap() else {
            panic!("new payload expected")
        };
        // A second complete delivery while Engine is pending remains deduplicated.
        service.on_new_block_inner(message.clone(), peer, true);
        assert_eq!(service.pending_imports.len(), 1);
        assert!(service.queued_blocks.contains(&hash));
        tx.send(Ok(PayloadStatus::new(PayloadStatusEnum::Syncing, None))).unwrap();
        assert!(Pin::new(&mut service).poll(&mut cx).is_pending());
        assert_eq!(service.pending_imports.len(), 1, "ETH-only parent recovery now remains active");
        assert!(service.queued_blocks.contains(&hash));
        assert!(!service.failed_heads.is_cooling(&hash));
        assert!(unrelated_rx.try_recv().is_err());
        assert!(events.try_recv().is_err());
        let gate = service.recovery_gate.clone();
        drop(service);
        assert!(gate.try_acquire(3).is_ok());
    }

    struct WakeCounter(std::sync::atomic::AtomicUsize);

    impl futures::task::ArcWake for WakeCounter {
        fn wake_by_ref(this: &Arc<Self>) {
            this.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[tokio::test]
    async fn configured_head_capacity_above_poll_batch_reschedules_remaining_completions() {
        let config = crate::node::network::block_import::config::BlockSyncArgs {
            max_head_fetches: 48,
            ..Default::default()
        }
        .into_config()
        .unwrap();
        let (service, _engine, _events) = fetched_fixture(MockProvider::new());
        let mut service = service.with_sync_config(config);
        for n in 0..33 {
            service.head_fetches.insert(B256::with_last_byte(n), async {
                Err(eyre::eyre!("simulated failed download"))
            });
        }
        let counter = Arc::new(WakeCounter(std::sync::atomic::AtomicUsize::new(0)));
        let waker = futures::task::waker(counter.clone());
        let mut cx = Context::from_waker(&waker);
        assert!(Pin::new(&mut service).poll(&mut cx).is_pending());
        assert_eq!(service.head_fetches.pending_len(), 1);
        assert!(counter.0.load(Ordering::Relaxed) > 0);
        assert!(Pin::new(&mut service).poll(&mut cx).is_pending());
        assert_eq!(service.head_fetches.pending_len(), 0);
        assert!(service.pending_imports.is_empty());
    }

    #[tokio::test]
    async fn configured_recovery_depth_is_also_the_pipeline_handoff_threshold() {
        let mut provider = MockProvider::new();
        provider.insert(Header { number: 10, ..Default::default() }, U256::from(100));
        let config = crate::node::network::block_import::config::BlockSyncArgs {
            max_depth: 2,
            near_head_distance: 1,
            stale_distance: 2,
            ..Default::default()
        }
        .into_config()
        .unwrap();
        let (service, mut engine, _events) = fetched_fixture(provider);
        let mut service = service.with_sync_config(config);
        let hash = B256::random();
        service.on_new_block_hashes(
            NewBlockHashes(vec![BlockHashNumber { hash, number: 13 }]),
            PeerId::random(),
        );
        let mut cx = Context::from_waker(futures::task::noop_waker_ref());
        assert!(Pin::new(&mut service).poll(&mut cx).is_pending());
        assert!(Pin::new(&mut service).poll(&mut cx).is_pending());
        let BeaconEngineMessage::ForkchoiceUpdated { state, .. } = engine.try_recv().unwrap()
        else {
            panic!("configured pipeline handoff expected")
        };
        assert_eq!(state.head_block_hash, hash);
        assert!(service.pending_imports.is_empty());
        assert!(obsolete_announcement(7, 10, 0, 2));
        assert!(!obsolete_announcement(7, 10, 0, 3));
    }
}
