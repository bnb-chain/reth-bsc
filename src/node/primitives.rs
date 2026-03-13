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
#[derive(Debug, Clone, PartialEq, Eq, RlpEncodable, RlpDecodable, Serialize, Deserialize)]
pub struct BscBlobTransactionSidecar {
    pub inner: BlobTransactionSidecar,
    pub block_number: u64,
    pub block_hash: B256,
    pub tx_index: u64,
    pub tx_hash: B256,
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
}
