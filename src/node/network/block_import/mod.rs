#![allow(unused)]
use handle::ImportHandle;
use reth_engine_primitives::EngineTypes;
use reth_eth_wire_types::DisconnectReason;
use reth_network::import::{BlockImport, BlockImportOutcome, NewBlockEvent};
use reth_network_api::Peers;
use reth_network_peers::PeerId;
use reth_payload_primitives::{BuiltPayload, PayloadTypes};
use reth_primitives_traits::NodePrimitives;
use service::{BlockMsg, ImportEvent, Outcome};
use std::{
    fmt,
    task::{ready, Context, Poll},
};

use crate::node::network::BscNewBlock;

pub(crate) mod fork_recover;
pub mod handle;
pub mod service;

const MAX_NEW_BLOCK_HASHES: usize = 1024;

#[derive(Debug)]
pub struct BscBlockImport {
    handle: ImportHandle,
}

impl BscBlockImport {
    pub fn new(handle: ImportHandle) -> Self {
        Self { handle }
    }
}

impl BlockImport<BscNewBlock> for BscBlockImport {
    fn on_new_block(&mut self, peer_id: PeerId, block_event: NewBlockEvent<BscNewBlock>) {
        match block_event {
            NewBlockEvent::Block(block) => {
                let _ = self.handle.send_block(block, peer_id);
            }
            NewBlockEvent::Hashes(hashes) => {
                if hashes.len() > MAX_NEW_BLOCK_HASHES {
                    tracing::warn!(
                        target: "bsc::block_import",
                        peer = %peer_id,
                        count = hashes.len(),
                        "Rejecting oversized block hash announcement"
                    );
                    if let Some(network) = crate::shared::get_network_handle() {
                        network
                            .disconnect_peer_with_reason(peer_id, DisconnectReason::ProtocolBreach);
                    }
                    return;
                }
                let _ = self.handle.send_hashes(hashes, peer_id);
            }
        }
    }

    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<ImportEvent> {
        match ready!(self.handle.poll_outcome(cx)) {
            Some(outcome) => Poll::Ready(outcome),
            None => Poll::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{B256, U256};
    use reth_eth_wire::BlockHashNumber;
    use reth_eth_wire_types::broadcast::NewBlockHashes;
    use tokio::sync::mpsc::{error::TryRecvError, unbounded_channel};

    #[test]
    fn block_hash_announcement_limit() {
        let (to_import, _blocks) = unbounded_channel();
        let (to_hashes, mut hashes_rx) = unbounded_channel();
        let (_outcomes, import_outcome) = unbounded_channel();
        let mut importer =
            BscBlockImport::new(ImportHandle::new(to_import, to_hashes, import_outcome));
        let peer = PeerId::default();

        for count in [0, 1, 1024, 1025, 1] {
            let hashes = NewBlockHashes(
                (0..count)
                    .map(|number| BlockHashNumber {
                        hash: B256::from(U256::from(number).to_be_bytes::<32>()),
                        number,
                    })
                    .collect(),
            );
            importer.on_new_block(peer, NewBlockEvent::Hashes(hashes.clone()));
            if count <= 1024 {
                let (received, sender) = hashes_rx.try_recv().expect("accepted announcement");
                assert_eq!(received, hashes);
                assert_eq!(sender, peer);
            }
            assert!(matches!(hashes_rx.try_recv(), Err(TryRecvError::Empty)));
        }
    }
}
