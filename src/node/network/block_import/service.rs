use super::handle::ImportHandle;
use crate::{
    consensus::{ParliaConsensus, ParliaConsensusErr, parlia::vote_pool},
    node::{engine_api::payload::BscPayloadTypes, network::BscNewBlock},
    BscBlock, BscBlockBody,
};
use alloy_consensus::{BlockBody, Header};
use alloy_primitives::{B256, U128};
use alloy_rpc_types::engine::{ForkchoiceState, PayloadStatusEnum};
use futures::{future::Either, stream::FuturesUnordered, StreamExt};
use reth::network::cache::LruCache;
use reth_engine_primitives::{ConsensusEngineHandle, EngineTypes};
use reth_network::{
    import::{BlockImportError, BlockImportEvent, BlockImportOutcome, BlockValidation},
    message::NewBlockMessage,
};
use reth_network_api::PeerId;
use reth_node_ethereum::EthEngineTypes;
use reth_payload_primitives::{BuiltPayload, EngineApiMessageVersion, PayloadTypes};
use reth_primitives::NodePrimitives;
use reth_primitives_traits::{AlloyBlockHeader, Block};
use reth_provider::{BlockHashReader, BlockNumReader};
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

/// Size of the LRU cache for processed blocks.
const LRU_PROCESSED_BLOCKS_SIZE: u32 = 100;

/// A service that handles bidirectional block import communication with the network.
/// It receives new blocks from the network via `from_network` channel and sends back
/// import outcomes via `to_network` channel.
pub struct ImportService<Provider>
where
    Provider: BlockNumReader + Clone,
{
    /// The handle to communicate with the engine service
    engine: ConsensusEngineHandle<BscPayloadTypes>,
    /// The consensus implementation
    consensus: Arc<ParliaConsensus<Provider>>,
    /// Receive the new block from the network
    from_network: UnboundedReceiver<IncomingBlock>,
    /// Send the event of the import to the network
    to_network: UnboundedSender<ImportEvent>,
    /// Pending block imports.
    pending_imports: FuturesUnordered<ImportFut>,
    /// Cache of processed block hashes to avoid reprocessing the same block.
    processed_blocks: LruCache<B256>,
}

impl<Provider> ImportService<Provider>
where
    Provider: BlockNumReader + Clone + 'static,
{
    /// Create a new block import service
    pub fn new(
        consensus: Arc<ParliaConsensus<Provider>>,
        engine: ConsensusEngineHandle<BscPayloadTypes>,
        from_network: UnboundedReceiver<IncomingBlock>,
        to_network: UnboundedSender<ImportEvent>,
    ) -> Self {
        Self {
            engine,
            consensus,
            from_network,
            to_network,
            pending_imports: FuturesUnordered::new(),
            processed_blocks: LruCache::new(LRU_PROCESSED_BLOCKS_SIZE),
        }
    }

    /// Process a new payload and return the outcome
    fn new_payload(&self, block: BlockMsg, peer_id: PeerId) -> ImportFut {
        let engine = self.engine.clone();

        Box::pin(async move {
            let sealed_block = block.block.0.block.clone().seal();
            let payload = BscPayloadTypes::block_to_payload(sealed_block);

            match engine.new_payload(payload).await {
                Ok(payload_status) => match payload_status.status {
                    PayloadStatusEnum::Valid => {
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
    fn update_fork_choice(&self, block: BlockMsg, peer_id: PeerId) -> ImportFut {
        let engine = self.engine.clone();
        let consensus = self.consensus.clone();
        let sealed_block = block.block.0.block.clone().seal();
        let hash = sealed_block.hash();
        let number = sealed_block.number();

        Box::pin(async move {
            let (head_block_hash, current_hash) = match consensus.canonical_head(hash, number) {
                Ok(hash) => hash,
                Err(_) => return None,
            };

            let state = ForkchoiceState {
                head_block_hash,
                safe_block_hash: head_block_hash,
                finalized_block_hash: head_block_hash,
            };

            match engine.fork_choice_updated(state, None, EngineApiMessageVersion::default()).await
            {
                Ok(response) => match response.payload_status.status {
                    PayloadStatusEnum::Valid => {
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

    /// Add a new block import task to the pending imports
    fn on_new_block(&mut self, block: BlockMsg, peer_id: PeerId) {
        // When bench-test feature is enabled, skip block import processing
        #[cfg(feature = "bench-test")]
        {
            return;
        }
        
        if self.processed_blocks.contains(&block.hash) {
            return;
        }

        let payload_fut = self.new_payload(block.clone(), peer_id);
        self.pending_imports.push(payload_fut);

        let fcu_fut = self.update_fork_choice(block, peer_id);
        self.pending_imports.push(fcu_fut);
    }
}

impl<Provider> Future for ImportService<Provider>
where
    Provider: BlockNumReader + BlockHashReader + Clone + 'static + Unpin,
{
    type Output = Result<(), Box<dyn std::error::Error>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        // Receive new blocks from network
        while let Poll::Ready(Some((block, peer_id))) = this.from_network.poll_recv(cx) {
            this.on_new_block(block, peer_id);
        }

        // Process completed imports and send events to network
        while let Poll::Ready(Some(outcome)) = this.pending_imports.poll_next_unpin(cx) {
            if let Some(outcome) = outcome {
                if let Ok(BlockValidation::ValidBlock { block }) = &outcome.result {
                    this.processed_blocks.insert(block.hash);
                    
                    // Prune old votes from the vote pool based on the new block number
                    let block_number = block.block.0.block.header.number();
                    vote_pool::prune(block_number);
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
    use alloy_primitives::{B256, U128};
    use alloy_rpc_types::engine::PayloadStatus;
    use reth_chainspec::ChainInfo;
    use reth_engine_primitives::{BeaconEngineMessage, OnForkChoiceUpdated};
    use reth_eth_wire::NewBlock;
    use reth_node_ethereum::EthEngineTypes;
    use reth_primitives::Block;
    use reth_provider::ProviderError;
    use std::{
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
    async fn can_handle_invalid_fcu() {
        let mut fixture = TestFixture::new(EngineResponses::invalid_fcu()).await;
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
        let mut outcomes = Vec::new();

        // Wait for both NewPayload and FCU outcomes from first block
        while outcomes.len() < 2 {
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
    struct MockProvider;

    impl BlockNumReader for MockProvider {
        fn chain_info(&self) -> Result<ChainInfo, ProviderError> {
            unimplemented!()
        }

        fn best_block_number(&self) -> Result<u64, ProviderError> {
            Ok(0)
        }

        fn last_block_number(&self) -> Result<u64, ProviderError> {
            Ok(0)
        }

        fn block_number(&self, _hash: B256) -> Result<Option<u64>, ProviderError> {
            Ok(None)
        }
    }

    impl BlockHashReader for MockProvider {
        fn block_hash(&self, _number: u64) -> Result<Option<B256>, ProviderError> {
            Ok(Some(B256::ZERO))
        }

        fn canonical_hashes_range(
            &self,
            _start: u64,
            _end: u64,
        ) -> Result<Vec<B256>, ProviderError> {
            Ok(vec![])
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
            let consensus = Arc::new(ParliaConsensus { provider: MockProvider });
            let (to_engine, from_engine) = mpsc::unbounded_channel();
            let engine_handle = ConsensusEngineHandle::new(to_engine);

            handle_engine_msg(from_engine, responses).await;

            let (to_import, from_network) = mpsc::unbounded_channel();
            let (to_network, import_outcome) = mpsc::unbounded_channel();

            let handle = ImportHandle::new(to_import, import_outcome);

            let service = ImportService::new(consensus, engine_handle, from_network, to_network);
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
            while outcomes.len() < 2 {
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
