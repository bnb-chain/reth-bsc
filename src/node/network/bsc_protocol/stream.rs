use alloy_primitives::bytes::BytesMut;
use alloy_rlp::{Decodable, Encodable};
use bytes::Bytes;
use futures::{Stream, StreamExt};
use reth_eth_wire::multiplex::ProtocolConnection;
use reth_network_api::PeerId;
use std::{collections::HashMap, sync::Arc};
use std::{
    pin::Pin,
    task::{ready, Context, Poll},
};
use tokio::sync::{mpsc::UnboundedReceiver, oneshot};
use tokio::time::Duration;
use tokio_stream::wrappers::UnboundedReceiverStream;

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

/// In-flight `GetBlocksByRange` requests, keyed by request id, each with the
/// waiter to resolve and the instant it was issued (for TTL pruning).
type PendingRangeReqs =
    HashMap<u64, (oneshot::Sender<Result<BlocksByRangePacket, String>>, std::time::Instant)>;

/// Stream that handles incoming BSC protocol messages and returns outgoing messages to send.
pub struct BscProtocolConnection {
    conn: ProtocolConnection,
    commands: UnboundedReceiverStream<BscCommand>,
    is_dialer: bool,
    initial_capability: Option<BscCommand>,
    /// Pending in-flight GetBlocksByRange requests mapped by request_id
    pending_range_reqs: PendingRangeReqs,
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
        // We still send the capability packet as our first frame: nodes older than
        // bsc#3483 block on reading it before they will talk to us. Peers on
        // bsc#3737 and later no longer send one, and drop ours in their no-op
        // `handleBscCap`, so sending it stays safe across every peer generation.
        // BSC sends []byte{00} which in RLP is encoded as a single byte 0x00
        let initial_capability = Some(BscCommand::Capability {
            protocol_version: proto_version,
            extra: Bytes::from_static(&[0x00u8]), // Raw RLP: single 0x00 byte represents []byte{00}
        });

        Self {
            conn,
            commands: UnboundedReceiverStream::new(commands),
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

    /// Handle an inbound protocol message.
    ///
    /// Thin wrapper over [`Self::dispatch`] so the dispatch path can be
    /// exercised without a [`ProtocolConnection`], which has no public
    /// constructor.
    fn handle_protocol_message(&mut self, frame: &BytesMut) -> Option<BytesMut> {
        Self::dispatch(frame, &mut self.pending_range_reqs, self._peer_id, self.proto_version)
    }

    /// Route one inbound frame by message id, returning a frame to send back if
    /// the message requires a reply.
    ///
    /// There is deliberately no handshake gate here. The `bsc` subprotocol used
    /// to require `Capability` (0x00) as the first inbound frame, but bsc#3483
    /// removed the blocking read and bsc#3737 stops sending the packet
    /// altogether, so a peer's first frame is normally `Votes`. Rejecting that
    /// tore down the whole RLPx session — a satellite stream ending drops the
    /// `eth` primary with it — so 0x00 is now ignored exactly like geth's
    /// no-op `handleBscCap`.
    fn dispatch(
        frame: &BytesMut,
        pending_range_reqs: &mut PendingRangeReqs,
        peer_id: Option<PeerId>,
        proto_version: u64,
    ) -> Option<BytesMut> {
        let slice = frame.as_ref();
        let msg_id = slice[0];

        tracing::trace!(target: "bsc_protocol", msg_id = format_args!("{msg_id:#04x}"), "Processing message");
        match msg_id {
            x if x == BscProtoMessageId::Capability as u8 => {
                // Legacy handshake packet. Peers older than bsc#3737 still send
                // it; the version it carries was only ever a restatement of the
                // devp2p-negotiated version we already hold, so drop it.
                tracing::trace!(target: "bsc_protocol", peer = ?peer_id, "Ignoring legacy BSC capability message");
                None
            }
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
                match crate::node::network::blocks_by_range::decode_blocks_by_range(
                    &mut &slice[..],
                ) {
                    Ok(res) => {
                        tracing::debug!(
                            target: "bsc_protocol",
                            req_id = res.request_id,
                            blocks = res.blocks.len(),
                            "Received BlocksByRange"
                        );
                        if let Some((waiter, _)) = pending_range_reqs.remove(&res.request_id) {
                            let _ = waiter.send(Ok(res));
                        } else {
                            tracing::trace!(target: "bsc_protocol", "No waiter for request_id; dropping BlocksByRange");
                        }
                        None
                    }
                    Err(err) => {
                        // The decode error carries the request id when it was
                        // readable, so the pending waiter fails now instead of
                        // burning the full fetch timeout; the extra fields make
                        // the sender diagnosable in the field (issue #374).
                        tracing::warn!(
                            target: "bsc_protocol",
                            error = %err.error,
                            peer = ?peer_id,
                            proto_version,
                            frame_len = slice.len(),
                            request_id = ?err.request_id,
                            "Failed to decode BlocksByRangePacket"
                        );
                        if let Some(req_id) = err.request_id {
                            if let Some((waiter, _)) = pending_range_reqs.remove(&req_id) {
                                let _ = waiter.send(Err(format!(
                                    "failed to decode BlocksByRangePacket: {}",
                                    err.error
                                )));
                            }
                        }
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

            if let Some(response) = this.handle_protocol_message(&raw_frame) {
                return Poll::Ready(Some(response));
            }
            // No reply needed; loop back for more commands/incoming frames.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::consensus::parlia::{
        vote::{VoteData, VoteEnvelope},
        votes,
    };
    use alloy_primitives::{FixedBytes, B256};

    /// `ProtocolConnection` has no public constructor, so these tests drive
    /// [`BscProtocolConnection::dispatch`] directly. That is the function the
    /// deleted handshake gate used to sit in front of.
    fn dispatch(frame: &BytesMut) -> Option<BytesMut> {
        let mut pending = HashMap::new();
        BscProtocolConnection::dispatch(frame, &mut pending, None, 2)
    }

    fn frame_of(msg: impl Encodable) -> BytesMut {
        let mut buf = BytesMut::new();
        msg.encode(&mut buf);
        buf
    }

    fn cap_frame(protocol_version: u64) -> BytesMut {
        frame_of(BscCapPacket { protocol_version, extra: Bytes::from_static(&[0x00u8]) })
    }

    /// A synthetic vote. Signature/address are never verified on this path, and
    /// `target_number` is kept high so vote-pool pruning cannot evict it.
    fn vote(target_number: u64, target_hash: B256) -> VoteEnvelope {
        VoteEnvelope {
            vote_address: FixedBytes::<48>::repeat_byte(0xab),
            signature: FixedBytes::<96>::repeat_byte(0xcd),
            data: VoteData {
                source_number: target_number - 1,
                source_hash: B256::repeat_byte(0x11),
                target_number,
                target_hash,
            },
        }
    }

    // === The bsc#3737 peer generation ===

    /// A peer on bsc#3737 sends no capability packet at all, so its first frame
    /// is a vote. The old handshake gate answered that with `Poll::Ready(None)`,
    /// which ended the satellite stream and took the whole RLPx session (`eth`
    /// included) down with it. It must now be processed normally.
    #[test]
    fn votes_as_very_first_frame_are_processed() {
        let target_hash = B256::repeat_byte(0x5a);
        let before = votes::len();

        let reply = dispatch(&frame_of(VotesPacket(vec![vote(30_000_000, target_hash)])));

        assert!(reply.is_none(), "a votes frame needs no reply");
        assert_eq!(votes::len(), before + 1, "the vote must reach the pool");
    }

    /// Same peer generation, but the first frame is a range request. The old
    /// gate dropped the session here too, which is what broke fork recovery.
    #[test]
    fn get_blocks_by_range_as_very_first_frame_is_answered() {
        let req = GetBlocksByRangePacket {
            request_id: 0xfeed,
            start_block_height: 1,
            start_block_hash: B256::repeat_byte(0x22),
            count: 1,
        };

        let reply = dispatch(&frame_of(req)).expect("a range request must be answered");

        assert_eq!(reply[0], BscProtoMessageId::BlocksByRange as u8);
        let decoded =
            crate::node::network::blocks_by_range::decode_blocks_by_range(&mut &reply[..])
                .expect("reply must decode");
        assert_eq!(decoded.request_id, 0xfeed, "reply must echo the request id");
    }

    // === Legacy capability packet: tolerated, never fatal ===

    /// Peers older than bsc#3737 still send the packet. Ignore it, as geth's
    /// no-op `handleBscCap` does.
    #[test]
    fn legacy_capability_frame_is_ignored() {
        assert!(dispatch(&cap_frame(2)).is_none());
    }

    /// The old gate tore the session down on a version mismatch. The devp2p
    /// `Hello` exchange already fixed the version, so this packet's copy of it
    /// carries no authority and a disagreement must not be fatal.
    #[test]
    fn capability_frame_with_mismatched_version_is_ignored() {
        assert!(dispatch(&cap_frame(1)).is_none(), "version 1 cap on a v2 conn");
        assert!(dispatch(&cap_frame(99)).is_none(), "nonsense version");
    }

    /// The old gate also tore the session down when the packet failed to decode.
    /// We no longer decode it at all, so a truncated or garbage body is inert.
    #[test]
    fn malformed_capability_frame_is_ignored() {
        let mut truncated = cap_frame(2);
        truncated.truncate(2);
        assert!(dispatch(&truncated).is_none(), "truncated cap payload");

        let bare_id = BytesMut::from(&[BscProtoMessageId::Capability as u8][..]);
        assert!(dispatch(&bare_id).is_none(), "message id with no payload");
    }

    // === Unchanged behaviour, guarded against the refactor ===

    #[test]
    fn blocks_by_range_resolves_the_pending_waiter() {
        let (tx, rx) = oneshot::channel();
        let mut pending = HashMap::new();
        pending.insert(7u64, (tx, std::time::Instant::now()));

        let frame = frame_of(BlocksByRangePacket { request_id: 7, blocks: Vec::new() });
        let reply = BscProtocolConnection::dispatch(&frame, &mut pending, None, 2);

        assert!(reply.is_none(), "a range response needs no reply");
        assert!(pending.is_empty(), "the waiter must be consumed");
        let resolved = rx.blocking_recv().expect("waiter must be resolved");
        assert_eq!(resolved.expect("response must be Ok").request_id, 7);
    }

    #[test]
    fn unknown_message_id_is_ignored() {
        assert!(dispatch(&BytesMut::from(&[0x7fu8, 0xc0][..])).is_none());
    }

    /// We must keep *sending* the capability packet: nodes older than bsc#3483
    /// block on reading it. Guards the frame we put on the wire first.
    #[test]
    fn outbound_capability_frame_is_still_well_formed() {
        let encoded = BscProtocolConnection::encode_command(BscCommand::Capability {
            protocol_version: 2,
            extra: Bytes::from_static(&[0x00u8]),
        });

        assert_eq!(encoded[0], BscProtoMessageId::Capability as u8);
        let decoded = BscCapPacket::decode(&mut &encoded[..]).expect("must round-trip");
        assert_eq!(decoded.protocol_version, 2);
    }
}
