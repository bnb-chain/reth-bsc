#![allow(clippy::owned_cow)]
use alloy_consensus::{BlobTransactionSidecar, Header};
use alloy_primitives::B256;
use alloy_rlp::{Encodable, RlpDecodable, RlpEncodable};
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
/// This struct is RLP-compatible with geth's `BlobSidecar` type, which uses
/// an embedded struct that gets flattened during RLP encoding. The RLP format is:
/// `[blobs, commitments, proofs, block_number, block_hash, tx_index, tx_hash]`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BscBlobTransactionSidecar {
    pub inner: BlobTransactionSidecar,
    pub block_number: u64,
    pub block_hash: B256,
    pub tx_index: u64,
    pub tx_hash: B256,
}

// Manual RLP implementation to match geth's flattened encoding format
mod sidecar_rlp {
    use super::*;
    use alloy_rlp::{Decodable, Encodable, Header};
    use bytes::BufMut;

    impl Encodable for BscBlobTransactionSidecar {
        fn encode(&self, out: &mut dyn BufMut) {
            // Calculate total payload length (flattened fields)
            let payload_length = self.inner.blobs.length()
                + self.inner.commitments.length()
                + self.inner.proofs.length()
                + self.block_number.length()
                + self.block_hash.length()
                + self.tx_index.length()
                + self.tx_hash.length();

            // Write RLP list header
            Header { list: true, payload_length }.encode(out);

            // Encode flattened fields (matching geth's embedded struct behavior)
            self.inner.blobs.encode(out);
            self.inner.commitments.encode(out);
            self.inner.proofs.encode(out);
            self.block_number.encode(out);
            self.block_hash.encode(out);
            self.tx_index.encode(out);
            self.tx_hash.encode(out);
        }

        fn length(&self) -> usize {
            let payload_length = self.inner.blobs.length()
                + self.inner.commitments.length()
                + self.inner.proofs.length()
                + self.block_number.length()
                + self.block_hash.length()
                + self.tx_index.length()
                + self.tx_hash.length();
            Header { list: true, payload_length }.length() + payload_length
        }
    }

    impl Decodable for BscBlobTransactionSidecar {
        fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
            let header = Header::decode(buf)?;
            if !header.list {
                return Err(alloy_rlp::Error::UnexpectedString);
            }
            if buf.len() < header.payload_length {
                return Err(alloy_rlp::Error::InputTooShort);
            }

            let remaining = buf.len();

            // Decode flattened fields (matching geth's embedded struct behavior)
            let blobs = Decodable::decode(buf)?;
            let commitments = Decodable::decode(buf)?;
            let proofs = Decodable::decode(buf)?;
            let block_number = Decodable::decode(buf)?;
            let block_hash = Decodable::decode(buf)?;
            let tx_index = Decodable::decode(buf)?;
            let tx_hash = Decodable::decode(buf)?;

            // Verify we consumed exactly the payload
            if buf.len() + header.payload_length != remaining {
                return Err(alloy_rlp::Error::UnexpectedLength);
            }

            Ok(Self {
                inner: BlobTransactionSidecar { blobs, commitments, proofs },
                block_number,
                block_hash,
                tx_index,
                tx_hash,
            })
        }
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
    use alloy_rlp::Decodable;

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

            Self {
                transactions: Cow::Borrowed(transactions),
                ommers: Cow::Borrowed(ommers),
                withdrawals: withdrawals.as_ref().map(Cow::Borrowed),
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

            Self {
                header: Cow::Borrowed(header),
                transactions: Cow::Borrowed(transactions),
                ommers: Cow::Borrowed(ommers),
                withdrawals: withdrawals.as_ref().map(Cow::Borrowed),
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
    use alloy_primitives::FixedBytes;
    use alloy_rlp::{Decodable, Encodable};
    use bytes::BytesMut;

    /// Test that BscBlobTransactionSidecar RLP encoding produces a flattened format
    /// compatible with geth's embedded struct encoding.
    #[test]
    fn test_bsc_blob_sidecar_rlp_roundtrip() {
        let sidecar = BscBlobTransactionSidecar {
            inner: BlobTransactionSidecar {
                blobs: vec![],
                commitments: vec![],
                proofs: vec![],
            },
            block_number: 12345,
            block_hash: B256::from([0xab; 32]),
            tx_index: 7,
            tx_hash: B256::from([0xcd; 32]),
        };

        let mut buf = BytesMut::new();
        sidecar.encode(&mut buf);

        let decoded = BscBlobTransactionSidecar::decode(&mut buf.as_ref()).unwrap();
        assert_eq!(sidecar, decoded);
    }

    /// Test that the RLP encoding has the correct flattened structure.
    /// geth encodes BlobSidecar as: [blobs, commitments, proofs, block_number, block_hash, tx_index, tx_hash]
    /// This test verifies we produce exactly 7 top-level fields, not a nested structure.
    #[test]
    fn test_bsc_blob_sidecar_rlp_flattened_format() {
        use alloy_rlp::Header;

        let sidecar = BscBlobTransactionSidecar {
            inner: BlobTransactionSidecar {
                blobs: vec![],
                commitments: vec![],
                proofs: vec![],
            },
            block_number: 100,
            block_hash: B256::ZERO,
            tx_index: 0,
            tx_hash: B256::ZERO,
        };

        let mut buf = BytesMut::new();
        sidecar.encode(&mut buf);

        // Decode the RLP header to verify it's a list
        let mut slice = buf.as_ref();
        let header = Header::decode(&mut slice).unwrap();
        assert!(header.list, "Should encode as RLP list");

        // Count the number of top-level items in the list
        // geth format has 7 items: blobs, commitments, proofs, block_number, block_hash, tx_index, tx_hash
        let mut count = 0;
        while !slice.is_empty() {
            let item_header = Header::decode(&mut slice).unwrap();
            if item_header.list {
                // Skip list contents
                slice = &slice[item_header.payload_length..];
            } else {
                // Skip string/bytes contents
                slice = &slice[item_header.payload_length..];
            }
            count += 1;
        }

        assert_eq!(count, 7, "Should have 7 top-level fields (flattened format), got {}", count);
    }

    /// Test with non-empty blobs to ensure proper encoding
    #[test]
    fn test_bsc_blob_sidecar_rlp_with_data() {
        use alloy_eips::eip4844::{Blob, BYTES_PER_BLOB};

        let blob = Blob::from([0x42; BYTES_PER_BLOB]);
        let commitment = FixedBytes::<48>::from([0x11; 48]);
        let proof = FixedBytes::<48>::from([0x22; 48]);

        let sidecar = BscBlobTransactionSidecar {
            inner: BlobTransactionSidecar {
                blobs: vec![blob],
                commitments: vec![commitment],
                proofs: vec![proof],
            },
            block_number: 999999,
            block_hash: B256::from([0xff; 32]),
            tx_index: 42,
            tx_hash: B256::from([0xee; 32]),
        };

        let mut buf = BytesMut::new();
        sidecar.encode(&mut buf);

        let decoded = BscBlobTransactionSidecar::decode(&mut buf.as_ref()).unwrap();
        assert_eq!(sidecar.inner.blobs.len(), decoded.inner.blobs.len());
        assert_eq!(sidecar.inner.commitments, decoded.inner.commitments);
        assert_eq!(sidecar.inner.proofs, decoded.inner.proofs);
        assert_eq!(sidecar.block_number, decoded.block_number);
        assert_eq!(sidecar.block_hash, decoded.block_hash);
        assert_eq!(sidecar.tx_index, decoded.tx_index);
        assert_eq!(sidecar.tx_hash, decoded.tx_hash);
    }

    /// Test BscBlock RLP roundtrip with sidecars
    #[test]
    fn test_bsc_block_rlp_with_sidecars() {
        use alloy_eips::eip4844::{Blob, BYTES_PER_BLOB};
        use reth_ethereum_primitives::BlockBody;

        let blob = Blob::from([0x42; BYTES_PER_BLOB]);
        let commitment = FixedBytes::<48>::from([0x11; 48]);
        let proof = FixedBytes::<48>::from([0x22; 48]);

        let sidecar = BscBlobTransactionSidecar {
            inner: BlobTransactionSidecar {
                blobs: vec![blob],
                commitments: vec![commitment],
                proofs: vec![proof],
            },
            block_number: 12345,
            block_hash: B256::from([0xab; 32]),
            tx_index: 0,
            tx_hash: B256::from([0xcd; 32]),
        };

        let block = BscBlock {
            header: Header::default(),
            body: BscBlockBody {
                inner: BlockBody::default(),
                sidecars: Some(vec![sidecar]),
            },
        };

        let mut buf = BytesMut::new();
        block.encode(&mut buf);

        let decoded = BscBlock::decode(&mut buf.as_ref()).unwrap();
        assert_eq!(block.header, decoded.header);
        assert!(decoded.body.sidecars.is_some());
        let decoded_sidecars = decoded.body.sidecars.unwrap();
        assert_eq!(decoded_sidecars.len(), 1);
        assert_eq!(decoded_sidecars[0].block_number, 12345);
        assert_eq!(decoded_sidecars[0].tx_index, 0);
    }

    /// Test that BscBlock RLP format matches geth's BlockData format:
    /// [header, txs, uncles, withdrawals?, sidecars?]
    #[test]
    fn test_bsc_block_rlp_format_matches_geth_blockdata() {
        use alloy_rlp::Header;
        use reth_ethereum_primitives::BlockBody;

        // Block without optional fields
        let block = BscBlock {
            header: alloy_consensus::Header::default(),
            body: BscBlockBody {
                inner: BlockBody::default(),
                sidecars: None,
            },
        };

        let mut buf = BytesMut::new();
        block.encode(&mut buf);

        // Decode RLP structure and count top-level fields
        let mut slice = buf.as_ref();
        let header = Header::decode(&mut slice).unwrap();
        assert!(header.list, "BscBlock should encode as RLP list");

        // Count fields: header, txs, uncles (minimum 3 fields)
        let mut count = 0;
        while !slice.is_empty() {
            let item_header = Header::decode(&mut slice).unwrap();
            if item_header.list {
                slice = &slice[item_header.payload_length..];
            } else {
                slice = &slice[item_header.payload_length..];
            }
            count += 1;
        }

        // geth's BlockData has: Header, Txs, Uncles, Withdrawals?, Sidecars?
        // Without optional fields, we should have 3 fields
        assert!(count >= 3, "BscBlock should have at least 3 fields (header, txs, uncles), got {}", count);
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
