//! Credits to <https://github.com/bnb-chain/revm/blob/d66170e712460ae766fc26a063f106658ce33e9d/crates/precompile/src/double_sign.rs>
//!
//! This precompile must agree with go-bsc byte for byte: it is reachable by any caller at
//! address 0x68, and the calling contract can branch on the returned success bit and persist
//! it, so any disagreement about which inputs are valid is a consensus divergence.
//!
//! go-bsc decodes the evidence into `*big.Int` / `types.Header`
//! (`core/vm/contracts.go: verifyDoubleSignEvidence.Run`). Two properties of that data model
//! have to be reproduced exactly, and neither survives a naive port to fixed-width Rust types:
//!
//! 1. `*big.Int` fields (evidence chain id, header difficulty, header number) accept *any*
//!    canonically encoded big-endian integer, with no width limit at all
//!    (`rlp/decode.go: Stream.decodeBigInt` allocates an arbitrary-size buffer). Decoding them
//!    into `u64`/`U256` silently narrows the accepted domain, so inputs go-bsc accepts get
//!    rejected here. See [`RlpBigInt`].
//! 2. `types.Header` carries trailing `rlp:"optional"` fields, and `types.EncodeSigHeader`
//!    folds part of that tail into the seal hash. Omitting them rejects every post-Cancun
//!    header outright. See [`Header`] and [`seal_hash`].
//!
//! go-bsc also decodes with `rlp.DecodeBytes`, which rejects trailing bytes
//! (`rlp/decode.go: ErrMoreThanOneValue`), so every decode here uses [`alloy_rlp::decode_exact`]
//! rather than [`Decodable::decode`], which would ignore them.

use crate::evm::precompiles::error::BscPrecompileError;
use alloy_primitives::{keccak256, Bytes, B256, B512};
use alloy_rlp::{
    BufMut, Decodable, Encodable, Error as RlpError, Header as RlpHeader, RlpDecodable,
    RlpEncodable, EMPTY_STRING_CODE,
};
use core::cmp::Ordering;
use revm::precompile::{
    secp256k1, u64_to_address, Precompile, PrecompileHalt, PrecompileId, PrecompileOutput,
    PrecompileResult,
};
use std::borrow::Cow;

/// Double sign evidence validation precompile for BSC.
pub(crate) const DOUBLE_SIGN_EVIDENCE_VALIDATION: Precompile = Precompile::new(
    PrecompileId::Custom(Cow::Borrowed("VERIFY_DOUBLE_SIGN_EVIDENCE")),
    u64_to_address(104),
    double_sign_evidence_validation_run,
);

const EXTRA_SEAL_LENGTH: usize = 65;

/// Widest block number accepted as evidence, matching go-bsc's
/// `len(header.Number.Bytes()) > 32` bound check.
const MAX_BLOCK_NUMBER_BYTES: usize = 32;

/// An RLP integer with go-bsc `*big.Int` semantics: arbitrary precision, canonical minimal
/// big-endian encoding, no leading zeros.
///
/// The value is kept as its raw canonical big-endian bytes rather than being parsed into a
/// fixed-width integer. Every field this type stands in for is only ever compared for equality,
/// measured, or re-encoded into the seal hash — never used arithmetically — so preserving the
/// bytes verbatim reproduces go-bsc's behaviour for the entire unbounded domain, including
/// values that no fixed-width Rust integer could hold.
///
/// An empty byte string is zero, exactly as `big.Int.SetBytes(nil)` yields zero.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RlpBigInt(Bytes);

impl RlpBigInt {
    /// Minimal big-endian representation, matching go-bsc's `big.Int.Bytes()` (empty for zero).
    fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Width of the minimal big-endian representation, in bytes.
    fn byte_len(&self) -> usize {
        self.0.len()
    }
}

impl Decodable for RlpBigInt {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let payload = RlpHeader::decode_bytes(buf, false)?;

        // go-bsc rejects leading zero bytes with `ErrCanonInt`. The other canonical-form rule
        // it enforces — a single byte below 0x80 must use the short form, `ErrCanonSize` — is
        // already applied by `Header::decode` as `NonCanonicalSingleByte`.
        if payload.first() == Some(&0) {
            return Err(RlpError::LeadingZero);
        }

        Ok(Self(Bytes::copy_from_slice(payload)))
    }
}

impl Encodable for RlpBigInt {
    fn encode(&self, out: &mut dyn BufMut) {
        // A canonical minimal big-endian byte string encodes identically to the `*big.Int`
        // go-bsc would have encoded, so the seal hash stays byte-identical.
        let payload: &[u8] = &self.0;
        payload.encode(out);
    }

    fn length(&self) -> usize {
        let payload: &[u8] = &self.0;
        payload.length()
    }
}

/// Double sign evidence with two different headers.
#[derive(Debug, RlpDecodable, RlpEncodable, PartialEq)]
pub(crate) struct DoubleSignEvidence {
    pub(crate) chain_id: RlpBigInt,
    pub(crate) header_bytes1: Bytes,
    pub(crate) header_bytes2: Bytes,
}

/// Header of a block, mirroring go-bsc's `types.Header`.
///
/// The trailing fields are go-bsc's `rlp:"optional"` tail: they may be absent, but they are
/// positional, so a field being present implies every field before it is present too.
#[derive(Debug, PartialEq, Clone)]
pub(crate) struct Header {
    pub(crate) parent_hash: [u8; 32],
    pub(crate) uncle_hash: [u8; 32],
    pub(crate) coinbase: [u8; 20],
    pub(crate) root: [u8; 32],
    pub(crate) tx_hash: [u8; 32],
    pub(crate) receipt_hash: [u8; 32],
    pub(crate) bloom: [u8; 256],
    pub(crate) difficulty: RlpBigInt,
    pub(crate) number: RlpBigInt,
    pub(crate) gas_limit: u64,
    pub(crate) gas_used: u64,
    pub(crate) time: u64,
    pub(crate) extra: Bytes,
    pub(crate) mix_digest: [u8; 32],
    pub(crate) nonce: [u8; 8],
    /// EIP-1559.
    pub(crate) base_fee: Option<RlpBigInt>,
    /// EIP-4895.
    pub(crate) withdrawals_hash: Option<[u8; 32]>,
    /// EIP-4844.
    pub(crate) blob_gas_used: Option<u64>,
    /// EIP-4844.
    pub(crate) excess_blob_gas: Option<u64>,
    /// EIP-4788.
    pub(crate) parent_beacon_root: Option<[u8; 32]>,
    /// EIP-7685.
    pub(crate) requests_hash: Option<[u8; 32]>,
    /// EIP-7928.
    pub(crate) block_access_list_hash: Option<[u8; 32]>,
    /// EIP-7843.
    pub(crate) slot_number: Option<u64>,
}

impl Decodable for Header {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let mut payload = RlpHeader::decode_bytes(buf, true)?;
        let body = &mut payload;

        let mut header = Self {
            parent_hash: Decodable::decode(body)?,
            uncle_hash: Decodable::decode(body)?,
            coinbase: Decodable::decode(body)?,
            root: Decodable::decode(body)?,
            tx_hash: Decodable::decode(body)?,
            receipt_hash: Decodable::decode(body)?,
            bloom: Decodable::decode(body)?,
            difficulty: Decodable::decode(body)?,
            number: Decodable::decode(body)?,
            gas_limit: Decodable::decode(body)?,
            gas_used: Decodable::decode(body)?,
            time: Decodable::decode(body)?,
            extra: Decodable::decode(body)?,
            mix_digest: Decodable::decode(body)?,
            nonce: Decodable::decode(body)?,
            base_fee: None,
            withdrawals_hash: None,
            blob_gas_used: None,
            excess_blob_gas: None,
            parent_beacon_root: None,
            requests_hash: None,
            block_access_list_hash: None,
            slot_number: None,
        };

        // Trailing `rlp:"optional"` fields, decoded in declaration order for as long as payload
        // remains. Stopping at the first exhausted field is what makes them positional.
        if !body.is_empty() {
            header.base_fee = Some(Decodable::decode(body)?);
        }
        if !body.is_empty() {
            header.withdrawals_hash = Some(Decodable::decode(body)?);
        }
        if !body.is_empty() {
            header.blob_gas_used = Some(Decodable::decode(body)?);
        }
        if !body.is_empty() {
            header.excess_blob_gas = Some(Decodable::decode(body)?);
        }
        if !body.is_empty() {
            header.parent_beacon_root = Some(Decodable::decode(body)?);
        }
        if !body.is_empty() {
            header.requests_hash = Some(Decodable::decode(body)?);
        }
        if !body.is_empty() {
            header.block_access_list_hash = Some(Decodable::decode(body)?);
        }
        if !body.is_empty() {
            header.slot_number = Some(Decodable::decode(body)?);
        }

        // More fields than `types.Header` defines: go-bsc reports "too many elements".
        if !body.is_empty() {
            return Err(RlpError::UnexpectedLength);
        }

        Ok(header)
    }
}

impl Encodable for Header {
    fn encode(&self, out: &mut dyn BufMut) {
        let mut payload = Vec::new();

        self.parent_hash.encode(&mut payload);
        self.uncle_hash.encode(&mut payload);
        self.coinbase.encode(&mut payload);
        self.root.encode(&mut payload);
        self.tx_hash.encode(&mut payload);
        self.receipt_hash.encode(&mut payload);
        self.bloom.encode(&mut payload);
        self.difficulty.encode(&mut payload);
        self.number.encode(&mut payload);
        self.gas_limit.encode(&mut payload);
        self.gas_used.encode(&mut payload);
        self.time.encode(&mut payload);
        self.extra.encode(&mut payload);
        self.mix_digest.encode(&mut payload);
        self.nonce.encode(&mut payload);

        // Trailing optional fields are emitted up to the last one present; absent fields before
        // it are written as an empty string, which is how go-bsc encodes a nil pointer.
        let tail = [
            self.base_fee.as_ref().map(encode_to_vec),
            self.withdrawals_hash.as_ref().map(encode_to_vec),
            self.blob_gas_used.as_ref().map(encode_to_vec),
            self.excess_blob_gas.as_ref().map(encode_to_vec),
            self.parent_beacon_root.as_ref().map(encode_to_vec),
            self.requests_hash.as_ref().map(encode_to_vec),
            self.block_access_list_hash.as_ref().map(encode_to_vec),
            self.slot_number.as_ref().map(encode_to_vec),
        ];
        if let Some(last_present) = tail.iter().rposition(Option::is_some) {
            for field in &tail[..=last_present] {
                match field {
                    Some(encoded) => payload.extend_from_slice(encoded),
                    None => payload.push(EMPTY_STRING_CODE),
                }
            }
        }

        RlpHeader { list: true, payload_length: payload.len() }.encode(out);
        out.put_slice(&payload);
    }
}

fn encode_to_vec<T: Encodable>(value: &T) -> Vec<u8> {
    let mut buf = Vec::new();
    value.encode(&mut buf);
    buf
}

/// Run the double sign evidence validation precompile.
///
/// input: rlp encoded DoubleSignEvidence
///
/// return:
///
/// signer address| evidence height|
///
/// 20 bytes      | 32 bytes       |
fn double_sign_evidence_validation_run(
    input: &[u8],
    gas_limit: u64,
    reservoir: u64,
) -> PrecompileResult {
    const DOUBLE_SIGN_EVIDENCE_VALIDATION_BASE: u64 = 10_000;

    if DOUBLE_SIGN_EVIDENCE_VALIDATION_BASE > gas_limit {
        return Ok(PrecompileOutput::halt(PrecompileHalt::OutOfGas, reservoir));
    }

    let revert = || {
        Ok(PrecompileOutput::revert(
            DOUBLE_SIGN_EVIDENCE_VALIDATION_BASE,
            Default::default(),
            reservoir,
        ))
    };

    // `decode_exact` rather than `decode`: go-bsc's `rlp.DecodeBytes` rejects trailing bytes.
    let evidence = match alloy_rlp::decode_exact::<DoubleSignEvidence>(input) {
        Ok(evidence) => evidence,
        Err(err) => {
            tracing::debug!("Rejected double sign evidence, malformed envelope: {}", err);
            return revert();
        }
    };

    let header1 = match alloy_rlp::decode_exact::<Header>(evidence.header_bytes1.as_ref()) {
        Ok(header) => header,
        Err(err) => {
            tracing::debug!("Rejected double sign evidence, malformed header 1: {}", err);
            return revert();
        }
    };

    let header2 = match alloy_rlp::decode_exact::<Header>(evidence.header_bytes2.as_ref()) {
        Ok(header) => header,
        Err(err) => {
            tracing::debug!("Rejected double sign evidence, malformed header 2: {}", err);
            return revert();
        }
    };

    // basic check
    if header1.number.byte_len() > MAX_BLOCK_NUMBER_BYTES ||
        header2.number.byte_len() > MAX_BLOCK_NUMBER_BYTES
    {
        tracing::debug!("Rejected double sign evidence, block number wider than 32 bytes");
        return Ok(PrecompileOutput::halt(
            BscPrecompileError::DoubleSignInvalidEvidence.into(),
            reservoir,
        ));
    }
    // Canonical minimal encodings compare equal exactly when the integers do.
    if header1.number != header2.number {
        return Ok(PrecompileOutput::halt(
            BscPrecompileError::DoubleSignInvalidEvidence.into(),
            reservoir,
        ));
    }
    if header1.parent_hash.cmp(&header2.parent_hash) != Ordering::Equal {
        return Ok(PrecompileOutput::halt(
            BscPrecompileError::DoubleSignInvalidEvidence.into(),
            reservoir,
        ));
    }

    if header1.extra.len() < EXTRA_SEAL_LENGTH || header2.extra.len() < EXTRA_SEAL_LENGTH {
        return Ok(PrecompileOutput::halt(
            BscPrecompileError::DoubleSignInvalidEvidence.into(),
            reservoir,
        ));
    }

    let sig1 = &header1.extra[header1.extra.len() - EXTRA_SEAL_LENGTH..];
    let sig2 = &header2.extra[header2.extra.len() - EXTRA_SEAL_LENGTH..];
    if sig1.eq(sig2) {
        return Ok(PrecompileOutput::halt(
            BscPrecompileError::DoubleSignInvalidEvidence.into(),
            reservoir,
        ));
    }

    // check signature
    let msg_hash1 = seal_hash(&header1, &evidence.chain_id);
    let msg_hash2 = seal_hash(&header2, &evidence.chain_id);

    if msg_hash1.eq(&msg_hash2) {
        return Ok(PrecompileOutput::halt(
            BscPrecompileError::DoubleSignInvalidEvidence.into(),
            reservoir,
        ));
    }

    let recid1 = sig1[64];
    let sig1 = <&B512>::try_from(&sig1[..64]).unwrap();
    let Ok(addr1) = secp256k1::ecrecover(sig1, recid1, &msg_hash1) else { return revert() };

    let recid2 = sig2[64];
    let sig2 = <&B512>::try_from(&sig2[..64]).unwrap();
    let Ok(addr2) = secp256k1::ecrecover(sig2, recid2, &msg_hash2) else { return revert() };

    if !addr1.eq(&addr2) {
        return Ok(PrecompileOutput::halt(
            BscPrecompileError::DoubleSignInvalidEvidence.into(),
            reservoir,
        ));
    }

    let mut res = [0; 52];
    let signer = &addr1[12..];
    res[..20].clone_from_slice(signer);
    // go-bsc right-aligns `header.Number.Bytes()`, the minimal big-endian form. The bound check
    // above guarantees this never reaches back into the signer.
    let number = header1.number.as_bytes();
    res[52 - number.len()..].clone_from_slice(number);

    Ok(PrecompileOutput::new(
        DOUBLE_SIGN_EVIDENCE_VALIDATION_BASE,
        Bytes::copy_from_slice(&res),
        reservoir,
    ))
}

/// Seal hash of a header, mirroring go-bsc's `types.SealHash` / `types.EncodeSigHeader`.
///
/// The post-Cancun tail is appended only when `parent_beacon_root` is present, and
/// `requests_hash` only on top of that — matching go-bsc's nested conditionals exactly.
/// Note this deliberately differs from [`crate::consensus::parlia::util::encode_header_with_chain_id`],
/// which additionally requires the beacon root to be zero.
fn seal_hash(header: &Header, chain_id: &RlpBigInt) -> B256 {
    let mut payload = Vec::new();

    chain_id.encode(&mut payload);
    header.parent_hash.encode(&mut payload);
    header.uncle_hash.encode(&mut payload);
    header.coinbase.encode(&mut payload);
    header.root.encode(&mut payload);
    header.tx_hash.encode(&mut payload);
    header.receipt_hash.encode(&mut payload);
    header.bloom.encode(&mut payload);
    header.difficulty.encode(&mut payload);
    header.number.encode(&mut payload);
    header.gas_limit.encode(&mut payload);
    header.gas_used.encode(&mut payload);
    header.time.encode(&mut payload);
    // Caller guarantees `extra` is at least `EXTRA_SEAL_LENGTH` long.
    let sealed_extra: &[u8] = &header.extra[..header.extra.len() - EXTRA_SEAL_LENGTH];
    sealed_extra.encode(&mut payload);
    header.mix_digest.encode(&mut payload);
    header.nonce.encode(&mut payload);

    if let Some(parent_beacon_root) = header.parent_beacon_root {
        encode_optional(header.base_fee.as_ref(), &mut payload);
        encode_optional(header.withdrawals_hash.as_ref(), &mut payload);
        encode_optional(header.blob_gas_used.as_ref(), &mut payload);
        encode_optional(header.excess_blob_gas.as_ref(), &mut payload);
        parent_beacon_root.encode(&mut payload);

        // https://github.com/bnb-chain/BEPs/blob/master/BEPs/BEP-466.md
        if let Some(requests_hash) = header.requests_hash {
            requests_hash.encode(&mut payload);
        }
    }

    let mut encoded = Vec::with_capacity(payload.len() + 9);
    RlpHeader { list: true, payload_length: payload.len() }.encode(&mut encoded);
    encoded.extend_from_slice(&payload);

    keccak256(&encoded)
}

/// Encodes an optional field the way go-bsc encodes the pointer behind it: the value if set,
/// otherwise an empty string for nil.
///
/// Absent fields are unreachable here in practice, because the optional tail is positional and
/// this is only called once `parent_beacon_root` — a later field — is known to be present.
fn encode_optional<T: Encodable>(value: Option<&T>, out: &mut Vec<u8>) {
    match value {
        Some(value) => value.encode(out),
        None => out.push(EMPTY_STRING_CODE),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::hex;
    // `::` prefixed: `super::*` re-exports revm's `secp256k1` precompile module.
    use ::secp256k1::{Message, SecretKey, SECP256K1};

    /// Canonical minimal big-endian bytes of 123456, the height used by the crafted vectors.
    const HEIGHT_123456: [u8; 3] = [0x01, 0xe2, 0x40];

    fn big(bytes: &[u8]) -> RlpBigInt {
        RlpBigInt(Bytes::copy_from_slice(bytes))
    }

    fn base_header() -> Header {
        Header {
            parent_hash: [0x11; 32],
            uncle_hash: [0x22; 32],
            coinbase: [0x33; 20],
            root: [0x44; 32],
            tx_hash: [0x55; 32],
            receipt_hash: [0x66; 32],
            bloom: [0u8; 256],
            difficulty: big(&[0x02]),
            number: big(&HEIGHT_123456),
            gas_limit: 1_000_000,
            gas_used: 0,
            time: 0,
            // 32 bytes of vanity followed by the 65 byte seal.
            extra: Bytes::from(vec![0u8; 32 + EXTRA_SEAL_LENGTH]),
            mix_digest: [0x77; 32],
            nonce: [0u8; 8],
            base_fee: None,
            withdrawals_hash: None,
            blob_gas_used: None,
            excess_blob_gas: None,
            parent_beacon_root: None,
            requests_hash: None,
            block_access_list_hash: None,
            slot_number: None,
        }
    }

    fn signing_key() -> SecretKey {
        SecretKey::from_slice(&[0x11u8; 32]).unwrap()
    }

    /// Signs `header` in place over its seal hash, writing the signature into the seal slot.
    /// The seal slot is excluded from the hash, so overwriting it afterwards is sound.
    fn sign_header(header: &mut Header, chain_id: &RlpBigInt, key: &SecretKey) {
        let hash = seal_hash(header, chain_id);
        let signature =
            SECP256K1.sign_ecdsa_recoverable(&Message::from_digest(hash.0), key);
        let (recovery_id, data) = signature.serialize_compact();

        let mut extra = header.extra.to_vec();
        let len = extra.len();
        extra[len - EXTRA_SEAL_LENGTH..len - 1].copy_from_slice(&data);
        extra[len - 1] = i32::from(recovery_id) as u8;
        header.extra = Bytes::from(extra);
    }

    /// Builds valid evidence: two headers at the same height off the same parent, differing in
    /// state root, each signed by the same key.
    fn signed_evidence_with(chain_id: RlpBigInt, mutate: impl Fn(&mut Header, &mut Header)) -> Vec<u8> {
        let key = signing_key();
        let (mut header1, mut header2) = (base_header(), base_header());
        header2.root = [0x99; 32];
        mutate(&mut header1, &mut header2);

        sign_header(&mut header1, &chain_id, &key);
        sign_header(&mut header2, &chain_id, &key);

        alloy_rlp::encode(&DoubleSignEvidence {
            chain_id,
            header_bytes1: Bytes::from(alloy_rlp::encode(&header1)),
            header_bytes2: Bytes::from(alloy_rlp::encode(&header2)),
        })
    }

    fn signed_evidence(chain_id: RlpBigInt) -> Vec<u8> {
        signed_evidence_with(chain_id, |_, _| {})
    }

    /// Asserts a successful validation returning the expected height, and yields the signer.
    fn assert_valid(input: &[u8]) -> Vec<u8> {
        let output = double_sign_evidence_validation_run(input, 10_000, 0)
            .expect("should not return a fatal error");
        assert!(!output.is_halt(), "evidence should have been accepted, got a halt");
        assert_eq!(output.bytes.len(), 52, "expected signer||height");

        let mut expected_height = [0u8; 32];
        expected_height[32 - HEIGHT_123456.len()..].copy_from_slice(&HEIGHT_123456);
        assert_eq!(&output.bytes[20..], &expected_height[..], "height mismatch");

        output.bytes[..20].to_vec()
    }

    #[test]
    fn test_double_sign_evidence_validation_run() {
        let input = hex::decode("f906278202cab9030ff9030ca01062d3d5015b9242bc193a9b0769f3d3780ecb55f97f40a752ae26d0b68cd0d8a0fae1a05fcb14bfd9b8a9f2b65007a9b6c2000de0627a73be644dd993d32342c494976ea74026e726554db657fa54763abd0c3a0aa9a0f385cc58ed297ff0d66eb5580b02853d3478ba418b1819ac659ee05df49b9794a0bf88464af369ed6b8cf02db00f0b9556ffa8d49cd491b00952a7f83431446638a00a6d0870e586a76278fbfdcedf76ef6679af18fc1f9137cfad495f434974ea81b901000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001820cdf830f4240830f4240846555fa64b90111d983010301846765746888676f312e32302e378664617277696e00007abd731ef8ae07b86091cb8836d58f5444b883422a18825d899035d3e6ea39ad1a50069bf0b86da8b5573dde1cb4a0a34f19ce94e0ef78ff7518c80265b8a3ca56e3c60167523590d4e8dcc324900559465fc0fa403774096614e135de280949b58a45cc96f2ba9e17f848820d41a08429d0d8b33ee72a84f750fefea846cbca54e487129c7961c680bb72309ca888820d42a08c9db14d938b19f9e2261bbeca2679945462be2b58103dfff73665d0d150fb8a804ae755e0fe64b59753f4db6308a1f679747bce186aa2c62b95fa6eeff3fbd08f3b0667e45428a54ade15bad19f49641c499b431b36f65803ea71b379e6b61de501a0232c9ba2d41b40d36ed794c306747bcbc49bf61a0f37409c18bfe2b5bef26a2d880000000000000000b9030ff9030ca01062d3d5015b9242bc193a9b0769f3d3780ecb55f97f40a752ae26d0b68cd0d8a0b2789a5357827ed838335283e15c4dcc42b9bebcbf2919a18613246787e2f96094976ea74026e726554db657fa54763abd0c3a0aa9a071ce4c09ee275206013f0063761bc19c93c13990582f918cc57333634c94ce89a00e095703e5c9b149f253fe89697230029e32484a410b4b1f2c61442d73c3095aa0d317ae19ede7c8a2d3ac9ef98735b049bcb7278d12f48c42b924538b60a25e12b901000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001820cdf830f4240830f4240846555fa64b90111d983010301846765746888676f312e32302e378664617277696e00007abd731ef8ae07b86091cb8836d58f5444b883422a18825d899035d3e6ea39ad1a50069bf0b86da8b5573dde1cb4a0a34f19ce94e0ef78ff7518c80265b8a3ca56e3c60167523590d4e8dcc324900559465fc0fa403774096614e135de280949b58a45cc96f2ba9e17f848820d41a08429d0d8b33ee72a84f750fefea846cbca54e487129c7961c680bb72309ca888820d42a08c9db14d938b19f9e2261bbeca2679945462be2b58103dfff73665d0d150fb8a80c0b17bfe88534296ff064cb7156548f6deba2d6310d5044ed6485f087dc6ef232e051c28e1909c2b50a3b4f29345d66681c319bef653e52e5d746480d5a3983b00a0b56228685be711834d0f154292d07826dea42a0fad3e4f56c31470b7fbfbea26880000000000000000").unwrap();

        let res = double_sign_evidence_validation_run(&Bytes::from(input), 10_000, 0).unwrap();

        let gas = res.gas_used;
        assert_eq!(gas, 10_000u64);

        let res = hex::encode(res.bytes);
        assert_eq!(res, "15d34aaf54267db7d7c367839aaf71a00a2c6a650000000000000000000000000000000000000000000000000000000000000cdf")
    }

    #[test]
    fn test_double_sign_evidence_validation_invalid_header_number_length() {
        // Two identical headers: rejected because both carry the same seal.
        let header1 = base_header();
        let header2 = header1.clone();

        let evidence = DoubleSignEvidence {
            chain_id: big(&[0x01]),
            header_bytes1: Bytes::from(alloy_rlp::encode(&header1)),
            header_bytes2: Bytes::from(alloy_rlp::encode(&header2)),
        };

        let input = alloy_rlp::encode(&evidence);
        let output = double_sign_evidence_validation_run(&input, 10_000, 0)
            .expect("should not return fatal error");

        assert!(output.is_halt());
        assert!(
            matches!(output.halt_reason(), Some(PrecompileHalt::Other(s)) if s == "double sign invalid evidence")
        );
    }

    /// This vector carries a 33-byte block number (2^256), so it exercises go-bsc's
    /// `len(header.Number.Bytes()) > 32` bound check.
    ///
    /// go-bsc returns `errInvalidEvidence` here, which consumes all gas rather than reverting.
    /// Before the SRC-1509 fix reth-bsc could not represent the number at all and bailed out one
    /// step earlier with an RLP overflow, returning a *revert* — a divergence in its own right.
    /// The expectation below is the go-bsc-matching outcome.
    #[test]
    fn test_double_sign_evidence_validation_run_invalid_evidence() {
        let input = hex::decode("f9066b38b90332f9032fa01062d3d5015b9242bc193a9b0769f3d3780ecb55f97f40a752ae26d0b68cd0d8a0fae1a05fcb14bfd9b8a9f2b65007a9b6c2000de0627a73be644dd993d32342c494df87f0e2b8519ea2dd4abd8b639cdd628497ed25a0f385cc58ed297ff0d66eb5580b02853d3478ba418b1819ac659ee05df49b9794a0bf88464af369ed6b8cf02db00f0b9556ffa8d49cd491b00952a7f83431446638a00a6d0870e586a76278fbfdcedf76ef6679af18fc1f9137cfad495f434974ea81b901000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001a1010000000000000000000000000000000000000000000000000000000000000000830f4240830f42408465bc6996b90115d983010306846765746889676f312e32302e3131856c696e7578000053474aa9f8b25fb860b0844a5082bfaa2299d2a23f076e2f6b17b15f839cc3e7d5a875656f6733fd4b87ba3401f906d15f3dea263cd9a6076107c7db620a4630dd3832c4a4b57eb8f497e28a3d69e5c03b30205c4b45675747d513e1accd66329770f3c35b18c9d023f84c84023a5ad6a086a28d985d9a6c8e7f9a4feadd5ace0adba9818e1e1727edca755fcc0bd8344684023a5ad7a0bc3492196b2e68b8e6ceea87cfa7588b4d590089eb885c4f2c1e9d9fb450f7b980988e1b9d0beb91dab063e04879a24c43d33baae3759dee41fd62ffa83c77fd202bea27a829b49e8025bdd198393526dd12b223ab16052fd26a43f3aabf63e76901a0232c9ba2d41b40d36ed794c306747bcbc49bf61a0f37409c18bfe2b5bef26a2d880000000000000000b90332f9032fa01062d3d5015b9242bc193a9b0769f3d3780ecb55f97f40a752ae26d0b68cd0d8a0b2789a5357827ed838335283e15c4dcc42b9bebcbf2919a18613246787e2f96094df87f0e2b8519ea2dd4abd8b639cdd628497ed25a071ce4c09ee275206013f0063761bc19c93c13990582f918cc57333634c94ce89a00e095703e5c9b149f253fe89697230029e32484a410b4b1f2c61442d73c3095aa0d317ae19ede7c8a2d3ac9ef98735b049bcb7278d12f48c42b924538b60a25e12b901000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001a1010000000000000000000000000000000000000000000000000000000000000000830f4240830f42408465bc6996b90115d983010306846765746889676f312e32302e3131856c696e7578000053474aa9f8b25fb860b0844a5082bfaa2299d2a23f076e2f6b17b15f839cc3e7d5a875656f6733fd4b87ba3401f906d15f3dea263cd9a6076107c7db620a4630dd3832c4a4b57eb8f497e28a3d69e5c03b30205c4b45675747d513e1accd66329770f3c35b18c9d023f84c84023a5ad6a086a28d985d9a6c8e7f9a4feadd5ace0adba9818e1e1727edca755fcc0bd8344684023a5ad7a0bc3492196b2e68b8e6ceea87cfa7588b4d590089eb885c4f2c1e9d9fb450f7b9804c71ed015dd0c5c2d7393b68c2927f83f0a5da4c66f761f09e2f950cc610832c7876144599368404096ddef0eadacfde57717e2c7d23982b927285b797d41bfa00a0b56228685be711834d0f154292d07826dea42a0fad3e4f56c31470b7fbfbea26880000000000000000").unwrap();

        let output = double_sign_evidence_validation_run(&Bytes::from(input), 10_000, 0)
            .expect("should not return fatal error");

        assert!(output.is_halt(), "go-bsc reports errInvalidEvidence, not a revert");
        assert!(
            matches!(output.halt_reason(), Some(PrecompileHalt::Other(s)) if s == "double sign invalid evidence")
        );
    }

    /// SRC-1509: chain ids go-bsc represents as `*big.Int` must be accepted regardless of width.
    /// 2^64 is the reported vector; it sits one bit past what a `u64` could hold.
    #[test]
    fn test_chain_id_beyond_u64_is_accepted() {
        let two_pow_64 = {
            let mut v = vec![0x01];
            v.extend_from_slice(&[0u8; 8]);
            v
        };
        let two_pow_256 = {
            let mut v = vec![0x01];
            v.extend_from_slice(&[0u8; 32]);
            v
        };

        let vectors: [(&str, Vec<u8>); 5] = [
            ("0", vec![]),
            ("56", vec![0x38]),
            ("2^64 - 1", vec![0xff; 8]),
            ("2^64", two_pow_64),
            ("2^256", two_pow_256),
        ];

        let mut signers = Vec::new();
        for (label, chain_id) in vectors {
            let input = signed_evidence(big(&chain_id));
            let signer = assert_valid(&input);
            println!("chain id {label} accepted, signer {}", hex::encode(&signer));
            signers.push(signer);
        }

        // The chain id is domain separation for the seal hash, not an identity input, so the
        // recovered signer is the same key throughout.
        assert!(signers.windows(2).all(|w| w[0] == w[1]), "signer should not depend on chain id");
    }

    /// The boundary itself: 2^64 - 1 fits a `u64`, 2^64 does not. Both must behave identically.
    #[test]
    fn test_chain_id_u64_boundary_is_not_a_cliff() {
        let below = signed_evidence(big(&[0xff; 8]));
        let above = signed_evidence(big(&{
            let mut v = vec![0x01];
            v.extend_from_slice(&[0u8; 8]);
            v
        }));

        assert_valid(&below);
        assert_valid(&above);
    }

    /// A block number wider than 32 bytes is invalid evidence rather than a revert, matching
    /// go-bsc's explicit bound check.
    #[test]
    fn test_block_number_beyond_32_bytes_is_invalid_evidence() {
        let oversized = {
            let mut v = vec![0x01];
            v.extend_from_slice(&[0u8; 32]);
            v
        };
        let input = signed_evidence_with(big(&[0x38]), |header1, header2| {
            header1.number = big(&oversized);
            header2.number = big(&oversized);
        });

        let output = double_sign_evidence_validation_run(&input, 10_000, 0)
            .expect("should not return fatal error");

        assert!(output.is_halt());
        assert!(
            matches!(output.halt_reason(), Some(PrecompileHalt::Other(s)) if s == "double sign invalid evidence")
        );
    }

    /// A block number that needs more than 64 bits but stays within the 32-byte bound is valid,
    /// and its minimal big-endian form is what gets right-aligned into the result.
    #[test]
    fn test_block_number_beyond_u64_is_accepted() {
        let height = {
            let mut v = vec![0x01];
            v.extend_from_slice(&[0u8; 8]);
            v
        };
        let input = signed_evidence_with(big(&[0x38]), |header1, header2| {
            header1.number = big(&height);
            header2.number = big(&height);
        });

        let output = double_sign_evidence_validation_run(&input, 10_000, 0).unwrap();
        assert!(!output.is_halt(), "2^64 block number should be valid evidence");

        let mut expected = [0u8; 32];
        expected[32 - height.len()..].copy_from_slice(&height);
        assert_eq!(&output.bytes[20..], &expected[..]);
    }

    /// go-bsc's `rlp.DecodeBytes` rejects trailing bytes with `ErrMoreThanOneValue`; the previous
    /// `Decodable::decode` here ignored them, so reth-bsc accepted what go-bsc reverted.
    #[test]
    fn test_trailing_bytes_are_rejected() {
        let valid = signed_evidence(big(&[0x38]));
        assert_valid(&valid);

        let mut with_trailing = valid.clone();
        with_trailing.push(0x00);
        assert_eq!(
            double_sign_evidence_validation_run(&with_trailing, 10_000, 0),
            Ok(PrecompileOutput::revert(10_000, Default::default(), 0)),
            "trailing byte after the evidence envelope must revert"
        );
    }

    /// Same strictness for the inner header blobs.
    #[test]
    fn test_trailing_bytes_in_header_are_rejected() {
        let key = signing_key();
        let chain_id = big(&[0x38]);
        let (mut header1, mut header2) = (base_header(), base_header());
        header2.root = [0x99; 32];
        sign_header(&mut header1, &chain_id, &key);
        sign_header(&mut header2, &chain_id, &key);

        let mut header_bytes1 = alloy_rlp::encode(&header1);
        header_bytes1.push(0x00);

        let input = alloy_rlp::encode(&DoubleSignEvidence {
            chain_id,
            header_bytes1: Bytes::from(header_bytes1),
            header_bytes2: Bytes::from(alloy_rlp::encode(&header2)),
        });

        assert_eq!(
            double_sign_evidence_validation_run(&input, 10_000, 0),
            Ok(PrecompileOutput::revert(10_000, Default::default(), 0)),
            "trailing byte after a header must revert"
        );
    }

    /// Post-Cancun headers carry an `rlp:"optional"` tail. The previous 15-field struct rejected
    /// them outright, so legitimate evidence for any recent block diverged from go-bsc.
    #[test]
    fn test_post_cancun_header_is_accepted() {
        let input = signed_evidence_with(big(&[0x38]), |header1, header2| {
            for header in [header1, header2] {
                header.base_fee = Some(big(&[]));
                header.withdrawals_hash = Some([0xaa; 32]);
                header.blob_gas_used = Some(0);
                header.excess_blob_gas = Some(0);
                header.parent_beacon_root = Some([0u8; 32]);
            }
        });

        assert_valid(&input);
    }

    /// The optional tail participates in the seal hash, so changing it changes the signature
    /// domain. If the tail were dropped these two would collide.
    #[test]
    fn test_optional_tail_is_covered_by_seal_hash() {
        let chain_id = big(&[0x38]);

        let bare = base_header();
        let mut with_tail = base_header();
        with_tail.base_fee = Some(big(&[]));
        with_tail.withdrawals_hash = Some([0xaa; 32]);
        with_tail.blob_gas_used = Some(0);
        with_tail.excess_blob_gas = Some(0);
        with_tail.parent_beacon_root = Some([0u8; 32]);

        assert_ne!(
            seal_hash(&bare, &chain_id),
            seal_hash(&with_tail, &chain_id),
            "the post-Cancun tail must be folded into the seal hash"
        );

        // `requests_hash` extends the tail further still.
        let mut with_requests = with_tail.clone();
        with_requests.requests_hash = Some([0xbb; 32]);
        assert_ne!(seal_hash(&with_tail, &chain_id), seal_hash(&with_requests, &chain_id));
    }

    /// A header whose tail stops before `parent_beacon_root` seals exactly like go-bsc: the tail
    /// is not appended at all.
    #[test]
    fn test_tail_before_beacon_root_is_not_sealed() {
        let chain_id = big(&[0x38]);

        let bare = base_header();
        let mut base_fee_only = base_header();
        base_fee_only.base_fee = Some(big(&[0x07]));

        assert_eq!(
            seal_hash(&bare, &chain_id),
            seal_hash(&base_fee_only, &chain_id),
            "go-bsc only appends the tail once ParentBeaconRoot is present"
        );
    }

    /// Emits the SRC-1509 cross-client vector set as JSON on stdout, together with this
    /// build's outcome for each. Consumed by the go-bsc differential harness.
    /// Run with: `cargo test -p reth_bsc dump_ab_vectors -- --nocapture --ignored`
    #[test]
    #[ignore = "vector generator for the go-bsc differential harness, not an assertion"]
    fn dump_ab_vectors() {
        fn outcome(input: &[u8]) -> String {
            match double_sign_evidence_validation_run(input, 10_000, 0) {
                Ok(output) if output.is_halt() => match output.halt_reason() {
                    Some(PrecompileHalt::Other(reason)) => format!("halt:{reason}"),
                    other => format!("halt:{other:?}"),
                },
                Ok(output) if output.bytes.is_empty() => "revert".to_string(),
                Ok(output) => format!("ok:{}", hex::encode(output.bytes)),
                Err(err) => format!("fatal:{err:?}"),
            }
        }

        let wide = |lead: u8, zeros: usize| {
            let mut v = vec![lead];
            v.extend(std::iter::repeat_n(0u8, zeros));
            v
        };

        let mut vectors: Vec<(String, Vec<u8>)> = Vec::new();

        for (label, chain_id) in [
            ("chain_id_0", vec![]),
            ("chain_id_56", vec![0x38]),
            ("chain_id_2pow64_minus_1", vec![0xff; 8]),
            ("chain_id_2pow64", wide(0x01, 8)),
            ("chain_id_2pow256", wide(0x01, 32)),
        ] {
            vectors.push((label.to_string(), signed_evidence(big(&chain_id))));
        }

        // Block number wider than u64 but within the 32 byte bound: valid evidence.
        vectors.push((
            "number_2pow64".to_string(),
            signed_evidence_with(big(&[0x38]), |h1, h2| {
                let n = wide(0x01, 8);
                h1.number = big(&n);
                h2.number = big(&n);
            }),
        ));

        // Block number past the 32 byte bound: invalid evidence on both clients.
        vectors.push((
            "number_2pow256_oversized".to_string(),
            signed_evidence_with(big(&[0x38]), |h1, h2| {
                let n = wide(0x01, 32);
                h1.number = big(&n);
                h2.number = big(&n);
            }),
        ));

        // Post-Cancun optional tail, with and without the BEP-466 requests hash.
        vectors.push((
            "post_cancun_tail".to_string(),
            signed_evidence_with(big(&[0x38]), |h1, h2| {
                for h in [h1, h2] {
                    h.base_fee = Some(big(&[]));
                    h.withdrawals_hash = Some([0xaa; 32]);
                    h.blob_gas_used = Some(0);
                    h.excess_blob_gas = Some(0);
                    h.parent_beacon_root = Some([0u8; 32]);
                }
            }),
        ));
        vectors.push((
            "post_cancun_tail_with_requests_hash".to_string(),
            signed_evidence_with(big(&[0x38]), |h1, h2| {
                for h in [h1, h2] {
                    h.base_fee = Some(big(&[0x07]));
                    h.withdrawals_hash = Some([0xaa; 32]);
                    h.blob_gas_used = Some(1);
                    h.excess_blob_gas = Some(2);
                    h.parent_beacon_root = Some([0u8; 32]);
                    h.requests_hash = Some([0xbb; 32]);
                }
            }),
        ));

        // Trailing bytes: rejected by go-bsc's rlp.DecodeBytes.
        let mut envelope_trailing = signed_evidence(big(&[0x38]));
        envelope_trailing.push(0x00);
        vectors.push(("trailing_byte_envelope".to_string(), envelope_trailing));

        {
            let key = signing_key();
            let chain_id = big(&[0x38]);
            let (mut h1, mut h2) = (base_header(), base_header());
            h2.root = [0x99; 32];
            sign_header(&mut h1, &chain_id, &key);
            sign_header(&mut h2, &chain_id, &key);
            let mut header_bytes1 = alloy_rlp::encode(&h1);
            header_bytes1.push(0x00);
            vectors.push((
                "trailing_byte_header".to_string(),
                alloy_rlp::encode(&DoubleSignEvidence {
                    chain_id,
                    header_bytes1: Bytes::from(header_bytes1),
                    header_bytes2: Bytes::from(alloy_rlp::encode(&h2)),
                }),
            ));
        }

        // The upstream vector shipped with this precompile, as a control.
        vectors.push((
            "upstream_control_vector".to_string(),
            hex::decode("f906278202cab9030ff9030ca01062d3d5015b9242bc193a9b0769f3d3780ecb55f97f40a752ae26d0b68cd0d8a0fae1a05fcb14bfd9b8a9f2b65007a9b6c2000de0627a73be644dd993d32342c494976ea74026e726554db657fa54763abd0c3a0aa9a0f385cc58ed297ff0d66eb5580b02853d3478ba418b1819ac659ee05df49b9794a0bf88464af369ed6b8cf02db00f0b9556ffa8d49cd491b00952a7f83431446638a00a6d0870e586a76278fbfdcedf76ef6679af18fc1f9137cfad495f434974ea81b901000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001820cdf830f4240830f4240846555fa64b90111d983010301846765746888676f312e32302e378664617277696e00007abd731ef8ae07b86091cb8836d58f5444b883422a18825d899035d3e6ea39ad1a50069bf0b86da8b5573dde1cb4a0a34f19ce94e0ef78ff7518c80265b8a3ca56e3c60167523590d4e8dcc324900559465fc0fa403774096614e135de280949b58a45cc96f2ba9e17f848820d41a08429d0d8b33ee72a84f750fefea846cbca54e487129c7961c680bb72309ca888820d42a08c9db14d938b19f9e2261bbeca2679945462be2b58103dfff73665d0d150fb8a804ae755e0fe64b59753f4db6308a1f679747bce186aa2c62b95fa6eeff3fbd08f3b0667e45428a54ade15bad19f49641c499b431b36f65803ea71b379e6b61de501a0232c9ba2d41b40d36ed794c306747bcbc49bf61a0f37409c18bfe2b5bef26a2d880000000000000000b9030ff9030ca01062d3d5015b9242bc193a9b0769f3d3780ecb55f97f40a752ae26d0b68cd0d8a0b2789a5357827ed838335283e15c4dcc42b9bebcbf2919a18613246787e2f96094976ea74026e726554db657fa54763abd0c3a0aa9a071ce4c09ee275206013f0063761bc19c93c13990582f918cc57333634c94ce89a00e095703e5c9b149f253fe89697230029e32484a410b4b1f2c61442d73c3095aa0d317ae19ede7c8a2d3ac9ef98735b049bcb7278d12f48c42b924538b60a25e12b901000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001820cdf830f4240830f4240846555fa64b90111d983010301846765746888676f312e32302e378664617277696e00007abd731ef8ae07b86091cb8836d58f5444b883422a18825d899035d3e6ea39ad1a50069bf0b86da8b5573dde1cb4a0a34f19ce94e0ef78ff7518c80265b8a3ca56e3c60167523590d4e8dcc324900559465fc0fa403774096614e135de280949b58a45cc96f2ba9e17f848820d41a08429d0d8b33ee72a84f750fefea846cbca54e487129c7961c680bb72309ca888820d42a08c9db14d938b19f9e2261bbeca2679945462be2b58103dfff73665d0d150fb8a80c0b17bfe88534296ff064cb7156548f6deba2d6310d5044ed6485f087dc6ef232e051c28e1909c2b50a3b4f29345d66681c319bef653e52e5d746480d5a3983b00a0b56228685be711834d0f154292d07826dea42a0fad3e4f56c31470b7fbfbea26880000000000000000").unwrap(),
        ));

        println!("---BEGIN_AB_VECTORS---");
        println!("[");
        for (i, (name, input)) in vectors.iter().enumerate() {
            let comma = if i + 1 == vectors.len() { "" } else { "," };
            println!(
                "  {{\"name\":\"{}\",\"input\":\"{}\",\"reth\":\"{}\"}}{}",
                name,
                hex::encode(input),
                outcome(input),
                comma
            );
        }
        println!("]");
        println!("---END_AB_VECTORS---");
    }

    #[test]
    fn test_rlp_big_int_canonical_forms() {
        // Empty string is zero.
        assert_eq!(RlpBigInt::decode(&mut &hex!("80")[..]).unwrap().byte_len(), 0);
        // Leading zeros are non-canonical, as in go-bsc's ErrCanonInt.
        assert!(RlpBigInt::decode(&mut &hex!("8200f4")[..]).is_err());
        assert!(RlpBigInt::decode(&mut &hex!("00")[..]).is_err());
        // A single byte below 0x80 must use the short form, as in go-bsc's ErrCanonSize.
        assert!(RlpBigInt::decode(&mut &hex!("8105")[..]).is_err());
        // Lists are not integers.
        assert!(RlpBigInt::decode(&mut &hex!("c101")[..]).is_err());

        // The SRC-1509 vector, 2^64, round-trips unchanged.
        let encoded = hex!("89010000000000000000");
        let decoded = RlpBigInt::decode(&mut &encoded[..]).unwrap();
        assert_eq!(decoded.byte_len(), 9);
        assert_eq!(decoded.as_bytes(), &hex!("010000000000000000")[..]);
        assert_eq!(alloy_rlp::encode(&decoded), &encoded[..]);
    }
}
