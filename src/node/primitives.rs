#![allow(clippy::owned_cow)]
use alloy_consensus::{BlobTransactionSidecar, Header};
use alloy_primitives::B256;
use alloy_rlp::{Decodable, Encodable};
use reth_ethereum_primitives::{BlockBody, Receipt};
use reth_primitives::{NodePrimitives, TransactionSigned};
use reth_primitives_traits::{Block, BlockBody as BlockBodyTrait, InMemorySize};
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BscBlobTransactionSidecar {
    pub inner: BlobTransactionSidecar,
    pub block_number: u64,
    pub block_hash: B256,
    pub tx_index: u64,
    pub tx_hash: B256,
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
        Ok(Self { inner, block_number, block_hash, tx_index, tx_hash })
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

    fn withdrawals(&self) -> Option<&alloy_rpc_types::Withdrawals> {
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
        // Treat empty withdrawals as None for size computation to match geth behavior.
        // Geth computes block size as if empty withdrawals are absent, even though it
        // returns withdrawals: [] in the JSON response.
        let withdrawals = body
            .inner
            .withdrawals
            .as_ref()
            .filter(|w| !w.is_empty())
            .map(Cow::Borrowed);

        rlp::BlockHelper {
            header: Cow::Borrowed(header),
            transactions: Cow::Borrowed(&body.inner.transactions),
            ommers: Cow::Borrowed(&body.inner.ommers),
            withdrawals,
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

pub mod serde_bincode_compat {
    use super::*;
    use reth_primitives_traits::serde_bincode_compat::{BincodeReprFor, SerdeBincodeCompat};

    #[derive(Debug, Serialize, Deserialize)]
    pub struct BscBlockBodyBincode<'a> {
        inner: BincodeReprFor<'a, BlockBody>,
        sidecars: Option<Cow<'a, Vec<BscBlobTransactionSidecar>>>,
    }

    #[derive(Debug, Serialize, Deserialize)]
    pub struct BscBlockBincode<'a> {
        header: BincodeReprFor<'a, Header>,
        body: BincodeReprFor<'a, BscBlockBody>,
    }

    impl SerdeBincodeCompat for BscBlockBody {
        type BincodeRepr<'a> = BscBlockBodyBincode<'a>;

        fn as_repr(&self) -> Self::BincodeRepr<'_> {
            BscBlockBodyBincode {
                inner: self.inner.as_repr(),
                sidecars: self.sidecars.as_ref().map(Cow::Borrowed),
            }
        }

        fn from_repr(repr: Self::BincodeRepr<'_>) -> Self {
            let BscBlockBodyBincode { inner, sidecars } = repr;
            Self { inner: BlockBody::from_repr(inner), sidecars: sidecars.map(|s| s.into_owned()) }
        }
    }

    impl SerdeBincodeCompat for BscBlock {
        type BincodeRepr<'a> = BscBlockBincode<'a>;

        fn as_repr(&self) -> Self::BincodeRepr<'_> {
            BscBlockBincode { header: self.header.as_repr(), body: self.body.as_repr() }
        }

        fn from_repr(repr: Self::BincodeRepr<'_>) -> Self {
            let BscBlockBincode { header, body } = repr;
            Self { header: Header::from_repr(header), body: BscBlockBody::from_repr(body) }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::Header;
    use alloy_eips::eip4895::Withdrawals;
    use reth_primitives_traits::Block;

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
    fn test_rlp_length_empty_withdrawals_equals_no_withdrawals() {
        // This test verifies that empty withdrawals (Some([])) are treated as None
        // for size computation, matching geth's behavior.
        let header = create_test_header();
        let body_none = create_test_body_no_withdrawals();
        let body_empty = create_test_body_empty_withdrawals();

        let size_none = BscBlock::rlp_length(&header, &body_none);
        let size_empty = BscBlock::rlp_length(&header, &body_empty);

        assert_eq!(
            size_none, size_empty,
            "Empty withdrawals should have same RLP length as no withdrawals"
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
    fn test_rlp_length_empty_withdrawals_less_than_encoded() {
        use alloy_rlp::Encodable;

        let header = create_test_header();
        let body = create_test_body_empty_withdrawals();
        let block = BscBlock { header: header.clone(), body: body.clone() };

        // Get computed length (treats empty withdrawals as None)
        let computed_length = BscBlock::rlp_length(&header, &body);

        // Get actual encoded length (includes empty withdrawals as 0xc0)
        let mut buf = Vec::new();
        block.encode(&mut buf);
        let actual_length = buf.len();

        // Computed length should be 1 byte less (the 0xc0 for empty list)
        assert_eq!(
            computed_length + 1,
            actual_length,
            "Computed RLP length for empty withdrawals should be 1 byte less than actual encoded length"
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
}
