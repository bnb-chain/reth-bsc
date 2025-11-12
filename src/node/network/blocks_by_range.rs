use alloy_primitives::B256;
use alloy_rlp::{Decodable, Encodable, RlpDecodable, RlpEncodable};
use bytes::BufMut;

use crate::node::primitives::BscBlock;
use crate::node::network::bsc_protocol::protocol::proto::BscProtoMessageId;

/// Max range allowed in a single request, mirroring geth's constant.
pub const MAX_REQUEST_RANGE_BLOCKS_COUNT: u64 = 64;

/// GetBlocksByRange request packet (message id 0x02)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GetBlocksByRangePacket {
    pub request_id: u64,
    pub start_block_height: u64,
    pub start_block_hash: B256,
    pub count: u64,
}

impl Encodable for GetBlocksByRangePacket {
    fn encode(&self, out: &mut dyn BufMut) {
        // Prefix with the message ID, then encode struct as RLP list
        out.put_u8(BscProtoMessageId::GetBlocksByRange as u8);
        GetBlocksByRangePacketInner::from(self).encode(out);
    }
}

impl Decodable for GetBlocksByRangePacket {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        if buf.is_empty() {
            return Err(alloy_rlp::Error::InputTooShort);
        }
        let msg = buf[0];
        *buf = &buf[1..];
        if msg != (BscProtoMessageId::GetBlocksByRange as u8) {
            return Err(alloy_rlp::Error::Custom("Invalid message ID for GetBlocksByRangePacket"));
        }
        let inner = GetBlocksByRangePacketInner::decode(buf)?;
        Ok(inner.into())
    }
}

/// BlocksByRange response packet (message id 0x03)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlocksByRangePacket {
    pub request_id: u64,
    pub blocks: Vec<BscBlock>,
}

impl Encodable for BlocksByRangePacket {
    fn encode(&self, out: &mut dyn BufMut) {
        out.put_u8(BscProtoMessageId::BlocksByRange as u8);
        BlocksByRangePacketInner::from(self).encode(out);
    }
}

impl Decodable for BlocksByRangePacket {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        if buf.is_empty() {
            return Err(alloy_rlp::Error::InputTooShort);
        }
        let msg = buf[0];
        *buf = &buf[1..];
        if msg != (BscProtoMessageId::BlocksByRange as u8) {
            return Err(alloy_rlp::Error::Custom("Invalid message ID for BlocksByRangePacket"));
        }
        let inner = BlocksByRangePacketInner::decode(buf)?;
        Ok(inner.into())
    }
}

/// Build a best-effort response for a GetBlocksByRange request using only headers if needed.
///
/// Note: This currently constructs blocks with empty bodies if full data is not available
/// via global providers. It prioritizes `start_block_hash` if non-zero, otherwise uses
/// `start_block_height`. The traversal follows parent hashes up to `count` blocks.
pub fn build_blocks_by_range_response(req: &GetBlocksByRangePacket) -> BlocksByRangePacket {
    use crate::shared::{ get_cached_block_by_hash, get_cached_block_by_number };

    let mut blocks: Vec<BscBlock> = Vec::new();

    // Resolve starting block: only include full blocks (cached or provider). If not found, return empty.
    let mut current_block: Option<BscBlock> = if req.start_block_hash != B256::ZERO {
        get_cached_block_by_hash(&req.start_block_hash)
    } else {
        get_cached_block_by_number(req.start_block_height)
    };

    // Walk back by parents up to count
    let mut remaining = req.count.min(MAX_REQUEST_RANGE_BLOCKS_COUNT);
    while let (Some(block), r) = (current_block.clone(), remaining) {
        if r == 0 { break; }
        // Push the current full block
        blocks.push(block.clone());

        // Prepare next parent
        let parent_hash = block.header.parent_hash;
        current_block = if parent_hash != B256::ZERO {
            get_cached_block_by_hash(&parent_hash)
        } else { None };
        remaining -= 1;
    }

    let requested = req.count.min(MAX_REQUEST_RANGE_BLOCKS_COUNT) as usize;
    if blocks.len() < requested {
        tracing::debug!(
            target: "bsc_protocol",
            request_id = req.request_id,
            requested = requested,
            produced = blocks.len(),
            "Truncated BlocksByRange due to missing parent/body"
        );
    }

    BlocksByRangePacket { request_id: req.request_id, blocks }
}

// === RLP inner helpers (without message-id prefix) ===

#[derive(Debug, Clone, PartialEq, Eq, RlpEncodable, RlpDecodable)]
struct GetBlocksByRangePacketInner {
    request_id: u64,
    start_block_height: u64,
    start_block_hash: B256,
    count: u64,
}

impl From<&GetBlocksByRangePacket> for GetBlocksByRangePacketInner {
    fn from(v: &GetBlocksByRangePacket) -> Self {
        Self {
            request_id: v.request_id,
            start_block_height: v.start_block_height,
            start_block_hash: v.start_block_hash,
            count: v.count,
        }
    }
}

impl From<GetBlocksByRangePacketInner> for GetBlocksByRangePacket {
    fn from(v: GetBlocksByRangePacketInner) -> Self {
        Self {
            request_id: v.request_id,
            start_block_height: v.start_block_height,
            start_block_hash: v.start_block_hash,
            count: v.count,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, RlpEncodable, RlpDecodable)]
struct BlocksByRangePacketInner {
    request_id: u64,
    blocks: Vec<BscBlock>,
}

impl From<&BlocksByRangePacket> for BlocksByRangePacketInner {
    fn from(v: &BlocksByRangePacket) -> Self {
        Self { request_id: v.request_id, blocks: v.blocks.clone() }
    }
}

impl From<BlocksByRangePacketInner> for BlocksByRangePacket {
    fn from(v: BlocksByRangePacketInner) -> Self {
        Self { request_id: v.request_id, blocks: v.blocks }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::Header;
    use crate::BscBlockBody;
    use bytes::BytesMut;

    #[test]
    fn test_get_blocks_by_range_codec_roundtrip() {
        let req = GetBlocksByRangePacket {
            request_id: 42,
            start_block_height: 123,
            start_block_hash: B256::from([1u8; 32]),
            count: 5,
        };
        let mut bytes = BytesMut::new();
        req.encode(&mut bytes);
        let mut slice = bytes.as_ref();
        let dec = GetBlocksByRangePacket::decode(&mut slice).unwrap();
        assert_eq!(req, dec);

        // Response roundtrip with 1 block
        let b = BscBlock { header: Header::default(), body: BscBlockBody { inner: reth_ethereum_primitives::BlockBody::default(), sidecars: None } };
        let res = BlocksByRangePacket { request_id: 7, blocks: vec![b.clone()] };
        let mut bytes = BytesMut::new();
        res.encode(&mut bytes);
        let mut slice = bytes.as_ref();
        let dec = BlocksByRangePacket::decode(&mut slice).unwrap();
        assert_eq!(res.request_id, dec.request_id);
        assert_eq!(res.blocks.len(), dec.blocks.len());
        assert_eq!(res.blocks[0].header.hash_slow(), dec.blocks[0].header.hash_slow());
    }

    #[test]
    fn test_build_blocks_by_range_uses_cache() {
        // Clear cache to ensure test isolation
        crate::shared::clear_body_cache();

        // Build a 2-block chain: parent <- child
        let parent_header = Header { number: 1, ..Default::default() };
        let parent_block = BscBlock { header: parent_header, body: BscBlockBody { inner: reth_ethereum_primitives::BlockBody::default(), sidecars: None } };
        let parent_hash = parent_block.header.hash_slow();

        let child_header = Header { parent_hash, number: 2, ..Default::default() };
        let child_block = BscBlock { header: child_header, body: BscBlockBody { inner: reth_ethereum_primitives::BlockBody::default(), sidecars: None } };
        let child_hash = child_block.header.hash_slow();

        // Cache both blocks so builder can fetch full bodies from cache
        crate::shared::cache_full_block(parent_block.clone());
        crate::shared::cache_full_block(child_block.clone());

        let req = GetBlocksByRangePacket { request_id: 1, start_block_height: 0, start_block_hash: child_hash, count: 2 };
        let resp = build_blocks_by_range_response(&req);
        assert_eq!(resp.request_id, 1);
        assert_eq!(resp.blocks.len(), 2);
        // First should be child, second parent
        assert_eq!(resp.blocks[0].header.hash_slow(), child_hash);
        assert_eq!(resp.blocks[1].header.hash_slow(), parent_hash);
    }

    #[test]
    fn test_build_blocks_by_range_truncates_when_parent_missing() {
        // Clear cache to ensure test isolation
        crate::shared::clear_body_cache();

        // Build a 2-block chain but cache only the child
        let parent_header = Header::default();
        let parent_block = BscBlock { header: parent_header, body: BscBlockBody { inner: reth_ethereum_primitives::BlockBody::default(), sidecars: None } };
        let parent_hash = parent_block.header.hash_slow();

        let child_header = Header { parent_hash, ..Default::default() };
        let child_block = BscBlock { header: child_header, body: BscBlockBody { inner: reth_ethereum_primitives::BlockBody::default(), sidecars: None } };
        let child_hash = child_block.header.hash_slow();

        // Cache only child block
        crate::shared::cache_full_block(child_block.clone());

        let req = GetBlocksByRangePacket { request_id: 2, start_block_height: 0, start_block_hash: child_hash, count: 2 };
        let resp = build_blocks_by_range_response(&req);
        assert_eq!(resp.request_id, 2);
        // Parent missing => only child should be included
        assert_eq!(resp.blocks.len(), 1);
        assert_eq!(resp.blocks[0].header.hash_slow(), child_hash);
    }
}
