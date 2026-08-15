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
use crate::consensus::parlia::bid_block::{
    extract_bid_block_deposit_value, verify_bid_block_system_txs, BidBlockSystemTxError,
};
use crate::consensus::parlia::{
    consensus::Parlia,
    constants::{DIFF_INTURN, DIFF_NOTURN},
    Snapshot, SnapshotProvider,
};
use crate::hardforks::BscHardforks;
use crate::node::miner::block_mev_info::{set_block_mev_info, BlockMevInfoVersion};
use crate::node::miner::signer::sign_system_transaction;
use crate::node::miner::util::finalize_new_header;
use crate::node::primitives::{BscBlobTransactionSidecar, BscBlock, BscBlockBody};
use alloy_consensus::transaction::RlpEcdsaDecodableTx;
use alloy_consensus::{Header, Transaction, TxLegacy};
use alloy_eips::eip2718::Decodable2718;
use alloy_primitives::{keccak256, Address, Bytes, Signature, B256, U256};
use alloy_rlp::Decodable;
use reth::primitives::SealedHeader;
use reth_chainspec::EthChainSpec;
use reth_ethereum_primitives::{BlockBody, TransactionSigned};
use reth_node_ethereum::engine::EthPayloadAttributes;
use reth_primitives_traits::{RecoveredBlock, SignerRecoverable};
use std::sync::Arc;
use std::{fmt, vec::Vec};

/// Sidecar version carrying EIP-7594 cell proofs (PeerDAS). BSC does not support it yet, so a
/// BidBlock declaring it is rejected; legacy EIP-4844 blob proofs are version `0`.
const BLOB_SIDECAR_VERSION_CELL_PROOF: u8 = 1;

/// Per-transaction gas cap (EIP-7825, `params.MaxTxGas` in go-bsc): `2^24` = 16,777,216. go-bsc
/// applies it to every user tx in `preSealVerifyBidBlock`; BidBlocks only exist post-Pasteur (which
/// is past Osaka), so the cap is always in force there.
const MAX_TX_GAS: u64 = 1 << 24;

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
    /// The header hash — the digest the builder signs.
    ///
    /// Matches geth's `BidBlock.Hash()` as of bsc #3742 ("miner: optimize BidBlock signing hash",
    /// shipped in v1.7.6), which replaced `rlpHash([header, transactions, sidecars])` with
    /// `b.Header.Hash()`. Re-hashing the body on every call meant re-hashing up to 6 × 128 KiB of
    /// blob data on the admission hot path; the header is ~600 bytes.
    ///
    /// The body is still bound to the signature, indirectly: `header.transactions_root` commits to
    /// the transactions and is verified in [`verify_bid_block_payload`], and blob sidecars are
    /// bound through the blob versioned hashes carried inside those transactions. **That tx-root
    /// check is what makes this digest safe — do not narrow the digest without it.** Recovering a
    /// signature over the wrong digest does not fail; it silently yields a different address, so a
    /// mismatch here surfaces as a bogus "builder is not registered" rather than a signature error.
    pub fn hash(&self) -> B256 {
        self.header.hash_slow()
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

/// Decode one raw EIP-2718 transaction, falling back to [`decode_unsigned_legacy_tx`] for
/// go-bsc's unsigned trailing system txs.
///
/// go-bsc builds these as untagged legacy transactions with `V = R = S = 0`
/// (`types.NewTransaction` leaves the signature fields nil, which RLP-encodes as zero) — see
/// `consensus/parlia/bid_block.go`'s `isUnsignedSystemTxCandidate`. alloy's legacy-tx decoder
/// rejects that `v` value outright (`from_eip155_value` only accepts `27`, `28`, or `>= 35`), so a
/// real BidBlock's trailing system txs fail `TransactionSigned::decode_2718` before any BidBlock
/// validation logic runs. The placeholder signature is discarded once the validator bind-signs
/// these txs (see [`crate::consensus::parlia::bid_block::is_unsigned_system_tx_candidate`] and
/// [`bind_sign_bid_block_system_txs`]), so byte-exact re-encoding of the placeholder isn't needed —
/// only the transaction body fields (nonce, gas price, gas limit, to, value, input) matter.
fn decode_bid_block_tx(bytes: &[u8]) -> Result<TransactionSigned, String> {
    match TransactionSigned::decode_2718(&mut &*bytes) {
        Ok(tx) => Ok(tx),
        // Report the standard decoder's error, not the fallback's: it's the more informative one
        // for every shape other than the unsigned-system-tx case the fallback exists for.
        Err(primary_err) => decode_unsigned_legacy_tx(bytes).map_err(|_| primary_err.to_string()),
    }
}

/// Decode an untagged legacy transaction whose trailing `V, R, S` are all zero — go-bsc's
/// unsigned-system-tx convention (see [`decode_bid_block_tx`]). Anything else is rejected, so this
/// stays as strict as the standard decoder for every other transaction shape.
fn decode_unsigned_legacy_tx(bytes: &[u8]) -> Result<TransactionSigned, alloy_rlp::Error> {
    let buf = &mut &*bytes;
    let header = alloy_rlp::Header::decode(buf)?;
    if !header.list {
        return Err(alloy_rlp::Error::UnexpectedString);
    }
    let remaining = buf.len();

    let tx = TxLegacy::rlp_decode_fields(buf)?;
    let v = U256::decode(buf)?;
    let r = U256::decode(buf)?;
    let s = U256::decode(buf)?;

    if remaining.saturating_sub(buf.len()) != header.payload_length {
        return Err(alloy_rlp::Error::ListLengthMismatch {
            expected: header.payload_length,
            got: remaining - buf.len(),
        });
    }
    if v != U256::ZERO || r != U256::ZERO || s != U256::ZERO {
        return Err(alloy_rlp::Error::Custom("invalid parity value"));
    }

    let signature = Signature::new(U256::ZERO, U256::ZERO, false);
    Ok(TransactionSigned::new_unhashed(tx.into(), signature))
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
                decode_bid_block_tx(bytes.as_ref())
                    .map_err(|detail| BidBlockError::TxDecode { index: i, detail })
            })
            .collect()
    }

    /// Convert to the validator-side decoded representation for the given recovered `builder`.
    pub fn to_decoded_bid_block(&self, builder: Address) -> Result<DecodedBidBlock, BidBlockError> {
        Ok(DecodedBidBlock {
            builder,
            header: self.bid_block.header.clone(),
            txs: self.decode_txs()?,
            submitted_tx_root: submitted_tx_root(&self.bid_block.transactions),
            sidecars: self.bid_block.sidecars.clone(),
            gas_fee: U256::ZERO,
            system_tx_start: 0,
            bid_hash: self.bid_block.hash(),
        })
    }
}

/// Transactions trie root over raw (EIP-2718) tx bytes — go-bsc `DeriveSha(txs, StackTrie)`.
///
/// Hashes the submitted bytes verbatim rather than re-encoding decoded transactions, which would
/// not round-trip the unsigned system txs (`V=R=S=0`).
pub fn submitted_tx_root(raw_txs: &[Bytes]) -> B256 {
    alloy_consensus::proofs::ordered_trie_root_with_encoder(raw_txs, |tx: &Bytes, buf| {
        buf.extend_from_slice(tx.as_ref())
    })
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
    /// Transactions trie root over the **raw submitted** tx bytes, computed at decode time.
    ///
    /// Must be taken from the raw bytes, not by re-encoding [`Self::txs`]: the trailing system txs
    /// arrive unsigned with `V=R=S=0`, and reth's `Signature` stores only a parity bit, so
    /// re-encoding a legacy tx emits `v=27` and yields a different root than go-bsc's `DeriveSha`
    /// over the same input. The raw bytes are also precisely what the builder committed to.
    pub submitted_tx_root: B256,
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

/// Why a selected BidBlock's blob KZG proofs are invalid (see [`validate_bid_block_blob_kzg`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlobKzgError {
    /// No sidecar present for the blob tx at `tx_index`.
    MissingSidecar { tx_index: usize },
    /// KZG proof / versioned-hash verification failed for the blob tx at `tx_index`.
    Invalid { tx_index: usize, detail: String },
}

impl fmt::Display for BlobKzgError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingSidecar { tx_index } => {
                write!(f, "missing sidecar for blob tx at index {tx_index}")
            }
            Self::Invalid { tx_index, detail } => {
                write!(f, "blob KZG invalid for tx at index {tx_index}: {detail}")
            }
        }
    }
}

impl std::error::Error for BlobKzgError {}

/// Verify EIP-4844 KZG proofs for a selected BidBlock's blob txs (go-bsc `validateBidBlockBlobTxs`
/// → `txpool.ValidateBlobTx`).
///
/// This is the **expensive** proof check that the cheap admission-time
/// [`validate_bid_block_blob_sidecars`] deliberately skips (it only checks structural sidecar
/// invariants). go-bsc runs it in `prepareBidBlockTask` — on the *selected* block, *before* sealing
/// and broadcast — and revokes the builder on failure. Under zero-simulate it must run before the
/// block is broadcast, since full re-execution (which would also catch a bad blob) is deferred to
/// after broadcast. Each blob tx in the user-tx region (`txs[..system_tx_start]`) is paired in order
/// with the next sidecar and its commitments/proofs are verified against the tx's versioned hashes.
pub fn validate_bid_block_blob_kzg(
    txs: &[TransactionSigned],
    sidecars: &[BscBlobTransactionSidecar],
    system_tx_start: usize,
) -> Result<(), BlobKzgError> {
    let proof_settings = alloy_eips::eip4844::env_settings::EnvKzgSettings::Default;
    let proof_settings = proof_settings.get();
    let end = system_tx_start.min(txs.len());
    let mut sidecar_index = 0usize;
    for (tx_index, tx) in txs[..end].iter().enumerate() {
        if !tx.is_eip4844() {
            continue;
        }
        let Some(sidecar) = sidecars.get(sidecar_index) else {
            return Err(BlobKzgError::MissingSidecar { tx_index });
        };
        let versioned = tx.blob_versioned_hashes().unwrap_or(&[]);
        sidecar
            .inner
            .validate(versioned, proof_settings)
            .map_err(|e| BlobKzgError::Invalid { tx_index, detail: e.to_string() })?;
        sidecar_index += 1;
    }
    Ok(())
}

/// Sum of per-tx gas used over a BidBlock's non-system-tx region `receipts[..system_tx_start]` —
/// go-bsc's `calcNonSystemGasUsed`. Reth/alloy receipts only carry `cumulative_gas_used` (there is
/// no stored per-tx figure), but since cumulative gas used is monotonic from the block's first tx,
/// summing each receipt's per-tx gas over `[0, system_tx_start)` telescopes to the last
/// non-system receipt's cumulative total — so no explicit summation loop is needed.
pub fn non_system_gas_used<R: alloy_consensus::TxReceipt>(
    receipts: &[R],
    system_tx_start: usize,
) -> u64 {
    if system_tx_start == 0 {
        return 0;
    }
    receipts.get(system_tx_start - 1).map(|r| r.cumulative_gas_used()).unwrap_or(0)
}

/// Post-import average-gas-price floor check (go-bsc `validateBidBlockAverageGasPrice`), run once
/// `new_payload` confirms the BidBlock is valid. The deposit-derived `gas_fee` the bid was selected
/// on is the sole source of the fee ranking; without this check a builder could pad it via the
/// system deposit while filling the user-tx region with near-zero-gas-price transactions. This does
/// **not** reject the (already-canonical) block — it only informs whether to revoke the builder's
/// future `SendBidBlock` permission.
///
/// Returns `Ok(())` when there's no non-system gas used to check (matching go-bsc's `gasUsed == 0`
/// early return — avoids a division by zero) or the average clears `min_gas_price`; otherwise
/// `Err(avg_gas_price)`.
pub fn validate_bid_block_average_gas_price<R: alloy_consensus::TxReceipt>(
    gas_fee: U256,
    receipts: &[R],
    system_tx_start: usize,
    min_gas_price: U256,
) -> Result<(), U256> {
    let gas_used = non_system_gas_used(receipts, system_tx_start);
    if gas_used == 0 {
        return Ok(());
    }
    let avg_gas_price = gas_fee / U256::from(gas_used);
    if avg_gas_price < min_gas_price {
        return Err(avg_gas_price);
    }
    Ok(())
}

/// Pre-seal verification of an admitted BidBlock (go-bsc `bidSimulator.preSealVerifyBidBlock`).
///
/// Runs the cheap checks a validator makes before sealing a builder block, in go-bsc's order:
/// coinbase is the validator, gas limit matches the in-turn target, the header is a valid unsealed
/// Parlia header, the timestamp is within the slot, the deposit (gas-fee) value is non-zero, blob
/// sidecars are well-formed, no user tx exceeds the per-tx gas cap, and the trailing system-tx
/// region is valid. KZG proofs and parent-relative cascading fields are re-checked at block
/// insertion. Returns the located `(system_tx_start, gas_fee)`.
///
/// `expected_gas_limit` is the caller's `calculate_block_gas_limit(parent.gas_limit, ceil)` (reth's
/// `core.CalcGasLimit`); `etherbase` is the validator address; `snap` is the parent's snapshot.
#[allow(clippy::too_many_arguments)]
pub fn pre_seal_verify_bid_block(
    parlia: &Parlia<BscChainSpec>,
    chain_spec: &BscChainSpec,
    decoded: &DecodedBidBlock,
    parent: &Header,
    snap: &Snapshot,
    etherbase: Address,
    expected_gas_limit: u64,
) -> Result<(usize, U256), PreSealVerifyError> {
    // go-bsc checks coinbase/gasLimit before VerifyUnsealedHeader (see preSealVerifyBidBlock) —
    // matters now that verify_bid_block_header's cascading checks depend on the header's coinbase
    // too, so a header with a *wrong* coinbase must surface `InvalidCoinbase`, not
    // `UnauthorizedValidator`, matching which error go-bsc would return first.
    verify_bid_block_coinbase_and_gas_limit(&decoded.header, etherbase, expected_gas_limit)?;
    verify_bid_block_header(parlia, &decoded.header, parent, snap)?;
    verify_bid_block_payload(chain_spec, decoded, parent, etherbase, expected_gas_limit)
}

/// Header half of [`pre_seal_verify_bid_block`]: the unsealed Parlia header-field checks
/// (`validate_header`: extra, ommers, gas, base fee, withdrawals, 4844, mix digest, beacon root,
/// requests hash), the cascading fields go-bsc's `VerifyUnsealedHeader` checks against the parent
/// snapshot (coinbase is an authorized, not-recently-signed validator; difficulty matches its
/// in-turn/no-turn status), and the slot timestamp bound (both directions: the existing upper
/// bound plus go-bsc's `blockTimeVerifyForRamanujanFork` lower bound).
///
/// The cascading checks use `header.beneficiary` directly rather than recovering a seal signer:
/// go-bsc's function is explicitly named "Unsealed" because it runs on headers that may not carry
/// a valid seal yet (this function's first, admission-time call site — [`pre_seal_verify_bid_block`]
/// — runs on the builder's raw, unfinalized header). Signature-based verification of the seal
/// itself happens later, when the finalized block is executed on import.
///
/// Split out because it must also run on the **finalized** header (which carries the validator's
/// extra and seal) after `finalize_new_header`, to confirm that step didn't itself produce an
/// invalid header, whereas [`verify_bid_block_payload`] must run **before** finalize (its
/// `system_tx_start` feeds bind-signing, which mutates the tx set and therefore must precede
/// finalize).
pub fn verify_bid_block_header(
    parlia: &Parlia<BscChainSpec>,
    header: &Header,
    parent: &Header,
    snap: &Snapshot,
) -> Result<(), PreSealVerifyError> {
    let sealed = SealedHeader::seal_slow(header.clone());
    // go-bsc's `VerifyUnsealedHeader` scope: the standalone field checks WITHOUT the
    // wall-clock future bound — a bid's next-slot timestamp is legitimately in the future
    // (by up to one block interval) when it arrives, and geth only applies the future check
    // on the sync path (`verifyHeader`).
    parlia
        .validate_unsealed_header_fields(&sealed)
        .map_err(|e| PreSealVerifyError::InvalidHeader(e.to_string()))?;

    if !snap.validators.contains(&header.beneficiary) {
        return Err(PreSealVerifyError::UnauthorizedValidator { validator: header.beneficiary });
    }
    if snap.sign_recently(header.beneficiary) {
        return Err(PreSealVerifyError::SignedTooRecently { validator: header.beneficiary });
    }
    let want_difficulty =
        if snap.is_inturn(header.beneficiary) { DIFF_INTURN } else { DIFF_NOTURN };
    if header.difficulty != want_difficulty {
        return Err(PreSealVerifyError::WrongDifficulty {
            got: header.difficulty,
            want: want_difficulty,
        });
    }

    parlia
        .block_time_verify_for_ramanujan_fork(snap, header, parent)
        .map_err(|e| PreSealVerifyError::InvalidHeader(e.to_string()))?;
    parlia
        .block_time_upper_check(snap, header, parent)
        .map_err(|e| PreSealVerifyError::InvalidHeader(e.to_string()))?;
    Ok(())
}

/// Coinbase-is-the-validator and gas-limit-matches-target checks — go-bsc's `preSealVerifyBidBlock`
/// runs these *before* `VerifyUnsealedHeader`'s cascading fields, which matters once a wrong
/// coinbase could otherwise surface as [`PreSealVerifyError::UnauthorizedValidator`] instead of the
/// more specific [`PreSealVerifyError::InvalidCoinbase`].
fn verify_bid_block_coinbase_and_gas_limit(
    header: &Header,
    etherbase: Address,
    expected_gas_limit: u64,
) -> Result<(), PreSealVerifyError> {
    if header.beneficiary != etherbase {
        return Err(PreSealVerifyError::InvalidCoinbase { got: header.beneficiary, want: etherbase });
    }
    if header.gas_limit != expected_gas_limit {
        return Err(PreSealVerifyError::InvalidGasLimit {
            got: header.gas_limit,
            want: expected_gas_limit,
        });
    }
    Ok(())
}

/// Payload half of [`pre_seal_verify_bid_block`]: the checks that do not depend on the finalized
/// header — coinbase is the validator, gas limit matches the in-turn target, the deposit gas-fee is
/// non-zero, blob sidecars are well-formed, no user tx exceeds the per-tx gas cap, and the trailing
/// system-tx region is valid. Returns the located `(system_tx_start, gas_fee)`. Needs no `Parlia`
/// engine, so it is runnable (and testable) before finalize/seal.
pub fn verify_bid_block_payload(
    chain_spec: &BscChainSpec,
    decoded: &DecodedBidBlock,
    parent: &Header,
    etherbase: Address,
    expected_gas_limit: u64,
) -> Result<(usize, U256), PreSealVerifyError> {
    let header = &decoded.header;
    verify_bid_block_coinbase_and_gas_limit(header, etherbase, expected_gas_limit)?;

    // go-bsc `preSealVerifyBidBlock`: `DeriveSha(decoded.Txs) == header.TxHash`.
    //
    // Load-bearing, not a formality. Since bsc #3742 the builder's signature covers only the
    // header, so `transactions_root` is the *sole* commitment binding the submitted body to that
    // signature. Without this check anyone could take an honest builder's (header, signature) off
    // the wire, resubmit it with a substituted transaction list, and have it proposed under that
    // builder's identity — the validator later overwrites `transactions_root` with the root of
    // whatever body it was handed (see `simulate_bid_block`), so the forgery would leave no trace.
    //
    // Compares the txs exactly as submitted, before `bind_sign_bid_block_system_txs` re-signs the
    // trailing system txs — that as-submitted set (unsigned system txs, V=R=S=0) is what the
    // builder committed to. go-bsc likewise checks before blind-signing.
    if decoded.submitted_tx_root != header.transactions_root {
        return Err(PreSealVerifyError::TxRootMismatch {
            got: header.transactions_root,
            want: decoded.submitted_tx_root,
        });
    }

    let (system_tx_start, gas_fee) = extract_bid_block_deposit_value(&decoded.txs);
    if gas_fee.is_zero() {
        return Err(PreSealVerifyError::EmptyGasFee);
    }

    validate_bid_block_blob_sidecars(
        header,
        &decoded.txs,
        &decoded.sidecars,
        system_tx_start,
        chain_spec,
    )
    .map_err(PreSealVerifyError::Blob)?;

    for (i, tx) in decoded.txs[..system_tx_start].iter().enumerate() {
        if tx.gas_limit() > MAX_TX_GAS {
            return Err(PreSealVerifyError::TxGasTooHigh {
                tx_index: i,
                gas: tx.gas_limit(),
                cap: MAX_TX_GAS,
            });
        }
    }

    verify_bid_block_system_txs(&decoded.txs, header, parent, system_tx_start)
        .map_err(PreSealVerifyError::SystemTx)?;

    Ok((system_tx_start, gas_fee))
}

/// Why an admitted BidBlock failed pre-seal verification (see [`pre_seal_verify_bid_block`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreSealVerifyError {
    /// Header coinbase is not the in-turn validator.
    InvalidCoinbase { got: Address, want: Address },
    /// Header gas limit does not equal the in-turn target.
    InvalidGasLimit { got: u64, want: u64 },
    /// The unsealed Parlia header is invalid, or the timestamp exceeds the slot bound.
    InvalidHeader(String),
    /// Coinbase is not an authorized validator in the parent snapshot (go-bsc
    /// `errUnauthorizedValidator`).
    UnauthorizedValidator { validator: Address },
    /// Coinbase signed too recently to sign again (go-bsc `errRecentlySigned`).
    SignedTooRecently { validator: Address },
    /// Difficulty does not match the coinbase's in-turn/no-turn status (go-bsc
    /// `errWrongDifficulty`).
    WrongDifficulty { got: U256, want: U256 },
    /// The deposit (gas-fee) value is zero.
    EmptyGasFee,
    /// Header `transactions_root` does not commit to the submitted transactions. Since bsc #3742
    /// the signature covers only the header, so this root is what binds the body to it.
    TxRootMismatch { got: B256, want: B256 },
    /// A user tx exceeds the per-tx gas cap.
    TxGasTooHigh { tx_index: usize, gas: u64, cap: u64 },
    /// Blob-sidecar validation failed.
    Blob(BlobSidecarError),
    /// Trailing system-tx region validation failed.
    SystemTx(BidBlockSystemTxError),
}

impl fmt::Display for PreSealVerifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCoinbase { got, want } => {
                write!(f, "invalid coinbase: got {got}, want {want}")
            }
            Self::InvalidGasLimit { got, want } => {
                write!(f, "invalid gasLimit: got {got}, want {want}")
            }
            Self::InvalidHeader(detail) => write!(f, "invalid header: {detail}"),
            Self::UnauthorizedValidator { validator } => {
                write!(f, "unauthorized validator: {validator}")
            }
            Self::SignedTooRecently { validator } => {
                write!(f, "validator {validator} signed recently")
            }
            Self::WrongDifficulty { got, want } => {
                write!(f, "wrong difficulty: got {got}, want {want}")
            }
            Self::EmptyGasFee => write!(f, "empty gasFee"),
            Self::TxRootMismatch { got, want } => {
                write!(f, "invalid tx root: got {got}, want {want}")
            }
            Self::TxGasTooHigh { tx_index, gas, cap } => {
                write!(f, "tx {tx_index} gas {gas} exceeds cap {cap}")
            }
            Self::Blob(e) => write!(f, "{e}"),
            Self::SystemTx(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for PreSealVerifyError {}

/// Blind-sign the trailing unsigned system txs of a verified BidBlock with the validator key
/// (go-bsc `bindSignBidBlockSystemTxs` + `parlia.SignSystemTx`).
///
/// The leading user txs (`[..system_tx_start]`, already builder-signed) are copied through
/// unchanged; each trailing tx — an unsigned placeholder the validator owns — is re-signed with the
/// global validator key. Signing these is what makes the builder-assembled block sealable by the
/// validator. Returns the full transaction list ready for execution.
pub fn bind_sign_bid_block_system_txs(
    txs: &[TransactionSigned],
    system_tx_start: usize,
    chain_id: u64,
) -> Result<Vec<TransactionSigned>, BindSignError> {
    let mut out = Vec::with_capacity(txs.len());
    out.extend_from_slice(&txs[..system_tx_start]);
    for (offset, tx) in txs[system_tx_start..].iter().enumerate() {
        // Recover the typed transaction, dropping the all-zero placeholder signature, then re-sign.
        let mut unsigned = tx.clone().into_typed_transaction();
        // geth's wire format for unsigned system txs carries no chain id (it only exists in `v`
        // after signing), so the decoded placeholder has `chain_id: None`. go-bsc bind-signs with
        // the EIP-155 signer (`signTxFn(..., chainID)`); mirror that here — otherwise the signed
        // tx's signature_hash disagrees with the executor's regenerated template (which carries
        // the chain id) and every geth-built BidBlock fails import with `UnexpectedSystemTx`.
        if let alloy_consensus::EthereumTypedTransaction::Legacy(ref mut legacy) = unsigned {
            legacy.chain_id = Some(chain_id);
        }
        let signed = sign_system_transaction(unsigned)
            .map_err(|e| BindSignError { index: system_tx_start + offset, detail: e.to_string() })?;
        out.push(signed);
    }
    Ok(out)
}

/// A trailing system tx at `index` could not be signed with the validator key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindSignError {
    /// Index in the BidBlock transaction list.
    pub index: usize,
    /// Underlying signer error.
    pub detail: String,
}

impl fmt::Display for BindSignError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "failed to sign system tx {}: {}", self.index, self.detail)
    }
}

/// EVM payload attributes for executing a BidBlock, preserving the builder's block context.
///
/// The validator must re-execute the builder's block against its **exact** EVM `BlockContext`;
/// changing any context field would diverge the re-executed state root from what the builder
/// produced and fail block insertion (go-bsc `prepareBidBlockTask`: "Do not touch fields that enter
/// the EVM BlockContext — GasLimit, Coinbase, Time, Difficulty, BaseFee"). So every field is taken
/// verbatim from the builder's header — notably `prev_randao`, which on BSC is the header
/// difficulty (the EVM exposes it via PREVRANDAO), so it must NOT be recomputed from the snapshot as
/// the local-build path does. The gas limit, coinbase, timestamp and base fee live on the header
/// itself and are likewise consumed unchanged when the block builder is constructed.
///
/// Consequently the downstream finalize/seal step for a BidBlock must also preserve the builder's
/// difficulty rather than recompute it — that is the integration-gated hazard tracked for the
/// execution wiring.
pub fn bid_block_env_attributes(header: &Header) -> EthPayloadAttributes {
    EthPayloadAttributes {
        timestamp: header.timestamp,
        suggested_fee_recipient: header.beneficiary,
        // BSC PREVRANDAO returns difficulty; preserve the builder's value verbatim.
        prev_randao: header.difficulty.into(),
        withdrawals: None,
        parent_beacon_block_root: header.parent_beacon_block_root,
        slot_number: None,
    }
}

/// The validator-finalized, sealed block produced from an admitted BidBlock, ready to execute.
///
/// Mirrors go-bsc's `task` / `bidBlockTaskInfo` (`miner/bid_block.go`): `block` + `gasFee` +
/// `systemTxStart` + `builder` + `bidHash`. (Receipts/state live on the caller's payload, since
/// execution is deferred.)
#[derive(Clone)]
pub struct BidBlockTask {
    /// Sealed block with the validator's extra/seal and the bind-signed system txs.
    pub block: RecoveredBlock<BscBlock>,
    /// Deposit (gas-fee) value located during payload verification.
    pub gas_fee: U256,
    /// Index where the trailing system-tx region begins.
    pub system_tx_start: usize,
    /// Builder that submitted the BidBlock (for selection logging / revoke-on-mismatch).
    pub builder: Address,
    /// Hash of the original BidBlock payload.
    pub bid_hash: B256,
}

/// Why simulating an admitted BidBlock failed.
#[derive(Debug)]
pub enum SimulateBidBlockError {
    /// Payload or finalized-header verification failed.
    Verify(PreSealVerifyError),
    /// Blind-signing a trailing system tx failed.
    BindSign(BindSignError),
    /// Finalizing/sealing the header failed.
    Finalize(String),
    /// Recovering a transaction sender failed.
    SenderRecovery(String),
}

impl fmt::Display for SimulateBidBlockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Verify(e) => write!(f, "verify: {e}"),
            Self::BindSign(e) => write!(f, "bind-sign: {e}"),
            Self::Finalize(e) => write!(f, "finalize: {e}"),
            Self::SenderRecovery(e) => write!(f, "sender recovery: {e}"),
        }
    }
}

impl std::error::Error for SimulateBidBlockError {}

/// Stamp a BidBlock header with BEP-675 MEV info (go-bsc `setBidMevInfo` for the BidBlock case): the
/// validator records `(version = BidBlock, builder)` in `requests_hash`. BidBlock is post-Prague (so
/// `requests_hash` is present), hence this only applies when Prague is active — matching go-bsc,
/// where BidBlock blocks are always post-Prague.
pub fn set_bid_block_mev_info(header: &mut Header, builder: Address, prague_active: bool) {
    set_block_mev_info(header, BlockMevInfoVersion::BidBlock, builder, prague_active);
}

/// Validator-side simulation of an admitted BidBlock: payload-verify, blind-sign the trailing system
/// txs, install the validator's own block context (its extra + the recomputed tx root), finalize and
/// seal the header — producing the consensus-valid block the validator would propose.
///
/// Execution (to obtain the state root / build a payload) is left to the caller, since it differs by
/// environment (miner trie backend vs test DB overlay). All other EVM block-context fields are kept
/// as the builder set them so the re-executed state root matches the builder's.
///
/// `vanity` is the validator's extra-data vanity (finalize appends the 65-byte seal slot);
/// `block_timestamp_ms` is the millisecond timestamp for Lorentz.
#[allow(clippy::too_many_arguments)]
pub fn simulate_bid_block(
    parlia: Arc<Parlia<BscChainSpec>>,
    chain_spec: &BscChainSpec,
    decoded: &DecodedBidBlock,
    parent: &SealedHeader,
    parent_snap: &Snapshot,
    snapshot_provider: &Arc<dyn SnapshotProvider + Send + Sync>,
    validator: Address,
    expected_gas_limit: u64,
    vanity: Bytes,
    block_timestamp_ms: u64,
) -> Result<BidBlockTask, SimulateBidBlockError> {
    let (system_tx_start, gas_fee) =
        verify_bid_block_payload(chain_spec, decoded, parent.header(), validator, expected_gas_limit)
            .map_err(SimulateBidBlockError::Verify)?;

    let txs =
        bind_sign_bid_block_system_txs(&decoded.txs, system_tx_start, chain_spec.chain().id())
            .map_err(SimulateBidBlockError::BindSign)?;

    // Install the validator's block context: its own extra (vanity; finalize appends the seal slot)
    // and the tx root for the now-signed tx set. Other block-context fields are left as the builder
    // set them so the re-executed state root matches.
    let mut header = decoded.header.clone();
    header.extra_data = vanity;
    header.transactions_root = alloy_consensus::proofs::calculate_transaction_root(&txs);
    // Tag the header with BEP-675 BidBlock MEV info (go-bsc setBidMevInfo).
    let prague_active =
        chain_spec.is_prague_active_at_block_and_timestamp(header.number, header.timestamp);
    set_bid_block_mev_info(&mut header, decoded.builder, prague_active);

    finalize_new_header(
        parlia.clone(),
        parent_snap,
        parent,
        &mut header,
        snapshot_provider,
        block_timestamp_ms,
    )
    .map_err(|e| SimulateBidBlockError::Finalize(e.to_string()))?;

    // The finalized (sealed) header must pass the unsealed-header + slot-time checks.
    verify_bid_block_header(&parlia, &header, parent.header(), parent_snap)
        .map_err(SimulateBidBlockError::Verify)?;

    let senders = txs
        .iter()
        .map(SignerRecoverable::recover_signer)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| SimulateBidBlockError::SenderRecovery(e.to_string()))?;

    let sidecars =
        (!decoded.sidecars.is_empty()).then(|| decoded.sidecars.clone());
    let withdrawals = header.withdrawals_root.map(|_| Default::default());
    let body =
        BscBlockBody { inner: BlockBody { transactions: txs, ommers: Vec::new(), withdrawals }, sidecars };
    let block = RecoveredBlock::new_unhashed(BscBlock { header, body }, senders);

    Ok(BidBlockTask { block, gas_fee, system_tx_start, builder: decoded.builder, bid_hash: decoded.hash() })
}

#[cfg(test)]
mod tests {
    use super::*;
    // Needed by the hand-rolled RLP fixtures below; the production path no longer RLP-encodes.
    use alloy_rlp::Encodable;
    use alloy_primitives::{address, b256, bytes, hex};

    /// Repro for the blob-BidBlock admission stack overflow: run the decode+hash path a tokio
    /// worker takes (2 MiB stack) on a BidBlockArgs carrying real 128 KiB blobs.
    fn run_blob_admission_on_stack(stack_bytes: usize, num_blobs: usize) -> std::thread::Result<()> {
        use alloy_consensus::{BlobTransactionSidecar, TxEip4844};
        use alloy_eips::eip4844::{Blob, Bytes48};
        use alloy_eips::eip2718::Encodable2718;

        // Build a blob tx + matching sidecar exactly like a real submission (non-empty blobs).
        let tx = TxEip4844 {
            chain_id: 714,
            nonce: 0,
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
        let signed = TransactionSigned::new_unhashed(tx.into(), dummy_sig());
        let raw = Bytes::from(signed.encoded_2718());
        let sidecar = BscBlobTransactionSidecar {
            inner: BlobTransactionSidecar {
                blobs: vec![Blob::default(); num_blobs],
                commitments: vec![Bytes48::default(); num_blobs],
                proofs: vec![Bytes48::default(); num_blobs],
            },
            block_number: 1,
            block_hash: B256::ZERO,
            tx_index: 0,
            tx_hash: *signed.hash(),
            version: 0,
        };
        let args = BidBlockArgs {
            bid_block: BidBlock {
                header: vector_a_block().header,
                transactions: vec![raw],
                sidecars: vec![sidecar],
            },
            signature: Bytes::from(vec![0u8; 65]),
        };
        // Round-trip through JSON like the RPC layer does, then run the admission decode/hash.
        let json = serde_json::to_string(&args).unwrap();

        let handle = std::thread::Builder::new()
            .stack_size(stack_bytes)
            .spawn(move || {
                // The full pre-KZG admission decode path the RPC layer runs per submission.
                let parsed: BidBlockArgs = serde_json::from_str(&json).unwrap();
                let _hash = parsed.bid_block.hash();
                let _decoded = parsed.to_decoded_bid_block(Address::ZERO).unwrap();
                std::hint::black_box(&_decoded);
            })
            .unwrap();
        handle.join()
    }

    #[test]
    fn blob_admission_does_not_overflow_tokio_stack() {
        // Regression guard for the blob-BidBlock stack overflow: a tokio worker has a 2 MiB stack
        // by default, and admission (JSON decode + hash) runs on it. A max-blob (6) BidBlock must
        // fit — otherwise any whitelisted builder crashes the validator via mev_sendBidBlock. The
        // fix keeps blob decoding on the heap (see `hex_decode_fixed` in node::primitives); before
        // it, even a single blob overflowed in a debug build.
        assert!(
            run_blob_admission_on_stack(2 * 1024 * 1024, 6).is_ok(),
            "blob BidBlock admission overflowed a 2 MiB stack (tokio worker default)"
        );
    }

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
            b256!("0x45908d0719d520fb290125d2df35591b639a6667a8ca453b807f0577ab3a8eba"),
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
            b256!("0xad40d96e6b75df67950f6a63e6659936814b5e11407c6cdcf3b5c63675aa59f8"),
        );
        // Post-#3742 the digest is exactly the header hash; the two transactions above do not
        // enter it (they are bound via `transactions_root` instead — see `verify_bid_block_payload`).
        assert_eq!(block.hash(), block.header.hash_slow());
    }

    #[test]
    fn ecrecover_matches_geth_vector_c() {
        // The same fixed signature reth-bsc has always pinned, recovered over the post-#3742
        // digest. Regenerated with go-bsc `BidBlockArgs.EcrecoverSender()`: narrowing the digest
        // changes which address a given signature recovers to, which is exactly why the old
        // mismatch surfaced as a bogus "builder is not registered".
        let args = BidBlockArgs {
            bid_block: vector_a_block(),
            signature: Bytes::from(hex!(
                "d0326aa35df594eefa1b8018bbb69c5ec008dc219896c26e2c0a3d7aea6788745c5626621d144b86a59e48c46ef8e833a03d9b070d960074e1c62ad6854ed1ea00"
            )),
        };
        assert_eq!(
            args.ecrecover_sender().unwrap(),
            address!("0xd6df9A7DF6A570f65d7BC2B1e1001e0dD8500040"),
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
        // Post-#3742 sidecars are NOT part of the signing digest, so attaching one to vector A's
        // header must leave the hash unchanged. Verified against go-bsc `BidBlock.Hash()` over
        // [header, [], [sidecar]], which returns vector A's hash. Sidecars are instead bound
        // through the blob versioned hashes carried inside the (root-committed) transactions.
        assert_eq!(
            block.hash(),
            b256!("0x45908d0719d520fb290125d2df35591b639a6667a8ca453b807f0577ab3a8eba"),
            "sidecars must not affect the signing digest"
        );
        assert_eq!(block.hash(), vector_a_block().hash());
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

    // ---- validate_bid_block_blob_kzg (pre-broadcast proof gate) ----

    #[test]
    fn kzg_accepts_block_without_blob_txs() {
        // No EIP-4844 txs in the user region → nothing to verify, no sidecars consumed.
        let txs = vec![legacy_tx(0), legacy_tx(1)];
        assert_eq!(validate_bid_block_blob_kzg(&txs, &[], 2), Ok(()));
    }

    #[test]
    fn kzg_rejects_blob_tx_without_sidecar() {
        // A blob tx with no matching sidecar is rejected before any crypto runs (go-bsc would have
        // no sidecar to hand ValidateBlobTx). The error carries the offending tx index.
        let txs = vec![blob_tx(0)];
        assert_eq!(
            validate_bid_block_blob_kzg(&txs, &[], 1),
            Err(BlobKzgError::MissingSidecar { tx_index: 0 })
        );
    }

    #[test]
    fn kzg_rejects_invalid_proof() {
        // A blob tx paired with an all-zero (bogus) sidecar fails KZG verification: the commitment
        // does not hash to the tx's versioned hash / is not a valid point. Must be a typed error,
        // never a panic.
        let tx = blob_tx(0);
        let sidecar = sidecar_for(&tx, 0, 0, 1); // all-zero blob/commitment/proof
        assert!(matches!(
            validate_bid_block_blob_kzg(&[tx], &[sidecar], 1),
            Err(BlobKzgError::Invalid { tx_index: 0, .. })
        ));
    }

    // ---- validate_bid_block_average_gas_price / non_system_gas_used (post-import floor check) ----

    fn receipt_with_cumulative_gas(cumulative_gas_used: u64) -> alloy_consensus::Receipt {
        alloy_consensus::Receipt {
            status: alloy_consensus::Eip658Value::Eip658(true),
            cumulative_gas_used,
            logs: Vec::new(),
        }
    }

    #[test]
    fn non_system_gas_used_reads_last_non_system_receipt() {
        // Sum of per-tx gas over receipts[..system_tx_start] telescopes to the cumulative total at
        // system_tx_start - 1, regardless of what the trailing (system) receipts' totals are.
        let receipts = vec![
            receipt_with_cumulative_gas(21_000),
            receipt_with_cumulative_gas(50_000),
            receipt_with_cumulative_gas(9_999_999), // trailing system tx: must not count
        ];
        assert_eq!(non_system_gas_used(&receipts, 2), 50_000);
    }

    #[test]
    fn non_system_gas_used_is_zero_when_system_tx_start_is_zero() {
        // Matches go-bsc: systemTxStart == 0 means no user txs at all.
        let receipts = vec![receipt_with_cumulative_gas(21_000)];
        assert_eq!(non_system_gas_used(&receipts, 0), 0);
    }

    #[test]
    fn validate_average_gas_price_accepts_at_or_above_floor() {
        // gas_fee=1_050_000 over 50_000 gas => avg=21 (integer division), clears a floor of 21.
        let receipts = vec![receipt_with_cumulative_gas(21_000), receipt_with_cumulative_gas(50_000)];
        assert_eq!(
            validate_bid_block_average_gas_price(U256::from(1_050_000), &receipts, 2, U256::from(21)),
            Ok(())
        );
    }

    #[test]
    fn validate_average_gas_price_rejects_below_floor() {
        // gas_fee=1_000 over 50_000 gas => avg=0, below any positive floor.
        let receipts = vec![receipt_with_cumulative_gas(21_000), receipt_with_cumulative_gas(50_000)];
        assert_eq!(
            validate_bid_block_average_gas_price(U256::from(1_000), &receipts, 2, U256::from(1)),
            Err(U256::ZERO)
        );
    }

    #[test]
    fn validate_average_gas_price_skips_check_when_no_non_system_gas_used() {
        // Matches go-bsc's `gasUsed == 0` early return: avoids a division by zero and simply
        // passes when there is nothing to check (e.g. a BidBlock with only system txs).
        let receipts = vec![receipt_with_cumulative_gas(9_999_999)];
        assert_eq!(
            validate_bid_block_average_gas_price(U256::ZERO, &receipts, 0, U256::from(1_000_000)),
            Ok(())
        );
    }

    #[test]
    fn validate_average_gas_price_ignores_trailing_system_tx_gas() {
        // A huge trailing system-tx gas total must not dilute the average — go-bsc excludes it via
        // `receipts[:systemTxStart]`, and a builder must not be able to hide an underpriced
        // user-tx region behind expensive system txs.
        let receipts = vec![
            receipt_with_cumulative_gas(21_000),
            receipt_with_cumulative_gas(9_021_000), // system tx: consumes ~9M gas on its own
        ];
        // If the system-tx gas were (wrongly) included, avg = fee / 9_021_000 would clear any
        // reasonable floor; excluding it correctly, avg = fee / 21_000 must still fail low floors.
        assert_eq!(
            validate_bid_block_average_gas_price(U256::from(1_000), &receipts, 1, U256::from(1)),
            Err(U256::ZERO)
        );
    }

    // ---- pre_seal_verify_bid_block ----

    use crate::consensus::parlia::bid_block::DEPOSIT_SELECTOR;
    use crate::system_contracts::VALIDATOR_CONTRACT;
    use alloy_consensus::EMPTY_OMMER_ROOT_HASH;
    use std::sync::Arc;

    /// Plain mainnet spec: all BSC forks inactive at block 1, so the header has no base fee, blob,
    /// beacon-root, requests-hash or millisecond-mix-digest fields to satisfy.
    fn preseal_spec() -> BscChainSpec {
        BscChainSpec::from(reth_chainspec::ChainSpecBuilder::mainnet().build())
    }

    fn parlia_engine(spec: BscChainSpec) -> Parlia<BscChainSpec> {
        Parlia::new(Arc::new(spec), 200)
    }

    /// Single-validator snapshot authorizing `Address::repeat_byte(0x11)` — the etherbase every
    /// `pre_seal_*` test below uses — so it passes the authorized-validator/in-turn cascading
    /// checks `verify_bid_block_header` now runs.
    fn snap_with_interval(interval: u64) -> Snapshot {
        let mut snap =
            Snapshot::new(vec![Address::repeat_byte(0x11)], 0, B256::ZERO, 200, None);
        snap.block_interval = interval;
        snap
    }

    /// A valid unsealed Parlia header for block 1: in-turn validator coinbase, EIP-1559/Cancun/etc.
    /// fields all absent (pre-fork), extra = 32-byte vanity + 65-byte seal slot (non-epoch).
    fn valid_bid_header(etherbase: Address, gas_limit: u64) -> Header {
        Header {
            number: 1,
            timestamp: 1,
            beneficiary: etherbase,
            gas_limit,
            gas_used: 21_000,
            ommers_hash: EMPTY_OMMER_ROOT_HASH,
            extra_data: Bytes::from(vec![0u8; 32 + 65]),
            // `snap_with_interval`'s lone validator is always in-turn (a single-member set has
            // nothing to rotate with), so a genuinely valid header must claim DIFF_INTURN.
            difficulty: DIFF_INTURN,
            ..Default::default()
        }
    }

    /// An unsigned `deposit(...)` system tx (zero gas price, zero signature, validator contract).
    fn deposit_system_tx(value: u64) -> TransactionSigned {
        use alloy_consensus::TxLegacy;
        let tx = TxLegacy {
            chain_id: Some(1),
            nonce: 0,
            gas_price: 0,
            gas_limit: 21_000,
            to: TxKind::Call(VALIDATOR_CONTRACT),
            value: U256::from(value),
            input: Bytes::from(DEPOSIT_SELECTOR.to_vec()),
        };
        TransactionSigned::new_unhashed(tx.into(), Signature::new(U256::ZERO, U256::ZERO, false))
    }

    /// Builds a well-formed `DecodedBidBlock`, deriving `transactions_root` from `txs` the way a
    /// real builder must: since bsc #3742 the signature covers only the header, so that root is the
    /// commitment binding the body, and `verify_bid_block_payload` rejects a mismatch. Tests that
    /// want a mismatch should overwrite the root explicitly (see
    /// `pre_seal_rejects_tx_root_mismatch`).
    fn decoded_block(
        header: Header,
        txs: Vec<TransactionSigned>,
        sidecars: Vec<BscBlobTransactionSidecar>,
    ) -> DecodedBidBlock {
        let mut header = header;
        let root = alloy_consensus::proofs::calculate_transaction_root(&txs);
        header.transactions_root = root;
        DecodedBidBlock {
            builder: Address::ZERO,
            header,
            txs,
            submitted_tx_root: root,
            sidecars,
            gas_fee: U256::ZERO,
            system_tx_start: 0,
            bid_hash: B256::ZERO,
        }
    }

    #[test]
    fn submitted_tx_root_matches_geth_for_unsigned_system_tx() {
        // Cross-client vector generated with go-bsc `DeriveSha(txs, NewStackTrie(nil))` over one
        // unsigned parlia system tx (`types.NewTransaction` leaves V=R=S nil, wire-encoded as 0).
        //
        // This is the case that forces the root to be taken from the RAW submitted bytes: reth's
        // `Signature` carries only a parity bit, so decoding this tx and re-encoding it emits
        // `v=27` instead of `v=0` and produces a different root. Computing the root by
        // re-encoding would therefore reject every legitimate BidBlock.
        let raw = bytes!(
            "0xf8498080887fffffffffffffff94000000000000000000000000000000000000100064a4f340fa01000000000000000000000000bcdd0d2cda5f6423e57b6a4dcd75decbe31aecf0808080"
        );
        let geth_root =
            b256!("0x4527823c77294bc45e8d370fc7c3e95cf779bdbf98f4adacdb57e361050a1ddf");

        assert_eq!(submitted_tx_root(std::slice::from_ref(&raw)), geth_root, "must match go-bsc DeriveSha");

        // And demonstrate why the raw form is required: the re-encoded root differs.
        // `decode_bid_block_tx` is the production decoder — plain `decode_2718` rejects this
        // shape outright (`UnexpectedType(0)`), which is itself why reth-bsc needs a bespoke one.
        let decoded = decode_bid_block_tx(raw.as_ref()).expect("decode system tx");
        assert_ne!(
            alloy_consensus::proofs::calculate_transaction_root(&[decoded]),
            geth_root,
            "re-encoding a decoded unsigned system tx must NOT reproduce the root — if this ever \
             starts matching, the raw-bytes path can be simplified"
        );
    }

    #[test]
    fn pre_seal_rejects_tx_root_mismatch() {
        // The security-critical half of bsc #3742. The signature covers only the header, so
        // `transactions_root` is the sole binding between the signed header and the submitted body.
        // Without this rejection, anyone could replay an honest builder's (header, signature) with a
        // substituted transaction list: recovery would still yield the whitelisted builder, and
        // `simulate_bid_block` would overwrite the root to match the forged body, leaving no trace.
        let spec = preseal_spec();
        let etherbase = Address::repeat_byte(0x11);
        let parent = Header { number: 0, timestamp: 1, gas_limit: 30_000_000, ..Default::default() };
        let txs = vec![legacy_tx(0), deposit_system_tx(100)];
        let mut d = decoded_block(valid_bid_header(etherbase, 30_000_000), txs, vec![]);

        // Sanity: the well-formed fixture passes, so the failure below is attributable to the root.
        assert!(verify_bid_block_payload(
            &spec,
            &d,
            &parent,
            etherbase,
            d.header.gas_limit
        )
        .is_ok());

        // Now claim a different body than the one supplied — the replay/substitution shape.
        let honest_root = d.header.transactions_root;
        d.header.transactions_root = B256::repeat_byte(0xab);
        assert_eq!(
            verify_bid_block_payload(&spec, &d, &parent, etherbase, d.header.gas_limit),
            Err(PreSealVerifyError::TxRootMismatch {
                got: B256::repeat_byte(0xab),
                want: honest_root,
            }),
        );
    }

    #[test]
    fn signing_digest_ignores_body_and_is_the_header_hash() {
        // Post-#3742 property, verified against go-bsc: the digest is a pure function of the
        // header, so neither transactions nor sidecars perturb it. This is the inverse of what the
        // pre-#3742 vectors asserted, and it is only safe because `verify_bid_block_payload` checks
        // `transactions_root` — these two tests must be read together.
        let bare = vector_a_block();
        let with_txs = BidBlock {
            header: bare.header.clone(),
            transactions: vec![bytes!("0x010203"), bytes!("0xdeadbeef")],
            sidecars: Vec::new(),
        };

        assert_eq!(bare.hash(), bare.header.hash_slow(), "digest must be the header hash");
        assert_eq!(with_txs.hash(), bare.hash(), "transactions must not enter the digest");
    }

    #[test]
    fn pre_seal_accepts_valid_bid_block() {
        let spec = preseal_spec();
        let etherbase = Address::repeat_byte(0x11);
        let parlia = parlia_engine(spec.clone());
        let snap = snap_with_interval(3_000);
        let header = valid_bid_header(etherbase, 30_000_000);
        let parent = Header { number: 0, timestamp: 1, gas_limit: 30_000_000, ..Default::default() };
        // user tx (signed, non-system) then a trailing unsigned deposit tx carrying the gas fee.
        let txs = vec![legacy_tx(0), deposit_system_tx(100)];
        let d = decoded_block(header.clone(), txs, vec![]);
        assert_eq!(
            pre_seal_verify_bid_block(
                &parlia,
                &spec,
                &d,
                &parent,
                &snap,
                etherbase,
                header.gas_limit
            ),
            Ok((1, U256::from(100)))
        );
    }

    #[test]
    fn pre_seal_rejects_wrong_coinbase() {
        let spec = preseal_spec();
        let parlia = parlia_engine(spec.clone());
        let snap = snap_with_interval(3_000);
        let etherbase = Address::repeat_byte(0x11);
        let header = valid_bid_header(Address::repeat_byte(0x22), 30_000_000);
        let parent = Header { number: 0, timestamp: 1, gas_limit: 30_000_000, ..Default::default() };
        let d = decoded_block(header, vec![], vec![]);
        assert!(matches!(
            pre_seal_verify_bid_block(&parlia, &spec, &d, &parent, &snap, etherbase, 30_000_000),
            Err(PreSealVerifyError::InvalidCoinbase { .. })
        ));
    }

    #[test]
    fn pre_seal_rejects_wrong_gas_limit() {
        let spec = preseal_spec();
        let parlia = parlia_engine(spec.clone());
        let snap = snap_with_interval(3_000);
        let etherbase = Address::repeat_byte(0x11);
        let header = valid_bid_header(etherbase, 30_000_000);
        let parent = Header { number: 0, timestamp: 1, gas_limit: 30_000_000, ..Default::default() };
        let d = decoded_block(header, vec![], vec![]);
        assert!(matches!(
            pre_seal_verify_bid_block(&parlia, &spec, &d, &parent, &snap, etherbase, 29_000_000),
            Err(PreSealVerifyError::InvalidGasLimit { .. })
        ));
    }

    // ---- verify_bid_block_header cascading checks (go-bsc VerifyUnsealedHeader) ----

    #[test]
    fn pre_seal_rejects_unauthorized_validator() {
        // Coinbase matches the caller's etherbase (passes the earlier InvalidCoinbase check) but
        // is not a member of the parent snapshot's validator set at all.
        let spec = preseal_spec();
        let parlia = parlia_engine(spec.clone());
        let etherbase = Address::repeat_byte(0x11);
        // A snapshot that only authorizes a *different* validator.
        let snap = {
            let mut s = Snapshot::new(vec![Address::repeat_byte(0x99)], 0, B256::ZERO, 200, None);
            s.block_interval = 3_000;
            s
        };
        let header = valid_bid_header(etherbase, 30_000_000);
        let parent = Header { number: 0, timestamp: 1, gas_limit: 30_000_000, ..Default::default() };
        let d = decoded_block(header, vec![], vec![]);
        assert_eq!(
            pre_seal_verify_bid_block(&parlia, &spec, &d, &parent, &snap, etherbase, 30_000_000),
            Err(PreSealVerifyError::UnauthorizedValidator { validator: etherbase })
        );
    }

    #[test]
    fn pre_seal_rejects_signed_too_recently() {
        // An authorized, correctly-in-turn validator that already signed within the lookback
        // window must still be rejected (go-bsc errRecentlySigned).
        let spec = preseal_spec();
        let parlia = parlia_engine(spec.clone());
        let etherbase = Address::repeat_byte(0x11);
        let mut snap = snap_with_interval(3_000);
        // count_recent_proposers only counts entries strictly after `block_number - lookback`
        // (0 here, since the snapshot's block_number is 0); block 0 itself would be skipped.
        snap.recent_proposers.insert(1, etherbase);
        let header = valid_bid_header(etherbase, 30_000_000);
        let parent = Header { number: 0, timestamp: 1, gas_limit: 30_000_000, ..Default::default() };
        let d = decoded_block(header, vec![], vec![]);
        assert_eq!(
            pre_seal_verify_bid_block(&parlia, &spec, &d, &parent, &snap, etherbase, 30_000_000),
            Err(PreSealVerifyError::SignedTooRecently { validator: etherbase })
        );
    }

    #[test]
    fn pre_seal_rejects_wrong_difficulty() {
        // snap_with_interval's lone validator is always in-turn, so DIFF_NOTURN is wrong here.
        let spec = preseal_spec();
        let parlia = parlia_engine(spec.clone());
        let etherbase = Address::repeat_byte(0x11);
        let snap = snap_with_interval(3_000);
        let header = Header { difficulty: DIFF_NOTURN, ..valid_bid_header(etherbase, 30_000_000) };
        let parent = Header { number: 0, timestamp: 1, gas_limit: 30_000_000, ..Default::default() };
        let d = decoded_block(header, vec![], vec![]);
        assert_eq!(
            pre_seal_verify_bid_block(&parlia, &spec, &d, &parent, &snap, etherbase, 30_000_000),
            Err(PreSealVerifyError::WrongDifficulty { got: DIFF_NOTURN, want: DIFF_INTURN })
        );
    }

    #[test]
    fn pre_seal_rejects_empty_gas_fee() {
        let spec = preseal_spec();
        let etherbase = Address::repeat_byte(0x11);
        let parlia = parlia_engine(spec.clone());
        let snap = snap_with_interval(3_000);
        let header = valid_bid_header(etherbase, 30_000_000);
        let parent = Header { number: 0, timestamp: 1, gas_limit: 30_000_000, ..Default::default() };
        // deposit value 0 => gas fee is zero.
        let txs = vec![legacy_tx(0), deposit_system_tx(0)];
        let d = decoded_block(header.clone(), txs, vec![]);
        assert_eq!(
            pre_seal_verify_bid_block(
                &parlia,
                &spec,
                &d,
                &parent,
                &snap,
                etherbase,
                header.gas_limit
            ),
            Err(PreSealVerifyError::EmptyGasFee)
        );
    }

    #[test]
    fn verify_bid_block_payload_runs_without_parlia() {
        // The payload half needs no Parlia engine / finalized header — it locates the system-tx
        // region and validates it, returning (system_tx_start, gas_fee).
        let spec = preseal_spec();
        let etherbase = Address::repeat_byte(0x11);
        let header = valid_bid_header(etherbase, 30_000_000);
        let parent = Header { number: 0, timestamp: 1, gas_limit: 30_000_000, ..Default::default() };
        let txs = vec![legacy_tx(0), deposit_system_tx(100)];
        let d = decoded_block(header.clone(), txs, vec![]);
        assert_eq!(
            verify_bid_block_payload(&spec, &d, &parent, etherbase, header.gas_limit),
            Ok((1, U256::from(100)))
        );
        // Wrong coinbase is rejected by the payload half alone.
        let bad = decoded_block(
            valid_bid_header(Address::repeat_byte(0x22), 30_000_000),
            vec![legacy_tx(0), deposit_system_tx(100)],
            vec![],
        );
        assert!(matches!(
            verify_bid_block_payload(&spec, &bad, &parent, etherbase, 30_000_000),
            Err(PreSealVerifyError::InvalidCoinbase { .. })
        ));
    }

    #[test]
    fn bid_block_intake_queue_is_fifo() {
        // Drain any residue first (the queue is a process-global; tests run single-threaded).
        while crate::shared::pop_bid_block_package().is_some() {}

        let first = decoded_block(valid_bid_header(Address::ZERO, 30_000_000), vec![], vec![]);
        let mut second_header = valid_bid_header(Address::ZERO, 30_000_000);
        second_header.number = 2;
        let second = decoded_block(second_header, vec![], vec![]);

        crate::shared::push_bid_block_package(first);
        crate::shared::push_bid_block_package(second);
        assert_eq!(crate::shared::bid_block_queue_len(), 2);

        assert_eq!(crate::shared::pop_bid_block_package().unwrap().block_number(), 1);
        assert_eq!(crate::shared::pop_bid_block_package().unwrap().block_number(), 2);
        assert!(crate::shared::pop_bid_block_package().is_none());
    }

    #[test]
    fn bind_sign_signs_trailing_system_txs() {
        use reth_primitives_traits::SignerRecoverable;

        // Same dev key the other miner tests use, so the process-global signer is consistent
        // regardless of test order (init is first-wins; we ignore AlreadyInitialized).
        let raw = alloy_primitives::hex::decode(
            "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80",
        )
        .unwrap();
        let _ = crate::node::miner::signer::init_global_signer(B256::from_slice(&raw));

        // user tx, then two unsigned (zero-signature) system txs.
        let txs = vec![legacy_tx(0), deposit_system_tx(100), deposit_system_tx(0)];
        let out = bind_sign_bid_block_system_txs(&txs, 1, 714).unwrap();
        assert_eq!(out.len(), 3);

        // The bind-signed legacy system txs must carry the chain id (EIP-155), matching the
        // executor's regenerated template — geth signs system txs with the chain-id signer.
        assert_eq!(out[1].chain_id(), Some(714));
        assert_eq!(out[2].chain_id(), Some(714));

        // Leading user tx is untouched.
        assert_eq!(out[0], txs[0]);

        // The validator address the global signer signs with.
        let validator = sign_system_transaction(deposit_system_tx(7).into_typed_transaction())
            .unwrap()
            .recover_signer()
            .unwrap();
        assert_ne!(validator, Address::ZERO);

        // Trailing system txs now carry a real signature recovering to the validator, and differ
        // from the unsigned placeholders.
        assert_eq!(out[1].recover_signer().unwrap(), validator);
        assert_eq!(out[2].recover_signer().unwrap(), validator);
        assert_ne!(out[1], txs[1]);
    }

    #[test]
    fn bid_block_attributes_preserve_builder_block_context() {
        let header = Header {
            number: 5,
            timestamp: 1_700_000_000,
            beneficiary: Address::repeat_byte(0x33),
            difficulty: U256::from(2),
            parent_beacon_block_root: Some(B256::ZERO),
            ..Default::default()
        };
        let attrs = bid_block_env_attributes(&header);
        // Every EVM block-context field is taken verbatim from the builder's header.
        assert_eq!(attrs.timestamp, header.timestamp);
        assert_eq!(attrs.suggested_fee_recipient, header.beneficiary);
        // PREVRANDAO == difficulty on BSC; must be the builder's value, not snapshot-recomputed.
        assert_eq!(attrs.prev_randao, B256::from(header.difficulty));
        assert_eq!(attrs.parent_beacon_block_root, header.parent_beacon_block_root);
    }

    /// Trivial snapshot provider for finalize (vote-attestation is skipped pre-Luban, so lookups
    /// don't actually fire — this just satisfies the parameter).
    struct TestSnapProvider(Snapshot);
    impl SnapshotProvider for TestSnapProvider {
        fn snapshot_by_hash(&self, _hash: &B256) -> Option<Snapshot> {
            Some(self.0.clone())
        }
        fn insert(&self, _snapshot: Snapshot) {}
    }

    #[test]
    fn simulate_bid_block_produces_sealed_block() {
        use reth_primitives_traits::SignerRecoverable;

        // Validator = Anvil dev key 0; init the global signer so finalize can seal as it.
        let validator = address!("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
        let key = b256!("0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80");
        let _ = crate::node::miner::signer::init_global_signer(key);

        let chain_spec = std::sync::Arc::new(preseal_spec());
        let parlia = std::sync::Arc::new(Parlia::new(chain_spec.clone(), 200));

        let parent_header = Header { number: 0, timestamp: 1, gas_limit: 30_000_000, ..Default::default() };
        let parent = SealedHeader::new(parent_header.clone(), parent_header.hash_slow());
        let mut snap = Snapshot::new(vec![validator], 0, parent.hash(), 200, None);
        snap.block_interval = 3_000;
        let snapshot_provider: std::sync::Arc<dyn SnapshotProvider + Send + Sync> =
            std::sync::Arc::new(TestSnapProvider(snap.clone()));

        // BidBlock: a user tx then an unsigned deposit system tx (gas fee 100).
        let decoded = decoded_block(
            valid_bid_header(validator, 30_000_000),
            vec![legacy_tx(0), deposit_system_tx(100)],
            vec![],
        );

        let sim = simulate_bid_block(
            parlia,
            &chain_spec,
            &decoded,
            &parent,
            &snap,
            &snapshot_provider,
            validator,
            30_000_000,
            Bytes::from(vec![0u8; 32]),
            1_000,
        )
        .expect("simulate");

        assert_eq!(sim.block.header().number, 1);
        assert_eq!(sim.gas_fee, U256::from(100));
        assert_eq!(sim.system_tx_start, 1);
        // Header is sealed: 32-byte vanity + 65-byte seal.
        assert_eq!(sim.block.header().extra_data.len(), 32 + 65);
        // The trailing deposit tx is now validator-signed.
        let txs: Vec<_> = sim.block.body().transactions().collect();
        assert_eq!(txs[1].recover_signer().unwrap(), validator);
    }

    #[test]
    fn set_bid_block_mev_info_tags_requests_hash_only_when_prague() {
        use crate::node::miner::block_mev_info::decode_block_mev_info;

        let builder = Address::repeat_byte(0xbb);
        let mut header = Header::default();

        // Pre-Prague: the header is left untagged (requests_hash must stay None there).
        set_bid_block_mev_info(&mut header, builder, false);
        assert!(header.requests_hash.is_none());

        // Prague active: requests_hash carries the (BidBlock, builder) tag.
        set_bid_block_mev_info(&mut header, builder, true);
        let tag = header.requests_hash.expect("requests_hash tagged");
        assert_eq!(decode_block_mev_info(tag), Some((BlockMevInfoVersion::BidBlock, builder)));
    }

    /// RLP-encodes a legacy tx exactly the way go-bsc's `types.NewTransaction` does for BidBlock's
    /// unsigned trailing system txs: literal `V = R = S = 0` (alloy's own encoder would instead
    /// write `V = 27` for a zero-parity signature, which is not the wire format geth produces).
    fn encode_geth_style_unsigned_legacy_tx(tx: &TxLegacy) -> Bytes {
        let mut payload = Vec::new();
        tx.nonce.encode(&mut payload);
        tx.gas_price.encode(&mut payload);
        tx.gas_limit.encode(&mut payload);
        tx.to.encode(&mut payload);
        tx.value.encode(&mut payload);
        tx.input.encode(&mut payload);
        0u8.encode(&mut payload); // v
        0u8.encode(&mut payload); // r
        0u8.encode(&mut payload); // s

        let mut out = Vec::new();
        alloy_rlp::Header { list: true, payload_length: payload.len() }.encode(&mut out);
        out.extend_from_slice(&payload);
        Bytes::from(out)
    }

    #[test]
    fn decode_txs_accepts_geths_unsigned_system_tx_wire_format() {
        use crate::system_contracts::VALIDATOR_CONTRACT;

        let tx = TxLegacy {
            chain_id: None,
            nonce: 5,
            gas_price: 0,
            gas_limit: 100_000,
            to: alloy_primitives::TxKind::Call(VALIDATOR_CONTRACT),
            value: U256::ZERO,
            input: Bytes::from(hex!("f340fa01")), // deposit selector
        };
        let raw = encode_geth_style_unsigned_legacy_tx(&tx);

        // The standard EIP-2718 decoder must reject this on its own: v=0 is not in {27,28,35+}.
        assert!(TransactionSigned::decode_2718(&mut raw.as_ref()).is_err());

        let args = BidBlockArgs {
            bid_block: BidBlock {
                header: vector_a_block().header,
                transactions: vec![raw],
                sidecars: Vec::new(),
            },
            signature: Bytes::from(vec![0u8; 65]),
        };

        let decoded = args.decode_txs().expect("geth-style unsigned system tx must decode");
        assert_eq!(decoded.len(), 1);
        let decoded_tx = &decoded[0];
        assert_eq!(decoded_tx.nonce(), 5);
        assert_eq!(decoded_tx.to(), Some(VALIDATOR_CONTRACT));
        assert_eq!(decoded_tx.input().as_ref(), &hex!("f340fa01"));
        assert!(decoded_tx.signature().r().is_zero());
        assert!(decoded_tx.signature().s().is_zero());
        assert!(!decoded_tx.signature().v());
    }

    #[test]
    fn decode_txs_rejects_non_zero_rs_with_zero_v() {
        // Only the exact all-zero placeholder is accepted; a nonzero r/s with v=0 is not a valid
        // signature under any scheme and must still be rejected, not silently coerced.
        let tx = TxLegacy {
            chain_id: None,
            nonce: 0,
            gas_price: 0,
            gas_limit: 21_000,
            to: alloy_primitives::TxKind::Call(Address::ZERO),
            value: U256::ZERO,
            input: Bytes::new(),
        };
        let mut payload = Vec::new();
        tx.nonce.encode(&mut payload);
        tx.gas_price.encode(&mut payload);
        tx.gas_limit.encode(&mut payload);
        tx.to.encode(&mut payload);
        tx.value.encode(&mut payload);
        tx.input.encode(&mut payload);
        0u8.encode(&mut payload); // v = 0
        1u8.encode(&mut payload); // r = 1 (not the placeholder)
        0u8.encode(&mut payload); // s = 0
        let mut out = Vec::new();
        alloy_rlp::Header { list: true, payload_length: payload.len() }.encode(&mut out);
        out.extend_from_slice(&payload);

        let args = BidBlockArgs {
            bid_block: BidBlock {
                header: vector_a_block().header,
                transactions: vec![Bytes::from(out)],
                sidecars: Vec::new(),
            },
            signature: Bytes::from(vec![0u8; 65]),
        };
        assert!(args.decode_txs().is_err());
    }

    #[test]
    fn decode_txs_still_accepts_normal_signed_legacy_tx() {
        // Regression guard: the geth-unsigned-tx fallback must not interfere with ordinary
        // properly-signed transactions, which take the standard `decode_2718` path.
        use alloy_eips::eip2718::Encodable2718;
        let tx = TxLegacy {
            chain_id: Some(56),
            nonce: 1,
            gas_price: 1_000_000_000,
            gas_limit: 21_000,
            to: alloy_primitives::TxKind::Call(Address::repeat_byte(0x11)),
            value: U256::from(1),
            input: Bytes::new(),
        };
        let signature = Signature::new(U256::from(1), U256::from(2), true);
        let signed = TransactionSigned::new_unhashed(tx.into(), signature);
        let raw = Bytes::from(signed.encoded_2718());

        let args = BidBlockArgs {
            bid_block: BidBlock {
                header: vector_a_block().header,
                transactions: vec![raw],
                sidecars: Vec::new(),
            },
            signature: Bytes::from(vec![0u8; 65]),
        };
        let decoded = args.decode_txs().expect("normal signed tx must still decode");
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].signature().r(), U256::from(1));
        assert_eq!(decoded[0].signature().s(), U256::from(2));
    }
}
