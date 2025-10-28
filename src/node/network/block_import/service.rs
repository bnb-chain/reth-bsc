use super::handle::ImportHandle;
use crate::{
    consensus::{ParliaConsensus, ParliaConsensusErr, parlia::vote_pool},
    node::{engine_api::payload::BscPayloadTypes, network::BscNewBlock},
    BscBlock, BscBlockBody,
};
use alloy_consensus::{BlockBody, Header};
use alloy_eips::BlockNumberOrTag;
use alloy_primitives::{B256, U128};
use alloy_rpc_types::engine::{ForkchoiceState, PayloadStatusEnum};
use futures::{future::Either, stream::FuturesUnordered, StreamExt};
use parking_lot::RwLock;
use reth::network::cache::LruCache;
use reth_engine_primitives::{BeaconConsensusEngineHandle, EngineTypes};
use reth_network::{
    import::{BlockImportError, BlockImportEvent, BlockImportOutcome, BlockValidation},
    message::{NewBlockMessage, PeerMessage},
};
use reth_network_api::PeerId;
use reth_node_ethereum::EthEngineTypes;
use reth_payload_primitives::{BuiltPayload, EngineApiMessageVersion, PayloadTypes};
use reth_primitives::NodePrimitives;
use reth_primitives_traits::{AlloyBlockHeader, Block};
use reth_provider::{BlockHashReader, BlockNumReader, BlockReaderIdExt, HeaderProvider};
use reth_eth_wire_types::broadcast::NewBlockHashes;
use reth_eth_wire::{GetBlockHeaders, BlockHashNumber};
use reth_network::{NetworkHandle, message::{PeerResponse, BlockRequest}, FetchClient};
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

/// Channel message type for incoming block hashes
pub(crate) type IncomingHashes = (NewBlockHashes, PeerId);

/// Size of the LRU cache for processed blocks.
const LRU_PROCESSED_BLOCKS_SIZE: u32 = 100;

/// A service that handles bidirectional block import communication with the network.
/// It receives new blocks from the network via `from_network` channel and sends back
/// import outcomes via `to_network` channel.
pub struct ImportService<Provider>
where
    Provider: BlockNumReader + HeaderProvider + Clone,
{
    /// The handle to communicate with the engine service
    engine: BeaconConsensusEngineHandle<BscPayloadTypes>,
    /// The consensus implementation
    consensus: Arc<RwLock<ParliaConsensus<Provider>>>,
    /// Receive the new block from the network
    from_network: UnboundedReceiver<IncomingBlock>,
    /// Receive block hashes from the network for downloading
    from_hashes: UnboundedReceiver<IncomingHashes>,
    /// Send the event of the import to the network
    to_network: UnboundedSender<ImportEvent>,
    /// Pending block imports.
    pending_imports: FuturesUnordered<ImportFut>,
    /// Cache of processed block hashes to avoid reprocessing the same block.
    processed_blocks: LruCache<B256>,
}

impl<Provider> ImportService<Provider>
where
    Provider: BlockNumReader + HeaderProvider<Header = Header> + Clone + 'static,
{
    /// Create a new block import service
    pub fn new(
        consensus: Arc<RwLock<ParliaConsensus<Provider>>>,
        engine: BeaconConsensusEngineHandle<BscPayloadTypes>,
        from_network: UnboundedReceiver<IncomingBlock>,
        from_hashes: UnboundedReceiver<IncomingHashes>,
        to_network: UnboundedSender<ImportEvent>,
    ) -> Self {
        Self {
            engine,
            consensus,
            from_network,
            from_hashes,
            to_network,
            pending_imports: FuturesUnordered::new(),
            processed_blocks: LruCache::new(LRU_PROCESSED_BLOCKS_SIZE),
        }
    }

    /// Process a new payload and return the outcome
    fn new_payload(&self, block: BlockMsg, peer_id: PeerId) -> ImportFut {
        let engine = self.engine.clone();
        let consensus = self.consensus.clone();

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
                        if let Err(e) = Self::update_fork_choice(engine, consensus, header).await {
                            tracing::warn!(target: "bsc::block_import", "Failed to update fork choice: {}", e);
                        }
                        Outcome { peer: peer_id, result: Ok(BlockValidation::ValidBlock { block }) }
                            .into()
                    }
                    PayloadStatusEnum::Invalid { validation_error } => Outcome {
                        peer: peer_id,
                        result: Err(BlockImportError::Other(validation_error.into())),
                    }
                    .into(),
                    _ => None,
                },
                Err(err) => None,
            }
        })
    }

    /// Process a forkchoice update and return the outcome  
    async fn update_fork_choice(
        engine: BeaconConsensusEngineHandle<BscPayloadTypes>, 
        consensus: Arc<RwLock<ParliaConsensus<Provider>>>,
        new_header: Header) -> Result<(), ParliaConsensusErr> {
        let best_number = consensus.read().provider.chain_info()?.best_number;
        tracing::debug!(target: "parlia", "Best canonical number: {:?}, new_header = {:?}", best_number, new_header);
        let current_head = consensus.read().provider.header_by_number(best_number)?.ok_or(ParliaConsensusErr::HeadHashNotFound)?;
        let (new_td, current_td) = consensus.write().header_td_fcu(&engine, &new_header, &current_head).await?;
        let new_canonical_head = consensus.read().canonical_head((&new_header, new_td), (&current_head, current_td))?;
        // get safe block and finalized block with new canonical head
        // ref: https://github.com/bnb-chain/bsc/blob/f70aaa8399ccee429804eecf3fc4c6fd8d9e6cab/eth/api_backend.go#L72
        let (safe_block_number, safe_block_hash) = consensus.read().get_justified_number_and_hash(new_canonical_head).unwrap_or((0, B256::ZERO));
        let (finalized_block_number, finalized_block_hash) = consensus.read().get_finalized_number_and_hash(new_canonical_head).unwrap_or((0, B256::ZERO));
        let state = ForkchoiceState {
            head_block_hash: new_canonical_head.hash_slow(),
            safe_block_hash,
            finalized_block_hash,
        };

        tracing::debug!(target: "parlia", "Fork choice updated: state = {:?}, new_canonical_head = ({:?}, {:?}), new_header = ({:?}, {:?})", 
            state, new_canonical_head.number, new_canonical_head.hash_slow(), new_header.number, new_header.hash_slow());
        match engine.fork_choice_updated(state, None, EngineApiMessageVersion::default()).await
        {
            Ok(response) => match response.payload_status.status {
                PayloadStatusEnum::Invalid { validation_error } => 
                Err(ParliaConsensusErr::ForkChoiceUpdateError(validation_error)),
                _ => Ok(()),
            },
            Err(err) => Err(ParliaConsensusErr::ForkChoiceUpdateError(err.to_string())),
        }
    }

    /// Add a new block import task to the pending imports
    fn on_new_block(&mut self, block: BlockMsg, peer_id: PeerId) {
        if self.processed_blocks.contains(&block.hash) {
            return;
        }

        let payload_fut = self.new_payload(block.clone(), peer_id);
        self.pending_imports.push(payload_fut);
    }

    /// Handle incoming block hashes by using Reth engine-tree download mechanism
    fn on_new_block_hashes(&mut self, hashes: NewBlockHashes, peer_id: PeerId) {
        let hash_numbers = hashes.0.clone();
        
        for hash_number in hash_numbers {
            if self.processed_blocks.contains(&hash_number.hash) {
                continue;
            }

            tracing::trace!(
                target: "bsc::block_import",
                peer_id = %peer_id,
                block_hash = %hash_number.hash,
                block_number = hash_number.number,
                "Requesting block download by simulating FCU for NewBlockHashes"
            );

            let forkchoice_state = ForkchoiceState {
                head_block_hash: hash_number.hash,
                safe_block_hash: hash_number.hash, 
                finalized_block_hash: hash_number.hash,
            };

            let engine = self.engine.clone();
            let block_hash = hash_number.hash;
            let download_fut = Box::pin(async move {
                match engine.fork_choice_updated(forkchoice_state, None, EngineApiMessageVersion::V1).await {
                    Ok(result) => {
                        tracing::debug!(
                            target: "bsc::block_import",
                            block_hash = %block_hash,
                            status = ?result.payload_status.status,
                            "FCU result for missing block download"
                        );
                    }
                    Err(err) => {
                        tracing::warn!(
                            target: "bsc::block_import", 
                            block_hash = %block_hash,
                            error = %err,
                            "Failed to trigger block download via FCU"
                        );
                    }
                }
                None
            });

            self.pending_imports.push(download_fut);
            self.processed_blocks.insert(hash_number.hash);
        }
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

        // Receive new block hashes from network
        while let Poll::Ready(Some((hashes, peer_id))) = this.from_hashes.poll_recv(cx) {
            this.on_new_block_hashes(hashes, peer_id);
        }

        // Process completed imports and send events to network
        while let Poll::Ready(Some(outcome)) = this.pending_imports.poll_next_unpin(cx) {
            if let Some(outcome) = outcome {
                if let Ok(BlockValidation::ValidBlock { block }) = &outcome.result {
                    this.processed_blocks.insert(block.hash);
                    // If from proxied validators, target EVN peers with ETH NewBlockHashes.
                    if let Some(cfg) = crate::node::network::evn::get_global_evn_config() {
                        let header_ref = &block.block.0.block.header;
                        let coinbase = header_ref.beneficiary;
                        if cfg.proxyed_validators.contains(&coinbase) {
                            if let Some(net) = crate::shared::get_network_handle() {
                                let peers = crate::node::network::evn_peers::snapshot();
                                for (peer_id, info) in peers {
                                    if info.is_evn {
                                        // Send full NewBlock to EVN peers to avoid re-fetching.
                                        net.send_eth_message(peer_id, PeerMessage::NewBlock(block.clone()));
                                    }
                                }
                            }
                        }
                    }
                    // Prune old votes from the vote pool based on the new block number
                    let block_number = block.block.0.block.header.number();
                    vote_pool::prune(block_number);

                    // Produce and broadcast a local vote for this new canonical head, if eligible
                    if let Some(sp) = crate::shared::get_snapshot_provider() {
                        let sp = Arc::clone(sp);
                        let spec = this.consensus.read().chain_spec.clone();
                        let header = block.block.0.block.header.clone();
                        tokio::spawn(async move {
                            crate::node::vote_producer::maybe_produce_and_broadcast_for_head(
                                spec,
                                sp.as_ref(),
                                &header,
                            );
                        });
                    }
                }

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
    use std::{
        collections::HashMap, sync::Arc, task::{Context, Poll}
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

    // FCU has been called after import payload is validated, skip this test now.
    // #[tokio::test]
    // async fn can_handle_invalid_fcu() {
    //     let mut fixture = TestFixture::new(EngineResponses::invalid_fcu()).await;
    //     fixture
    //         .assert_block_import(|outcome| {
    //             matches!(
    //                 outcome,
    //                 BlockImportEvent::Outcome(BlockImportOutcome {
    //                     peer: _,
    //                     result: Err(BlockImportError::Other(_))
    //                 })
    //             )
    //         })
    //         .await;
    // }

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
        let mut outcomes = Vec::new();

        // Wait for both NewPayload and FCU outcomes from first block
        while outcomes.is_empty() {
            match fixture.handle.poll_outcome(&mut cx) {
                Poll::Ready(Some(outcome)) => {
                    outcomes.push(outcome);
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
            Self { headers_by_number, headers_by_hash, td_by_hash, head_number: 0, head_hash: BlockHash::ZERO }
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

        fn canonical_hashes_range(&self, _start: BlockNumber, _end: BlockNumber) -> Result<Vec<B256>, ProviderError> {
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
            let start = match range.start_bound() { Included(&s) => s, Excluded(&s) => s + 1, Unbounded => 0 };
            let end = match range.end_bound() { Included(&e) => e, Excluded(&e) => e - 1, Unbounded => self.head_number };
            let mut out = Vec::new();
            for n in start..=end {
                if let Some(h) = self.headers_by_number.get(&n) {
                    out.push(h.clone());
                }
            }
            Ok(out)
        }

        fn sealed_header(&self, number: BlockNumber) -> Result<Option<SealedHeader<Self::Header>>, ProviderError> {
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
                if !predicate(&sh) { break; }
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
            let consensus = Arc::new(ParliaConsensus { 
                provider: MockProvider::new(), 
                chain_spec: Arc::new(crate::chainspec::BscChainSpec::from(crate::chainspec::bsc::bsc_mainnet())),
            });
            let (to_engine, from_engine) = mpsc::unbounded_channel();
            let engine_handle = BeaconConsensusEngineHandle::new(to_engine);

            handle_engine_msg(from_engine, responses).await;

            let (to_import, from_network) = mpsc::unbounded_channel();
            let (to_hashes, from_hashes) = mpsc::unbounded_channel();
            let (to_network, import_outcome) = mpsc::unbounded_channel();

            let handle = ImportHandle::new(to_import, to_hashes, import_outcome);

            let service = ImportService::new(
                consensus, 
                engine_handle, 
                from_network, 
                from_hashes,
                to_network
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

            // Wait for both NewPayload and FCU outcomes
            while outcomes.is_empty() {
                match self.handle.poll_outcome(&mut cx) {
                    Poll::Ready(Some(outcome)) => {
                        outcomes.push(outcome);
                    }
                    Poll::Ready(None) => break,
                    Poll::Pending => tokio::task::yield_now().await,
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
