use std::task::{Context, Poll};

use reth_engine_primitives::EngineTypes;
use reth_network::import::BlockImportError;
use reth_network_api::PeerId;
use reth_payload_primitives::PayloadTypes;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use reth_eth_wire_types::broadcast::NewBlockHashes;

use super::service::{BlockMsg, ImportEvent, IncomingBlock, Outcome, IncomingHashes};

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
    to_import: UnboundedSender<IncomingBlock>,
    /// Send block hashes to the service for downloading
    to_hashes: UnboundedSender<IncomingHashes>,
    /// Receive the event(Announcement/Outcome) of the import
    import_outcome: UnboundedReceiver<ImportEvent>,
}

impl ImportHandle {
    /// Create a new handle with the provided channels
    pub fn new(
        to_import: UnboundedSender<IncomingBlock>,
        to_hashes: UnboundedSender<IncomingHashes>,
        import_outcome: UnboundedReceiver<ImportEvent>,
    ) -> Self {
        Self { to_import, to_hashes, import_outcome }
    }

    /// Sends the block to import to the service.
    /// Returns a [`BlockImportError`] if the channel to the import service is closed.
    pub fn send_block(&self, block: BlockMsg, peer_id: PeerId) -> Result<(), BlockImportError> {
        self.to_import
            .send((block, peer_id))
            .map_err(|_| BlockImportError::Other("block import service channel closed".into()))
    }

    /// Sends block hashes to the service for downloading.
    /// Returns a [`BlockImportError`] if the channel to the import service is closed.
    pub fn send_hashes(&self, hashes: NewBlockHashes, peer_id: PeerId) -> Result<(), BlockImportError> {
        self.to_hashes
            .send((hashes, peer_id))
            .map_err(|_| BlockImportError::Other("block hash service channel closed".into()))
    }

    /// Poll for the next import event
    pub fn poll_outcome(&mut self, cx: &mut Context<'_>) -> Poll<Option<ImportEvent>> {
        self.import_outcome.poll_recv(cx)
    }
}
