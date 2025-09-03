use alloy_rlp::{Decodable, Encodable, RlpDecodable, RlpEncodable};
use bytes::{BufMut, Bytes};

use crate::consensus::parlia::{vote::VoteEnvelope, votes};
use crate::node::network::bsc_protocol::protocol::proto::BscProtoMessageId;

/// BSC capability packet: version + extra RLP value (opaque), message id 0x00
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BscCapPacket {
    pub protocol_version: u64,
    pub extra: Bytes, // This holds raw RLP data, like Go's rlp.RawValue
}

impl Encodable for BscCapPacket {
    fn encode(&self, out: &mut dyn BufMut) {
        // Message ID should be sent as raw byte, not RLP-encoded
        out.put_u8(BscProtoMessageId::Capability as u8);
        
        // Encode as RLP list: [protocol_version, extra]
        // Extra is raw RLP data (like Go's rlp.RawValue), so insert directly
        let protocol_version_encoded = alloy_rlp::encode(self.protocol_version);
        
        // Calculate list payload length (protocol_version + raw extra data)
        let payload_length = protocol_version_encoded.len() + self.extra.len();
        
        // Encode list header  
        alloy_rlp::Header { list: true, payload_length }.encode(out);
        
        // Encode protocol version
        out.put_slice(&protocol_version_encoded);
        // Insert raw extra data directly (no additional RLP encoding)
        out.put_slice(&self.extra);
    }
}

impl Decodable for BscCapPacket {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        // Message ID is sent as raw byte, not RLP-encoded
        if buf.is_empty() {
            return Err(alloy_rlp::Error::InputTooShort);
        }
        let message_id = buf[0];
        *buf = &buf[1..];
        if message_id != (BscProtoMessageId::Capability as u8) {
            return Err(alloy_rlp::Error::Custom("Invalid message ID for BscCapPacket"));
        }
        
        // Decode RLP list: [protocol_version, extra]
        let header = alloy_rlp::Header::decode(buf)?;
        if !header.list {
            return Err(alloy_rlp::Error::UnexpectedString);
        }
        
        let protocol_version = u64::decode(buf)?;
        
        // Extra is raw RLP data - read remaining bytes directly
        let remaining_len = header.payload_length - 1; // -1 for protocol_version (single byte)
        if buf.len() < remaining_len {
            return Err(alloy_rlp::Error::InputTooShort);
        }
        let extra = Bytes::copy_from_slice(&buf[..remaining_len]);
        *buf = &buf[remaining_len..];
        
        Ok(Self { protocol_version, extra })
    }
}

/// VotesPacket carries a list of votes (message id 0x01)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VotesPacket(pub Vec<VoteEnvelope>);

/// Wrapper to match Go's RLP of struct{Votes []*VoteEnvelope}
#[derive(RlpEncodable, RlpDecodable)]
struct VotesWrapper(Vec<VoteEnvelope>);

impl Encodable for VotesPacket {
    fn encode(&self, out: &mut dyn BufMut) {
        // Message ID is a raw byte followed by bare []VoteEnvelope
        out.put_u8(BscProtoMessageId::Votes as u8);
        self.0.encode(out);
    }
}

impl Decodable for VotesPacket {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        // Message ID is a raw byte
        if buf.is_empty() {
            return Err(alloy_rlp::Error::InputTooShort);
        }
        let message_id = buf[0];
        *buf = &buf[1..];
        if message_id != (BscProtoMessageId::Votes as u8) {
            return Err(alloy_rlp::Error::Custom("Invalid message ID for VotesPacket"));
        }

        // First, try the bare []VoteEnvelope encoding (what we emit)
        let mut inner = *buf;
        if let Ok(votes) = Vec::<VoteEnvelope>::decode(&mut inner) {
            *buf = inner;
            return Ok(Self(votes));
        }

        // Fallback: accept Go-style wrapper: struct{Votes []*VoteEnvelope}
        let mut inner = *buf;
        if let Ok(VotesWrapper(votes)) = VotesWrapper::decode(&mut inner) {
            *buf = inner;
            return Ok(Self(votes));
        }

        Err(alloy_rlp::Error::Custom("Invalid votes payload"))
    }
}

/// Handle an incoming `VotesPacket` from a peer.
/// To avoid DoS from massive batches, only enqueue the first vote if present,
/// mirroring Geth's logic.
pub fn handle_votes_broadcast(packet: VotesPacket) {
    if let Some(first) = packet.0.into_iter().next() {
        votes::put_vote(first);
    }
}
