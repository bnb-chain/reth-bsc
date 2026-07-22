use alloy_primitives::B256;
use alloy_rlp::{Decodable, Encodable, RlpDecodable, RlpEncodable};
use alloy_rpc_types::Withdrawals;
use bytes::BufMut;

use crate::node::network::bsc_protocol::protocol::proto::BscProtoMessageId;
use crate::node::primitives::{BscBlock, BscBlobTransactionSidecar};
use crate::BscBlockBody;
use reth_ethereum_primitives::{BlockBody, TransactionSigned};

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
        decode_blocks_by_range(buf).map_err(|e| e.error)
    }
}

/// Failure from [`decode_blocks_by_range`], carrying the `request_id` when it
/// was readable before the failure point. The id is the first field of the
/// packet, so it survives almost any malformation of the block payload —
/// letting the stream fail the matching pending request immediately instead
/// of letting it burn the full fetch timeout.
#[derive(Debug)]
pub struct BlocksByRangeDecodeError {
    pub request_id: Option<u64>,
    pub error: alloy_rlp::Error,
}

/// Decode a BlocksByRange frame (msg-id byte + packet). Same semantics as
/// [`BlocksByRangePacket`]'s [`Decodable`] impl, but errors keep the
/// already-decoded `request_id` (see [`BlocksByRangeDecodeError`]).
pub fn decode_blocks_by_range(
    buf: &mut &[u8],
) -> Result<BlocksByRangePacket, BlocksByRangeDecodeError> {
    let no_id = |error| BlocksByRangeDecodeError { request_id: None, error };

    if buf.is_empty() {
        return Err(no_id(alloy_rlp::Error::InputTooShort));
    }
    let msg = buf[0];
    *buf = &buf[1..];
    if msg != (BscProtoMessageId::BlocksByRange as u8) {
        return Err(no_id(alloy_rlp::Error::Custom("Invalid message ID for BlocksByRangePacket")));
    }
    let head = alloy_rlp::Header::decode(buf).map_err(no_id)?;
    if !head.list {
        return Err(no_id(alloy_rlp::Error::UnexpectedString));
    }
    if buf.len() < head.payload_length {
        return Err(no_id(alloy_rlp::Error::InputTooShort));
    }
    let started = buf.len();

    let request_id = u64::decode(buf).map_err(no_id)?;
    let with_id = |error| BlocksByRangeDecodeError { request_id: Some(request_id), error };

    let blocks_head = alloy_rlp::Header::decode(buf).map_err(with_id)?;
    if !blocks_head.list {
        return Err(with_id(alloy_rlp::Error::UnexpectedString));
    }
    let blocks_started = buf.len();
    let mut blocks = Vec::new();
    while blocks_started - buf.len() < blocks_head.payload_length {
        blocks.push(decode_wire_block_data(buf).map_err(with_id)?);
    }
    if blocks_started - buf.len() != blocks_head.payload_length {
        return Err(with_id(alloy_rlp::Error::ListLengthMismatch {
            expected: blocks_head.payload_length,
            got: blocks_started - buf.len(),
        }));
    }

    // Packet-level tolerance is pure future-proofing: BAL-era geth only
    // extended the per-block encoding (see `decode_wire_block_data`), not the
    // packet itself, so this skip is a no-op against every known sender.
    skip_unknown_trailing(buf, head.payload_length, started, "BlocksByRangePacket")
        .map_err(with_id)?;
    Ok(BlocksByRangePacket { request_id, blocks })
}

/// Decode one wire `BlockData`
/// (`[header, txs, ommers, withdrawals?, sidecars?, <unknown trailing>*]`).
///
/// Known fields are decoded strictly; unknown trailing list elements are
/// skipped instead of failing the whole packet. geth-bsc releases between
/// bnb-chain/bsc#3374 (2025-09-30) and #3690 (2026-05-19) append a BEP-592
/// block access list here, which a strict 5-field decode rejects with
/// `unexpected list length` — starving fork recovery on any node whose only
/// live bsc/2 peers run those releases (issue #374). Tolerating trailing
/// data is confined to this wire path; `BscBlock`'s own RLP stays strict.
fn decode_wire_block_data(buf: &mut &[u8]) -> alloy_rlp::Result<BscBlock> {
    let head = alloy_rlp::Header::decode(buf)?;
    if !head.list {
        return Err(alloy_rlp::Error::UnexpectedString);
    }
    if buf.len() < head.payload_length {
        return Err(alloy_rlp::Error::InputTooShort);
    }
    let started = buf.len();

    let header = alloy_consensus::Header::decode(buf)?;
    let transactions = Vec::<TransactionSigned>::decode(buf)?;
    let ommers = Vec::<alloy_consensus::Header>::decode(buf)?;
    let mut withdrawals = None;
    let mut sidecars = None;
    if started - buf.len() < head.payload_length {
        withdrawals = Some(Withdrawals::decode(buf)?);
    }
    if started - buf.len() < head.payload_length {
        sidecars = Some(Vec::<BscBlobTransactionSidecar>::decode(buf)?);
    }

    // BAL-era BlockData (geth v1.6.2–v1.7.5) is handled HERE: those releases
    // append a 6th element — the BEP-592 block access list — after
    // `sidecars`. It reaches this point as unconsumed trailing bytes of the
    // block's list and is skipped, so the five known fields above decode the
    // block exactly as if the BAL were absent. Any future trailing extension
    // takes the same path.
    skip_unknown_trailing(buf, head.payload_length, started, "BlockData")?;
    Ok(BscBlock {
        header,
        body: BscBlockBody { inner: BlockBody { transactions, ommers, withdrawals }, sidecars },
    })
}

/// Advance `buf` past any bytes left in the enclosing list after the known
/// fields were decoded. Errors only if the fields overran the declared
/// payload (a genuinely malformed message).
fn skip_unknown_trailing(
    buf: &mut &[u8],
    payload_length: usize,
    started: usize,
    what: &'static str,
) -> alloy_rlp::Result<()> {
    let consumed = started - buf.len();
    if consumed > payload_length {
        return Err(alloy_rlp::Error::ListLengthMismatch { expected: payload_length, got: consumed });
    }
    let extra = payload_length - consumed;
    if extra > 0 {
        tracing::trace!(
            target: "bsc_protocol",
            what,
            extra_bytes = extra,
            "ignoring unknown trailing wire fields"
        );
        *buf = &buf[extra..];
    }
    Ok(())
}

/// Pure variant of [`build_blocks_by_range_response`] parameterised by a block
/// lookup. `lookup(hash, number_opt)` returns `Some(block)` when the block is
/// available. The first call receives `number_opt = Some(start_block_height)`
/// only when `start_block_hash == B256::ZERO`; subsequent hops always pass the
/// parent hash with `number_opt = None`. Exposed `pub(crate)` for testing.
pub(crate) fn build_blocks_by_range_response_with<F>(
    req: &GetBlocksByRangePacket,
    mut lookup: F,
) -> BlocksByRangePacket
where
    F: FnMut(&B256, Option<u64>) -> Option<BscBlock>,
{
    let mut blocks: Vec<BscBlock> = Vec::new();

    let mut current_block: Option<BscBlock> = if req.start_block_hash != B256::ZERO {
        lookup(&req.start_block_hash, None)
    } else {
        lookup(&B256::ZERO, Some(req.start_block_height))
    };

    let mut remaining = req.count.min(MAX_REQUEST_RANGE_BLOCKS_COUNT);
    while let (Some(block), r) = (current_block.clone(), remaining) {
        if r == 0 {
            break;
        }
        blocks.push(block.clone());

        let parent_hash = block.header.parent_hash;
        current_block =
            if parent_hash != B256::ZERO { lookup(&parent_hash, None) } else { None };
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

/// Build a response for a `GetBlocksByRange` request using the BODY_CACHE
/// first, then the installed [`crate::shared::FullBlockProvider`]
/// (DB / canonical state).
pub fn build_blocks_by_range_response(req: &GetBlocksByRangePacket) -> BlocksByRangePacket {
    use crate::shared::{
        get_cached_block_by_hash, get_cached_block_by_number, get_full_block_provider,
    };

    let provider = get_full_block_provider();
    build_blocks_by_range_response_with(req, |hash, number_opt| match number_opt {
        Some(num) => get_cached_block_by_number(num)
            .or_else(|| provider.as_ref().and_then(|p| p.block_by_number(num))),
        None => get_cached_block_by_hash(hash)
            .or_else(|| provider.as_ref().and_then(|p| p.block_by_hash(hash))),
    })
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

/// Encode-only shape of the response packet. Decoding deliberately does NOT
/// go through a derived `RlpDecodable` here — the derive's strict
/// consumed-equals-declared check is exactly what rejected BAL-era peers
/// (issue #374); the tolerant [`decode_blocks_by_range`] is the only decode
/// path.
#[derive(Debug, Clone, PartialEq, Eq, RlpEncodable)]
struct BlocksByRangePacketInner {
    request_id: u64,
    blocks: Vec<BscBlock>,
}

impl From<&BlocksByRangePacket> for BlocksByRangePacketInner {
    fn from(v: &BlocksByRangePacket) -> Self {
        // Geth decodes sidecar-carrying `BlocksByRangePacket.Blocks[i].Withdrawals` as a list type.
        // Ensure absent withdrawals are encoded as an empty list (`[]`) instead of RLP null (`0x80`)
        // only when sidecars are present, to preserve pre-Cancun semantics for historical blocks.
        let blocks = v
            .blocks
            .iter()
            .cloned()
            .map(|mut block| {
                if block.body.sidecars.is_some() && block.body.inner.withdrawals.is_none() {
                    block.body.inner.withdrawals = Some(Withdrawals::default());
                }
                block
            })
            .collect();
        Self { request_id: v.request_id, blocks }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::primitives::BscBlobTransactionSidecar;
    use crate::BscBlockBody;
    use alloy_consensus::Header;
    use alloy_rlp::RlpDecodable;
    use reth_ethereum_primitives::TransactionSigned;
    use bytes::BytesMut;
    use std::collections::HashMap;

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
        let b = BscBlock {
            header: Header::default(),
            body: BscBlockBody {
                inner: reth_ethereum_primitives::BlockBody::default(),
                sidecars: None,
            },
        };
        let res = BlocksByRangePacket { request_id: 7, blocks: vec![b.clone()] };
        let mut bytes = BytesMut::new();
        res.encode(&mut bytes);
        let mut slice = bytes.as_ref();
        let dec = BlocksByRangePacket::decode(&mut slice).unwrap();
        assert_eq!(res.request_id, dec.request_id);
        assert_eq!(res.blocks.len(), dec.blocks.len());
        assert_eq!(res.blocks[0].header.hash_slow(), dec.blocks[0].header.hash_slow());
    }

    #[derive(Debug, RlpDecodable)]
    #[rlp(trailing)]
    struct BlockWithListWithdrawals {
        header: Header,
        transactions: Vec<TransactionSigned>,
        ommers: Vec<Header>,
        withdrawals: Withdrawals,
        sidecars: Option<Vec<BscBlobTransactionSidecar>>,
    }

    #[derive(Debug, RlpDecodable)]
    #[rlp(trailing)]
    struct BlocksByRangeInnerWithListWithdrawals {
        request_id: u64,
        blocks: Vec<BlockWithListWithdrawals>,
    }

    #[test]
    fn test_blocks_by_range_encodes_absent_withdrawals_as_empty_list_when_sidecars_present() {
        let block = BscBlock {
            header: Header::default(),
            body: BscBlockBody {
                inner: reth_ethereum_primitives::BlockBody::default(),
                sidecars: Some(Vec::new()),
            },
        };
        let resp = BlocksByRangePacket { request_id: 11, blocks: vec![block] };

        let mut bytes = BytesMut::new();
        resp.encode(&mut bytes);

        // Strip message id (0x03), then decode payload as a type that requires withdrawals to be a list.
        let mut payload = &bytes[1..];
        let decoded = BlocksByRangeInnerWithListWithdrawals::decode(&mut payload).unwrap();

        assert_eq!(decoded.request_id, 11);
        assert_eq!(decoded.blocks.len(), 1);
        assert_eq!(decoded.blocks[0].header.number, 0);
        assert!(decoded.blocks[0].transactions.is_empty());
        assert!(decoded.blocks[0].ommers.is_empty());
        assert!(decoded.blocks[0].withdrawals.is_empty());
        assert!(decoded.blocks[0].sidecars.is_some());
    }

    #[test]
    fn test_blocks_by_range_keeps_absent_withdrawals_when_no_sidecars() {
        let block = BscBlock {
            header: Header::default(),
            body: BscBlockBody {
                inner: reth_ethereum_primitives::BlockBody::default(),
                sidecars: None,
            },
        };
        let resp = BlocksByRangePacket { request_id: 12, blocks: vec![block] };

        let mut bytes = BytesMut::new();
        resp.encode(&mut bytes);

        let mut payload = bytes.as_ref();
        let decoded = BlocksByRangePacket::decode(&mut payload).unwrap();

        assert_eq!(decoded.request_id, 12);
        assert_eq!(decoded.blocks.len(), 1);
        assert!(decoded.blocks[0].body.inner.withdrawals.is_none());
        assert!(decoded.blocks[0].body.sidecars.is_none());
    }

    #[test]
    fn test_build_blocks_by_range_uses_cache() {
        // Clear cache to ensure test isolation
        crate::shared::clear_body_cache();

        // Build a 2-block chain: parent <- child
        let parent_header = Header { number: 1, ..Default::default() };
        let parent_block = BscBlock {
            header: parent_header,
            body: BscBlockBody {
                inner: reth_ethereum_primitives::BlockBody::default(),
                sidecars: None,
            },
        };
        let parent_hash = parent_block.header.hash_slow();

        let child_header = Header { parent_hash, number: 2, ..Default::default() };
        let child_block = BscBlock {
            header: child_header,
            body: BscBlockBody {
                inner: reth_ethereum_primitives::BlockBody::default(),
                sidecars: None,
            },
        };
        let child_hash = child_block.header.hash_slow();

        // Cache both blocks so builder can fetch full bodies from cache
        crate::shared::cache_full_block(parent_block.clone());
        crate::shared::cache_full_block(child_block.clone());

        let req = GetBlocksByRangePacket {
            request_id: 1,
            start_block_height: 0,
            start_block_hash: child_hash,
            count: 2,
        };
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
        let parent_block = BscBlock {
            header: parent_header,
            body: BscBlockBody {
                inner: reth_ethereum_primitives::BlockBody::default(),
                sidecars: None,
            },
        };
        let parent_hash = parent_block.header.hash_slow();

        let child_header = Header { parent_hash, ..Default::default() };
        let child_block = BscBlock {
            header: child_header,
            body: BscBlockBody {
                inner: reth_ethereum_primitives::BlockBody::default(),
                sidecars: None,
            },
        };
        let child_hash = child_block.header.hash_slow();

        // Cache only child block
        crate::shared::cache_full_block(child_block.clone());

        let req = GetBlocksByRangePacket {
            request_id: 2,
            start_block_height: 0,
            start_block_hash: child_hash,
            count: 2,
        };
        let resp = build_blocks_by_range_response(&req);
        assert_eq!(resp.request_id, 2);
        // Parent missing => only child should be included
        assert_eq!(resp.blocks.len(), 1);
        assert_eq!(resp.blocks[0].header.hash_slow(), child_hash);
    }

    fn mk_block_t3(number: u64, parent: B256) -> BscBlock {
        BscBlock {
            header: Header { number, parent_hash: parent, ..Default::default() },
            body: BscBlockBody {
                inner: reth_ethereum_primitives::BlockBody::default(),
                sidecars: None,
            },
        }
    }

    #[test]
    fn build_response_falls_back_to_provider_lookup_on_cache_miss() {
        // Simulate a 4-block parent chain only available via the "provider" map.
        let b3 = mk_block_t3(3, B256::ZERO);
        let b2 = mk_block_t3(2, b3.header.hash_slow());
        let b1 = mk_block_t3(1, b2.header.hash_slow());
        let b0 = mk_block_t3(0, b1.header.hash_slow());

        let mut by_hash: HashMap<B256, BscBlock> = HashMap::new();
        by_hash.insert(b0.header.hash_slow(), b0.clone());
        by_hash.insert(b1.header.hash_slow(), b1.clone());
        by_hash.insert(b2.header.hash_slow(), b2.clone());
        by_hash.insert(b3.header.hash_slow(), b3.clone());

        let req = GetBlocksByRangePacket {
            request_id: 42,
            start_block_height: 0,
            start_block_hash: b0.header.hash_slow(),
            count: 4,
        };
        let resp =
            build_blocks_by_range_response_with(&req, |h, _| by_hash.get(h).cloned());

        assert_eq!(resp.request_id, 42);
        assert_eq!(resp.blocks.len(), 4, "should walk the full chain via provider fallback");
        assert_eq!(resp.blocks[0].header.number, 0);
        assert_eq!(resp.blocks[3].header.number, 3);
    }

    #[test]
    fn build_response_returns_empty_when_start_block_unknown() {
        let req = GetBlocksByRangePacket {
            request_id: 7,
            start_block_height: 0,
            start_block_hash: B256::repeat_byte(0xaa),
            count: 4,
        };
        let resp = build_blocks_by_range_response_with(&req, |_, _| None);
        assert!(resp.blocks.is_empty());
    }

    #[test]
    fn build_response_looks_up_by_number_when_hash_is_zero() {
        let b0 = mk_block_t3(10, B256::ZERO);
        let req = GetBlocksByRangePacket {
            request_id: 9,
            start_block_height: 10,
            start_block_hash: B256::ZERO,
            count: 1,
        };
        let b0_clone = b0.clone();
        let resp = build_blocks_by_range_response_with(&req, move |_, n| {
            if n == Some(10) { Some(b0_clone.clone()) } else { None }
        });
        assert_eq!(resp.blocks.len(), 1);
        assert_eq!(resp.blocks[0].header.number, 10);
    }

    #[test]
    fn build_response_returns_empty_when_cache_miss_and_no_provider_installed() {
        // Pin the public wrapper's fallback wiring: with the broadcast cache cleared
        // and no FullBlockProvider installed in this test process, a request for an
        // unknown start block must return an empty response instead of walking a
        // stale entry. Protects against future refactors dropping the
        // get_full_block_provider() plumbing in build_blocks_by_range_response.
        crate::shared::clear_body_cache();

        let req = GetBlocksByRangePacket {
            request_id: 99,
            start_block_height: 0,
            start_block_hash: B256::repeat_byte(0x55),
            count: 4,
        };
        let resp = build_blocks_by_range_response(&req);
        assert_eq!(resp.request_id, 99);
        assert!(
            resp.blocks.is_empty(),
            "no cache entry + no FullBlockProvider installed must yield empty response"
        );
    }

    // ---- BEP-592 BAL-era interop (issue #374) ----
    //
    // geth-bsc between bnb-chain/bsc#3374 (2025-09-30) and #3690 (2026-05-19)
    // encoded `BlockData` with a 6th trailing element:
    //   [header, txs, uncles, withdrawals, sidecars, BAL]
    // where BAL is `BlockAccessListEncode { version, number, hash, sign_data,
    // accounts }`. `NewBlockData` attached it unconditionally (no bsc/3
    // gating), so bsc/2 peers receive it too whenever the serving geth holds
    // a BAL for the block.

    #[derive(Debug, RlpEncodable)]
    struct BalStorageItem {
        tx_index: u32,
        dirty: bool,
        key: B256,
    }

    #[derive(Debug, RlpEncodable)]
    struct BalAccount {
        tx_index: u32,
        address: alloy_primitives::Address,
        storage_items: Vec<BalStorageItem>,
    }

    /// Mirrors go-bsc `types.BlockAccessListEncode` (BEP-592 era).
    #[derive(Debug, RlpEncodable)]
    struct BalEncode {
        version: u32,
        number: u64,
        hash: B256,
        sign_data: alloy_primitives::Bytes,
        accounts: Vec<BalAccount>,
    }

    /// Mirrors go-bsc BAL-era `bsc.BlockData` (6 elements). Go's
    /// `rlp:"optional"` semantics force the earlier optionals (withdrawals,
    /// sidecars) to be present (as empty lists) whenever BAL is attached.
    #[derive(Debug, RlpEncodable)]
    struct BalEraBlockData {
        header: Header,
        transactions: Vec<TransactionSigned>,
        ommers: Vec<Header>,
        withdrawals: Withdrawals,
        sidecars: Vec<BscBlobTransactionSidecar>,
        bal: BalEncode,
    }

    #[derive(Debug, RlpEncodable)]
    struct BalEraPacketInner {
        request_id: u64,
        blocks: Vec<BalEraBlockData>,
    }

    fn dummy_bal(number: u64) -> BalEncode {
        BalEncode {
            version: 1,
            number,
            hash: B256::repeat_byte(0xbb),
            sign_data: alloy_primitives::Bytes::from(vec![0u8; 65]),
            accounts: vec![BalAccount {
                tx_index: 0,
                address: alloy_primitives::Address::repeat_byte(0xaa),
                storage_items: vec![
                    BalStorageItem { tx_index: 0, dirty: true, key: B256::repeat_byte(0x01) },
                    BalStorageItem { tx_index: 0, dirty: false, key: B256::repeat_byte(0x02) },
                ],
            }],
        }
    }

    #[test]
    fn blocks_by_range_ignores_bal_era_blockdata_trailing_field() {
        // Regression for issue #374: before the tolerant wire decode this
        // exact packet failed with `unexpected list length (got …, expected …)`
        // and starved fork recovery. It must now decode, with the BAL ignored.
        let packet = BalEraPacketInner {
            request_id: 374,
            blocks: vec![BalEraBlockData {
                header: Header::default(),
                transactions: Vec::new(),
                ommers: Vec::new(),
                withdrawals: Withdrawals::default(),
                sidecars: Vec::new(),
                bal: dummy_bal(1),
            }],
        };
        let mut bytes = BytesMut::new();
        bytes.put_u8(BscProtoMessageId::BlocksByRange as u8);
        packet.encode(&mut bytes);

        let mut slice = bytes.as_ref();
        let decoded = BlocksByRangePacket::decode(&mut slice)
            .expect("BAL-era 6-element BlockData must decode with the BAL ignored");
        assert_eq!(decoded.request_id, 374);
        assert_eq!(decoded.blocks.len(), 1);
        assert_eq!(
            decoded.blocks[0].header.hash_slow(),
            Header::default().hash_slow(),
            "header must survive the tolerant decode intact"
        );
        assert_eq!(decoded.blocks[0].body.inner.withdrawals, Some(Withdrawals::default()));
        assert_eq!(decoded.blocks[0].body.sidecars, Some(Vec::new()));
    }

    #[test]
    fn blocks_by_range_ignores_packet_level_trailing_fields() {
        // Same tolerance one level up: unknown elements after `blocks` in the
        // packet list must not fail the decode either.
        #[derive(Debug, RlpEncodable)]
        struct PacketWithTrailing {
            request_id: u64,
            blocks: Vec<BalEraBlockData>,
            future_field: alloy_primitives::Bytes,
        }
        let packet = PacketWithTrailing {
            request_id: 99,
            blocks: vec![BalEraBlockData {
                header: Header::default(),
                transactions: Vec::new(),
                ommers: Vec::new(),
                withdrawals: Withdrawals::default(),
                sidecars: Vec::new(),
                bal: dummy_bal(2),
            }],
            future_field: alloy_primitives::Bytes::from(vec![0x42u8; 33]),
        };
        let mut bytes = BytesMut::new();
        bytes.put_u8(BscProtoMessageId::BlocksByRange as u8);
        packet.encode(&mut bytes);

        let mut slice = bytes.as_ref();
        let decoded = BlocksByRangePacket::decode(&mut slice)
            .expect("packet-level trailing fields must be ignored");
        assert_eq!(decoded.request_id, 99);
        assert_eq!(decoded.blocks.len(), 1);
    }

    #[test]
    fn blocks_by_range_still_rejects_truncated_packet() {
        // Tolerance must not become acceptance of garbage: a packet whose
        // fields overrun the declared list payload still fails.
        let packet = BalEraPacketInner {
            request_id: 7,
            blocks: vec![BalEraBlockData {
                header: Header::default(),
                transactions: Vec::new(),
                ommers: Vec::new(),
                withdrawals: Withdrawals::default(),
                sidecars: Vec::new(),
                bal: dummy_bal(3),
            }],
        };
        let mut bytes = BytesMut::new();
        bytes.put_u8(BscProtoMessageId::BlocksByRange as u8);
        packet.encode(&mut bytes);
        let truncated = &bytes[..bytes.len() - 10];
        let mut slice = truncated;
        BlocksByRangePacket::decode(&mut slice)
            .expect_err("truncated packet must still fail to decode");
    }

    #[test]
    fn decode_error_carries_request_id_for_fail_fast() {
        // A frame whose body is garbage after the request id fails to decode,
        // but the error still yields the id — so the stream can fail the
        // pending waiter immediately instead of waiting out the fetch timeout.
        #[derive(Debug, RlpEncodable)]
        struct GarbagePacket {
            request_id: u64,
            junk: alloy_primitives::Bytes,
        }
        let mut bytes = BytesMut::new();
        bytes.put_u8(BscProtoMessageId::BlocksByRange as u8);
        GarbagePacket { request_id: 4242, junk: alloy_primitives::Bytes::from(vec![7u8; 16]) }
            .encode(&mut bytes);
        let err = decode_blocks_by_range(&mut bytes.as_ref())
            .expect_err("garbage block payload must fail to decode");
        assert_eq!(err.request_id, Some(4242));

        // Failures before the request id is reachable carry no id.
        let err = decode_blocks_by_range(&mut &[0xffu8, 0xc0][..])
            .expect_err("wrong message id must fail");
        assert_eq!(err.request_id, None);
        let err = decode_blocks_by_range(&mut &[][..]).expect_err("empty frame must fail");
        assert_eq!(err.request_id, None);
    }

    #[test]
    fn blocks_by_range_accepts_five_element_blockdata_control() {
        // Control: identical shape minus the BAL element (what post-#3690 geth
        // sends, withdrawals/sidecars present-but-empty) decodes fine, proving
        // the rejection above is caused by the trailing BAL specifically.
        #[derive(Debug, RlpEncodable)]
        struct FiveElementBlockData {
            header: Header,
            transactions: Vec<TransactionSigned>,
            ommers: Vec<Header>,
            withdrawals: Withdrawals,
            sidecars: Vec<BscBlobTransactionSidecar>,
        }
        #[derive(Debug, RlpEncodable)]
        struct FiveElementPacketInner {
            request_id: u64,
            blocks: Vec<FiveElementBlockData>,
        }

        let packet = FiveElementPacketInner {
            request_id: 375,
            blocks: vec![FiveElementBlockData {
                header: Header::default(),
                transactions: Vec::new(),
                ommers: Vec::new(),
                withdrawals: Withdrawals::default(),
                sidecars: Vec::new(),
            }],
        };
        let mut bytes = BytesMut::new();
        bytes.put_u8(BscProtoMessageId::BlocksByRange as u8);
        packet.encode(&mut bytes);

        let mut slice = bytes.as_ref();
        let decoded = BlocksByRangePacket::decode(&mut slice)
            .expect("5-element BlockData must decode");
        assert_eq!(decoded.request_id, 375);
        assert_eq!(decoded.blocks.len(), 1);
    }
}
