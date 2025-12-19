use super::handle::ImportHandle;
use crate::{
    chainspec::BscChainSpec,
    consensus::{parlia::vote_pool, ParliaConsensusErr},
    node::{
        consensus::BscForkChoiceEngine, engine::BscBuiltPayload,
        engine_api::payload::BscPayloadTypes, evm::util::insert_header_to_cache,
        network::BscNewBlock,
    },
    BscBlock, BscBlockBody,
};
use alloy_consensus::{BlockBody, Header};
use alloy_eips::BlockNumberOrTag;
use alloy_primitives::{B256, U128};
use alloy_rpc_types::engine::{ForkchoiceState, PayloadStatusEnum};
use futures::{future::Either, stream::FuturesUnordered, StreamExt};
use parking_lot::RwLock;
use reth::consensus::HeaderValidator;
use reth::network::cache::LruCache;
use reth_engine_primitives::{ConsensusEngineHandle, EngineTypes};
use reth_eth_wire::{BlockHashNumber, GetBlockHeaders, NewBlock};
use reth_eth_wire_types::broadcast::NewBlockHashes;
use reth_network::{
    import::{BlockImportError, BlockImportEvent, BlockImportOutcome, BlockValidation},
    message::{NewBlockMessage, PeerMessage},
};
use reth_network::{
    message::{BlockRequest, PeerResponse},
    FetchClient, NetworkHandle,
};
use reth_network_api::PeerId;
use reth_node_ethereum::EthEngineTypes;
use reth_payload_builder_primitives::Events;
use reth_payload_primitives::{BuiltPayload, EngineApiMessageVersion, PayloadTypes};
use reth_primitives::NodePrimitives;
use reth_primitives_traits::{AlloyBlockHeader, Block};
use reth_provider::{BlockHashReader, BlockNumReader, BlockReaderIdExt, HeaderProvider};
use schnellru::{ByLength, LruMap};
use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

/// Network message containing a new block
pub(crate) type BlockMsg = NewBlockMessage<BscNewBlock>;

/// Import outcome for a block
pub(crate) type Outcome = BlockImportOutcome<BscNewBlock>;

/// Import event for a block
pub(crate) type ImportEvent = BlockImportEvent<BscNewBlock>;

/// Future that processes a block import and returns its outcome
type ImportFut = Pin<Box<dyn Future<Output = Option<Outcome>> + Send + Sync>>;

/// Channel message type for incoming blocks
pub(crate) type IncomingBlock = (BlockMsg, PeerId);

/// Channel message type for incoming mined blocks
pub(crate) type IncomingMinedBlock = (BscBuiltPayload, BlockMsg);

/// Channel message type for incoming block hashes
pub(crate) type IncomingHashes = (NewBlockHashes, PeerId);

/// Size of the LRU cache for processed blocks.
const LRU_PROCESSED_BLOCKS_SIZE: u32 = 100;

/// Cooldown duration for downloading block hashes to avoid re-downloading the same block.
const DOWNLOAD_COOLDOWN_DURATION_MS: u128 = 200;

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
    /// Receive the new block from the network
    from_network: UnboundedReceiver<IncomingBlock>,
    /// Receive the new block from the network
    from_builder: UnboundedReceiver<IncomingMinedBlock>,
    /// Receive block hashes from the network for downloading
    from_hashes: UnboundedReceiver<IncomingHashes>,
    /// Send the event of the import to the network
    to_network: UnboundedSender<ImportEvent>,
    /// Pending block imports.
    pending_imports: FuturesUnordered<ImportFut>,
    /// Cache of processed block hashes to avoid reprocessing the same block.
    processed_blocks: LruCache<B256>,
    /// Cache of queued block hashes to avoid processing the same block.
    queued_blocks: LruCache<B256>,
    /// Cache of downloading block hashes to avoid re-downloading the same block.
    downloading_blocks: LruMap<B256, u128, ByLength>,
}

impl<Provider> ImportService<Provider>
where
    Provider: BlockNumReader + HeaderProvider<Header = Header> + Clone + 'static,
{
    /// Create a new block import service
    pub fn new(
        provider: Provider,
        chain_spec: Arc<BscChainSpec>,
        engine: ConsensusEngineHandle<BscPayloadTypes>,
        from_network: UnboundedReceiver<IncomingBlock>,
        from_builder: UnboundedReceiver<IncomingMinedBlock>,
        from_hashes: UnboundedReceiver<IncomingHashes>,
        to_network: UnboundedSender<ImportEvent>,
    ) -> Self {
        let forkchoice_engine = BscForkChoiceEngine::new(provider, engine.clone(), chain_spec);

        if let Err(e) = crate::shared::set_fork_choice_engine(forkchoice_engine.clone()) {
            tracing::warn!(target: "bsc::block_import", error = %e, "Fork choice engine already initialized; skipping global set");
        }

        Self {
            engine,
            forkchoice_engine,
            from_network,
            from_builder,
            from_hashes,
            to_network,
            pending_imports: FuturesUnordered::new(),
            processed_blocks: LruCache::new(LRU_PROCESSED_BLOCKS_SIZE),
            queued_blocks: LruCache::new(LRU_PROCESSED_BLOCKS_SIZE),
            downloading_blocks: LruMap::new(ByLength::new(LRU_PROCESSED_BLOCKS_SIZE)),
        }
    }

    /// Process a new payload and return the outcome
    fn new_payload(&self, block: BlockMsg, peer_id: PeerId) -> ImportFut {
        let engine = self.engine.clone();
        let forkchoice_engine = self.forkchoice_engine.clone();

        tracing::debug!(target: "bsc::block_import", "New payload: block = ({:?}, {:?}), peer_id = {:?}", block.block.0.block.header.number, block.block.0.block.header.hash_slow(), peer_id);
        Box::pin(async move {
            let sealed_block = block.block.0.block.clone().seal();
            let header = sealed_block.header().clone();
            let payload = BscPayloadTypes::block_to_payload(sealed_block);
            match engine.new_payload(payload).await {
                Ok(payload_status) => match payload_status.status {
                    PayloadStatusEnum::Valid => {
                        tracing::debug!(target: "bsc::block_import", "New payload is valid, block = {:?}, peer_id = {:?}", block, peer_id);
                        // handle fork choice update with valid payload
                        if let Err(e) = forkchoice_engine.update_forkchoice(&header).await {
                            tracing::warn!(target: "bsc::block_import", "Failed to update fork choice: {}", e);
                        } else {
                            tracing::debug!(target: "bsc::block_import", "Succeed to update fork choice for new payload: number = {:?}, hash = {:?}", header.number, header.hash_slow());
                        }
                        Outcome { peer: peer_id, result: Ok(BlockValidation::ValidBlock { block }) }
                            .into()
                    }
                    PayloadStatusEnum::Invalid { validation_error } => Outcome {
                        peer: peer_id,
                        result: Err(BlockImportError::Other(validation_error.into())),
                    }
                    .into(),
                    PayloadStatusEnum::Syncing => {
                        // When new_payload returns Syncing status, we need to manually trigger FCU
                        // to avoid the engine-tree being stuck in syncing state without any driver.
                        // By calling FCU, we inform the engine about the new head block hash,
                        // which can help trigger additional sync/download activities in the engine-tree.
                        let block_hash = header.hash_slow();
                        let block_number = header.number;
                        tracing::debug!(
                            target: "bsc::block_import",
                            block_hash = %block_hash,
                            block_number = block_number,
                            "New payload returned Syncing status - attempting fork choice update"
                        );

                        // Direct FCU call to help sync progress
                        let forkchoice_state = alloy_rpc_types::engine::ForkchoiceState {
                            head_block_hash: block_hash,
                            safe_block_hash: alloy_primitives::B256::ZERO,
                            finalized_block_hash: alloy_primitives::B256::ZERO,
                        };
                        match engine
                            .fork_choice_updated(
                                forkchoice_state,
                                None,
                                reth_payload_primitives::EngineApiMessageVersion::V1,
                            )
                            .await
                        {
                            Ok(result) => {
                                tracing::debug!(
                                    target: "bsc::block_import",
                                    block_hash = %block_hash,
                                    block_number = block_number,
                                    status = ?result.payload_status.status,
                                    "FCU result for syncing block"
                                );
                            }
                            Err(err) => {
                                tracing::trace!(
                                    target: "bsc::block_import",
                                    block_hash = %block_hash,
                                    block_number = block_number,
                                    error = %err,
                                    "Failed to update fork choice for syncing block"
                                );
                            }
                        }
                        None
                    }
                    _ => None,
                },
                Err(err) => None,
            }
        })
    }

    /// Add a new block import task to the pending imports
    fn on_new_mined_block(
        &mut self,
        payload: BscBuiltPayload,
        block_msg: NewBlockMessage<BscNewBlock>,
    ) {
        let block = &block_msg.block.0.block;
        // insert header to cache
        insert_header_to_cache(block.header.clone());
        // Cache the full block body for later range responses.
        crate::shared::cache_full_block(block.clone());
        let block_hash = block_msg.hash;
        // Clone header for FCU update
        let header_for_fcu = block.header.clone();

        // send to EVN peers first
        if let Err(e) = self.transfer_to_evn_peers(block_msg.clone()) {
            tracing::warn!(target: "bsc::block_import", "Failed to transfer block to EVN peers: number = {:?}, hash = {:?}, error = {}", block.header.number, block.header.hash_slow(), e);
        }
        // Send ValidHeader announcement to trigger NewBlock diffusion from few peers
        let _ =
            self.to_network.send(BlockImportEvent::Announcement(BlockValidation::ValidHeader {
                block: block_msg.clone(),
            }));
        let _ = self
            .to_network
            .send(BlockImportEvent::Announcement(BlockValidation::ValidBlock { block: block_msg }));

        // Broadcast built payload event for fast consumers
        if let Some(tx) = crate::shared::get_payload_events_tx() {
            tracing::debug!(target: "bsc::block_import", "Sending built payload event for mined block: {:?}", block_hash);
            let _ = tx.send(Events::<BscPayloadTypes>::BuiltPayload(payload));
        } else {
            tracing::warn!(
                "Failed to send mined block due to payload events channel not initialised"
            );
        }

        // Update fork choice for the mined block
        {
            let forkchoice_engine = self.forkchoice_engine.clone();
            tokio::spawn(async move {
                tracing::debug!(target: "bsc::block_import", "Updating fork choice for mined block: number = {:?}, hash = {:?}", header_for_fcu.number, header_for_fcu.hash_slow());
                if let Err(e) = forkchoice_engine.update_forkchoice(&header_for_fcu).await {
                    tracing::warn!(target: "bsc::block_import", "Failed to update fork choice for mined block: number = {:?}, hash = {:?}, error = {}", header_for_fcu.number, header_for_fcu.hash_slow(), e);
                } else {
                    tracing::debug!(target: "bsc::block_import", "Succeed to update fork choice for mined block: number = {:?}, hash = {:?}", header_for_fcu.number, header_for_fcu.hash_slow());
                }
            });
        }
        // Cache the block hash to avoid re-processing the same block.
        self.processed_blocks.insert(block_hash);
    }

    /// Add a new block import task to the pending imports
    fn on_new_block(&mut self, block: BlockMsg, peer_id: PeerId) {
        tracing::debug!(target: "bsc::block_import", "Receiving new block from network: number = {:?}, hash = {:?}, peer = {:?}", block.block.0.block.header.number, block.hash, peer_id);
        if self.processed_blocks.contains(&block.hash) {
            tracing::trace!(target: "bsc::block_import", "Block already processed when receiving new block: number = {:?}, hash = {:?}", block.block.0.block.header.number, block.hash);
            return;
        }
        if self.queued_blocks.contains(&block.hash) {
            tracing::trace!(target: "bsc::block_import", "Block already queued when receiving new block: number = {:?}, hash = {:?}", block.block.0.block.header.number, block.hash);
            return;
        }
        self.queued_blocks.insert(block.hash);

        // send to EVN peers first
        if let Err(e) = self.transfer_to_evn_peers(block.clone()) {
            tracing::warn!(target: "bsc::block_import", "Failed to transfer block to EVN peers: number = {:?}, hash = {:?}, error = {}", block.block.0.block.header.number, block.hash, e);
        }
        // Send ValidHeader announcement to trigger NewBlock diffusion from few peers
        // TODO: add header validation later
        let _ =
            self.to_network.send(BlockImportEvent::Announcement(BlockValidation::ValidHeader {
                block: block.clone(),
            }));

        tracing::debug!(target: "bsc::block_import", "Sending new block to import service: number = {:?}, hash = {:?}", block.block.0.block.header.number, block.hash);
        let payload_fut = self.new_payload(block.clone(), peer_id);
        self.pending_imports.push(payload_fut);
    }

    /// Handle incoming block hashes by using Reth engine-tree download mechanism
    fn on_new_block_hashes(&mut self, hashes: NewBlockHashes, peer_id: PeerId) {
        let hash_numbers = hashes.0.clone();

        for hash_number in hash_numbers {
            // Skip if the block is already processed.
            if self.processed_blocks.contains(&hash_number.hash) {
                tracing::trace!(target: "bsc::block_import", "Block already processed when requesting block hashes: number = {:?}, hash = {:?}", hash_number.number, hash_number.hash);
                continue;
            }
            if self.queued_blocks.contains(&hash_number.hash) {
                tracing::trace!(target: "bsc::block_import", "Block already queued when requesting block hashes: number = {:?}, hash = {:?}", hash_number.number, hash_number.hash);
                continue;
            }

            // Check if the block is already being downloaded, if it times out, download it again.
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis();
            if let Some(last_requested) = self.downloading_blocks.get(&hash_number.hash) {
                if *last_requested + DOWNLOAD_COOLDOWN_DURATION_MS > now {
                    continue;
                }
            }

            tracing::debug!(
                target: "bsc::block_import",
                peer_id = %peer_id,
                block_hash = %hash_number.hash,
                block_number = hash_number.number,
                "Requesting block download for NewBlockHashes"
            );

            // Try quick range fetch via BSC subprotocol (mimic geth asyncFetchRangeBlocks)
            // Prefer the announcing peer; if it doesn't have bsc extension, fallback to any bsc peer.
            let start_height = hash_number.number;
            let start_hash = hash_number.hash;
            let announcing_peer = peer_id;
            // Resolve target bsc peer
            let target_peer = if crate::node::network::bsc_protocol::registry::has_registered_peer(
                announcing_peer,
            ) {
                Some(announcing_peer)
            } else {
                crate::node::network::bsc_protocol::registry::list_registered_peers()
                    .into_iter()
                    .next()
            };
            if let Some(bsc_peer) = target_peer {
                tracing::debug!(
                    target: "bsc::block_import",
                    peer_id = %bsc_peer,
                    block_hash = %start_hash,
                    block_number = start_height,
                    "Requesting block with block range for NewBlockHashes"
                );
                tokio::spawn(async move {
                    use std::time::Duration;
                    // Bump request timeout to 1000ms to accommodate slower peers
                    let req_timeout = Duration::from_millis(DOWNLOAD_COOLDOWN_DURATION_MS as u64);
                    let _ = crate::node::network::bsc_protocol::registry::batch_request_range_and_await_import(
                        bsc_peer,
                        start_height,
                        start_hash,
                        1,
                        req_timeout,
                    ).await;
                });
            }
            self.downloading_blocks.insert(hash_number.hash, now);
        }
    }

    /// Transfer the block to EVN peers if from proxied validators or validator address.
    fn transfer_to_evn_peers(&self, block: BlockMsg) -> Result<(), Box<dyn std::error::Error>> {
        let mining_config = crate::node::miner::config::get_global_mining_config().ok_or("Mining config is not set")?;
        let cfg = crate::node::network::evn::get_global_evn_config().ok_or("EVN config is not set")?;
        if !cfg.enabled {
            return Ok(());
        }
        let header_ref = &block.block.0.block.header;
        let coinbase = header_ref.beneficiary;
        // If from proxied validators or validator address, target EVN peers with ETH NewBlockHashes.
        if cfg.proxyed_validators.contains(&coinbase) || (mining_config.enabled && mining_config.validator_address.unwrap_or_default() == coinbase) {
            if let Some(net) = crate::shared::get_network_handle() {
                let peers = crate::node::network::evn_peers::snapshot();
                for (peer_id, info) in peers {
                    // Send to EVN peers or proxyed peers
                    let is_proxyed = crate::node::network::bsc_protocol::registry::is_proxyed_peer(&peer_id);
                    if info.is_evn || is_proxyed {
                        // Send full NewBlock to EVN/proxyed peers to avoid re-fetching.
                        net.send_eth_message(
                            peer_id,
                            PeerMessage::NewBlock(block.clone()),
                        );
                        tracing::debug!(target: "bsc::block_import", "Sent full NewBlock to EVN/proxyed peer: number = {:?}, hash = {:?}, peer = {:?}", block.block.0.block.header.number, block.hash, peer_id);
                    }
                }
            }
        }
        Ok(())
    }
}

impl<Provider> Future for ImportService<Provider>
where
    Provider: BlockNumReader + HeaderProvider<Header = Header> + Clone + 'static + Unpin,
{
    type Output = Result<(), Box<dyn std::error::Error>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        // Receive new blocks from network
        while let Poll::Ready(Some((block, peer_id))) = this.from_network.poll_recv(cx) {
            this.on_new_block(block, peer_id);
        }

        // Receive new mined blocks from builder
        while let Poll::Ready(Some((payload, block_msg))) = this.from_builder.poll_recv(cx) {
            this.on_new_mined_block(payload, block_msg);
        }

        // Receive new block hashes from network
        while let Poll::Ready(Some((hashes, peer_id))) = this.from_hashes.poll_recv(cx) {
            this.on_new_block_hashes(hashes, peer_id);
        }

        // Process completed imports and send events to network
        while let Poll::Ready(Some(outcome)) = this.pending_imports.poll_next_unpin(cx) {
            if let Some(outcome) = outcome {
                let mut block_hash = None;
                if let Ok(BlockValidation::ValidBlock { block }) = &outcome.result {
                    block_hash = Some(block.hash);
                    this.processed_blocks.insert(block.hash);
                    // Cache the full block body for later range responses.
                    crate::shared::cache_full_block(block.block.0.block.clone());
                    if let Err(e) = this.transfer_to_evn_peers(block.clone()) {
                        tracing::warn!(target: "bsc::block_import", "Failed to transfer block to EVN peers: number = {:?}, hash = {:?}, error = {}", block.block.0.block.header.number, block.hash, e);
                    }
                }

                // TODO: add queued blocks removal later, to avoid milicious block import, and trigger next download.
                // now, it must wait backfilling to download the correct block.
                // the verified header can drop the peer later, it cannot transfer a bad header now.
                // if let Some(block_hash) = outcome.block.hash {
                //     this.queued_blocks.remove(&block_hash);
                // }

                if let Err(e) = this.to_network.send(BlockImportEvent::Outcome(outcome)) {
                    return Poll::Ready(Err(Box::new(e)));
                }
            }
        }

        Poll::Pending
    }
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
    use reth_node_ethereum::EthEngineTypes;
    use reth_primitives::{Block, SealedHeader};
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

    #[tokio::test]
    async fn can_handle_invalid_new_payload() {
        let mut fixture = TestFixture::new(EngineResponses::invalid_new_payload()).await;
        fixture
            .assert_block_import(|outcome| {
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

    #[derive(Clone)]
    struct MockProvider {
        headers_by_number: HashMap<BlockNumber, Header>,
        headers_by_hash: HashMap<BlockHash, Header>,
        td_by_hash: HashMap<BlockHash, U256>,
        head_number: BlockNumber,
        head_hash: BlockHash,
    }

    impl MockProvider {
        fn new() -> Self {
            let headers_by_number = HashMap::new();
            let headers_by_hash = HashMap::new();
            let td_by_hash = HashMap::new();
            Self {
                headers_by_number,
                headers_by_hash,
                td_by_hash,
                head_number: 0,
                head_hash: BlockHash::ZERO,
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
    }

    impl BlockHashReader for MockProvider {
        fn block_hash(&self, number: BlockNumber) -> Result<Option<B256>, ProviderError> {
            Ok(self.headers_by_number.get(&number).map(|h| h.hash_slow()))
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

        fn header(&self, block_hash: &B256) -> Result<Option<Self::Header>, ProviderError> {
            Ok(self.headers_by_hash.get(block_hash).cloned())
        }

        fn header_by_number(&self, num: u64) -> Result<Option<Self::Header>, ProviderError> {
            Ok(self.headers_by_number.get(&num).cloned())
        }

        fn header_td(&self, hash: &B256) -> Result<Option<U256>, ProviderError> {
            Ok(self.td_by_hash.get(hash).cloned())
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
        fcu: PayloadStatusEnum,
    }

    impl EngineResponses {
        fn both_valid() -> Self {
            Self { new_payload: PayloadStatusEnum::Valid, fcu: PayloadStatusEnum::Valid }
        }

        fn invalid_new_payload() -> Self {
            Self {
                new_payload: PayloadStatusEnum::Invalid { validation_error: "test error".into() },
                fcu: PayloadStatusEnum::Valid,
            }
        }

        fn invalid_fcu() -> Self {
            Self {
                new_payload: PayloadStatusEnum::Valid,
                fcu: PayloadStatusEnum::Invalid { validation_error: "fcu error".into() },
            }
        }
    }

    /// Test fixture for block import tests
    struct TestFixture {
        handle: ImportHandle,
    }

    impl TestFixture {
        /// Create a new test fixture with the given engine responses
        async fn new(responses: EngineResponses) -> Self {
            // Use mainnet chain spec for tests; it influences only fast-finality parsing.
            let provider = MockProvider::new();
            let chain_spec = Arc::new(crate::chainspec::BscChainSpec::from(
                crate::chainspec::bsc::bsc_mainnet(),
            ));

            let (to_engine, from_engine) = mpsc::unbounded_channel();
            let engine_handle = ConsensusEngineHandle::new(to_engine);

            handle_engine_msg(from_engine, responses).await;

            let (to_import, from_network) = mpsc::unbounded_channel();
            let (to_import_mined, from_builder) = mpsc::unbounded_channel();
            let (to_hashes, from_hashes) = mpsc::unbounded_channel();
            let (to_network, import_outcome) = mpsc::unbounded_channel();

            let handle = ImportHandle::new(to_import, to_hashes, import_outcome);

            let service = ImportService::new(
                provider,
                chain_spec,
                engine_handle,
                from_network,
                from_builder,
                from_hashes,
                to_network,
            );
            tokio::spawn(Box::pin(async move {
                service.await.unwrap();
            }));

            Self { handle }
        }

        /// Run a block import test with the given event assertion
        async fn assert_block_import<F>(&mut self, assert_fn: F)
        where
            F: Fn(&BlockImportEvent<BscNewBlock>) -> bool,
        {
            let block_msg = create_test_block();
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
        NewBlockMessage { hash, block: Arc::new(new_block), td: Some(U256::from(1)) }
    }

    /// Helper function to handle engine messages with specified payload statuses
    async fn handle_engine_msg(
        mut from_engine: mpsc::UnboundedReceiver<BeaconEngineMessage<BscPayloadTypes>>,
        responses: EngineResponses,
    ) {
        tokio::spawn(Box::pin(async move {
            while let Some(message) = from_engine.recv().await {
                match message {
                    BeaconEngineMessage::NewPayload { payload: _, tx } => {
                        tx.send(Ok(PayloadStatus::new(responses.new_payload.clone(), None)))
                            .unwrap();
                    }
                    BeaconEngineMessage::ForkchoiceUpdated {
                        state: _,
                        payload_attrs: _,
                        version: _,
                        tx,
                    } => {
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
    }
}
