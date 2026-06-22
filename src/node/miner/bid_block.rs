//! BEP-675 BidBlock types: the builder-proposed block carried by `mev_sendBidBlock`.
//!
//! Ported from bnb-chain/bsc `core/types/bid.go`. The builder signs [`BidBlock::hash`], so that
//! hash must match geth's `rlpHash` over `[header, transactions, sidecars]` byte-for-byte — it is
//! validated here against vectors generated from the Go implementation.
//!
//! Note: `hash()` is vector-verified for the **no-blob** case (empty sidecars). Hash parity for
//! non-empty blob sidecars depends on [`BscBlobTransactionSidecar`]'s encoding and needs its own
//! vector before the blob path is relied upon.

use crate::chainspec::BscChainSpec;
use crate::consensus::eip4844::is_blob_eligible_block;
use crate::node::primitives::BscBlobTransactionSidecar;
use alloy_consensus::Header;
use alloy_eips::eip2718::Decodable2718;
use alloy_primitives::{keccak256, Address, Bytes, B256, U256};
use alloy_rlp::Encodable;
use reth_chainspec::EthChainSpec;
use reth_ethereum_primitives::TransactionSigned;
use std::{fmt, vec::Vec};

/// Sidecar version carrying EIP-7594 cell proofs (PeerDAS). BSC does not support it yet, so a
/// BidBlock declaring it is rejected; legacy EIP-4844 blob proofs are version `0`.
const BLOB_SIDECAR_VERSION_CELL_PROOF: u8 = 1;

/// The builder-proposed block carried by [`BidBlockArgs`].
///
/// JSON field names mirror go-bsc's `core/types/bid.go` (`header`, `transactions`, `sidecars`) so
/// builder payloads deserialize unchanged.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BidBlock {
    /// Proposed block header.
    pub header: Header,
    /// Raw (EIP-2718) transactions: user txs first, unsigned system txs last.
    pub transactions: Vec<Bytes>,
    /// Blob sidecars for any blob transactions (empty/omitted when there are none).
    #[serde(default)]
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
///
/// JSON keys match go-bsc's `BidBlockArgs` (`BidBlock`, `signature`).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BidBlockArgs {
    /// The proposed block.
    #[serde(rename = "BidBlock")]
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

/// Cheap blob-sidecar invariants for an admitted BidBlock (go-bsc `validateBidBlockBlobSidecars`).
///
/// Walks the user-tx region (`txs[..system_tx_start]`) and, for each EIP-4844 tx, requires the next
/// sidecar in order to: exist, be a legacy (v0) proof sidecar, match the tx by hash and index, and
/// keep the running blob count within the per-block max. There must be exactly one sidecar per blob
/// tx and no trailing extras. KZG proof verification is deliberately *not* done here — only at final
/// block insertion — matching go-bsc, which runs only these cheap checks at admission.
pub fn validate_bid_block_blob_sidecars(
    header: &Header,
    txs: &[TransactionSigned],
    sidecars: &[BscBlobTransactionSidecar],
    system_tx_start: usize,
    chain_spec: &BscChainSpec,
) -> Result<(), BlobSidecarError> {
    let blob_eligible = is_blob_eligible_block(chain_spec, header.number, header.timestamp);
    let max_blob_count =
        chain_spec.blob_params_at_timestamp(header.timestamp).map(|p| p.max_blob_count).unwrap_or(0);

    let mut sidecar_index = 0usize;
    let mut blob_count: u64 = 0;
    for (tx_index, tx) in txs[..system_tx_start].iter().enumerate() {
        if !tx.is_eip4844() {
            continue;
        }
        if !blob_eligible {
            return Err(BlobSidecarError::NotEligible { block_number: header.number });
        }
        if sidecar_index >= sidecars.len() {
            return Err(BlobSidecarError::CountMismatch {
                sidecars: sidecars.len(),
                blob_txs_at_least: sidecar_index + 1,
            });
        }
        let sidecar = &sidecars[sidecar_index];
        if sidecar.version == BLOB_SIDECAR_VERSION_CELL_PROOF {
            return Err(BlobSidecarError::CellProofUnsupported { tx_index });
        }
        if sidecar.tx_hash != *tx.hash() {
            return Err(BlobSidecarError::TxHashMismatch { tx_index });
        }
        if sidecar.tx_index != tx_index as u64 {
            return Err(BlobSidecarError::TxIndexMismatch {
                tx_index,
                sidecar_tx_index: sidecar.tx_index,
            });
        }
        blob_count += sidecar.inner.blobs.len() as u64;
        if blob_count > max_blob_count {
            return Err(BlobSidecarError::TooManyBlobs { have: blob_count, permitted: max_blob_count });
        }
        sidecar_index += 1;
    }
    if sidecar_index != sidecars.len() {
        return Err(BlobSidecarError::TrailingSidecars {
            sidecars: sidecars.len(),
            blob_txs: sidecar_index,
        });
    }
    Ok(())
}

/// Why a BidBlock's blob sidecars are invalid (see [`validate_bid_block_blob_sidecars`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlobSidecarError {
    /// A blob tx appears in a block where blobs are not allowed (BEP-657 eligibility).
    NotEligible { block_number: u64 },
    /// Fewer sidecars than blob txs.
    CountMismatch { sidecars: usize, blob_txs_at_least: usize },
    /// The sidecar for the blob tx at `tx_index` declares an unsupported cell-proof (v1) version.
    CellProofUnsupported { tx_index: usize },
    /// The sidecar's `tx_hash` does not match the blob tx at `tx_index`.
    TxHashMismatch { tx_index: usize },
    /// The sidecar's `tx_index` does not match the blob tx's position.
    TxIndexMismatch { tx_index: usize, sidecar_tx_index: u64 },
    /// The cumulative blob count exceeds the per-block maximum.
    TooManyBlobs { have: u64, permitted: u64 },
    /// More sidecars than blob txs (trailing extras).
    TrailingSidecars { sidecars: usize, blob_txs: usize },
}

impl fmt::Display for BlobSidecarError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotEligible { block_number } => {
                write!(f, "blob transactions not allowed in block {block_number}")
            }
            Self::CountMismatch { sidecars, blob_txs_at_least } => write!(
                f,
                "blob info mismatch: sidecars {sidecars}, blob txs at least {blob_txs_at_least}"
            ),
            Self::CellProofUnsupported { tx_index } => {
                write!(f, "cell proof is not supported yet (blob tx {tx_index})")
            }
            Self::TxHashMismatch { tx_index } => {
                write!(f, "sidecar TxHash mismatch with blob tx at index {tx_index}")
            }
            Self::TxIndexMismatch { tx_index, sidecar_tx_index } => write!(
                f,
                "sidecar TxIndex {sidecar_tx_index} mismatch with blob tx at index {tx_index}"
            ),
            Self::TooManyBlobs { have, permitted } => {
                write!(f, "too many blobs in block: have {have}, permitted {permitted}")
            }
            Self::TrailingSidecars { sidecars, blob_txs } => {
                write!(f, "blob info mismatch: sidecars {sidecars}, blob txs {blob_txs}")
            }
        }
    }
}

impl std::error::Error for BlobSidecarError {}

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

    #[test]
    fn bid_block_args_json_roundtrip_preserves_hash() {
        // The hash is taken over the decoded structure, so a JSON round-trip must not perturb it.
        let args = BidBlockArgs {
            bid_block: vector_a_block(),
            signature: Bytes::from(vec![1u8; 65]),
        };
        let json = serde_json::to_value(&args).unwrap();
        // geth wire parity: outer keys are "BidBlock" and "signature".
        assert!(json.get("BidBlock").is_some());
        assert!(json.get("signature").is_some());

        let decoded: BidBlockArgs = serde_json::from_value(json).unwrap();
        assert_eq!(decoded.bid_block, args.bid_block);
        assert_eq!(decoded.bid_block.hash(), args.bid_block.hash());
    }

    #[test]
    fn bid_block_sidecars_default_when_omitted() {
        // Builder payloads with no blob txs omit "sidecars"; it must default to empty.
        let json = serde_json::json!({
            "header": serde_json::to_value(vector_a_block().header).unwrap(),
            "transactions": [],
        });
        let bb: BidBlock = serde_json::from_value(json).unwrap();
        assert!(bb.sidecars.is_empty());
    }

    #[test]
    fn hash_matches_geth_blob_sidecar_vector() {
        use alloy_consensus::BlobTransactionSidecar;
        use alloy_eips::eip4844::{Blob, Bytes48};

        // One BlobSidecar with a single all-zero blob/commitment/proof. go-bsc tags the sidecar
        // Version `rlp:"-"`, so it is excluded from the hash; the RLP layout is
        // [[blobs, commitments, proofs], blockNumber, blockHash, txIndex, txHash].
        let sidecar = BscBlobTransactionSidecar {
            inner: BlobTransactionSidecar {
                blobs: vec![Blob::default()],
                commitments: vec![Bytes48::default()],
                proofs: vec![Bytes48::default()],
            },
            block_number: 7,
            block_hash: b256!(
                "0x1111111111111111111111111111111111111111111111111111111111111111"
            ),
            tx_index: 3,
            tx_hash: b256!("0x2222222222222222222222222222222222222222222222222222222222222222"),
            version: 0,
        };
        let block = BidBlock {
            header: vector_a_block().header,
            transactions: Vec::new(),
            sidecars: vec![sidecar],
        };
        // Generated from go-bsc BidBlock.Hash() over [header, [], [sidecar]].
        assert_eq!(
            block.hash(),
            b256!("0x020136e0d39a0a27c9597e89c56f77170b1d43e6b391c4852a2138936184d9c5"),
        );
    }

    // ---- blob sidecar validation ----

    use alloy_primitives::{Signature, TxKind};

    /// A mainnet spec with Cancun active (blob params present, max 6); BSC Mendel stays inactive so
    /// every block is blob-eligible.
    fn blob_chain_spec() -> BscChainSpec {
        use reth_chainspec::ChainSpecBuilder;
        BscChainSpec::from(ChainSpecBuilder::mainnet().cancun_activated().build())
    }

    fn dummy_sig() -> Signature {
        Signature::new(U256::from(1), U256::from(1), false)
    }

    fn legacy_tx(nonce: u64) -> TransactionSigned {
        use alloy_consensus::TxLegacy;
        let tx = TxLegacy {
            chain_id: Some(1),
            nonce,
            gas_price: 1,
            gas_limit: 21_000,
            to: TxKind::Call(Address::ZERO),
            value: U256::ZERO,
            input: Bytes::new(),
        };
        TransactionSigned::new_unhashed(tx.into(), dummy_sig())
    }

    fn blob_tx(nonce: u64) -> TransactionSigned {
        use alloy_consensus::TxEip4844;
        let tx = TxEip4844 {
            chain_id: 1,
            nonce,
            gas_limit: 21_000,
            max_fee_per_gas: 1,
            max_priority_fee_per_gas: 1,
            to: Address::ZERO,
            value: U256::ZERO,
            access_list: Default::default(),
            blob_versioned_hashes: vec![B256::ZERO],
            max_fee_per_blob_gas: 1,
            input: Bytes::new(),
        };
        TransactionSigned::new_unhashed(tx.into(), dummy_sig())
    }

    fn sidecar_for(tx: &TransactionSigned, tx_index: u64, version: u8, blobs: usize) -> BscBlobTransactionSidecar {
        use alloy_consensus::BlobTransactionSidecar;
        use alloy_eips::eip4844::{Blob, Bytes48};
        BscBlobTransactionSidecar {
            inner: BlobTransactionSidecar {
                blobs: vec![Blob::default(); blobs],
                commitments: vec![Bytes48::default(); blobs],
                proofs: vec![Bytes48::default(); blobs],
            },
            block_number: 10,
            block_hash: B256::ZERO,
            tx_index,
            tx_hash: *tx.hash(),
            version,
        }
    }

    fn blob_header() -> Header {
        Header { number: 10, timestamp: 1, ..Default::default() }
    }

    #[test]
    fn blob_validation_accepts_no_blob_txs() {
        let spec = blob_chain_spec();
        let txs = vec![legacy_tx(0)];
        assert_eq!(
            validate_bid_block_blob_sidecars(&blob_header(), &txs, &[], 1, &spec),
            Ok(())
        );
    }

    #[test]
    fn blob_validation_rejects_trailing_sidecars() {
        let spec = blob_chain_spec();
        let txs = vec![legacy_tx(0)];
        let orphan = sidecar_for(&blob_tx(9), 0, 0, 1);
        assert_eq!(
            validate_bid_block_blob_sidecars(&blob_header(), &txs, &[orphan], 1, &spec),
            Err(BlobSidecarError::TrailingSidecars { sidecars: 1, blob_txs: 0 })
        );
    }

    #[test]
    fn blob_validation_accepts_matching_v0() {
        let spec = blob_chain_spec();
        let tx = blob_tx(0);
        let sidecar = sidecar_for(&tx, 0, 0, 1);
        assert_eq!(
            validate_bid_block_blob_sidecars(&blob_header(), &[tx], &[sidecar], 1, &spec),
            Ok(())
        );
    }

    #[test]
    fn blob_validation_rejects_cell_proof_v1() {
        let spec = blob_chain_spec();
        let tx = blob_tx(0);
        let sidecar = sidecar_for(&tx, 0, 1, 1);
        assert_eq!(
            validate_bid_block_blob_sidecars(&blob_header(), &[tx], &[sidecar], 1, &spec),
            Err(BlobSidecarError::CellProofUnsupported { tx_index: 0 })
        );
    }

    #[test]
    fn blob_validation_rejects_missing_sidecar() {
        let spec = blob_chain_spec();
        let tx = blob_tx(0);
        assert_eq!(
            validate_bid_block_blob_sidecars(&blob_header(), &[tx], &[], 1, &spec),
            Err(BlobSidecarError::CountMismatch { sidecars: 0, blob_txs_at_least: 1 })
        );
    }

    #[test]
    fn blob_validation_rejects_txhash_mismatch() {
        let spec = blob_chain_spec();
        let tx = blob_tx(0);
        // Sidecar built for a different tx → tx_hash will not match.
        let sidecar = sidecar_for(&blob_tx(99), 0, 0, 1);
        assert_eq!(
            validate_bid_block_blob_sidecars(&blob_header(), &[tx], &[sidecar], 1, &spec),
            Err(BlobSidecarError::TxHashMismatch { tx_index: 0 })
        );
    }

    #[test]
    fn blob_validation_rejects_too_many_blobs() {
        let spec = blob_chain_spec(); // Cancun max = 6
        let tx = blob_tx(0);
        let sidecar = sidecar_for(&tx, 0, 0, 7);
        assert_eq!(
            validate_bid_block_blob_sidecars(&blob_header(), &[tx], &[sidecar], 1, &spec),
            Err(BlobSidecarError::TooManyBlobs { have: 7, permitted: 6 })
        );
    }
}
