use alloy_primitives::bytes::BytesMut;
use alloy_rlp::{Decodable, Encodable};
use bytes::Bytes;
use futures::Future;
use futures::{Stream, StreamExt};
use reth_eth_wire::multiplex::ProtocolConnection;
use reth_network_api::PeerId;
use std::{collections::HashMap, sync::Arc};
use std::{
    pin::Pin,
    task::{ready, Context, Poll},
};
use tokio::sync::{mpsc::UnboundedReceiver, oneshot};
use tokio::time::{Duration, Sleep};
use tokio_stream::wrappers::UnboundedReceiverStream;

/// Handshake timeout, mirroring the Go implementation.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// TTL for pending range requests before being pruned
const PENDING_REQ_TTL: Duration = Duration::from_secs(15);
/// Minimum interval between pending-request pruning passes
const PRUNE_INTERVAL: Duration = Duration::from_secs(5);

use super::protocol::proto::BscProtoMessageId;
use crate::node::network::blocks_by_range::{
    build_blocks_by_range_response, BlocksByRangePacket, GetBlocksByRangePacket,
    MAX_REQUEST_RANGE_BLOCKS_COUNT,
};
use crate::node::network::votes::{handle_votes_broadcast, BscCapPacket, VotesPacket};

/// Commands that can be sent to the BSC connection.
#[allow(dead_code)]
#[derive(Debug)]
pub enum BscCommand {
    Capability {
        protocol_version: u64,
        extra: Bytes,
    },
    Votes(Arc<Vec<crate::consensus::parlia::vote::VoteEnvelope>>),
    GetBlocksByRange(
        crate::node::network::blocks_by_range::GetBlocksByRangePacket,
        oneshot::Sender<Result<crate::node::network::blocks_by_range::BlocksByRangePacket, String>>,
    ),
    BlocksByRange(BlocksByRangePacket),
}

/// Stream that handles incoming BSC protocol messages and returns outgoing messages to send.
pub struct BscProtocolConnection {
    conn: ProtocolConnection,
    commands: UnboundedReceiverStream<BscCommand>,
    handshake_deadline: Option<std::pin::Pin<Box<Sleep>>>,
    handshake_completed: bool,
    is_dialer: bool,
    initial_capability: Option<BscCommand>,
    /// Pending in-flight GetBlocksByRange requests mapped by request_id
    pending_range_reqs:
        HashMap<u64, (oneshot::Sender<Result<BlocksByRangePacket, String>>, std::time::Instant)>,
    /// Protocol version negotiated for this connection (1 or 2)
    proto_version: u64,
    /// PeerId for this connection, if known
    _peer_id: Option<PeerId>,
    /// Last time we pruned pending requests
    last_prune: std::time::Instant,
}

impl BscProtocolConnection {
    pub fn new(
        conn: ProtocolConnection,
        commands: UnboundedReceiver<BscCommand>,
        is_dialer: bool,
        proto_version: u64,
        peer_id: Option<PeerId>,
    ) -> Self {
        let handshake_deadline = Some(Box::pin(tokio::time::sleep(HANDSHAKE_TIMEOUT)));
        // Both sides should send initial capability in BSC protocol
        // BSC sends []byte{00} which in RLP is encoded as a single byte 0x00
        let initial_capability = Some(BscCommand::Capability {
            protocol_version: proto_version,
            extra: Bytes::from_static(&[0x00u8]), // Raw RLP: single 0x00 byte represents []byte{00}
        });

        Self {
            conn,
            commands: UnboundedReceiverStream::new(commands),
            handshake_deadline,
            handshake_completed: false,
            is_dialer,
            initial_capability,
            pending_range_reqs: HashMap::new(),
            proto_version,
            _peer_id: peer_id,
            last_prune: std::time::Instant::now(),
        }
    }

    fn prune_pending_requests(&mut self) {
        // Rate-limit pruning
        let now = std::time::Instant::now();
        if now.duration_since(self.last_prune) < PRUNE_INTERVAL {
            return;
        }
        self.last_prune = now;
        let mut to_remove: Vec<u64> = Vec::new();
        for (req_id, (tx, ts)) in self.pending_range_reqs.iter() {
            if tx.is_closed() || now.duration_since(*ts) > PENDING_REQ_TTL {
                to_remove.push(*req_id);
            }
        }
        for id in to_remove {
            self.pending_range_reqs.remove(&id);
            tracing::debug!(target: "bsc_protocol", req_id = id, "Pruned stale pending range request");
        }
    }

    fn encode_command(cmd: BscCommand) -> BytesMut {
        match cmd {
            BscCommand::Capability { protocol_version, extra } => {
                let mut buf = BytesMut::new();
                let cap_packet = BscCapPacket { protocol_version, extra };
                cap_packet.encode(&mut buf);

                tracing::trace!(
                    target: "bsc_protocol",
                    version = protocol_version,
                    extra_len = cap_packet.extra.len(),
                    encoded_len = buf.len(),
                    all_bytes = format!("{:02x?}", &buf[..]),
                    "Encoded BSC capability packet"
                );

                buf
            }
            BscCommand::Votes(votes) => {
                let mut buf = BytesMut::new();
                let vote_count = votes.len();
                VotesPacket(votes.as_ref().clone()).encode(&mut buf);

                tracing::trace!(
                    target: "bsc_protocol",
                    vote_count = vote_count,
                    encoded_len = buf.len(),
                    first_bytes = format!("{:02x?}", &buf[..buf.len().min(8)]),
                    "Encoded BSC votes packet"
                );

                buf
            }
            BscCommand::GetBlocksByRange(req, _tx) => {
                let mut buf = BytesMut::new();
                req.encode(&mut buf);
                tracing::debug!(
                    target: "bsc_protocol",
                    req_id = req.request_id,
                    count = req.count,
                    "Encoded GetBlocksByRange packet"
                );
                buf
            }
            BscCommand::BlocksByRange(packet) => {
                let mut buf = BytesMut::new();
                packet.encode(&mut buf);
                tracing::debug!(
                    target: "bsc_protocol",
                    req_id = packet.request_id,
                    blocks = packet.blocks.len(),
                    encoded_len = buf.len(),
                    "Encoded BlocksByRange packet"
                );
                buf
            }
        }
    }

    /// Poll for outgoing commands and encode them
    fn poll_outgoing_commands(&mut self, cx: &mut Context<'_>) -> Option<BytesMut> {
        tracing::trace!(target: "bsc_protocol", "Checking for outgoing commands");
        // Opportunistically prune stale pending requests
        self.prune_pending_requests();
        if let Poll::Ready(Some(cmd)) = self.commands.poll_next_unpin(cx) {
            tracing::trace!(target: "bsc_protocol", cmd = ?cmd, "Processing outgoing command");
            let encoded = match cmd {
                BscCommand::GetBlocksByRange(req, resp_tx) => {
                    // track pending request, then encode
                    let req_id = req.request_id;
                    // Overwrite existing pending if any
                    self.pending_range_reqs.insert(req_id, (resp_tx, std::time::Instant::now()));
                    let mut buf = BytesMut::new();
                    req.encode(&mut buf);
                    buf
                }
                other => Self::encode_command(other),
            };
            tracing::trace!(target: "bsc_protocol", len = encoded.len(), "Sending encoded command");
            Some(encoded)
        } else {
            tracing::trace!(target: "bsc_protocol", "No outgoing commands ready");
            None
        }
    }

    /// Poll for incoming frames from the peer
    fn poll_incoming_frame(&mut self, cx: &mut Context<'_>) -> Poll<Option<Option<BytesMut>>> {
        tracing::trace!(target: "bsc_protocol", "Polling for incoming frames");
        let Some(raw) = ready!(self.conn.poll_next_unpin(cx)) else {
            tracing::debug!(target: "bsc_protocol", "Connection closed by peer");
            return Poll::Ready(None);
        };

        if raw.is_empty() {
            tracing::trace!(target: "bsc_protocol", "Received empty frame");
            return Poll::Ready(Some(None));
        }

        // Opportunistically prune stale pending requests
        self.prune_pending_requests();
        tracing::trace!(target: "bsc_protocol", len = raw.len(), "Received frame");
        Poll::Ready(Some(Some(raw)))
    }

    /// Handle handshake-related frames
    fn handle_handshake_frame(
        &mut self,
        frame: &BytesMut,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Option<BytesMut>>> {
        tracing::debug!(target: "bsc_protocol", "Handshake not completed, processing handshake frame");
        // Check for handshake timeout
        if let Some(deadline) = self.handshake_deadline.as_mut() {
            if Future::poll(deadline.as_mut(), cx).is_ready() {
                tracing::warn!(target: "bsc_protocol", "BSC handshake timed out");
                return Poll::Ready(None);
            }
        }

        let slice = frame.as_ref();
        let msg_id = slice[0];

        tracing::debug!(target: "bsc_protocol", "Handshake not completed, processing handshake frame, msg_id: {:?}", msg_id);
        if msg_id != BscProtoMessageId::Capability as u8 {
            tracing::warn!(target: "bsc_protocol", got = format_args!("{:#04x}", msg_id), "Expected capability during handshake");
            return Poll::Ready(None);
        }

        // Debug: Show what we received
        tracing::trace!(
            target: "bsc_protocol",
            frame_len = slice.len(),
            frame_bytes = format!("{:02x?}", &slice[..slice.len().min(16)]),
            "Raw received frame in handshake"
        );

        match BscCapPacket::decode(&mut &slice[..]) {
            Ok(pkt) => {
                if pkt.protocol_version != self.proto_version {
                    tracing::warn!(target: "bsc_protocol", "Protocol version mismatch: {} != {}", pkt.protocol_version, self.proto_version);
                    return Poll::Ready(None);
                }

                tracing::trace!(target: "bsc_protocol", version = pkt.protocol_version, "Received peer capability");

                tracing::trace!(
                    target: "bsc_protocol",
                    is_dialer = self.is_dialer,
                    "BSC handshake completed successfully"
                );

                self.handshake_completed = true;
                self.handshake_deadline = None;
                tracing::trace!(target: "bsc_protocol", "BSC handshake completed");
                Poll::Ready(Some(None))
            }
            Err(e) => {
                tracing::warn!(target: "bsc_protocol", error = %e, "Failed to decode BSC capability during handshake");
                Poll::Ready(None)
            }
        }
    }

    /// Handle normal protocol messages after handshake
    fn handle_protocol_message(&mut self, frame: &BytesMut) -> Option<BytesMut> {
        tracing::trace!(target: "bsc_protocol", "Handshake completed, processing normal message");
        let slice = frame.as_ref();
        let msg_id = slice[0];

        tracing::trace!(target: "bsc_protocol", "Handshake completed, processing normal message, msg_id: {:?}", msg_id);
        match msg_id {
            x if x == BscProtoMessageId::Votes as u8 => {
                tracing::trace!(target: "bsc_protocol", "Processing votes message");
                match VotesPacket::decode(&mut &slice[..]) {
                    Ok(packet) => {
                        let count = packet.0.len();
                        handle_votes_broadcast(packet);
                        tracing::trace!(target: "bsc_protocol", count, "Processed votes packet");
                        None
                    }
                    Err(e) => {
                        tracing::warn!(target: "bsc_protocol", error = %e, "Failed to decode VotesPacket");
                        None
                    }
                }
            }
            x if x == BscProtoMessageId::GetBlocksByRange as u8 => {
                tracing::debug!(target: "bsc_protocol", "Processing GetBlocksByRange request");
                match GetBlocksByRangePacket::decode(&mut &slice[..]) {
                    Ok(req) => {
                        if req.count == 0 || req.count > MAX_REQUEST_RANGE_BLOCKS_COUNT {
                            tracing::warn!(
                                target: "bsc_protocol",
                                count = req.count,
                                "Invalid GetBlocksByRange count; ignoring"
                            );
                            return None;
                        }

                        let resp = build_blocks_by_range_response(&req);
                        let encoded = Self::encode_command(BscCommand::BlocksByRange(resp));
                        tracing::debug!(target: "bsc_protocol", "Replying BlocksByRange for request");
                        Some(encoded)
                    }
                    Err(e) => {
                        tracing::warn!(target: "bsc_protocol", error = %e, "Failed to decode GetBlocksByRangePacket");
                        None
                    }
                }
            }
            x if x == BscProtoMessageId::BlocksByRange as u8 => {
                tracing::debug!(target: "bsc_protocol", "Processing BlocksByRange response");
                match BlocksByRangePacket::decode(&mut &slice[..]) {
                    Ok(res) => {
                        tracing::debug!(
                            target: "bsc_protocol",
                            req_id = res.request_id,
                            blocks = res.blocks.len(),
                            "Received BlocksByRange"
                        );
                        if let Some((waiter, _)) = self.pending_range_reqs.remove(&res.request_id) {
                            let _ = waiter.send(Ok(res));
                        } else {
                            tracing::trace!(target: "bsc_protocol", "No waiter for request_id; dropping BlocksByRange");
                        }
                        None
                    }
                    Err(e) => {
                        tracing::warn!(target: "bsc_protocol", error = %e, "Failed to decode BlocksByRangePacket");
                        None
                    }
                }
            }
            _ => {
                tracing::debug!(target: "bsc_protocol", msg_id = format_args!("{:#04x}", msg_id), "Unknown BSC message id");
                None
            }
        }
    }
}

impl Stream for BscProtocolConnection {
    type Item = BytesMut;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        // Send initial capability (both dialer and responder)
        if let Some(initial_cmd) = this.initial_capability.take() {
            tracing::trace!(
                target: "bsc_protocol",
                is_dialer = this.is_dialer,
                "Sending initial BSC capability packet"
            );
            return Poll::Ready(Some(Self::encode_command(initial_cmd)));
        }

        loop {
            // Check for outgoing commands first
            if let Some(encoded_command) = this.poll_outgoing_commands(cx) {
                return Poll::Ready(Some(encoded_command));
            }

            // Get next incoming frame
            let raw_frame = match this.poll_incoming_frame(cx) {
                Poll::Ready(Some(Some(frame))) => frame,
                Poll::Ready(Some(None)) => continue, // Empty frame, try again
                Poll::Ready(None) => return Poll::Ready(None), // Connection closed
                Poll::Pending => return Poll::Pending,
            };

            // Process the frame based on handshake state
            if !this.handshake_completed {
                match this.handle_handshake_frame(&raw_frame, cx) {
                    Poll::Ready(Some(Some(response))) => return Poll::Ready(Some(response)),
                    Poll::Ready(Some(None)) => continue, // Handshake complete, no response needed
                    Poll::Ready(None) => return Poll::Ready(None), // Handshake failed
                    Poll::Pending => return Poll::Pending,
                }
            } else if let Some(response) = this.handle_protocol_message(&raw_frame) {
                return Poll::Ready(Some(response));
            } else {
                // After handshake, check if there are more messages to process
                // If not, we'll loop back and check for commands/incoming frames
                continue;
            }
        }
    }
}
