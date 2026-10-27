use std::task::{Context, Poll};

use super::propagation::PropagationReceiver;
use reth_engine_primitives::EngineTypes;
use reth_eth_wire_types::broadcast::NewBlockHashes;
use reth_network::import::BlockImportError;
use reth_network_api::PeerId;
use reth_payload_primitives::PayloadTypes;
use tokio::sync::mpsc::{error::TrySendError, Sender};

use super::metrics::Queue;

use super::service::{BlockMsg, ImportEvent, IncomingBlock, IncomingHashes, Outcome};

/// A handle for interacting with the block import service.
///
/// This handle provides a bidirectional communication channel with the
/// [`super::service::ImportService`]:
/// - Blocks can be sent to the service for import via [`send_block`](ImportHandle::send_block)
/// - Block hashes can be sent for download via [`send_hashes`](ImportHandle::send_hashes)
/// - Import outcomes can be received via [`poll_outcome`](ImportHandle::poll_outcome)`
#[derive(Debug)]
pub struct ImportHandle {
    /// Send the new block to the service
    to_import: Sender<IncomingBlock>,
    /// Send block hashes to the service for downloading
    to_hashes: Sender<IncomingHashes>,
    /// Receive the event(Announcement/Outcome) of the import
    import_outcome: PropagationReceiver,
}

impl ImportHandle {
    /// Create a new handle with the provided channels
    pub(crate) fn new(
        to_import: Sender<IncomingBlock>,
        to_hashes: Sender<IncomingHashes>,
        import_outcome: PropagationReceiver,
    ) -> Self {
        Self { to_import, to_hashes, import_outcome }
    }

    /// Sends the block to import to the service.
    /// Returns a [`BlockImportError`] if the queue is full or closed.
    pub fn send_block(&self, block: BlockMsg, peer_id: PeerId) -> Result<(), BlockImportError> {
        crate::node::miner::startup_diagnostics::ingress(
            block.hash,
            crate::consensus::parlia::util::calculate_millisecond_timestamp(
                &block.block.0.block.header,
            ),
            "network",
        );
        let result = self.to_import.try_send((block, peer_id));
        Queue::RemoteBlocks.observe(
            self.to_import.max_capacity() - self.to_import.capacity(),
            self.to_import.max_capacity(),
        );
        result.map_err(|error| {
            if matches!(error, TrySendError::Full(_)) {
                Queue::RemoteBlocks.full(self.to_import.max_capacity(), "drop");
            }
            BlockImportError::Other("block import service queue full or closed".into())
        })
    }

    /// Sends block hashes to the service for downloading.
    /// Returns a [`BlockImportError`] if the queue is full or closed.
    pub fn send_hashes(
        &self,
        hashes: NewBlockHashes,
        peer_id: PeerId,
    ) -> Result<(), BlockImportError> {
        let result = self.to_hashes.try_send((hashes, peer_id));
        Queue::RemoteHashes.observe(
            self.to_hashes.max_capacity() - self.to_hashes.capacity(),
            self.to_hashes.max_capacity(),
        );
        result.map_err(|error| {
            if matches!(error, TrySendError::Full(_)) {
                Queue::RemoteHashes.full(self.to_hashes.max_capacity(), "drop");
            }
            BlockImportError::Other("block hash service queue full or closed".into())
        })
    }

    /// Poll for the next import event
    pub fn poll_outcome(&mut self, cx: &mut Context<'_>) -> Poll<Option<ImportEvent>> {
        self.import_outcome.poll_recv(cx)
    }
}
