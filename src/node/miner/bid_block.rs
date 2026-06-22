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
use crate::consensus::parlia::{consensus::Parlia, Snapshot, SnapshotProvider};
use crate::node::miner::signer::sign_system_transaction;
use crate::node::miner::util::finalize_new_header;
use crate::node::primitives::{BscBlobTransactionSidecar, BscBlock, BscBlockBody};
use alloy_consensus::{Header, Transaction};
use alloy_eips::eip2718::Decodable2718;
use alloy_primitives::{keccak256, Address, Bytes, B256, U256};
use alloy_rlp::Encodable;
use reth::consensus::HeaderValidator;
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
    verify_bid_block_header(parlia, &decoded.header, parent, snap)?;
    verify_bid_block_payload(chain_spec, decoded, parent, etherbase, expected_gas_limit)
}

/// Header half of [`pre_seal_verify_bid_block`]: the unsealed Parlia header-field checks
/// (`validate_header`: extra, ommers, gas, base fee, withdrawals, 4844, mix digest, beacon root,
/// requests hash) plus the slot timestamp bound.
///
/// Split out because it must run on the **finalized** header (which carries the validator's extra
/// and seal), whereas [`verify_bid_block_payload`] must run **before** finalize (its `system_tx_start`
/// feeds bind-signing, which mutates the tx set and therefore must precede finalize).
pub fn verify_bid_block_header(
    parlia: &Parlia<BscChainSpec>,
    header: &Header,
    parent: &Header,
    snap: &Snapshot,
) -> Result<(), PreSealVerifyError> {
    let sealed = SealedHeader::seal_slow(header.clone());
    parlia
        .validate_header(&sealed)
        .map_err(|e| PreSealVerifyError::InvalidHeader(e.to_string()))?;
    parlia
        .block_time_upper_check(snap, header, parent)
        .map_err(|e| PreSealVerifyError::InvalidHeader(e.to_string()))?;
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

    if header.beneficiary != etherbase {
        return Err(PreSealVerifyError::InvalidCoinbase { got: header.beneficiary, want: etherbase });
    }
    if header.gas_limit != expected_gas_limit {
        return Err(PreSealVerifyError::InvalidGasLimit {
            got: header.gas_limit,
            want: expected_gas_limit,
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
    /// The deposit (gas-fee) value is zero.
    EmptyGasFee,
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
            Self::EmptyGasFee => write!(f, "empty gasFee"),
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
) -> Result<Vec<TransactionSigned>, BindSignError> {
    let mut out = Vec::with_capacity(txs.len());
    out.extend_from_slice(&txs[..system_tx_start]);
    for (offset, tx) in txs[system_tx_start..].iter().enumerate() {
        // Recover the typed transaction, dropping the all-zero placeholder signature, then re-sign.
        let unsigned = tx.clone().into_typed_transaction();
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
pub struct SimulatedBidBlock {
    /// Sealed block with the validator's extra/seal and the bind-signed system txs.
    pub block: RecoveredBlock<BscBlock>,
    /// Deposit (gas-fee) value located during payload verification.
    pub gas_fee: U256,
    /// Index where the trailing system-tx region begins.
    pub system_tx_start: usize,
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
) -> Result<SimulatedBidBlock, SimulateBidBlockError> {
    let (system_tx_start, gas_fee) =
        verify_bid_block_payload(chain_spec, decoded, parent.header(), validator, expected_gas_limit)
            .map_err(SimulateBidBlockError::Verify)?;

    let txs = bind_sign_bid_block_system_txs(&decoded.txs, system_tx_start)
        .map_err(SimulateBidBlockError::BindSign)?;

    // Install the validator's block context: its own extra (vanity; finalize appends the seal slot)
    // and the tx root for the now-signed tx set. Other block-context fields are left as the builder
    // set them so the re-executed state root matches.
    let mut header = decoded.header.clone();
    header.extra_data = vanity;
    header.transactions_root = alloy_consensus::proofs::calculate_transaction_root(&txs);

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

    Ok(SimulatedBidBlock { block, gas_fee, system_tx_start })
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

    fn snap_with_interval(interval: u64) -> Snapshot {
        let mut snap = Snapshot::new(vec![Address::ZERO], 0, B256::ZERO, 200, None);
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

    fn decoded_block(
        header: Header,
        txs: Vec<TransactionSigned>,
        sidecars: Vec<BscBlobTransactionSidecar>,
    ) -> DecodedBidBlock {
        DecodedBidBlock {
            builder: Address::ZERO,
            header,
            txs,
            sidecars,
            gas_fee: U256::ZERO,
            system_tx_start: 0,
            bid_hash: B256::ZERO,
        }
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
        let out = bind_sign_bid_block_system_txs(&txs, 1).unwrap();
        assert_eq!(out.len(), 3);

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
}
