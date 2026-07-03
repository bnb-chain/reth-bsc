#![allow(clippy::owned_cow)]
use alloy_consensus::{BlobTransactionSidecar, Header};
use alloy_eips::eip4895::Withdrawals;
use alloy_primitives::B256;
use alloy_rlp::{Decodable, Encodable};
use reth_ethereum_primitives::{BlockBody, Receipt, TransactionSigned};
use reth_primitives_traits::{Block, BlockBody as BlockBodyTrait, InMemorySize, NodePrimitives};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;

/// Primitive types for BSC.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct BscPrimitives;

impl NodePrimitives for BscPrimitives {
    type Block = BscBlock;
    type BlockHeader = Header;
    type BlockBody = BscBlockBody;
    type SignedTx = TransactionSigned;
    type Receipt = Receipt;
}

/// BSC representation of a EIP-4844 sidecar.
///
/// RLP encoding matches go-bsc's `BlobSidecar` nested layout:
///   `[[blobs, commitments, proofs], block_number, block_hash, tx_index, tx_hash]`
/// The inner `BlobTxSidecar` is encoded as a sub-list, matching the Go struct
/// `BlobSidecar { BlobTxSidecar, BlockNumber, BlockHash, TxIndex, TxHash }`.
///
/// JSON encoding is hand-written (see the `Serialize`/`Deserialize` impls below) rather than
/// derived: go-bsc's `BlobSidecar` has custom `MarshalJSON`/`UnmarshalJSON` that nests the inner
/// sidecar (including `version`, which the Go struct carries on `BlobTxSidecar`, not `BlobSidecar`
/// itself) under a `blobSidecar` key, alongside hex-quantity `blockNumber`/`txIndex` — a shape a
/// plain `#[derive(Serialize, Deserialize)]` with field renaming cannot produce.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BscBlobTransactionSidecar {
    pub inner: BlobTransactionSidecar,
    pub block_number: u64,
    pub block_hash: B256,
    pub tx_index: u64,
    pub tx_hash: B256,
    /// Sidecar proof version: `0` = legacy EIP-4844 blob proofs, `1` = EIP-7594 cell proofs.
    /// Mirrors go-bsc `BlobTxSidecar.Version`, which is tagged `rlp:"-"` — excluded from the RLP
    /// encoding (and therefore from the BidBlock hash) and carried only on the JSON wire, nested
    /// under `blobSidecar` alongside `blobs`/`commitments`/`proofs`.
    pub version: u8,
}

/// Hex-quantity string encoding for `u64`, matching go-ethereum's `hexutil.EncodeUint64` /
/// `hexutil.Big` — used for `BlobSidecar`'s `blockNumber` and `txIndex` on the JSON wire, which
/// are quantities (`"0x64"`), not plain JSON numbers.
mod hex_quantity {
    use serde::{de::Error as _, Deserialize, Deserializer, Serializer};

    pub(super) fn serialize<S: Serializer>(value: &u64, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&format!("0x{value:x}"))
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
        let s = String::deserialize(deserializer)?;
        let digits = s
            .strip_prefix("0x")
            .ok_or_else(|| D::Error::custom("hex string without 0x prefix"))?;
        u64::from_str_radix(digits, 16).map_err(D::Error::custom)
    }
}

/// JSON wire shape of the inner `BlobTxSidecar`: `blobs`/`commitments`/`proofs` plus `version`,
/// nested under the outer struct's `blobSidecar` key. Kept separate from
/// [`alloy_consensus::BlobTransactionSidecar`], which has no `version` field.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BlobTxSidecarWire<Blobs, Commitments, Proofs> {
    #[serde(default)]
    version: u8,
    blobs: Blobs,
    commitments: Commitments,
    proofs: Proofs,
}

/// JSON wire shape of [`BscBlobTransactionSidecar`], matching go-bsc's `BlobSidecar.MarshalJSON` /
/// `UnmarshalJSON`.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BlobSidecarWire<Sidecar> {
    blob_sidecar: Sidecar,
    #[serde(with = "hex_quantity")]
    block_number: u64,
    block_hash: B256,
    #[serde(with = "hex_quantity")]
    tx_index: u64,
    tx_hash: B256,
}

impl Serialize for BscBlobTransactionSidecar {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        BlobSidecarWire {
            blob_sidecar: BlobTxSidecarWire {
                version: self.version,
                blobs: &self.inner.blobs,
                commitments: &self.inner.commitments,
                proofs: &self.inner.proofs,
            },
            block_number: self.block_number,
            block_hash: self.block_hash,
            tx_index: self.tx_index,
            tx_hash: self.tx_hash,
        }
        .serialize(serializer)
    }
}

/// Hex-decode a list of hex strings into `FixedBytes<N>`, one at a time via a heap buffer.
///
/// Deliberately does NOT deserialize the blob fields as `Vec<FixedBytes<131072>>` directly: a
/// `Blob` is 128 KiB *by value*, and serde_json's deeply-nested generic `deserialize` frames move
/// those values up the call stack. In an unoptimized (debug) build those moves are not elided, so
/// several 128 KiB copies are live at once and a single blob overflows a tokio worker's 2 MiB
/// stack — a remotely-triggerable validator crash via `mev_sendBidBlock`. Decoding from hex
/// strings (which live on the heap) in this flat loop keeps peak stack to one blob-sized temporary
/// regardless of build profile. (Release elides the moves and fits in 2 MiB even at 6 blobs, but
/// relying on the optimizer for memory safety is exactly the fragility this avoids.)
fn hex_decode_fixed<const N: usize, E: serde::de::Error>(
    hexes: Vec<String>,
) -> Result<Vec<alloy_primitives::FixedBytes<N>>, E> {
    let mut out = Vec::with_capacity(hexes.len());
    for h in hexes {
        let s = h.strip_prefix("0x").unwrap_or(&h);
        let bytes = alloy_primitives::hex::decode(s)
            .map_err(|e| E::custom(format!("invalid hex: {e}")))?;
        if bytes.len() != N {
            return Err(E::custom(format!("expected {N} bytes, got {}", bytes.len())));
        }
        out.push(alloy_primitives::FixedBytes::<N>::from_slice(&bytes));
    }
    Ok(out)
}

impl<'de> Deserialize<'de> for BscBlobTransactionSidecar {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire =
            BlobSidecarWire::<BlobTxSidecarWire<Vec<String>, Vec<String>, Vec<String>>>::deserialize(
                deserializer,
            )?;
        Ok(Self {
            inner: BlobTransactionSidecar {
                blobs: hex_decode_fixed(wire.blob_sidecar.blobs)?,
                commitments: hex_decode_fixed(wire.blob_sidecar.commitments)?,
                proofs: hex_decode_fixed(wire.blob_sidecar.proofs)?,
            },
            block_number: wire.block_number,
            block_hash: wire.block_hash,
            tx_index: wire.tx_index,
            tx_hash: wire.tx_hash,
            version: wire.blob_sidecar.version,
        })
    }
}

impl Encodable for BscBlobTransactionSidecar {
    fn encode(&self, out: &mut dyn bytes::BufMut) {
        // Inner BlobTxSidecar encoded as a nested sub-list: [blobs, commitments, proofs]
        let inner_fields_len = self.inner.rlp_encoded_fields_length();
        let inner_header =
            alloy_rlp::Header { list: true, payload_length: inner_fields_len };

        let payload_len = inner_header.length() + inner_fields_len
            + self.block_number.length()
            + self.block_hash.length()
            + self.tx_index.length()
            + self.tx_hash.length();
        alloy_rlp::Header { list: true, payload_length: payload_len }.encode(out);
        inner_header.encode(out);
        self.inner.rlp_encode_fields(out);
        self.block_number.encode(out);
        self.block_hash.encode(out);
        self.tx_index.encode(out);
        self.tx_hash.encode(out);
    }

    fn length(&self) -> usize {
        let inner_fields_len = self.inner.rlp_encoded_fields_length();
        let inner_header =
            alloy_rlp::Header { list: true, payload_length: inner_fields_len };

        let payload_len = inner_header.length() + inner_fields_len
            + self.block_number.length()
            + self.block_hash.length()
            + self.tx_index.length()
            + self.tx_hash.length();
        alloy_rlp::Header { list: true, payload_length: payload_len }.length() + payload_len
    }
}

impl Decodable for BscBlobTransactionSidecar {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let header = alloy_rlp::Header::decode(buf)?;
        if !header.list {
            return Err(alloy_rlp::Error::UnexpectedString);
        }
        let remaining_before = buf.len();

        // Decode inner BlobTxSidecar from a nested sub-list
        let inner_header = alloy_rlp::Header::decode(buf)?;
        if !inner_header.list {
            return Err(alloy_rlp::Error::UnexpectedString);
        }
        let inner_remaining = buf.len();
        let inner = BlobTransactionSidecar::rlp_decode_fields(buf)?;
        let inner_consumed = inner_remaining - buf.len();
        if inner_consumed != inner_header.payload_length {
            return Err(alloy_rlp::Error::UnexpectedLength);
        }

        let block_number = u64::decode(buf)?;
        let block_hash = B256::decode(buf)?;
        let tx_index = u64::decode(buf)?;
        let tx_hash = B256::decode(buf)?;
        let consumed = remaining_before - buf.len();
        if consumed != header.payload_length {
            return Err(alloy_rlp::Error::UnexpectedLength);
        }
        // `version` is rlp:"-" in go-bsc: not present in the RLP, defaults to 0 (legacy proofs).
        Ok(Self { inner, block_number, block_hash, tx_index, tx_hash, version: 0 })
    }
}

/// Block body for BSC. It is equivalent to Ethereum [`BlockBody`] but additionally stores sidecars
/// for blob transactions.
#[derive(
    Debug,
    Clone,
    Default,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    derive_more::Deref,
    derive_more::DerefMut,
)]
pub struct BscBlockBody {
    #[serde(flatten)]
    #[deref]
    #[deref_mut]
    pub inner: BlockBody,
    pub sidecars: Option<Vec<BscBlobTransactionSidecar>>,
}

impl InMemorySize for BscBlockBody {
    fn size(&self) -> usize {
        self.inner.size() +
            self.sidecars
                .as_ref()
                .map_or(0, |s| s.capacity() * core::mem::size_of::<BscBlobTransactionSidecar>())
    }
}

impl BlockBodyTrait for BscBlockBody {
    type Transaction = TransactionSigned;
    type OmmerHeader = Header;

    fn transactions(&self) -> &[Self::Transaction] {
        BlockBodyTrait::transactions(&self.inner)
    }

    fn into_ethereum_body(self) -> BlockBody {
        self.inner
    }

    fn into_transactions(self) -> Vec<Self::Transaction> {
        self.inner.into_transactions()
    }

    fn withdrawals(&self) -> Option<&Withdrawals> {
        self.inner.withdrawals()
    }

    fn ommers(&self) -> Option<&[Self::OmmerHeader]> {
        self.inner.ommers()
    }
}

/// Block for BSC
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BscBlock {
    pub header: Header,
    pub body: BscBlockBody,
}

impl InMemorySize for BscBlock {
    fn size(&self) -> usize {
        self.header.size() + self.body.size()
    }
}

impl Block for BscBlock {
    type Header = Header;
    type Body = BscBlockBody;

    fn new(header: Self::Header, body: Self::Body) -> Self {
        Self { header, body }
    }

    fn header(&self) -> &Self::Header {
        &self.header
    }

    fn body(&self) -> &Self::Body {
        &self.body
    }

    fn split(self) -> (Self::Header, Self::Body) {
        (self.header, self.body)
    }

    fn rlp_length(header: &Self::Header, body: &Self::Body) -> usize {
        rlp::BlockHelper {
            header: Cow::Borrowed(header),
            transactions: Cow::Borrowed(&body.inner.transactions),
            ommers: Cow::Borrowed(&body.inner.ommers),
            withdrawals: body.inner.withdrawals.as_ref().map(Cow::Borrowed),
            sidecars: body.sidecars.as_ref().map(Cow::Borrowed),
        }
        .length()
    }
}

mod rlp {
    use super::*;
    use alloy_eips::eip4895::Withdrawals;
    use alloy_rlp::{Decodable, RlpDecodable, RlpEncodable};

    #[derive(RlpEncodable, RlpDecodable)]
    #[rlp(trailing)]
    struct BlockBodyHelper<'a> {
        transactions: Cow<'a, Vec<TransactionSigned>>,
        ommers: Cow<'a, Vec<Header>>,
        withdrawals: Option<Cow<'a, Withdrawals>>,
        sidecars: Option<Cow<'a, Vec<BscBlobTransactionSidecar>>>,
    }

    #[derive(RlpEncodable, RlpDecodable)]
    #[rlp(trailing)]
    pub(crate) struct BlockHelper<'a> {
        pub(crate) header: Cow<'a, Header>,
        pub(crate) transactions: Cow<'a, Vec<TransactionSigned>>,
        pub(crate) ommers: Cow<'a, Vec<Header>>,
        pub(crate) withdrawals: Option<Cow<'a, Withdrawals>>,
        pub(crate) sidecars: Option<Cow<'a, Vec<BscBlobTransactionSidecar>>>,
    }

    impl<'a> From<&'a BscBlockBody> for BlockBodyHelper<'a> {
        fn from(value: &'a BscBlockBody) -> Self {
            let BscBlockBody { inner: BlockBody { transactions, ommers, withdrawals }, sidecars } =
                value;

            // Geth decodes withdrawals as a list type. When sidecars are present but
            // withdrawals are absent, encode empty withdrawals as `[]` (0xC0) instead of
            // relying on the `#[rlp(trailing)]` placeholder `0x80` which Go rejects as
            // "wrong kind of empty value (got String, want List)".
            let withdrawals = match (withdrawals.as_ref(), sidecars.as_ref()) {
                (None, Some(_)) => Some(Cow::Owned(Withdrawals::default())),
                (Some(w), _) => Some(Cow::Borrowed(w)),
                (None, None) => None,
            };

            Self {
                transactions: Cow::Borrowed(transactions),
                ommers: Cow::Borrowed(ommers),
                withdrawals,
                sidecars: sidecars.as_ref().map(Cow::Borrowed),
            }
        }
    }

    impl<'a> From<&'a BscBlock> for BlockHelper<'a> {
        fn from(value: &'a BscBlock) -> Self {
            let BscBlock {
                header,
                body:
                    BscBlockBody { inner: BlockBody { transactions, ommers, withdrawals }, sidecars },
            } = value;

            // Same withdrawals backfill as BlockBodyHelper — see comment there.
            let withdrawals = match (withdrawals.as_ref(), sidecars.as_ref()) {
                (None, Some(_)) => Some(Cow::Owned(Withdrawals::default())),
                (Some(w), _) => Some(Cow::Borrowed(w)),
                (None, None) => None,
            };

            Self {
                header: Cow::Borrowed(header),
                transactions: Cow::Borrowed(transactions),
                ommers: Cow::Borrowed(ommers),
                withdrawals,
                sidecars: sidecars.as_ref().map(Cow::Borrowed),
            }
        }
    }

    impl Encodable for BscBlockBody {
        fn encode(&self, out: &mut dyn bytes::BufMut) {
            BlockBodyHelper::from(self).encode(out);
        }

        fn length(&self) -> usize {
            BlockBodyHelper::from(self).length()
        }
    }

    impl Decodable for BscBlockBody {
        fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
            let BlockBodyHelper { transactions, ommers, withdrawals, sidecars } =
                BlockBodyHelper::decode(buf)?;
            Ok(Self {
                inner: BlockBody {
                    transactions: transactions.into_owned(),
                    ommers: ommers.into_owned(),
                    withdrawals: withdrawals.map(|w| w.into_owned()),
                },
                sidecars: sidecars.map(|s| s.into_owned()),
            })
        }
    }

    impl Encodable for BscBlock {
        fn encode(&self, out: &mut dyn bytes::BufMut) {
            BlockHelper::from(self).encode(out);
        }

        fn length(&self) -> usize {
            BlockHelper::from(self).length()
        }
    }

    impl Decodable for BscBlock {
        fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
            let BlockHelper { header, transactions, ommers, withdrawals, sidecars } =
                BlockHelper::decode(buf)?;
            Ok(Self {
                header: header.into_owned(),
                body: BscBlockBody {
                    inner: BlockBody {
                        transactions: transactions.into_owned(),
                        ommers: ommers.into_owned(),
                        withdrawals: withdrawals.map(|w| w.into_owned()),
                    },
                    sidecars: sidecars.map(|s| s.into_owned()),
                },
            })
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::Header;
    use alloy_eips::eip4895::Withdrawals;

    fn create_test_header() -> Header {
        Header::default()
    }

    fn create_test_body_no_withdrawals() -> BscBlockBody {
        BscBlockBody {
            inner: BlockBody {
                transactions: vec![],
                ommers: vec![],
                withdrawals: None,
            },
            sidecars: None,
        }
    }

    fn create_test_body_empty_withdrawals() -> BscBlockBody {
        BscBlockBody {
            inner: BlockBody {
                transactions: vec![],
                ommers: vec![],
                withdrawals: Some(Withdrawals::default()),
            },
            sidecars: None,
        }
    }

    fn create_test_body_with_withdrawal() -> BscBlockBody {
        use alloy_eips::eip4895::Withdrawal;
        BscBlockBody {
            inner: BlockBody {
                transactions: vec![],
                ommers: vec![],
                withdrawals: Some(Withdrawals::new(vec![Withdrawal {
                    index: 0,
                    validator_index: 0,
                    address: Default::default(),
                    amount: 1000,
                }])),
            },
            sidecars: None,
        }
    }

    #[test]
    fn test_rlp_length_empty_withdrawals_larger_than_no_withdrawals() {
        // Geth includes the empty withdrawals list (0xc0, 1 byte) in size computation.
        // Some([]) encodes as a 1-byte RLP empty list, while None is omitted entirely.
        let header = create_test_header();
        let body_none = create_test_body_no_withdrawals();
        let body_empty = create_test_body_empty_withdrawals();

        let size_none = BscBlock::rlp_length(&header, &body_none);
        let size_empty = BscBlock::rlp_length(&header, &body_empty);

        assert_eq!(
            size_none + 1,
            size_empty,
            "Empty withdrawals (Some([])) should be 1 byte larger than no withdrawals (None)"
        );
    }

    #[test]
    fn test_rlp_length_non_empty_withdrawals_larger() {
        // Non-empty withdrawals should increase the RLP length
        let header = create_test_header();
        let body_empty = create_test_body_empty_withdrawals();
        let body_with_withdrawal = create_test_body_with_withdrawal();

        let size_empty = BscBlock::rlp_length(&header, &body_empty);
        let size_with_withdrawal = BscBlock::rlp_length(&header, &body_with_withdrawal);

        assert!(
            size_with_withdrawal > size_empty,
            "Non-empty withdrawals should have larger RLP length than empty withdrawals"
        );
    }

    #[test]
    fn test_rlp_length_sidecars_none_no_impact() {
        // Verify that sidecars: None doesn't add to the RLP length
        let header = create_test_header();
        let body = create_test_body_no_withdrawals();

        let block = BscBlock { header: header.clone(), body };
        let size = block.body.sidecars.is_none();

        assert!(size, "Sidecars should be None");

        // The size should be deterministic
        let size1 = BscBlock::rlp_length(&header, &block.body);
        let size2 = BscBlock::rlp_length(&header, &block.body);
        assert_eq!(size1, size2, "RLP length should be deterministic");
    }

    #[test]
    fn test_rlp_encode_decode_roundtrip() {
        use alloy_rlp::{Decodable, Encodable};

        let header = create_test_header();
        let body = create_test_body_empty_withdrawals();
        let block = BscBlock { header, body };

        // Encode
        let mut buf = Vec::new();
        block.encode(&mut buf);

        // Decode
        let decoded = BscBlock::decode(&mut buf.as_slice()).expect("Failed to decode block");

        assert_eq!(block, decoded, "Block should roundtrip through RLP encoding");
    }

    #[test]
    fn test_rlp_length_matches_encoded_length() {
        use alloy_rlp::Encodable;

        let header = create_test_header();
        let body = create_test_body_with_withdrawal();
        let block = BscBlock { header: header.clone(), body: body.clone() };

        // Get computed length
        let computed_length = BscBlock::rlp_length(&header, &body);

        // Get actual encoded length
        let mut buf = Vec::new();
        block.encode(&mut buf);
        let actual_length = buf.len();

        // Note: For blocks with non-empty withdrawals, computed length should match actual length
        // For empty withdrawals, computed length may be less (treating empty as None)
        assert_eq!(
            computed_length, actual_length,
            "Computed RLP length should match actual encoded length for non-empty withdrawals"
        );
    }

    #[test]
    fn test_body_encodes_empty_withdrawals_as_list_when_sidecars_present() {
        // Regression test: when sidecars are present but withdrawals is None,
        // the encoder must produce an empty list (0xC0) for withdrawals, NOT the
        // RLP empty string (0x80). Go's decoder rejects 0x80 for slice/struct
        // pointer types because nilKind = List.
        use alloy_rlp::{Decodable, Encodable};

        let body = BscBlockBody {
            inner: BlockBody {
                transactions: vec![],
                ommers: vec![],
                withdrawals: None,  // <-- None, but sidecars present
            },
            sidecars: Some(Vec::new()),
        };

        let mut buf = Vec::new();
        body.encode(&mut buf);

        // Decode the body back
        let decoded = BscBlockBody::decode(&mut buf.as_slice())
            .expect("Failed to decode body with backfilled withdrawals");

        // The decoded body should have withdrawals = Some(empty), not None
        assert!(
            decoded.inner.withdrawals.is_some(),
            "withdrawals should be Some after backfill"
        );
        assert!(
            decoded.inner.withdrawals.as_ref().unwrap().is_empty(),
            "withdrawals should be empty"
        );
        assert!(decoded.sidecars.is_some(), "sidecars should be preserved");

        // Verify the raw RLP bytes do NOT contain 0x80 as a withdrawals placeholder.
        // After [txs_list, ommers_list], the next byte should be 0xC0 (empty list),
        // not 0x80 (empty string).
        // The outer list header is the first few bytes, then txs=0xC0, ommers=0xC0,
        // so the 4th byte (index 3) should be 0xC0 (empty withdrawals list).
        // Outer header for small body: 0xC0 + len => 1 byte header
        // Then: txs=0xC0, ommers=0xC0, withdrawals=?, sidecars=0xC0
        assert!(
            !buf.windows(2).any(|w| w == [0x80, 0xC0]),
            "Should not have 0x80 (empty string) followed by 0xC0 — withdrawals must be 0xC0 (empty list)"
        );
    }

    #[test]
    fn test_block_encodes_empty_withdrawals_as_list_when_sidecars_present() {
        // Same test but for BscBlock encoding (BlockHelper path)
        use alloy_rlp::{Decodable, Encodable};

        let block = BscBlock {
            header: Header::default(),
            body: BscBlockBody {
                inner: BlockBody {
                    transactions: vec![],
                    ommers: vec![],
                    withdrawals: None,
                },
                sidecars: Some(Vec::new()),
            },
        };

        let mut buf = Vec::new();
        block.encode(&mut buf);

        let decoded = BscBlock::decode(&mut buf.as_slice())
            .expect("Failed to decode block with backfilled withdrawals");

        assert!(
            decoded.body.inner.withdrawals.is_some(),
            "withdrawals should be Some after backfill"
        );
        assert!(decoded.body.sidecars.is_some(), "sidecars should be preserved");
    }

    #[test]
    fn test_body_no_backfill_when_no_sidecars() {
        // When sidecars are None, withdrawals=None should remain None (no backfill)
        use alloy_rlp::{Decodable, Encodable};

        let body = BscBlockBody {
            inner: BlockBody {
                transactions: vec![],
                ommers: vec![],
                withdrawals: None,
            },
            sidecars: None,
        };

        let mut buf = Vec::new();
        body.encode(&mut buf);

        let decoded = BscBlockBody::decode(&mut buf.as_slice())
            .expect("Failed to decode body without sidecars");

        assert!(
            decoded.inner.withdrawals.is_none(),
            "withdrawals should remain None when no sidecars"
        );
        assert!(decoded.sidecars.is_none(), "sidecars should remain None");
    }

    #[test]
    fn test_rlp_length_empty_withdrawals_matches_encoded() {
        use alloy_rlp::Encodable;

        let header = create_test_header();
        let body = create_test_body_empty_withdrawals();
        let block = BscBlock { header: header.clone(), body: body.clone() };

        // Computed length must match the actual encoded length (empty withdrawals included).
        let computed_length = BscBlock::rlp_length(&header, &body);

        let mut buf = Vec::new();
        block.encode(&mut buf);
        let actual_length = buf.len();

        assert_eq!(
            computed_length, actual_length,
            "Computed RLP length should match actual encoded length for empty withdrawals"
        );
    }

    #[test]
    fn test_blob_sidecar_nested_rlp_layout() {
        // Regression test: BscBlobTransactionSidecar must encode BlobTxSidecar as a
        // nested sub-list to match go-bsc's struct layout:
        //   BlobSidecar { BlobTxSidecar, BlockNumber, BlockHash, TxIndex, TxHash }
        //
        // Expected RLP: [[blobs, commitments, proofs], block_number, block_hash, tx_index, tx_hash]
        // NOT the flat:  [blobs, commitments, proofs, block_number, block_hash, tx_index, tx_hash]
        //
        // go-bsc decodes field-by-field: it reads the first element as BlobTxSidecar
        // (expecting a list), then block_number, etc. A flat encoding causes:
        //   "rlp: expected input list for []kzg4844.Blob"
        use alloy_rlp::{Decodable, Encodable};

        let sidecar = BscBlobTransactionSidecar {
            inner: BlobTransactionSidecar {
                blobs: vec![alloy_eips::eip4844::Blob::default()],
                commitments: vec![alloy_eips::eip4844::Bytes48::default()],
                proofs: vec![alloy_eips::eip4844::Bytes48::default()],
            },
            block_number: 42,
            block_hash: B256::repeat_byte(0xab),
            tx_index: 7,
            tx_hash: B256::repeat_byte(0xcd),
            version: 0,
        };

        // Encode
        let mut buf = Vec::new();
        sidecar.encode(&mut buf);

        // Verify nested structure: outer list → first element must be a list (BlobTxSidecar)
        let mut cursor = buf.as_slice();
        let outer = alloy_rlp::Header::decode(&mut cursor).unwrap();
        assert!(outer.list, "outer should be a list");

        let first_elem = alloy_rlp::Header::decode(&mut cursor).unwrap();
        assert!(
            first_elem.list,
            "first element (BlobTxSidecar) must be a nested list, not a raw field — \
             go-bsc expects struct BlobSidecar {{ BlobTxSidecar, ... }}"
        );

        // Inside the nested BlobTxSidecar list, first sub-element should also be a list (blobs)
        let blobs_hdr = alloy_rlp::Header::decode(&mut cursor).unwrap();
        assert!(
            blobs_hdr.list,
            "blobs field inside BlobTxSidecar must be a list of Blob items"
        );

        // Roundtrip
        let decoded = BscBlobTransactionSidecar::decode(&mut buf.as_slice())
            .expect("Failed to decode sidecar");
        assert_eq!(sidecar, decoded, "sidecar should roundtrip through RLP");

        // Verify length() matches actual encoded size
        assert_eq!(
            sidecar.length(),
            buf.len(),
            "length() must match actual encoded size"
        );
    }

    fn sample_sidecar() -> BscBlobTransactionSidecar {
        BscBlobTransactionSidecar {
            inner: BlobTransactionSidecar {
                blobs: vec![alloy_eips::eip4844::Blob::default()],
                commitments: vec![alloy_eips::eip4844::Bytes48::default()],
                proofs: vec![alloy_eips::eip4844::Bytes48::default()],
            },
            block_number: 0x64,
            block_hash: B256::repeat_byte(0xab),
            tx_index: 0x7,
            tx_hash: B256::repeat_byte(0xcd),
            version: 0,
        }
    }

    #[test]
    fn blob_sidecar_json_matches_geth_wire_shape() {
        // Regression test for the JSON/RPC-layer break: go-bsc's `BlobSidecar` has custom
        // MarshalJSON that nests the inner sidecar (blobs/commitments/proofs + version) under a
        // "blobSidecar" key, with "blockNumber"/"txIndex" as hex-quantity strings — not the
        // previous derive-based shape ({"inner": {...}, "block_number": 100, ...}), which any
        // geth-conformant builder's BidBlock submission would fail to deserialize against.
        let json = serde_json::to_value(sample_sidecar()).unwrap();

        assert!(json.get("blobSidecar").is_some(), "must nest under 'blobSidecar', not 'inner'");
        assert!(json.get("inner").is_none());
        assert_eq!(json["blobSidecar"]["version"], 0);
        assert!(json["blobSidecar"]["blobs"].is_array());
        assert!(json["blobSidecar"]["commitments"].is_array());
        assert!(json["blobSidecar"]["proofs"].is_array());

        // blockNumber/txIndex are hex-quantity strings, matching hexutil.EncodeUint64, not plain
        // JSON numbers.
        assert_eq!(json["blockNumber"], "0x64");
        assert_eq!(json["txIndex"], "0x7");
        assert!(json.get("block_number").is_none());
        assert!(json.get("tx_index").is_none());

        assert_eq!(json["blockHash"], format!("{:#x}", B256::repeat_byte(0xab)));
        assert_eq!(json["txHash"], format!("{:#x}", B256::repeat_byte(0xcd)));
    }

    #[test]
    fn blob_sidecar_json_roundtrips() {
        let sidecar = sample_sidecar();
        let json = serde_json::to_value(&sidecar).unwrap();
        let decoded: BscBlobTransactionSidecar = serde_json::from_value(json).unwrap();
        assert_eq!(sidecar, decoded);
    }

    #[test]
    fn blob_sidecar_json_accepts_geth_style_payload() {
        // A payload shaped exactly like go-bsc's wire format (as a real builder would send it),
        // decoded without ever having gone through our own Serialize impl. Commitments/proofs use
        // real (small, 48-byte) values; blobs are omitted (128KB each) since only the envelope
        // shape — not blob content — is under test here (covered with real blob data via
        // `sample_sidecar()` in the roundtrip tests above).
        let json = serde_json::json!({
            "blobSidecar": {
                "version": 1,
                "blobs": [],
                "commitments": [format!("{:#x}", alloy_eips::eip4844::Bytes48::default())],
                "proofs": [format!("{:#x}", alloy_eips::eip4844::Bytes48::default())],
            },
            "blockNumber": "0x64",
            "blockHash": format!("{:#x}", B256::repeat_byte(0xab)),
            "txIndex": "0x7",
            "txHash": format!("{:#x}", B256::repeat_byte(0xcd)),
        });

        let decoded: BscBlobTransactionSidecar = serde_json::from_value(json).unwrap();
        assert_eq!(decoded.version, 1);
        assert_eq!(decoded.block_number, 0x64);
        assert_eq!(decoded.tx_index, 0x7);
        assert_eq!(decoded.block_hash, B256::repeat_byte(0xab));
        assert_eq!(decoded.tx_hash, B256::repeat_byte(0xcd));
    }

    #[test]
    fn blob_sidecar_json_defaults_version_when_absent() {
        // go-bsc's BlobTxSidecar.Version defaults to 0 (legacy EIP-4844 proofs) when absent, same
        // as the RLP side (`version` is `rlp:"-"`).
        let json = serde_json::json!({
            "blobSidecar": {
                "blobs": [],
                "commitments": [],
                "proofs": [],
            },
            "blockNumber": "0x1",
            "blockHash": format!("{:#x}", B256::ZERO),
            "txIndex": "0x0",
            "txHash": format!("{:#x}", B256::ZERO),
        });

        let decoded: BscBlobTransactionSidecar = serde_json::from_value(json).unwrap();
        assert_eq!(decoded.version, 0);
    }

    #[test]
    fn blob_sidecar_json_rejects_quantity_without_0x_prefix() {
        // Matches go-ethereum's hexutil.Big, which requires the "0x" prefix.
        let json = serde_json::json!({
            "blobSidecar": {
                "version": 0,
                "blobs": [],
                "commitments": [],
                "proofs": [],
            },
            "blockNumber": "64",
            "blockHash": format!("{:#x}", B256::ZERO),
            "txIndex": "0x0",
            "txHash": format!("{:#x}", B256::ZERO),
        });

        assert!(serde_json::from_value::<BscBlobTransactionSidecar>(json).is_err());
    }
}
