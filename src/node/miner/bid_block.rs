//! BEP-675 BidBlock types: the builder-proposed block carried by `mev_sendBidBlock`.
//!
//! Ported from bnb-chain/bsc `core/types/bid.go`. The builder signs [`BidBlock::hash`], so that
//! hash must match geth's `rlpHash` over `[header, transactions, sidecars]` byte-for-byte — it is
//! validated here against vectors generated from the Go implementation.
//!
//! Note: `hash()` is vector-verified for the **no-blob** case (empty sidecars). Hash parity for
//! non-empty blob sidecars depends on [`BscBlobTransactionSidecar`]'s encoding and needs its own
//! vector before the blob path is relied upon.

use crate::node::primitives::BscBlobTransactionSidecar;
use alloy_consensus::Header;
use alloy_eips::eip2718::Decodable2718;
use alloy_primitives::{keccak256, Address, Bytes, B256, U256};
use alloy_rlp::Encodable;
use reth_ethereum_primitives::TransactionSigned;
use std::{fmt, vec::Vec};

/// The builder-proposed block carried by [`BidBlockArgs`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BidBlock {
    /// Proposed block header.
    pub header: Header,
    /// Raw (EIP-2718) transactions: user txs first, unsigned system txs last.
    pub transactions: Vec<Bytes>,
    /// Blob sidecars for any blob transactions (empty when there are none).
    pub sidecars: Vec<BscBlobTransactionSidecar>,
}

impl BidBlock {
    /// `rlpHash([header, transactions, sidecars])` — the digest the builder signs.
    ///
    /// Matches geth's `BidBlock.Hash()`; see the module note on blob-sidecar parity.
    pub fn hash(&self) -> B256 {
        let payload_length =
            self.header.length() + self.transactions.length() + self.sidecars.length();

        let mut out = Vec::with_capacity(payload_length + 8);
        alloy_rlp::Header { list: true, payload_length }.encode(&mut out);
        self.header.encode(&mut out);
        self.transactions.encode(&mut out);
        self.sidecars.encode(&mut out);

        keccak256(&out)
    }
}

/// Input to the `mev_sendBidBlock` RPC: a [`BidBlock`] plus the builder's signature over its hash.
#[derive(Debug, Clone)]
pub struct BidBlockArgs {
    /// The proposed block.
    pub bid_block: BidBlock,
    /// secp256k1 signature (`r || s || v`, 65 bytes) over [`BidBlock::hash`].
    pub signature: Bytes,
}

impl BidBlockArgs {
    /// Recover the builder address from the signature over [`BidBlock::hash`].
    pub fn ecrecover_sender(&self) -> Result<Address, BidBlockError> {
        recover_signer(self.bid_block.hash(), &self.signature)
    }

    /// Decode the raw transactions (EIP-2718). Sender recovery is deferred to execution.
    pub fn decode_txs(&self) -> Result<Vec<TransactionSigned>, BidBlockError> {
        self.bid_block
            .transactions
            .iter()
            .enumerate()
            .map(|(i, bytes)| {
                TransactionSigned::decode_2718(&mut bytes.as_ref())
                    .map_err(|e| BidBlockError::TxDecode { index: i, detail: e.to_string() })
            })
            .collect()
    }

    /// Convert to the validator-side decoded representation for the given recovered `builder`.
    pub fn to_decoded_bid_block(&self, builder: Address) -> Result<DecodedBidBlock, BidBlockError> {
        Ok(DecodedBidBlock {
            builder,
            header: self.bid_block.header.clone(),
            txs: self.decode_txs()?,
            sidecars: self.bid_block.sidecars.clone(),
            gas_fee: U256::ZERO,
            system_tx_start: 0,
            bid_hash: self.bid_block.hash(),
        })
    }
}

/// Validator-side decoded representation of a [`BidBlock`].
#[derive(Debug, Clone)]
pub struct DecodedBidBlock {
    /// Builder recovered from [`BidBlockArgs::signature`].
    pub builder: Address,
    /// Proposed block header.
    pub header: Header,
    /// Decoded transactions.
    pub txs: Vec<TransactionSigned>,
    /// Blob sidecars.
    pub sidecars: Vec<BscBlobTransactionSidecar>,
    /// Fees collected from user txs (set during admission).
    pub gas_fee: U256,
    /// Index in `txs` where the trailing unsigned system-tx region begins (set during admission).
    pub system_tx_start: usize,
    /// Hash of the original BidBlock payload.
    bid_hash: B256,
}

impl DecodedBidBlock {
    /// Hash of the original BidBlock payload.
    pub fn hash(&self) -> B256 {
        self.bid_hash
    }

    /// Block number from the header.
    pub fn block_number(&self) -> u64 {
        self.header.number
    }

    /// Parent hash from the header.
    pub fn parent_hash(&self) -> B256 {
        self.header.parent_hash
    }
}

/// Errors from decoding / recovering a [`BidBlockArgs`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BidBlockError {
    /// Signature was not 65 bytes.
    InvalidSignatureLength(usize),
    /// secp256k1 recovery failed.
    Recovery(String),
    /// A transaction at `index` failed to decode.
    TxDecode { index: usize, detail: String },
}

impl fmt::Display for BidBlockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSignatureLength(len) => write!(f, "invalid signature length: {len}"),
            Self::Recovery(detail) => write!(f, "failed to recover builder: {detail}"),
            Self::TxDecode { index, detail } => write!(f, "failed to decode tx {index}: {detail}"),
        }
    }
}

impl std::error::Error for BidBlockError {}

/// Recover the signer address from a 65-byte `r||s||v` signature over `hash`.
fn recover_signer(hash: B256, signature: &[u8]) -> Result<Address, BidBlockError> {
    use secp256k1::{
        ecdsa::{RecoverableSignature, RecoveryId},
        Message, Secp256k1,
    };

    if signature.len() != 65 {
        return Err(BidBlockError::InvalidSignatureLength(signature.len()));
    }

    let message = Message::from_digest_slice(hash.as_slice())
        .map_err(|e| BidBlockError::Recovery(e.to_string()))?;

    // Ethereum encodes v as 27/28; normalize to 0/1.
    let v = signature[64];
    let recovery_id = if v >= 27 { v - 27 } else { v };
    let recovery_id = RecoveryId::try_from(i32::from(recovery_id))
        .map_err(|e| BidBlockError::Recovery(format!("{e:?}")))?;
    let recoverable = RecoverableSignature::from_compact(&signature[..64], recovery_id)
        .map_err(|e| BidBlockError::Recovery(e.to_string()))?;

    let public_key = Secp256k1::new()
        .recover_ecdsa(&message, &recoverable)
        .map_err(|e| BidBlockError::Recovery(e.to_string()))?;

    let uncompressed = public_key.serialize_uncompressed();
    // Drop the 0x04 marker; address = last 20 bytes of keccak(pubkey).
    let hashed = keccak256(&uncompressed[1..]);
    Ok(Address::from_slice(&hashed[12..]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, b256, bytes, hex};

    /// A header matching go-ethereum's literal `&Header{...}` defaults: the three roots and
    /// ommers hash are ZERO (alloy's `Header::default()` would set them to the empty-trie hashes).
    fn geth_header(set: impl FnOnce(&mut Header)) -> Header {
        let mut header = Header {
            ommers_hash: B256::ZERO,
            state_root: B256::ZERO,
            transactions_root: B256::ZERO,
            receipts_root: B256::ZERO,
            ..Default::default()
        };
        set(&mut header);
        header
    }

    /// Vector A: geth `&Header{Difficulty: 1, Number: 1, Extra: 32 zeros}`, no txs, nil sidecars.
    fn vector_a_block() -> BidBlock {
        let header = geth_header(|h| {
            h.difficulty = U256::from(1);
            h.number = 1;
            h.extra_data = Bytes::from(vec![0u8; 32]);
        });
        BidBlock { header, transactions: Vec::new(), sidecars: Vec::new() }
    }

    #[test]
    fn hash_matches_geth_vector_a() {
        // Generated from go-ethereum BidBlock.Hash() (nil sidecars).
        assert_eq!(
            vector_a_block().hash(),
            b256!("0xdc44b22e7cc5c067a0cc494d39871fa87de72bfd54db5711ccc3cdc31e948491"),
        );
    }

    #[test]
    fn hash_matches_geth_vector_b() {
        let header = geth_header(|h| {
            h.parent_hash =
                b256!("0x1111111111111111111111111111111111111111111111111111111111111111");
            h.beneficiary = address!("0x2222222222222222222222222222222222222222");
            h.difficulty = U256::from(2);
            h.number = 200;
            h.gas_limit = 30_000_000;
            h.gas_used = 21_000;
            h.timestamp = 1_700_000_000;
            h.extra_data = bytes!("0xaabbcc");
        });
        let block = BidBlock {
            header,
            transactions: vec![bytes!("0x010203"), bytes!("0xdeadbeef")],
            sidecars: Vec::new(),
        };
        assert_eq!(
            block.hash(),
            b256!("0x789bf84e2c1f41f6fe8d05cfc0f4b9ee72380c16f506e0640fcfb4c12d0ea6a5"),
        );
    }

    #[test]
    fn ecrecover_matches_geth_vector_c() {
        // Builder signed vector A's hash with a known key; geth recovered this address.
        let args = BidBlockArgs {
            bid_block: vector_a_block(),
            signature: Bytes::from(hex!(
                "d0326aa35df594eefa1b8018bbb69c5ec008dc219896c26e2c0a3d7aea6788745c5626621d144b86a59e48c46ef8e833a03d9b070d960074e1c62ad6854ed1ea00"
            )),
        };
        assert_eq!(
            args.ecrecover_sender().unwrap(),
            address!("0x9d8A62f656a8d1615C1294fd71e9CFb3E4855A4F"),
        );
    }

    #[test]
    fn ecrecover_rejects_bad_signature_length() {
        let args = BidBlockArgs { bid_block: vector_a_block(), signature: Bytes::from(vec![0u8; 64]) };
        assert_eq!(args.ecrecover_sender(), Err(BidBlockError::InvalidSignatureLength(64)));
    }
}
