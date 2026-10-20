//! Credits to <https://github.com/bnb-chain/revm/blob/d66170e712460ae766fc26a063f106658ce33e9d/crates/precompile/src/cometbft.rs>
use crate::evm::precompiles::{dedup::DuplicateTracker, error::BscPrecompileError};
use alloy_primitives::Bytes;
use cometbft::{
    block::{self, signed_header::SignedHeader, CommitSig},
    crypto::{default::Sha256, signature},
    hash::Hash,
    merkle::{self, simple_hash_from_byte_vectors},
    validator::Set,
    vote::{self, Power, SignedVote, ValidatorIndex, Vote},
    PublicKey, Signature,
};
use cometbft_light_client::types::LightBlock;
use cometbft_light_client_verifier::types::Validator;
use cometbft_proto::{
    google::protobuf::Timestamp,
    types::v1::{
        BlockId as RawBlockId, LightBlock as TmLightBlock, SimpleValidator as TmSimpleValidator,
        Validator as TmValidator, ValidatorSet as TmValidatorSet,
    },
    version::v1::Consensus as RawConsensusVersion,
    Protobuf,
};
use curve25519_dalek::{edwards::CompressedEdwardsY, EdwardsPoint, Scalar};
use prost::Message as _;
use prost12::Message;
use revm::precompile::{
    u64_to_address, PrecompileHalt, PrecompileOutput, PrecompileResult, Precompile, PrecompileId,
};
use sha2::{Digest, Sha512};
use std::{borrow::{Cow, ToOwned}, string::String, vec::Vec};

pub(crate) const COMETBFT_LIGHT_BLOCK_VALIDATION: Precompile =
    Precompile::new(PrecompileId::Custom(Cow::Borrowed("COMET_BFT_LIGHT_BLOCK_VALIDATE_HERTZ")), u64_to_address(103), cometbft_light_block_validation_run);

pub(crate) const COMETBFT_LIGHT_BLOCK_VALIDATION_BEFORE_HERTZ: Precompile =
    Precompile::new(PrecompileId::Custom(Cow::Borrowed("COMET_BFT_LIGHT_BLOCK_VALIDATE")), u64_to_address(103), cometbft_light_block_validation_run_before_hertz);

pub(crate) const COMETBFT_LIGHT_BLOCK_VALIDATION_PASTEUR: Precompile =
    Precompile::new(PrecompileId::Custom(Cow::Borrowed("COMET_BFT_LIGHT_BLOCK_VALIDATE_PASTEUR")), u64_to_address(103), cometbft_light_block_validation_run_pasteur);

const UINT64_TYPE_LENGTH: u64 = 8;
const CONSENSUS_STATE_LENGTH_BYTES_LENGTH: u64 = 32;
const VALIDATE_RESULT_METADATA_LENGTH: u64 = 32;

const CHAIN_ID_LENGTH: u64 = 32;
const HEIGHT_LENGTH: u64 = 8;
const VALIDATOR_SET_HASH_LENGTH: u64 = 32;
const VALIDATOR_PUBKEY_LENGTH: u64 = 32;
const VALIDATOR_VOTING_POWER_LENGTH: u64 = 8;
const RELAYER_ADDRESS_LENGTH: u64 = 20;
const RELAYER_BLS_KEY_LENGTH: u64 = 48;

const SINGLE_VALIDATOR_BYTES_LENGTH: u64 = VALIDATOR_PUBKEY_LENGTH +
    VALIDATOR_VOTING_POWER_LENGTH +
    RELAYER_ADDRESS_LENGTH +
    RELAYER_BLS_KEY_LENGTH;

const MAX_CONSENSUS_STATE_LENGTH: u64 = CHAIN_ID_LENGTH +
    HEIGHT_LENGTH +
    VALIDATOR_SET_HASH_LENGTH +
    99 * SINGLE_VALIDATOR_BYTES_LENGTH;

/// Base gas for the cometBFT light-block validation precompile.
const COMETBFT_LIGHT_BLOCK_VALIDATION_BASE: u64 = 3_000;

/// Per-input-byte gas charged from Pasteur, so cost scales with the validator/signature
/// count instead of being a flat fee.
const COMETBFT_LIGHT_BLOCK_VALIDATE_PER_BYTE_GAS: u64 = 16;

fn cometbft_light_block_validation_run(input: &[u8], gas_limit: u64, reservoir: u64) -> PrecompileResult {
    cometbft_light_block_validation_run_inner(input, gas_limit, reservoir, true, false, 0)
}

fn cometbft_light_block_validation_run_before_hertz(
    input: &[u8],
    gas_limit: u64,
    reservoir: u64,
) -> PrecompileResult {
    cometbft_light_block_validation_run_inner(input, gas_limit, reservoir, false, false, 0)
}

fn cometbft_light_block_validation_run_pasteur(
    input: &[u8],
    gas_limit: u64,
    reservoir: u64,
) -> PrecompileResult {
    // From Pasteur the cost scales with input size and duplicate validators are rejected.
    cometbft_light_block_validation_run_inner(
        input,
        gas_limit,
        reservoir,
        true,
        true,
        COMETBFT_LIGHT_BLOCK_VALIDATE_PER_BYTE_GAS,
    )
}

fn cometbft_light_block_validation_run_inner(
    input: &[u8],
    gas_limit: u64,
    reservoir: u64,
    is_hertz: bool,
    require_unique_validators: bool,
    per_byte_gas: u64,
) -> PrecompileResult {
    let cost = COMETBFT_LIGHT_BLOCK_VALIDATION_BASE
        .saturating_add((input.len() as u64).saturating_mul(per_byte_gas));

    if cost > gas_limit {
        return Ok(PrecompileOutput::halt(PrecompileHalt::OutOfGas, reservoir));
    }

    let (mut consensus_state, tm_light_block, randao_mix) =
        match decode_light_block_validation_input(input, require_unique_validators) {
            Ok(v) => v,
            Err(h) => return Ok(PrecompileOutput::halt(h, reservoir)),
        };

    let (light_block, validators) = match convert_light_block_from_proto(&tm_light_block) {
        Ok(v) => v,
        Err(h) => return Ok(PrecompileOutput::halt(h, reservoir)),
    };
    // Hash the validator set as supplied on the wire; the converted set cannot reproduce it.
    let validators_hash = match tm_light_block.validator_set.as_ref() {
        Some(validator_set) => validator_set_hash(validator_set),
        None => {
            return Ok(PrecompileOutput::halt(BscPrecompileError::InvalidInput.into(), reservoir))
        }
    };

    // From Pasteur, reject duplicate identities in the incoming light block's validator set
    // (the trusted consensus state set is checked during decoding above).
    if require_unique_validators {
        if let Err(h) = validate_unique_validator_set(&validators) {
            return Ok(PrecompileOutput::halt(h, reservoir));
        }
    }

    let mut validator_set_changed = match consensus_state.apply_light_block(
        &light_block,
        &randao_mix,
        validators_hash,
        validators,
    ) {
        Ok(v) => v,
        Err(h) => return Ok(PrecompileOutput::halt(h, reservoir)),
    };
    if !is_hertz {
        validator_set_changed = false;
    }

    let consensus_state_bytes = match consensus_state.encode() {
        Ok(v) => v,
        Err(h) => return Ok(PrecompileOutput::halt(h, reservoir)),
    };

    Ok(PrecompileOutput::new(
        cost,
        encode_light_block_validation_result(validator_set_changed, consensus_state_bytes),
        reservoir,
    ))
}

/// The light block, plus its validators in protobuf order. `Set` sorts validators by power and
/// address, but go-bsc keeps the wire order: commit signatures are bound to validators by
/// index, and the set is hashed and re-encoded in that order.
/// The CometBFT/Go zero time (`0001-01-01T00:00:00Z`), which is `EPOCH - 62135596800` seconds.
/// gogoproto's `nullable=false` stdtime fields decode an omitted value to exactly this, and it is
/// the same sentinel used for absent votes, so normalising an absent timestamp to it reproduces
/// go-bsc's decode result bit-for-bit.
fn go_zero_timestamp() -> Timestamp {
    Timestamp { seconds: -62_135_596_800, nanos: 0 }
}

type ConvertLightBlockResult = Result<(LightBlock, Vec<Validator>), PrecompileHalt>;
fn convert_light_block_from_proto(light_block_proto: &TmLightBlock) -> ConvertLightBlockResult {
    // Both fields are optional in the protobuf definition, so any caller can omit them by
    // crafting the precompile input; treat a missing field as invalid input instead of
    // unwrapping it.
    let signed_header_proto =
        light_block_proto.signed_header.as_ref().ok_or(BscPrecompileError::InvalidInput)?;

    // go-bsc models `Header.time` (protobuf field 4) as a non-nullable `time.Time`
    // (gogoproto `nullable=false, stdtime=true`), so an omitted field 4 decodes to Go's zero
    // time and `Header.ValidateBasic` never rejects it. The pinned Greenfield Rust model has
    // `time: Option<Timestamp>` and `Header::try_from` returns `missing_timestamp` when it is
    // absent, which would make reth reject a light block that canonical geth accepts. Reproduce
    // the gogoproto non-nullable semantics by normalising an absent timestamp to the Go/protobuf
    // zero time before domain conversion.
    let mut signed_header_proto = signed_header_proto.clone();
    if let Some(header) = signed_header_proto.header.as_mut() {
        header.time.get_or_insert_with(go_zero_timestamp);
    }

    let signed_header = match SignedHeader::try_from(signed_header_proto) {
        Ok(sh) => sh.clone(),
        Err(_) => return Err(BscPrecompileError::InvalidInput.into()),
    };

    let mut validator_set_proto =
        light_block_proto.validator_set.clone().ok_or(BscPrecompileError::InvalidInput)?;
    fill_unset_bridge_fields(&mut validator_set_proto);
    let validator_set = match Set::try_from(validator_set_proto.clone()) {
        Ok(vs) => vs.clone(),
        Err(_) => return Err(BscPrecompileError::InvalidInput.into()),
    };
    // Each zero-filled entry already converted successfully inside `Set::try_from` above.
    let validators = validator_set_proto
        .validators
        .iter()
        .map(|v| Validator::try_from(v.clone()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| BscPrecompileError::InvalidInput)?;

    let next_validator_set = validator_set.clone();
    let peer_id = cometbft::node::Id::new([0u8; 20]);
    Ok((LightBlock::new(signed_header, validator_set, next_validator_set, peer_id), validators))
}

/// go-bsc's `Validator.ValidateBasic` accepts an empty relayer BLS key or relayer address as
/// "unset", but the cometbft-rs conversion only accepts exactly 48 / 20 bytes. Zero-fill empty
/// fields, which is how go-bsc stores them in the encoded consensus state (fixed-width `copy`),
/// so the precompile accepts the same light blocks. Non-empty values of the wrong length are
/// left alone and still rejected by the conversion, as in go-bsc.
///
/// The validator-set hash differs: go-bsc omits an empty field from `SimpleValidator`, while a
/// `Set` built here always encodes 48 / 20 bytes. Never hash this `Set` to check it against
/// `header.validators_hash`; hash the raw protobuf validators with `validator_set_hash`.
fn fill_unset_bridge_fields(validator_set: &mut TmValidatorSet) {
    let fill = |validator: &mut TmValidator| {
        if validator.bls_key.is_empty() {
            validator.bls_key = vec![0; RELAYER_BLS_KEY_LENGTH as usize];
        }
        if validator.relayer_address.is_empty() {
            validator.relayer_address = vec![0; RELAYER_ADDRESS_LENGTH as usize];
        }
    };
    validator_set.validators.iter_mut().for_each(fill);
    // The proposer is converted separately and must stay equal to its entry in the set.
    validator_set.proposer.iter_mut().for_each(fill);
}

/// Go BSC's `ValidatorSet.Hash`: a merkle root over each validator's `SimpleValidator`
/// encoding, in wire order, where an empty BLS key or relayer address is omitted. The converted
/// `Set` cannot reproduce this: cometbft-rs sorts it and always encodes 48 / 20-byte fields.
fn validator_set_hash(validator_set: &TmValidatorSet) -> Hash {
    let validators: Vec<Vec<u8>> = validator_set
        .validators
        .iter()
        .map(|validator| {
            TmSimpleValidator {
                pub_key: validator.pub_key.clone(),
                voting_power: validator.voting_power,
                bls_key: validator.bls_key.clone(),
                relayer_address: validator.relayer_address.clone(),
            }
            .encode_to_vec()
        })
        .collect();
    Hash::Sha256(merkle::simple_hash_from_byte_vectors::<cometbft::crypto::default::Sha256>(
        &validators,
    ))
}

// The pinned Rust CometBFT protobuf omits Greenfield's header field 15. Go BSC includes that
// RandaoMix field in the header hash, even when it is empty, so retain it from the input bytes.
#[derive(Clone, PartialEq, prost::Message)]
struct HeaderExtensionFields {
    #[prost(bytes = "vec", tag = "15")]
    randao_mix: Vec<u8>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct SignedHeaderExtensionFields {
    #[prost(message, optional, tag = "1")]
    header: Option<HeaderExtensionFields>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct LightBlockExtensionFields {
    #[prost(message, optional, tag = "1")]
    signed_header: Option<SignedHeaderExtensionFields>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct BytesValue {
    #[prost(bytes = "vec", tag = "1")]
    value: Vec<u8>,
}

fn header_hash_with_randao_mix(header: &block::Header, randao_mix: &[u8]) -> Hash {
    // Match greenfield-cometbft/types.Header.Hash: 14 standard leaves plus RandaoMix.
    let fields = vec![
        Protobuf::<RawConsensusVersion>::encode_vec(header.version),
        header.chain_id.clone().encode_vec(),
        header.height.encode_vec(),
        header.time.encode_vec(),
        Protobuf::<RawBlockId>::encode_vec(header.last_block_id.unwrap_or_default()),
        header.last_commit_hash.unwrap_or_default().encode_vec(),
        header.data_hash.unwrap_or_default().encode_vec(),
        header.validators_hash.encode_vec(),
        header.next_validators_hash.encode_vec(),
        header.consensus_hash.encode_vec(),
        header.app_hash.clone().encode_vec(),
        header.last_results_hash.unwrap_or_default().encode_vec(),
        header.evidence_hash.unwrap_or_default().encode_vec(),
        header.proposer_address.encode_vec(),
        BytesValue { value: randao_mix.to_vec() }.encode_to_vec(),
    ];
    Hash::Sha256(merkle::simple_hash_from_byte_vectors::<cometbft::crypto::default::Sha256>(
        &fields,
    ))
}

fn decode_randao_mix(light_block_bytes: &[u8]) -> Result<Vec<u8>, PrecompileHalt> {
    let extension_fields = LightBlockExtensionFields::decode(light_block_bytes)
        .map_err(|_| BscPrecompileError::InvalidInput)?;
    let randao_mix = extension_fields
        .signed_header
        .and_then(|signed| signed.header)
        .map(|header| header.randao_mix)
        .unwrap_or_default();
    // Matches Greenfield Header.ValidateBasic (empty or one Ed25519 signature).
    if !randao_mix.is_empty() && randao_mix.len() != 64 {
        return Err(BscPrecompileError::InvalidInput.into());
    }
    Ok(randao_mix)
}

type DecodeLightBlockResult = Result<(ConsensusState, TmLightBlock, Vec<u8>), PrecompileHalt>;
fn decode_light_block_validation_input(
    input: &[u8],
    require_unique_validators: bool,
) -> DecodeLightBlockResult {
    let input_length = input.len() as u64;
    if input_length < CONSENSUS_STATE_LENGTH_BYTES_LENGTH {
        return Err(BscPrecompileError::InvalidInput.into());
    }

    let cs_length = u64::from_be_bytes(
        input[CONSENSUS_STATE_LENGTH_BYTES_LENGTH as usize - UINT64_TYPE_LENGTH as usize..
            CONSENSUS_STATE_LENGTH_BYTES_LENGTH as usize]
            .try_into()
            .unwrap(),
    );
    let input_length_checked = CONSENSUS_STATE_LENGTH_BYTES_LENGTH.checked_add(cs_length);
    if input_length_checked.is_none() {
        // overflow
        return Err(BscPrecompileError::InvalidInput.into());
    }

    if input_length < input_length_checked.unwrap() {
        return Err(BscPrecompileError::InvalidInput.into());
    }

    let decode_input = Bytes::from(
        input[CONSENSUS_STATE_LENGTH_BYTES_LENGTH as usize..
            (CONSENSUS_STATE_LENGTH_BYTES_LENGTH + cs_length) as usize]
            .to_vec(),
    );
    let consensus_state = decode_consensus_state(&decode_input, require_unique_validators)?;

    let light_block_bytes =
        &input[CONSENSUS_STATE_LENGTH_BYTES_LENGTH as usize + cs_length as usize..];
    let mut light_block_pb: TmLightBlock = TmLightBlock::default();
    match light_block_pb.merge(light_block_bytes) {
        Ok(pb) => pb,
        Err(_) => return Err(BscPrecompileError::InvalidInput.into()),
    };
    let randao_mix = decode_randao_mix(light_block_bytes)?;

    Ok((consensus_state, light_block_pb, randao_mix))
}

struct ConsensusState {
    chain_id: String,
    height: u64,
    next_validator_set_hash: Bytes,
    /// In wire order, as go-bsc keeps it (see `ConvertLightBlockResult`).
    validators: Vec<Validator>,
}

/// Bind the supplied validator set to the header and the header to the signed commit.
/// `validators_hash` is `validator_set_hash` of the validator set as supplied on the wire.
fn validate_light_block_bindings(
    light_block: &LightBlock,
    randao_mix: &[u8],
    validators_hash: Hash,
) -> Result<(), PrecompileHalt> {
    // These are the hash checks in Go BSC's LightBlock.ValidateBasic. Signature and
    // voting-power checks alone do not authenticate the header or the validator metadata.
    if validators_hash != light_block.signed_header.header().validators_hash ||
        header_hash_with_randao_mix(light_block.signed_header.header(), randao_mix) !=
            light_block.signed_header.commit.block_id.hash
    {
        return Err(BscPrecompileError::CometBftApplyBlockFailed.into());
    }
    Ok(())
}

impl ConsensusState {
    fn new(
        chain_id: String,
        height: u64,
        next_validator_set_hash: Bytes,
        validators: Vec<Validator>,
    ) -> Self {
        Self { chain_id, height, next_validator_set_hash, validators }
    }

    /// `validators` is the light block's validator set in wire order.
    fn apply_light_block(
        &mut self,
        light_block: &LightBlock,
        randao_mix: &[u8],
        validators_hash: Hash,
        validators: Vec<Validator>,
    ) -> Result<bool, PrecompileHalt> {
        if light_block.height().value() <= self.height {
            return Err(BscPrecompileError::InvalidInput.into());
        }
        if light_block.signed_header.header().chain_id.as_str() != self.chain_id {
            return Err(BscPrecompileError::InvalidInput.into());
        }

        // Required for both adjacent and skipping updates, before any state is changed.
        validate_light_block_bindings(light_block, randao_mix, validators_hash)?;

        let signed_header = &light_block.signed_header;
        if self.height + 1 == light_block.height().value() {
            if self.next_validator_set_hash.ne(signed_header.header().validators_hash.as_bytes()) {
                return Err(BscPrecompileError::CometBftApplyBlockFailed.into());
            }
            verify_commit_light(signed_header, &validators)?;
        } else {
            // More than 1/3 of the trusted set, then more than 2/3 of the new set, must have
            // signed. The second check stays last, as in go-bsc.
            verify_commit_light_trusting(signed_header, &self.validators)?;
            verify_commit_light(signed_header, &validators)?;
        }

        let validator_set_changed = trusted_validator_set_hash(&self.validators)
            .ne(signed_header.header().validators_hash.as_bytes());
        self.height = light_block.height().value();
        self.next_validator_set_hash =
            Bytes::from(signed_header.header().next_validators_hash.as_bytes().to_vec());
        self.validators = validators;

        Ok(validator_set_changed)
    }

    fn encode(&self) -> Result<Bytes, PrecompileHalt> {
        let validator_set_length = self.validators.len();
        let serialize_length = (CHAIN_ID_LENGTH +
            HEIGHT_LENGTH +
            VALIDATOR_SET_HASH_LENGTH +
            validator_set_length as u64 * SINGLE_VALIDATOR_BYTES_LENGTH)
            as usize;
        if serialize_length > MAX_CONSENSUS_STATE_LENGTH as usize {
            return Err(BscPrecompileError::CometBftEncodeConsensusStateFailed.into());
        }
        if self.chain_id.len() > CHAIN_ID_LENGTH as usize {
            return Err(BscPrecompileError::CometBftEncodeConsensusStateFailed.into());
        }

        let mut output = vec![0; serialize_length];
        let mut pos: usize = 0;
        let chain_id_bytes = self.chain_id.as_bytes();
        if chain_id_bytes.len() > CHAIN_ID_LENGTH as usize {
            return Err(BscPrecompileError::CometBftEncodeConsensusStateFailed.into());
        }
        let mut filled_chain_id = [0u8; 32];
        filled_chain_id[..chain_id_bytes.len()].copy_from_slice(chain_id_bytes);
        output[pos..pos + CHAIN_ID_LENGTH as usize]
            .copy_from_slice(filled_chain_id.to_vec().as_slice());
        pos += CHAIN_ID_LENGTH as usize;

        output[pos..pos + HEIGHT_LENGTH as usize].copy_from_slice(&self.height.to_be_bytes());
        pos += HEIGHT_LENGTH as usize;

        output[pos..pos + VALIDATOR_SET_HASH_LENGTH as usize]
            .copy_from_slice(self.next_validator_set_hash.as_ref());
        pos += VALIDATOR_SET_HASH_LENGTH as usize;

        for i in 0..validator_set_length {
            let validator = &self.validators[i];
            let voting_power = validator.power();

            output[pos..pos + VALIDATOR_PUBKEY_LENGTH as usize]
                .copy_from_slice(&validator.pub_key.to_bytes());
            pos += VALIDATOR_PUBKEY_LENGTH as usize;

            output[pos..pos + VALIDATOR_VOTING_POWER_LENGTH as usize]
                .copy_from_slice(&voting_power.to_be_bytes());
            pos += VALIDATOR_VOTING_POWER_LENGTH as usize;

            output[pos..pos + RELAYER_ADDRESS_LENGTH as usize]
                .copy_from_slice(validator.relayer_address.as_ref());
            pos += RELAYER_ADDRESS_LENGTH as usize;

            output[pos..pos + RELAYER_BLS_KEY_LENGTH as usize]
                .copy_from_slice(validator.bls_key.as_ref());
            pos += RELAYER_BLS_KEY_LENGTH as usize;
        }

        Ok(Bytes::from(output))
    }
}

type DecodeConsensusStateResult = Result<ConsensusState, PrecompileHalt>;
/// input:
/// | chainID   | height   | nextValidatorSetHash | [{validator pubkey, voting power, relayer address, relayer bls pubkey}] |
/// | 32 bytes  | 8 bytes  | 32 bytes             | [{32 bytes, 8 bytes, 20 bytes, 48 bytes}]
fn decode_consensus_state(
    input: &Bytes,
    require_unique_validators: bool,
) -> DecodeConsensusStateResult {
    let minimum_length = CHAIN_ID_LENGTH + HEIGHT_LENGTH + VALIDATOR_SET_HASH_LENGTH;
    let input_length = input.len() as u64;
    if input_length <= minimum_length ||
        !(input_length - minimum_length).is_multiple_of(SINGLE_VALIDATOR_BYTES_LENGTH)
    {
        return Err(BscPrecompileError::InvalidInput.into());
    }
    if input_length > MAX_CONSENSUS_STATE_LENGTH {
        return Err(BscPrecompileError::InvalidInput.into());
    }

    let mut pos = 0_u64;
    let chain_id = &input[..CHAIN_ID_LENGTH as usize];
    let chain_id = String::from_utf8_lossy(chain_id);
    let chain_id = chain_id.trim_end_matches('\0').to_owned();
    pos += CHAIN_ID_LENGTH;

    let height =
        u64::from_be_bytes(input[pos as usize..(pos + HEIGHT_LENGTH) as usize].try_into().unwrap());
    pos += HEIGHT_LENGTH;

    let next_validator_set_hash =
        Bytes::from(input[pos as usize..(pos + VALIDATOR_SET_HASH_LENGTH) as usize].to_vec());
    pos += VALIDATOR_SET_HASH_LENGTH;

    let validator_set_length = (input_length - minimum_length) / SINGLE_VALIDATOR_BYTES_LENGTH;
    let validator_set_bytes = input[pos as usize..].to_vec();
    let mut validator_set = Vec::with_capacity(validator_set_length as usize);
    for i in 0..validator_set_length {
        let validator = &validator_set_bytes[i as usize * SINGLE_VALIDATOR_BYTES_LENGTH as usize..
            (i + 1) as usize * SINGLE_VALIDATOR_BYTES_LENGTH as usize];

        let voting_power = u64::from_be_bytes(
            validator[VALIDATOR_PUBKEY_LENGTH as usize..
                (VALIDATOR_PUBKEY_LENGTH + VALIDATOR_VOTING_POWER_LENGTH) as usize]
                .try_into()
                .unwrap(),
        );
        let relayer_address = Bytes::from(
            validator[(VALIDATOR_PUBKEY_LENGTH + VALIDATOR_VOTING_POWER_LENGTH) as usize..
                (VALIDATOR_PUBKEY_LENGTH + VALIDATOR_VOTING_POWER_LENGTH + RELAYER_ADDRESS_LENGTH)
                    as usize]
                .to_vec(),
        );
        let relayer_bls_key = Bytes::from(
            validator[(VALIDATOR_PUBKEY_LENGTH +
                VALIDATOR_VOTING_POWER_LENGTH +
                RELAYER_ADDRESS_LENGTH) as usize..]
                .to_vec(),
        );
        let pk = match PublicKey::from_raw_ed25519(&validator[..VALIDATOR_PUBKEY_LENGTH as usize]) {
            Some(pk) => pk,
            None => return Err(BscPrecompileError::InvalidInput.into()),
        };
        let vp = Power::from(voting_power as u32);
        let validator_info = Validator::new_with_bls_and_relayer(
            pk,
            vp,
            relayer_bls_key.to_vec(),
            relayer_address.to_vec(),
        );
        validator_set.push(validator_info);
    }

    let consensus_state =
        ConsensusState::new(chain_id, height, next_validator_set_hash, validator_set);
    if require_unique_validators {
        validate_unique_validator_set(&consensus_state.validators)?;
    }

    Ok(consensus_state)
}

/// Ed25519 verifier with the semantics of Go's `crypto/ed25519.Verify`, which geth-bsc reaches
/// through greenfield-cometbft's `PubKey.VerifySignature`.
///
/// cometbft-rs' default verifier is `ed25519-consensus` (ZIP-215): it checks the cofactored
/// equation `[8][S]B = [8]R + [8][k]A`, so it accepts a non-canonical `R` encoding or an `R`
/// with a small-order component. Go checks the cofactorless equation and compares the canonical
/// encoding of the recomputed `R` with the signature bytes, rejecting both. Either difference
/// lets any caller make the precompile succeed on reth and fail on geth.
#[derive(Clone, Copy, Debug, Default)]
struct GoEd25519Verifier;

impl signature::Verifier for GoEd25519Verifier {
    fn verify(pubkey: PublicKey, msg: &[u8], sig: &Signature) -> Result<(), signature::Error> {
        #[allow(unreachable_patterns)]
        let pubkey: &[u8; 32] = match &pubkey {
            PublicKey::Ed25519(pk) => {
                pk.as_bytes().try_into().map_err(|_| signature::Error::MalformedPublicKey)?
            }
            _ => return Err(signature::Error::UnsupportedKeyType),
        };
        let sig: &[u8; 64] =
            sig.as_bytes().try_into().map_err(|_| signature::Error::MalformedSignature)?;
        if go_ed25519_verify(pubkey, msg, sig) {
            Ok(())
        } else {
            Err(signature::Error::VerificationFailed)
        }
    }
}

/// Mirrors Go's `crypto/internal/fips140/ed25519.verifyWithDom` (pure Ed25519, no context).
fn go_ed25519_verify(pubkey: &[u8; 32], msg: &[u8], sig: &[u8; 64]) -> bool {
    // Go's `edwards25519.Point.SetBytes` accepts an unreduced y and x = 0 with the sign bit
    // set, as `decompress` does.
    let Some(a) = CompressedEdwardsY(*pubkey).decompress() else { return false };
    if sig[63] & 0xe0 != 0 {
        return false;
    }
    let r_bytes = &sig[..32];
    let mut s_bytes = [0u8; 32];
    s_bytes.copy_from_slice(&sig[32..]);
    let Some(s) = Option::<Scalar>::from(Scalar::from_canonical_bytes(s_bytes)) else {
        return false;
    };
    let k = Scalar::from_bytes_mod_order_wide(
        &Sha512::new()
            .chain_update(r_bytes)
            .chain_update(pubkey)
            .chain_update(msg)
            .finalize()
            .into(),
    );
    // Cofactorless: R' = [S]B - [k]A, compared by canonical encoding.
    let r_prime = EdwardsPoint::vartime_double_scalar_mul_basepoint(&k, &-a, &s);
    r_prime.compress().as_bytes() == r_bytes
}

/// Reject validator sets that repeat any identity field, mirroring bnb-chain/bsc's Pasteur
/// `validateUniqueValidatorSet`. Validator address and consensus pubkey must always be unique;
/// the optional bridge fields (BLS key, relayer address) are only checked when set, since unset
/// fields are zero-filled by the fixed-width decoding and legitimately repeat.
fn validate_unique_validator_set(vals: &[Validator]) -> Result<(), PrecompileHalt> {
    let size = vals.len();

    let mut addresses = DuplicateTracker::new("validator address", size, false);
    let mut pubkeys = DuplicateTracker::new("validator pubkey", size, false);
    let mut bls_keys = DuplicateTracker::new("validator bls key", size, true);
    let mut relayer_addresses = DuplicateTracker::new("validator relayer address", size, true);

    for (idx, validator) in vals.iter().enumerate() {
        addresses.check(idx, validator.address.as_bytes())?;
        pubkeys.check(idx, &validator.pub_key.to_bytes())?;
        bls_keys.check(idx, validator.bls_key.as_ref())?;
        relayer_addresses.check(idx, validator.relayer_address.as_ref())?;
    }

    Ok(())
}

/// go-bsc's `ValidatorSet.VerifyCommitLight`: more than 2/3 of `validators` signed the commit,
/// where signature `i` must verify under `validators[i]`. The address in each `CommitSig` is
/// ignored: canonical vote bytes cover neither the address nor the index, so looking the
/// signer up by address would accept reordered or relabelled signatures that go-bsc rejects.
/// Go's height and block ID checks hold by construction here (`SignedHeader::new` checks the
/// height, and the block ID compared is the commit's own).
fn verify_commit_light(
    signed_header: &SignedHeader,
    validators: &[Validator],
) -> Result<(), PrecompileHalt> {
    let signatures = &signed_header.commit.signatures;
    if validators.len() != signatures.len() {
        return Err(BscPrecompileError::CometBftApplyBlockFailed.into())
    }

    let needed = total_voting_power(validators)? * 2 / 3;
    let mut tallied = 0_u64;
    for (idx, (commit_sig, validator)) in signatures.iter().zip(validators).enumerate() {
        // Absent and nil votes are neither verified nor counted.
        if !commit_sig.is_commit() {
            continue
        }
        verify_commit_sig(signed_header, idx, commit_sig, validator)?;
        tallied += validator.power();
        if tallied > needed {
            return Ok(())
        }
    }
    Err(BscPrecompileError::CometBftApplyBlockFailed.into())
}

/// go-bsc's `ValidatorSet.VerifyCommitLightTrusting` at the default 1/3 trust level: more than
/// 1/3 of the trusted `validators` signed the commit. The commit may come from a different set,
/// so signers are looked up by address; a trusted validator signing twice is rejected.
fn verify_commit_light_trusting(
    signed_header: &SignedHeader,
    validators: &[Validator],
) -> Result<(), PrecompileHalt> {
    let needed = total_voting_power(validators)? / 3;
    let mut tallied = 0_u64;
    let mut seen = vec![false; validators.len()];
    for (idx, commit_sig) in signed_header.commit.signatures.iter().enumerate() {
        let CommitSig::BlockIdFlagCommit { validator_address, .. } = commit_sig else { continue };
        // Go's `GetByAddress` returns the first match in set order.
        let Some(val_idx) = validators.iter().position(|v| v.address == *validator_address) else {
            continue
        };
        if std::mem::replace(&mut seen[val_idx], true) {
            return Err(BscPrecompileError::CometBftApplyBlockFailed.into())
        }
        verify_commit_sig(signed_header, idx, commit_sig, &validators[val_idx])?;
        tallied += validators[val_idx].power();
        if tallied > needed {
            return Ok(())
        }
    }
    Err(BscPrecompileError::CometBftApplyBlockFailed.into())
}

/// Verify the for-block signature at commit index `idx` under `validator`'s key.
fn verify_commit_sig(
    signed_header: &SignedHeader,
    idx: usize,
    commit_sig: &CommitSig,
    validator: &Validator,
) -> Result<(), PrecompileHalt> {
    let CommitSig::BlockIdFlagCommit { validator_address, timestamp, signature } = commit_sig
    else {
        return Err(BscPrecompileError::CometBftApplyBlockFailed.into());
    };
    let commit = &signed_header.commit;
    let vote = Vote {
        vote_type: vote::Type::Precommit,
        height: commit.height,
        round: commit.round,
        block_id: Some(commit.block_id),
        timestamp: Some(*timestamp),
        validator_address: *validator_address,
        validator_index: ValidatorIndex::try_from(idx)
            .map_err(|_| BscPrecompileError::CometBftApplyBlockFailed)?,
        signature: signature.clone(),
        extension: Vec::new(),
        extension_signature: None,
    };
    let signed_vote = SignedVote::from_vote(vote, signed_header.header().chain_id.clone())
        .ok_or(BscPrecompileError::CometBftApplyBlockFailed)?;
    validator
        .verify_signature::<GoEd25519Verifier>(&signed_vote.sign_bytes(), signed_vote.signature())
        .map_err(|_| BscPrecompileError::CometBftApplyBlockFailed.into())
}

fn total_voting_power(validators: &[Validator]) -> Result<u64, PrecompileHalt> {
    validators
        .iter()
        .try_fold(0_u64, |acc, v| acc.checked_add(v.power()))
        .filter(|total| *total <= Set::MAX_TOTAL_VOTING_POWER)
        .ok_or_else(|| BscPrecompileError::CometBftApplyBlockFailed.into())
}

/// go-bsc's `ValidatorSet.Hash` of the trusted set decoded from the consensus state, in the given
/// order (`Set::hash` sorts first). The decoded bridge fields are always full width, as in go-bsc.
fn trusted_validator_set_hash(validators: &[Validator]) -> [u8; 32] {
    let leaves: Vec<Vec<u8>> = validators.iter().map(Validator::hash_bytes).collect();
    simple_hash_from_byte_vectors::<Sha256>(&leaves)
}

/// output:
/// | validatorSetChanged | empty      | consensusStateBytesLength |  new consensusState |
/// | 1 byte              | 23 bytes   | 8 bytes                   |                     |
fn encode_light_block_validation_result(
    validator_set_changed: bool,
    consensus_state_bytes: Bytes,
) -> Bytes {
    let mut output =
        vec![0; (VALIDATE_RESULT_METADATA_LENGTH + consensus_state_bytes.len() as u64) as usize];
    output[0] = if validator_set_changed { 1 } else { 0 };
    output[24..32].copy_from_slice(consensus_state_bytes.len().to_be_bytes().as_ref());
    output[32..].copy_from_slice(consensus_state_bytes.as_ref());
    Bytes::from(output)
}

#[cfg(test)]
mod tests {
    use alloy_primitives::hex;
    use prost::Message as _;

    use super::*;

    /// The first valid `cometbft_light_block_validation` fixture, kept as a string so tests
    /// can rebuild an input around a modified light block.
    const VALID_LIGHT_BLOCK_INPUT: &str = "000000000000000000000000000000000000000000000000000000000000018c677265656e6669656c645f393030302d3132310000000000000000000000000000000000000000013c350cd55b99dc6c2b7da9bef5410fbfb869fede858e7b95bf7ca294e228bb40e33f6e876d63791ebd05ff617a1b4f4ad1aa2ce65e3c3a9cdfb33e0ffa7e8423000000000098968015154514f68ce65a0d9eecc578c0ab12da0a2a28a0805521b5b7ae56eb3fb24555efbfe59e1622bfe9f7be8c9022e9b3f2442739c1ce870b9adee169afe60f674edd7c86451c5363d89052fde8351895eeea166ce5373c36e31b518ed191d0c599aa0f5b0000000000989680432f6c4908a9aa5f3444421f466b11645235c99b831b2a2de9e504d7ea299e52a202ce529808618eb3bfc0addf13d8c5f2df821d81e18f9bc61583510b322d067d46323b0a572635c06a049c0a2a929e3c8184a50cf6a8b95708c25834ade456f399015a0000000000989680864cb9828254d712f8e59b164fc6a9402dc4e6c59065e38cff24f5323c8c5da888a0f97e5ee4ba1e11b0674b0a0d06204c1dfa247c370cd4be3e799fc4f6f48d977ac7ca0aeb060adb030a02080b1213677265656e6669656c645f393030302d3132311802220c08b2d7f3a10610e8d2adb3032a480a20ec6ecb5db4ffb17fabe40c60ca7b8441e9c5d77585d0831186f3c37aa16e9c15122408011220a2ab9e1eb9ea52812f413526e424b326aff2f258a56e00d690db9f805b60fe7e32200f40aeff672e8309b7b0aefbb9a1ae3d4299b5c445b7d54e8ff398488467f0053a20e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b85542203c350cd55b99dc6c2b7da9bef5410fbfb869fede858e7b95bf7ca294e228bb404a203c350cd55b99dc6c2b7da9bef5410fbfb869fede858e7b95bf7ca294e228bb405220294d8fbd0b94b767a7eba9840f299a3586da7fe6b5dead3b7eecba193c400f935a20bc50557c12d7392b0d07d75df0b61232d48f86a74fdea6d1485d9be6317d268c6220e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b8556a20e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b85572146699336aa109d1beab3946198c8e59f3b2cbd92f7a4065e3cd89e315ca39d87dee92835b98f8b8ec0861d6d9bb2c60156df5d375b3ceb1fbe71af6a244907d62548a694165caa660fec7a9b4e7b9198191361c71be0b128a0308021a480a20726abd0fdbfb6f779b0483e6e4b4b6f12241f6ea2bf374233ab1a316692b6415122408011220159f10ff15a8b58fc67a92ffd7f33c8cd407d4ce81b04ca79177dfd00ca19a67226808021214050cff76cc632760ba9db796c046004c900967361a0c08b3d7f3a10610808cadba03224080713027ffb776a702d78fd0406205c629ba473e1f8d6af646190f6eb9262cd67d69be90d10e597b91e06d7298eb6fa4b8f1eb7752ebf352a1f51560294548042268080212146699336aa109d1beab3946198c8e59f3b2cbd92f1a0c08b3d7f3a10610b087c1c00322405e2ddb70acfe4904438be3d9f4206c0ace905ac4fc306a42cfc9e86268950a0fbfd6ec5f526d3e41a3ef52bf9f9f358e3cb4c3feac76c762fa3651c1244fe004226808021214c55765fd2d0570e869f6ac22e7f2916a35ea300d1a0c08b3d7f3a10610f0b3d492032240ca17898bd22232fc9374e1188636ee321a396444a5b1a79f7628e4a11f265734b2ab50caf21e8092c55d701248e82b2f011426cb35ba22043b497a6b4661930612a0050aa8010a14050cff76cc632760ba9db796c046004c9009673612220a20e33f6e876d63791ebd05ff617a1b4f4ad1aa2ce65e3c3a9cdfb33e0ffa7e84231880ade2042080a6bbf6ffffffffff012a30a0805521b5b7ae56eb3fb24555efbfe59e1622bfe9f7be8c9022e9b3f2442739c1ce870b9adee169afe60f674edd7c86321415154514f68ce65a0d9eecc578c0ab12da0a2a283a14ee7a2a6a44d427f6949eeb8f12ea9fbb2501da880aa2010a146699336aa109d1beab3946198c8e59f3b2cbd92f12220a20451c5363d89052fde8351895eeea166ce5373c36e31b518ed191d0c599aa0f5b1880ade2042080ade2042a30831b2a2de9e504d7ea299e52a202ce529808618eb3bfc0addf13d8c5f2df821d81e18f9bc61583510b322d067d46323b3214432f6c4908a9aa5f3444421f466b11645235c99b3a14a0a7769429468054e19059af4867da0a495567e50aa2010a14c55765fd2d0570e869f6ac22e7f2916a35ea300d12220a200a572635c06a049c0a2a929e3c8184a50cf6a8b95708c25834ade456f399015a1880ade2042080ade2042a309065e38cff24f5323c8c5da888a0f97e5ee4ba1e11b0674b0a0d06204c1dfa247c370cd4be3e799fc4f6f48d977ac7ca3214864cb9828254d712f8e59b164fc6a9402dc4e6c53a143139916d97df0c589312b89950b6ab9795f34d1a12a8010a14050cff76cc632760ba9db796c046004c9009673612220a20e33f6e876d63791ebd05ff617a1b4f4ad1aa2ce65e3c3a9cdfb33e0ffa7e84231880ade2042080a6bbf6ffffffffff012a30a0805521b5b7ae56eb3fb24555efbfe59e1622bfe9f7be8c9022e9b3f2442739c1ce870b9adee169afe60f674edd7c86321415154514f68ce65a0d9eecc578c0ab12da0a2a283a14ee7a2a6a44d427f6949eeb8f12ea9fbb2501da88";

    fn valid_light_block_input() -> Vec<u8> {
        hex::decode(VALID_LIGHT_BLOCK_INPUT).expect("fixture is valid hex")
    }

    /// Non-empty bridge fields of the wrong length stay invalid, as in go-bsc's
    /// `Validator.ValidateBasic`; only the empty value means "unset".
    #[test]
    fn light_block_wrong_length_bridge_fields_are_invalid_input() {
        let (_, light_block, _) =
            decode_light_block_validation_input(&valid_light_block_input(), false).unwrap();
        assert!(convert_light_block_from_proto(&light_block).is_ok());

        for (bls_key, relayer_address) in [(Some(vec![1; 47]), None), (None, Some(vec![1; 21]))] {
            let mut proto = light_block.clone();
            let validator = &mut proto.validator_set.as_mut().unwrap().validators[0];
            if let Some(bls_key) = bls_key.clone() {
                validator.bls_key = bls_key;
            }
            if let Some(relayer_address) = relayer_address.clone() {
                validator.relayer_address = relayer_address;
            }
            assert_eq!(
                convert_light_block_from_proto(&proto).unwrap_err(),
                BscPrecompileError::InvalidInput.into(),
                "bls_key={bls_key:?} relayer_address={relayer_address:?}"
            );
        }
    }

    /// go-bsc's consensus-state encoding: validators in wire order and fixed-width fields, so an
    /// empty BLS key or relayer address is stored as zeros.
    fn go_consensus_state(
        chain_id: &str,
        height: u64,
        next_validators_hash: &[u8],
        validators: &[TmValidator],
    ) -> Vec<u8> {
        use cometbft_proto::crypto::v1::public_key::Sum;

        let mut cs = vec![0u8; CHAIN_ID_LENGTH as usize];
        cs[..chain_id.len()].copy_from_slice(chain_id.as_bytes());
        cs.extend_from_slice(&height.to_be_bytes());
        cs.extend_from_slice(next_validators_hash);
        for validator in validators {
            let Some(Sum::Ed25519(pubkey)) = validator.pub_key.clone().and_then(|key| key.sum)
            else {
                unreachable!("ed25519 keys only")
            };
            let mut relayer_address = validator.relayer_address.clone();
            relayer_address.resize(RELAYER_ADDRESS_LENGTH as usize, 0);
            let mut bls_key = validator.bls_key.clone();
            bls_key.resize(RELAYER_BLS_KEY_LENGTH as usize, 0);
            cs.extend_from_slice(&pubkey);
            cs.extend_from_slice(&(validator.voting_power as u64).to_be_bytes());
            cs.extend_from_slice(&relayer_address);
            cs.extend_from_slice(&bls_key);
        }
        cs
    }

    /// Prefixes a consensus state with the 32-byte header the precompile uses for input and
    /// output: the `validator_set_changed` byte, then the state length.
    fn with_state_prefix(validator_set_changed: bool, cs: Vec<u8>) -> Vec<u8> {
        let mut out = vec![0u8; CONSENSUS_STATE_LENGTH_BYTES_LENGTH as usize];
        out[0] = validator_set_changed as u8;
        out[24..32].copy_from_slice(&(cs.len() as u64).to_be_bytes());
        out.extend_from_slice(&cs);
        out
    }

    /// The output go-bsc returns when it accepts `light_block` over a trusted state holding the
    /// same validators. The trusted set is stored zero-filled and hashed at full width, while
    /// the header commits to the wire hash with empty fields omitted, so the set counts as
    /// changed exactly when a bridge field is empty.
    fn go_light_block_output(light_block: &TmLightBlock) -> Vec<u8> {
        let header = light_block.signed_header.as_ref().unwrap().header.as_ref().unwrap();
        let validators = &light_block.validator_set.as_ref().unwrap().validators;
        let changed =
            validators.iter().any(|v| v.bls_key.is_empty() || v.relayer_address.is_empty());
        let cs = go_consensus_state(
            &header.chain_id,
            header.height as u64,
            &header.next_validators_hash,
            validators,
        );
        with_state_prefix(changed, cs)
    }

    /// Rebuilds the fixture as an adjacent light block signed by four equal-power validators
    /// whose keys are generated at runtime. `order` receives them in cometbft-rs order (ascending
    /// address) and may reorder or edit them; each then signs a precommit, and `edit_sigs` may
    /// change the commit signatures. The header commits to the wire validator-set hash and the
    /// commit to Greenfield's header hash. Returns the input and go-bsc's output for it.
    fn runtime_signed_light_block(
        order: impl FnOnce(&mut Vec<(Scalar, TmValidator)>),
        edit_sigs: impl FnOnce(&mut Vec<cometbft_proto::types::v1::CommitSig>),
    ) -> (Vec<u8>, Vec<u8>) {
        use cometbft_proto::{
            crypto::v1::{public_key::Sum, PublicKey as RawPublicKey},
            types::v1::BlockIdFlag,
        };
        use curve25519_dalek::constants::ED25519_BASEPOINT_POINT;
        use sha2::Sha256;

        let input = valid_light_block_input();
        let mut light_block = TmLightBlock::decode(&input[fixture_split(&input)..])
            .expect("fixture light block decodes");

        let mut validator_set = light_block.validator_set.take().unwrap();
        let template = validator_set.validators[0].clone();
        let mut signers: Vec<_> = (0..4u8)
            .map(|i| {
                let secret = Scalar::from_bytes_mod_order(rand::random());
                let pubkey = (ED25519_BASEPOINT_POINT * secret).compress().to_bytes();
                let mut validator = template.clone();
                validator.address = Sha256::digest(pubkey)[..20].to_vec();
                validator.pub_key = Some(RawPublicKey { sum: Some(Sum::Ed25519(pubkey.to_vec())) });
                validator.voting_power = 10;
                validator.proposer_priority = 0;
                validator.relayer_address = vec![0x10 + i; 20];
                validator.bls_key = vec![0x20 + i; 48];
                (secret, validator)
            })
            .collect();
        signers.sort_by(|a, b| a.1.address.cmp(&b.1.address));
        order(&mut signers);
        validator_set.validators = signers.iter().map(|(_, v)| v.clone()).collect();
        validator_set.proposer = Some(signers[0].1.clone());
        validator_set.total_voting_power = 40;
        let validators_hash = validator_set_hash(&validator_set).as_bytes().to_vec();

        let mut signed_header = light_block.signed_header.take().unwrap();
        let mut header = signed_header.header.take().unwrap();
        header.validators_hash = validators_hash.clone();
        header.next_validators_hash = validators_hash.clone();
        // The prost re-encode below drops RandaoMix (field 15), so hash with an empty one.
        let header_hash = header_hash_with_randao_mix(
            &cometbft::block::Header::try_from(header.clone()).expect("header"),
            &[],
        );

        let mut commit = signed_header.commit.take().unwrap();
        commit.block_id.as_mut().unwrap().hash = header_hash.as_bytes().to_vec();
        let mut template_sig = commit.signatures[0].clone();
        template_sig.block_id_flag = BlockIdFlag::Commit as i32;
        commit.signatures = vec![template_sig.clone()];
        // A precommit's sign bytes cover the height, round, block ID, timestamp and chain ID,
        // not the signer, so every validator signs the same message.
        let sign_bytes = {
            let commit = cometbft::block::Commit::try_from(commit.clone()).expect("commit");
            let cometbft::block::CommitSig::BlockIdFlagCommit {
                validator_address,
                timestamp,
                signature,
            } = &commit.signatures[0]
            else {
                unreachable!("flag set to Commit above")
            };
            let vote = cometbft::vote::Vote {
                vote_type: cometbft::vote::Type::Precommit,
                height: commit.height,
                round: commit.round,
                block_id: Some(commit.block_id),
                timestamp: Some(*timestamp),
                validator_address: *validator_address,
                validator_index: 0u32.try_into().unwrap(),
                signature: signature.clone(),
                extension: Default::default(),
                extension_signature: None,
            };
            let chain_id = header.chain_id.clone().try_into().expect("chain id");
            cometbft::vote::SignedVote::from_vote(vote, chain_id).expect("signed").sign_bytes()
        };
        commit.signatures = signers
            .iter()
            .map(|(secret, validator)| {
                let nonce = Scalar::from_bytes_mod_order(rand::random());
                let (_, signature) =
                    sign_with_torsion(*secret, nonce, EdwardsPoint::default(), &sign_bytes);
                let mut commit_sig = template_sig.clone();
                commit_sig.validator_address = validator.address.clone();
                commit_sig.signature = signature.to_vec();
                commit_sig
            })
            .collect();
        edit_sigs(&mut commit.signatures);

        // Trusted state one block below, so the adjacent (`height + 1`) path is taken.
        let trusted = go_consensus_state(
            &header.chain_id,
            header.height as u64 - 1,
            &validators_hash,
            &validator_set.validators,
        );
        signed_header.header = Some(header);
        signed_header.commit = Some(commit);
        light_block.signed_header = Some(signed_header);
        light_block.validator_set = Some(validator_set);

        let expected = go_light_block_output(&light_block);
        let mut crafted = with_state_prefix(false, trusted);
        crafted.extend_from_slice(&light_block.encode_to_vec());
        (crafted, expected)
    }

    /// An adjacent light block signed by runtime-generated validators listed in descending
    /// address order, the reverse of cometbft-rs's order for equal powers.
    fn runtime_signed_wire_order_light_block_input() -> Vec<u8> {
        runtime_signed_light_block(|signers| signers.reverse(), |_| {}).0
    }

    /// go-bsc accepts a validator whose relayer BLS key or relayer address is empty and stores
    /// it zero-filled. Each header commits to go-bsc's validator-set hash, which omits the empty
    /// field, so a fix that hashed the zero-filled fields would not match.
    #[test]
    fn light_block_empty_bridge_fields_match_go_bsc() {
        type Edit = fn(&mut Vec<(Scalar, TmValidator)>);
        let cases: [(&str, Edit, bool); 5] = [
            ("control", |_| {}, false),
            ("zero BLS key", |signers| signers[0].1.bls_key = vec![0; 48], false),
            ("empty BLS key", |signers| signers[0].1.bls_key.clear(), true),
            ("empty relayer address", |signers| signers[0].1.relayer_address.clear(), true),
            (
                "all empty",
                |signers| {
                    for (_, validator) in signers.iter_mut() {
                        validator.bls_key.clear();
                        validator.relayer_address.clear();
                    }
                },
                true,
            ),
        ];
        for (name, edit, validator_set_changed) in cases {
            let (input, expected) = runtime_signed_light_block(edit, |_| {});
            assert_eq!(expected[0], validator_set_changed as u8, "{name}");
            for (fork, run) in [
                ("hertz", cometbft_light_block_validation_run as fn(&[u8], u64, u64) -> _),
                ("pasteur", cometbft_light_block_validation_run_pasteur),
            ] {
                let out = run(&input, 10_000_000, 0).unwrap();
                assert!(out.is_success(), "{name} ({fork}): halted with {:?}", out.halt_reason());
                assert_eq!(out.bytes.as_ref(), expected.as_slice(), "{name} ({fork})");
            }
        }
    }

    /// The header binds the validator set in wire order. cometbft-rs sorts the converted set,
    /// so its hash differs for a set go-bsc accepts; the binding must hash the wire set.
    #[test]
    fn validator_set_binding_uses_wire_order() {
        let input = runtime_signed_wire_order_light_block_input();
        let (_, proto, _) = decode_light_block_validation_input(&input, false).unwrap();
        let block = convert_light_block_from_proto(&proto).unwrap().0;
        let header_hash = block.signed_header.header().validators_hash;

        assert_eq!(validator_set_hash(proto.validator_set.as_ref().unwrap()), header_hash);
        assert_ne!(block.validators.hash(), header_hash);
        type Run = fn(&[u8], u64, u64) -> PrecompileResult;
        let runs: [Run; 3] = [
            cometbft_light_block_validation_run_before_hertz,
            cometbft_light_block_validation_run,
            cometbft_light_block_validation_run_pasteur,
        ];
        for run in runs {
            let output = run(&input, 10_000_000, 0).unwrap();
            assert!(output.is_success(), "halted with {:?}", output.halt_reason());
        }
    }

    /// Known answer from greenfield-cometbft v1.3.2 `ValidatorSet.Hash` for this set, built
    /// directly in this order (not via `NewValidatorSet`, which sorts it; sorted, the hash is
    /// 73acb32e...). The public keys are filler bytes, and the third validator's empty bridge
    /// fields are omitted from its `SimpleValidator` encoding.
    #[test]
    fn validator_set_hash_matches_go_wire_order() {
        use cometbft_proto::{
            crypto::v1::{public_key::Sum, PublicKey as RawPublicKey},
            types::v1::Validator as TmValidator,
        };

        let validator = |fill: u8, voting_power: i64, relayer_address: Vec<u8>, bls_key: Vec<u8>| {
            TmValidator {
                pub_key: Some(RawPublicKey { sum: Some(Sum::Ed25519(vec![fill; 32])) }),
                voting_power,
                relayer_address,
                bls_key,
                ..Default::default()
            }
        };
        let validator_set = TmValidatorSet {
            validators: vec![
                validator(0xa1, 10, vec![0xb1; 20], vec![0xc1; 48]),
                validator(0xa2, 30, vec![0xb2; 20], vec![0xc2; 48]),
                validator(0xa3, 20, vec![], vec![]),
            ],
            ..Default::default()
        };
        assert_eq!(
            validator_set_hash(&validator_set).as_bytes(),
            hex!("1142b2b89b3b06fa7a2665b9eac444878b49ce470c880d18aa36649bfce8bf97"),
        );
    }

    fn fixture_split(input: &[u8]) -> usize {
        let cs_length = u64::from_be_bytes(input[24..32].try_into().unwrap()) as usize;
        CONSENSUS_STATE_LENGTH_BYTES_LENGTH as usize + cs_length
    }

    fn modify_fixture_light_block(mut modify: impl FnMut(&mut TmLightBlock)) -> Vec<u8> {
        let input = valid_light_block_input();
        let split = fixture_split(&input);
        let randao_mix = decode_randao_mix(&input[split..]).unwrap();
        let mut light_block = TmLightBlock::decode(&input[split..]).unwrap();
        modify(&mut light_block);
        // Reattach Greenfield's field 15, which the pinned Rust protobuf drops on re-encoding.
        let mut wire_block = WireLightBlock::decode(light_block.encode_to_vec().as_slice()).unwrap();
        let mut signed = WireSignedHeader::decode(wire_block.signed_header.as_slice()).unwrap();
        signed.header.extend_from_slice(&HeaderExtensionFields { randao_mix }.encode_to_vec());
        wire_block.signed_header = signed.encode_to_vec();
        let mut result = input[..split].to_vec();
        result.extend_from_slice(&wire_block.encode_to_vec());
        result
    }

    fn assert_precompile_rejects(input: &[u8]) {
        for run in [
            cometbft_light_block_validation_run_before_hertz
                as fn(&[u8], u64, u64) -> PrecompileResult,
            cometbft_light_block_validation_run,
            cometbft_light_block_validation_run_pasteur,
        ] {
            let output = run(input, 100_000, 0).unwrap();
            assert!(output.is_halt(), "unbound light block must be rejected by every variant");
        }
    }

    #[test]
    fn rejects_validator_set_unbound_from_header() {
        let input = modify_fixture_light_block(|proto| {
            let validators = proto.validator_set.as_mut().unwrap();
            validators.validators[0].voting_power += 1;
        });
        let (_, proto, randao_mix) = decode_light_block_validation_input(&input, false).unwrap();
        let (block, validators) = convert_light_block_from_proto(&proto).unwrap();
        let validators_hash = validator_set_hash(proto.validator_set.as_ref().unwrap());
        assert_ne!(validators_hash, block.signed_header.header().validators_hash);
        assert_eq!(
            header_hash_with_randao_mix(block.signed_header.header(), &randao_mix),
            block.signed_header.commit.block_id.hash,
        );
        // Signature verification alone accepts this unchanged, valid signature.
        assert!(verify_commit_light(&block.signed_header, &validators).is_ok());
        assert_precompile_rejects(&input);

        // The same binding is required on the non-adjacent trusting path.
        let (mut trusted, _, _) = decode_light_block_validation_input(&input, false).unwrap();
        trusted.height = 0;
        trusted.validators = validators.clone();
        assert!(trusted
            .apply_light_block(&block, &randao_mix, validators_hash, validators)
            .is_err());
    }

    #[test]
    fn rejects_header_unbound_from_commit() {
        let input = modify_fixture_light_block(|proto| {
            let header = proto.signed_header.as_mut().unwrap().header.as_mut().unwrap();
            header.app_hash[0] ^= 1;
        });
        let (_, proto, randao_mix) = decode_light_block_validation_input(&input, false).unwrap();
        let (block, validators) = convert_light_block_from_proto(&proto).unwrap();
        assert_eq!(
            validator_set_hash(proto.validator_set.as_ref().unwrap()),
            block.signed_header.header().validators_hash,
        );
        assert_ne!(
            header_hash_with_randao_mix(block.signed_header.header(), &randao_mix),
            block.signed_header.commit.block_id.hash,
        );
        // Signatures still verify over the commit's old block ID.
        assert!(verify_commit_light(&block.signed_header, &validators).is_ok());
        assert_precompile_rejects(&input);
    }

    #[derive(Clone, PartialEq, prost::Message)]
    struct WireSignedHeader {
        #[prost(bytes = "vec", tag = "1")]
        header: Vec<u8>,
        #[prost(bytes = "vec", tag = "2")]
        commit: Vec<u8>,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    struct WireLightBlock {
        #[prost(bytes = "vec", tag = "1")]
        signed_header: Vec<u8>,
        #[prost(bytes = "vec", tag = "2")]
        validator_set: Vec<u8>,
    }

    #[test]
    fn header_hash_includes_randao_mix() {
        let input = valid_light_block_input();
        let (_, proto, original_randao) =
            decode_light_block_validation_input(&input, false).unwrap();
        let block = convert_light_block_from_proto(&proto).unwrap().0;
        assert_eq!(original_randao.len(), 64);
        assert_eq!(
            header_hash_with_randao_mix(block.signed_header.header(), &original_randao),
            block.signed_header.commit.block_id.hash,
        );
        assert_ne!(
            header_hash_with_randao_mix(block.signed_header.header(), &original_randao),
            block.signed_header.header.hash(),
        );

        let split = fixture_split(&input);
        let mut wire_block = WireLightBlock::decode(&input[split..]).unwrap();
        let mut signed = WireSignedHeader::decode(wire_block.signed_header.as_slice()).unwrap();
        signed.header = cometbft_proto::types::v1::Header::decode(signed.header.as_slice())
            .unwrap()
            .encode_to_vec();
        wire_block.signed_header = signed.clone().encode_to_vec();
        let mut without_randao = input[..split].to_vec();
        without_randao.extend_from_slice(&wire_block.encode_to_vec());
        let (_, proto, randao) =
            decode_light_block_validation_input(&without_randao, false).unwrap();
        let block = convert_light_block_from_proto(&proto).unwrap().0;
        assert!(randao.is_empty());
        // Computed independently with greenfield-cometbft v1.3.2's Header.Hash in Go.
        assert_eq!(
            header_hash_with_randao_mix(block.signed_header.header(), &randao),
            Hash::Sha256(hex!("5B297EDCE236E97E232A2328F2508350B4A7AEFB567BC13A2233978DFD26E946")),
        );
        assert_precompile_rejects(&without_randao);

        signed.header.extend_from_slice(
            &HeaderExtensionFields { randao_mix: vec![0x42; 32] }.encode_to_vec(),
        );
        wire_block.signed_header = signed.encode_to_vec();
        let mut with_randao = input[..split].to_vec();
        with_randao.extend_from_slice(&wire_block.encode_to_vec());

        assert!(matches!(
            decode_light_block_validation_input(&with_randao, false),
            Err(h) if h == BscPrecompileError::InvalidInput.into()
        ));
        // The hash-only vector remains useful, but its 32-byte RandaoMix is invalid input.
        let randao = vec![0x42; 32];
        // Computed independently with greenfield-cometbft v1.3.2's Header.Hash in Go.
        assert_eq!(
            header_hash_with_randao_mix(block.signed_header.header(), &randao),
            Hash::Sha256(hex!("D67D0F5D68BDE911412C46FD57E594807A869710927E0485278398A6F629DDB6")),
        );
        assert_precompile_rejects(&with_randao);
    }

    /// Structural rejection tests deliberately carry no commit signatures.
    fn unsigned_light_block() -> (LightBlock, Vec<u8>) {
        let (_, proto, randao_mix) =
            decode_light_block_validation_input(&valid_light_block_input(), true).unwrap();
        let mut block = convert_light_block_from_proto(&proto).unwrap().0;
        block.signed_header.commit.signatures.clear();
        (block, randao_mix)
    }

    #[test]
    fn greenfield_header_hash_matches_go_vector() {
        use sha2::{Digest, Sha256};

        // Independent golden vector from greenfield-cometbft v1.3.2,
        // types/block_test.go: TestHeaderHash, "Generates expected hash".
        let hash = |value: &str| Hash::Sha256(Sha256::digest(value.as_bytes()).into());
        let (mut block, _) = unsigned_light_block();
        let header = &mut block.signed_header.header;
        header.version = block::header::Version { block: 1, app: 2 };
        header.chain_id = "chainId".parse().unwrap();
        header.height = 3u32.into();
        header.time = "2019-10-13T16:14:44Z".parse().unwrap();
        header.last_block_id = Some(block::Id {
            hash: Hash::Sha256([0; 32]),
            part_set_header: block::parts::Header::new(6, Hash::Sha256([0; 32])).unwrap(),
        });
        header.last_commit_hash = Some(hash("last_commit_hash"));
        header.data_hash = Some(hash("data_hash"));
        header.validators_hash = hash("validators_hash");
        header.next_validators_hash = hash("next_validators_hash");
        header.consensus_hash = hash("consensus_hash");
        header.app_hash = hash("app_hash").as_bytes().to_vec().try_into().unwrap();
        header.last_results_hash = Some(hash("last_results_hash"));
        header.evidence_hash = Some(hash("evidence_hash"));
        header.proposer_address =
            hash("proposer_address").as_bytes()[..20].to_vec().try_into().unwrap();
        // This upstream hash-only vector uses 32 bytes, outside the decoder's allowed lengths.
        let randao_mix = hash("random_mix").as_bytes().to_vec();

        assert_eq!(
            header_hash_with_randao_mix(header, &randao_mix),
            "9C301304E5EC0CFDE729FE6DE7DD2D4EA7D0B1B8546DB07CF9A8B4811BE47334".parse().unwrap(),
        );
    }

    #[test]
    fn light_block_bindings_reject_header_commit_mismatch() {
        let (original, randao_mix) = unsigned_light_block();
        let mut wrong_commit = original.clone();
        wrong_commit.signed_header.commit.block_id.hash = Hash::Sha256([0; 32]);
        let mut wrong_header = original.clone();
        wrong_header.signed_header.header.app_hash = vec![0; 32].try_into().unwrap();
        let mut wrong_randao = randao_mix.clone();
        wrong_randao[0] ^= 1;

        for (block, randao_mix) in [
            (wrong_commit, randao_mix.clone()),
            (wrong_header, randao_mix),
            (original, wrong_randao),
        ] {
            assert_eq!(block.validators.hash(), block.signed_header.header.validators_hash);
            assert_ne!(
                header_hash_with_randao_mix(block.signed_header.header(), &randao_mix),
                block.signed_header.commit.block_id.hash,
            );
            assert_eq!(
                validate_light_block_bindings(&block, &randao_mix, block.validators.hash()),
                Err(BscPrecompileError::CometBftApplyBlockFailed.into()),
            );
        }
    }

    #[test]
    fn light_block_bindings_reject_uncommitted_validator_metadata() {
        let (original, randao_mix) = unsigned_light_block();
        for change_bls in [true, false] {
            let mut block = original.clone();
            let mut validators = block.validators.validators().clone();
            if change_bls {
                validators[0].bls_key = vec![0; 48].try_into().unwrap();
            } else {
                validators[0].relayer_address = vec![0; 20].try_into().unwrap();
            }
            block.validators = Set::without_proposer(validators);
            // Isolate the set-to-header check: the header still matches its commit.
            assert_eq!(
                header_hash_with_randao_mix(block.signed_header.header(), &randao_mix),
                block.signed_header.commit.block_id.hash,
            );
            assert_ne!(block.validators.hash(), block.signed_header.header.validators_hash);
            assert_eq!(
                validate_light_block_bindings(&block, &randao_mix, block.validators.hash()),
                Err(BscPrecompileError::CometBftApplyBlockFailed.into()),
            );
        }
    }

    #[test]
    fn light_block_binding_failure_preserves_consensus_state() {
        let (mut wrong_header, randao_mix) = unsigned_light_block();
        wrong_header.signed_header.commit.block_id.hash = Hash::Sha256([0; 32]);
        let (mut wrong_validators, _) = unsigned_light_block();
        wrong_validators.signed_header.header.validators_hash = Hash::Sha256([0; 32]);
        wrong_validators.signed_header.commit.block_id.hash =
            header_hash_with_randao_mix(wrong_validators.signed_header.header(), &randao_mix);

        // Both adjacent and skipping updates must leave every trusted field unchanged.
        for trusted_height in [0, 1] {
            for block in [&wrong_header, &wrong_validators] {
                let (mut state, _, _) =
                    decode_light_block_validation_input(&valid_light_block_input(), true).unwrap();
                state.height = trusted_height;
                let before = state.encode().unwrap();
                assert_eq!(
                    state.apply_light_block(
                        block,
                        &randao_mix,
                        block.validators.hash(),
                        block.validators.validators().clone(),
                    ),
                    Err(BscPrecompileError::CometBftApplyBlockFailed.into()),
                );
                assert_eq!(state.encode().unwrap(), before);
            }
        }
    }

    #[test]
    fn matching_bindings_still_require_commit_signatures() {
        let (block, randao_mix) = unsigned_light_block();
        let validators_hash = block.validators.hash();
        assert!(validate_light_block_bindings(&block, &randao_mix, validators_hash).is_ok());
        let (mut state, _, _) =
            decode_light_block_validation_input(&valid_light_block_input(), true).unwrap();
        let before = state.encode().unwrap();
        let validators = block.validators.validators().clone();
        assert!(state
            .apply_light_block(&block, &randao_mix, validators_hash, validators)
            .is_err());
        assert_eq!(state.encode().unwrap(), before);
    }

    #[test]
    fn greenfield_header_extension_preserves_protobuf_merge_semantics() {
        let (block, original_randao) = unsigned_light_block();
        let proto = TmLightBlock {
            signed_header: Some(block.signed_header.clone().into()),
            validator_set: Some(block.validators.clone().into()),
        };
        let standard_fields = proto.encode_to_vec();
        let fixture = valid_light_block_input();
        let trusted_state_prefix = &fixture[..fixture_split(&fixture)];
        for length in [0, 1, 32, 63, 64, 65] {
            let randao_mix = if length == 64 { original_randao.clone() } else { vec![0; length] };
            let extension = LightBlockExtensionFields {
                signed_header: Some(SignedHeaderExtensionFields {
                    header: Some(HeaderExtensionFields { randao_mix: randao_mix.clone() }),
                }),
            };
            let extension_fields = extension.encode_to_vec();
            // Repeated embedded messages merge in either order. Absent fields must not
            // erase standard header fields or the separately decoded RandaoMix.
            for parts in
                [[&standard_fields, &extension_fields], [&extension_fields, &standard_fields]]
            {
                let input =
                    [trusted_state_prefix, parts[0].as_slice(), parts[1].as_slice()].concat();
                let decoded = decode_light_block_validation_input(&input, true);
                if length == 0 || length == 64 {
                    let (_, decoded_proto, decoded_randao) =
                        decoded.expect("valid RandaoMix length");
                    let decoded = convert_light_block_from_proto(&decoded_proto).unwrap().0;
                    assert_eq!(decoded.signed_header, block.signed_header);
                    assert_eq!(decoded.validators, block.validators);
                    assert_eq!(decoded_randao, randao_mix);
                    if length == 64 {
                        assert_eq!(
                            header_hash_with_randao_mix(
                                decoded.signed_header.header(),
                                &decoded_randao
                            ),
                            block.signed_header.commit.block_id.hash,
                        );
                    }
                } else {
                    assert!(
                        matches!(decoded, Err(h) if h == BscPrecompileError::InvalidInput.into())
                    );
                }
            }
        }
    }

    #[test]
    fn all_cometbft_variants_accept_bound_fixture_with_unchanged_output() {
        let input = valid_light_block_input();
        let before_hertz =
            COMETBFT_LIGHT_BLOCK_VALIDATION_BEFORE_HERTZ.execute(&input, 1_000_000, 0).unwrap();
        let hertz = COMETBFT_LIGHT_BLOCK_VALIDATION.execute(&input, 1_000_000, 0).unwrap();
        let pasteur =
            COMETBFT_LIGHT_BLOCK_VALIDATION_PASTEUR.execute(&input, 1_000_000, 0).unwrap();
        for output in [&before_hertz, &hertz, &pasteur] {
            assert!(output.is_success());
            assert_eq!(output.bytes, hertz.bytes);
        }
        assert_eq!(before_hertz.gas_used, COMETBFT_LIGHT_BLOCK_VALIDATION_BASE);
        assert_eq!(hertz.gas_used, COMETBFT_LIGHT_BLOCK_VALIDATION_BASE);
        assert_eq!(
            pasteur.gas_used,
            COMETBFT_LIGHT_BLOCK_VALIDATION_BASE +
                input.len() as u64 * COMETBFT_LIGHT_BLOCK_VALIDATE_PER_BYTE_GAS,
        );
    }

    /// `signed_header` and `validator_set` are optional protobuf fields, so any caller can
    /// omit them. They used to be unwrapped, panicking the node before the precompile's own
    /// `InvalidInput` handling could run — reachable by anyone for the base gas cost.
    #[test]
    fn light_block_missing_optional_proto_fields_is_invalid_input() {
        assert!(convert_light_block_from_proto(&TmLightBlock::default()).is_err());

        let input = valid_light_block_input();
        let cs_length = u64::from_be_bytes(
            input[(CONSENSUS_STATE_LENGTH_BYTES_LENGTH - UINT64_TYPE_LENGTH) as usize..
                CONSENSUS_STATE_LENGTH_BYTES_LENGTH as usize]
                .try_into()
                .unwrap(),
        ) as usize;
        let split = CONSENSUS_STATE_LENGTH_BYTES_LENGTH as usize + cs_length;
        let (prefix, tail) = input.split_at(split);
        let full = TmLightBlock::decode(tail).expect("fixture light block decodes");

        // Neither field set, then only `validator_set` missing.
        let mut without_validator_set = full.clone();
        without_validator_set.validator_set = None;
        for proto in [TmLightBlock::default(), without_validator_set] {
            let mut crafted = prefix.to_vec();
            crafted.extend_from_slice(&proto.encode_to_vec());

            let output = cometbft_light_block_validation_run(&crafted, 10_000_000, 0)
                .expect("a missing proto field must not be a fatal error");
            assert!(output.is_halt(), "expected an InvalidInput halt");

            // Pasteur shares `convert_light_block_from_proto`.
            let output = cometbft_light_block_validation_run_pasteur(&crafted, 10_000_000, 0)
                .expect("a missing proto field must not be a fatal error");
            assert!(output.is_halt(), "expected an InvalidInput halt");
        }
    }

    #[test]
    fn test_cometbft_light_block_validate() {
        {
            let input = Bytes::from(valid_light_block_input());
            let except_output = Bytes::from(hex!(
                "000000000000000000000000000000000000000000000000000000000000018c677265656e6669656c645f393030302d3132310000000000000000000000000000000000000000023c350cd55b99dc6c2b7da9bef5410fbfb869fede858e7b95bf7ca294e228bb40e33f6e876d63791ebd05ff617a1b4f4ad1aa2ce65e3c3a9cdfb33e0ffa7e8423000000000098968015154514f68ce65a0d9eecc578c0ab12da0a2a28a0805521b5b7ae56eb3fb24555efbfe59e1622bfe9f7be8c9022e9b3f2442739c1ce870b9adee169afe60f674edd7c86451c5363d89052fde8351895eeea166ce5373c36e31b518ed191d0c599aa0f5b0000000000989680432f6c4908a9aa5f3444421f466b11645235c99b831b2a2de9e504d7ea299e52a202ce529808618eb3bfc0addf13d8c5f2df821d81e18f9bc61583510b322d067d46323b0a572635c06a049c0a2a929e3c8184a50cf6a8b95708c25834ade456f399015a0000000000989680864cb9828254d712f8e59b164fc6a9402dc4e6c59065e38cff24f5323c8c5da888a0f97e5ee4ba1e11b0674b0a0d06204c1dfa247c370cd4be3e799fc4f6f48d977ac7ca"
            ));

            let result = cometbft_light_block_validation_run(&input, 100_000, 0);
            let output = result.expect("cometbft_light_block_validation_run failed");
            assert_eq!(output.gas_used, 3_000);
            assert_eq!(output.bytes, except_output);
            assert!(output.is_success());
        }
        // apply light block failed
        {
            let input = Bytes::from(hex!(
                "0000000000000000000000000000000000000000000000000000000000000264677265656e6669656c645f393030302d313734310000000000000000000000000000000000000001af6b801dda578dddfa4da1d5d67fd1b32510db24ec271346fc573e9242b01c9a112b51dda2d336246bdc0cc51407ba0cb0e5087be0db5f1cdc3285bbaa8e647500000000000003e84202722cf6a34d727be762b46825b0d26b6263a0a9355ebf3c24bedac5a357a56feeb2cd8b6fed9f14cca15c3091f523b9fb21183b4bb31eb482a0321885e3f57072156448e2b2f7d9a3e7b668757d9cc0bbd28cd674c34ed1c2ed75c5de3b6a8f8cad4600000000000003e8668a0acd8f6db5cae959a0e02132f4d6a672c4d7a4726b542012cc8023ee07b29ab3971cc999d8751bbd16f23413968afcdb070ed66ab47e6e1842bf875bef21dfc5b8af6813bfd82860d361e339bd1ae2f801b6d6ee46b8497a3d51c80b50b6160ea1cc00000000000003e80dfa99423d3084c596c5e3bd6bcb4f654516517b8d4786703c56b300b70f085c0d0482e5d6a3c7208883f0ec8abd2de893f71d18e8f919e7ab198499201d87f92c57ebce83ed2b763bb872e9bc148fb216fd5c93b18819670d9a946ae4b3075672d726b800000000000003e824aab6f85470ff73e3048c64083a09e980d4cb7f8146d231a7b2051c5f7a9c07ab6e6bfe277bd5f4a94f901fe6ee7a6b6bd8479e9e5e448de4b1b33d5ddd74194c86b3852cc140a3f08a9c4149efd45643202f8bef2ad7eecf53e58951c6df6fd932004b00000000000003e84998f6ef8d999a0f36a851bfa29dbcf0364dd65695c286deb3f1657664859d59876bf1ec5a288f6e66e18b37b8a2a1e6ee4a3ef8fa50784d8b758d0c3e70a7cdfe65ab5d0ad7080adf030a02080b1214677265656e6669656c645f393030302d3137343118f7fdbd01220c08dca092aa0610e79ba49c012a480a209cda416227712ec137b852d1de0fbb957045e6bf7e541bb595d7f5391b481360122408011220fbb48c3c7cbbe68becfd80e7dcdcee8b8737afdbdc2e484cdc8fd52a659215e932207ddc7495ef0d0c1229ae33348d7907d90459ecccf6dcb3415724b41c0b4d1b7c3a20e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855422003ebb95f12cb304c52484a0821ba13f4e73468f3139f45e168f8c11d9ba74ce04a2003ebb95f12cb304c52484a0821ba13f4e73468f3139f45e168f8c11d9ba74ce05220eceb4055624b9f678278a2706ca2f43e11f31a641580d5aacf2f4383d8edef7e5a206796260ff3744ac8f8a5007a23408c72abe018d65df767b71290aac5da68d5ba6220694a73ea799423215418f2390a3f56f23b31391fbbab0de7a8f5b3c4822257906a20e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b85572144b7d2bc2f5c3381c9bbf55edface8747722bc9d07a4050a45d04cd3d64ee8bc00bd5842e8ee2458001e0dcb6fc78bb1f44271ebca04ea65c689c6776582962c27492a5850e80a919a98d889d21ecfa83c54f15c4de0812f20408f7fdbd011a480a202585ab3a1bc673b3c68cc77e08b8190667641f30edffa89ae5ddfc2e1f53d60b122408011220c55ae2ca75b9705b4a486067b088c275919a6c3fc2de51cf09973232457a16042268080212144b7d2bc2f5c3381c9bbf55edface8747722bc9d01a0c08dea092aa0610eca4d6a6012240243914b3cf5be06bc69a6c34f2fc41928c1043b5a7d1bff80fd567b27398c3ce890be932958ec0d2a2083c393afbd4c75776ce84c881ec58a73b1022fcb63a0a226808031214793cee4b478e537592c40ecfb2148ebe32b8f6051a0c08dea092aa0610dcdbcfd701224000c586fe47dc655b38e7f62c913f4dd217ce16fa143e9295e146918ad361c2ed777512de15ed65eefb479864a89eebe55e38ebb8644a3887f46970bda9472203226808031214d742fa5318dc3986e075e2b050529a22c6fa3b8b1a0c08dea092aa0610ec9dd2d701224007e0f3e5a3d209a75c5db60f1502880cf25c5fddad0717e4ecaa8bbd95ebf28e78ea3229e88d741a44d534f08ac283dc587a28ced5c25b9cc64657a5bc5ce50c226808031214f0f07dc2f5e159a35b9662553c6b4e51868502f71a0c08dea092aa0610b7c186d7012240546fbdcc448af86aa932a05d8db2e4bc99816b7035b00c494ffe514d562190a68c214e5468c16d7d3a8c2fa52a6165ce870f8fc2dd93fa7cee40f3fa74e118082268080312145fa8b3f3fcd4a3ea2495e11dd5dbd399b3d8d4f81a0c08dea092aa0610f2d4efd50122400fe7c2eb23f4e2b6e57722c79ead12b2180e87bc618fc26cceccb891107be796fe8242c700fb61eeac6936659503354dc1a66874725b80f316252820e308d90f220f08011a0b088092b8c398feffffff0112df070a91010a144b7d2bc2f5c3381c9bbf55edface8747722bc9d012220a20112b51dda2d336246bdc0cc51407ba0cb0e5087be0db5f1cdc3285bbaa8e647518d0ca1e209ed8ffffffffffffff012a30a9355ebf3c24bedac5a357a56feeb2cd8b6fed9f14cca15c3091f523b9fb21183b4bb31eb482a0321885e3f57072156432144202722cf6a34d727be762b46825b0d26b6263a00a88010a14793cee4b478e537592c40ecfb2148ebe32b8f60512220a206813bfd82860d361e339bd1ae2f801b6d6ee46b8497a3d51c80b50b6160ea1cc1889082089082a308d4786703c56b300b70f085c0d0482e5d6a3c7208883f0ec8abd2de893f71d18e8f919e7ab198499201d87f92c57ebce32140dfa99423d3084c596c5e3bd6bcb4f654516517b0a88010a14d742fa5318dc3986e075e2b050529a22c6fa3b8b12220a2083ed2b763bb872e9bc148fb216fd5c93b18819670d9a946ae4b3075672d726b818fe0720fe072a308146d231a7b2051c5f7a9c07ab6e6bfe277bd5f4a94f901fe6ee7a6b6bd8479e9e5e448de4b1b33d5ddd74194c86b385321424aab6f85470ff73e3048c64083a09e980d4cb7f0a88010a14f0f07dc2f5e159a35b9662553c6b4e51868502f712220a202cc140a3f08a9c4149efd45643202f8bef2ad7eecf53e58951c6df6fd932004b18fc0720fc072a3095c286deb3f1657664859d59876bf1ec5a288f6e66e18b37b8a2a1e6ee4a3ef8fa50784d8b758d0c3e70a7cdfe65ab5d32144998f6ef8d999a0f36a851bfa29dbcf0364dd6560a88010a145fa8b3f3fcd4a3ea2495e11dd5dbd399b3d8d4f812220a2048e2b2f7d9a3e7b668757d9cc0bbd28cd674c34ed1c2ed75c5de3b6a8f8cad4618f60720f6072a30a4726b542012cc8023ee07b29ab3971cc999d8751bbd16f23413968afcdb070ed66ab47e6e1842bf875bef21dfc5b8af3214668a0acd8f6db5cae959a0e02132f4d6a672c4d70a88010a1455b2b6281b02e991dd9cc790bc1e6cff9db1e2c612220a2057fef603948fe010f4410a27272fcf867287b3d8421eff0afd67a157b6facf3918e90720e9072a30acf60b4cfffda7d6b120cd513bfe39e0392b1a1c433f2bb6ec1fc9200ea8f4d0c44815d4d3872e2c685371f877454284321407e201acb9f7d331a37d52ab7ad246f5c8cd1ac11291010a144b7d2bc2f5c3381c9bbf55edface8747722bc9d012220a20112b51dda2d336246bdc0cc51407ba0cb0e5087be0db5f1cdc3285bbaa8e647518d0ca1e209ed8ffffffffffffff012a30a9355ebf3c24bedac5a357a56feeb2cd8b6fed9f14cca15c3091f523b9fb21183b4bb31eb482a0321885e3f57072156432144202722cf6a34d727be762b46825b0d26b6263a0"
            ));

            let result = cometbft_light_block_validation_run(&input, 100_000, 0);
            let output = result.expect("should not return fatal error");
            assert!(output.is_halt());
        }
        // consensus height >= light block height
        {
            let input = Bytes::from(hex!(
                "0000000000000000000000000000000000000000000000000000000000000264677265656e6669656c645f393030302d3137343100000000000000000000000000000000128d987caf6b801dda578dddfa4da1d5d67fd1b32510db24ec271346fc573e9242b01c9a112b51dda2d336246bdc0cc51407ba0cb0e5087be0db5f1cdc3285bbaa8e647500000000000003e84202722cf6a34d727be762b46825b0d26b6263a0a9355ebf3c24bedac5a357a56feeb2cd8b6fed9f14cca15c3091f523b9fb21183b4bb31eb482a0321885e3f57072156448e2b2f7d9a3e7b668757d9cc0bbd28cd674c34ed1c2ed75c5de3b6a8f8cad4600000000000003e8668a0acd8f6db5cae959a0e02132f4d6a672c4d7a4726b542012cc8023ee07b29ab3971cc999d8751bbd16f23413968afcdb070ed66ab47e6e1842bf875bef21dfc5b8af6813bfd82860d361e339bd1ae2f801b6d6ee46b8497a3d51c80b50b6160ea1cc00000000000003e80dfa99423d3084c596c5e3bd6bcb4f654516517b8d4786703c56b300b70f085c0d0482e5d6a3c7208883f0ec8abd2de893f71d18e8f919e7ab198499201d87f92c57ebce83ed2b763bb872e9bc148fb216fd5c93b18819670d9a946ae4b3075672d726b800000000000003e824aab6f85470ff73e3048c64083a09e980d4cb7f8146d231a7b2051c5f7a9c07ab6e6bfe277bd5f4a94f901fe6ee7a6b6bd8479e9e5e448de4b1b33d5ddd74194c86b3852cc140a3f08a9c4149efd45643202f8bef2ad7eecf53e58951c6df6fd932004b00000000000003e84998f6ef8d999a0f36a851bfa29dbcf0364dd65695c286deb3f1657664859d59876bf1ec5a288f6e66e18b37b8a2a1e6ee4a3ef8fa50784d8b758d0c3e70a7cdfe65ab5d0ad7080adf030a02080b1214677265656e6669656c645f393030302d3137343118f7fdbd01220c08dca092aa0610e79ba49c012a480a209cda416227712ec137b852d1de0fbb957045e6bf7e541bb595d7f5391b481360122408011220fbb48c3c7cbbe68becfd80e7dcdcee8b8737afdbdc2e484cdc8fd52a659215e932207ddc7495ef0d0c1229ae33348d7907d90459ecccf6dcb3415724b41c0b4d1b7c3a20e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855422003ebb95f12cb304c52484a0821ba13f4e73468f3139f45e168f8c11d9ba74ce04a2003ebb95f12cb304c52484a0821ba13f4e73468f3139f45e168f8c11d9ba74ce05220eceb4055624b9f678278a2706ca2f43e11f31a641580d5aacf2f4383d8edef7e5a206796260ff3744ac8f8a5007a23408c72abe018d65df767b71290aac5da68d5ba6220694a73ea799423215418f2390a3f56f23b31391fbbab0de7a8f5b3c4822257906a20e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b85572144b7d2bc2f5c3381c9bbf55edface8747722bc9d07a4050a45d04cd3d64ee8bc00bd5842e8ee2458001e0dcb6fc78bb1f44271ebca04ea65c689c6776582962c27492a5850e80a919a98d889d21ecfa83c54f15c4de0812f20408f7fdbd011a480a202585ab3a1bc673b3c68cc77e08b8190667641f30edffa89ae5ddfc2e1f53d60b122408011220c55ae2ca75b9705b4a486067b088c275919a6c3fc2de51cf09973232457a16042268080212144b7d2bc2f5c3381c9bbf55edface8747722bc9d01a0c08dea092aa0610eca4d6a6012240243914b3cf5be06bc69a6c34f2fc41928c1043b5a7d1bff80fd567b27398c3ce890be932958ec0d2a2083c393afbd4c75776ce84c881ec58a73b1022fcb63a0a226808031214793cee4b478e537592c40ecfb2148ebe32b8f6051a0c08dea092aa0610dcdbcfd701224000c586fe47dc655b38e7f62c913f4dd217ce16fa143e9295e146918ad361c2ed777512de15ed65eefb479864a89eebe55e38ebb8644a3887f46970bda9472203226808031214d742fa5318dc3986e075e2b050529a22c6fa3b8b1a0c08dea092aa0610ec9dd2d701224007e0f3e5a3d209a75c5db60f1502880cf25c5fddad0717e4ecaa8bbd95ebf28e78ea3229e88d741a44d534f08ac283dc587a28ced5c25b9cc64657a5bc5ce50c226808031214f0f07dc2f5e159a35b9662553c6b4e51868502f71a0c08dea092aa0610b7c186d7012240546fbdcc448af86aa932a05d8db2e4bc99816b7035b00c494ffe514d562190a68c214e5468c16d7d3a8c2fa52a6165ce870f8fc2dd93fa7cee40f3fa74e118082268080312145fa8b3f3fcd4a3ea2495e11dd5dbd399b3d8d4f81a0c08dea092aa0610f2d4efd50122400fe7c2eb23f4e2b6e57722c79ead12b2180e87bc618fc26cceccb891107be796fe8242c700fb61eeac6936659503354dc1a66874725b80f316252820e308d90f220f08011a0b088092b8c398feffffff0112df070a91010a144b7d2bc2f5c3381c9bbf55edface8747722bc9d012220a20112b51dda2d336246bdc0cc51407ba0cb0e5087be0db5f1cdc3285bbaa8e647518d0ca1e209ed8ffffffffffffff012a30a9355ebf3c24bedac5a357a56feeb2cd8b6fed9f14cca15c3091f523b9fb21183b4bb31eb482a0321885e3f57072156432144202722cf6a34d727be762b46825b0d26b6263a00a88010a14793cee4b478e537592c40ecfb2148ebe32b8f60512220a206813bfd82860d361e339bd1ae2f801b6d6ee46b8497a3d51c80b50b6160ea1cc1889082089082a308d4786703c56b300b70f085c0d0482e5d6a3c7208883f0ec8abd2de893f71d18e8f919e7ab198499201d87f92c57ebce32140dfa99423d3084c596c5e3bd6bcb4f654516517b0a88010a14d742fa5318dc3986e075e2b050529a22c6fa3b8b12220a2083ed2b763bb872e9bc148fb216fd5c93b18819670d9a946ae4b3075672d726b818fe0720fe072a308146d231a7b2051c5f7a9c07ab6e6bfe277bd5f4a94f901fe6ee7a6b6bd8479e9e5e448de4b1b33d5ddd74194c86b385321424aab6f85470ff73e3048c64083a09e980d4cb7f0a88010a14f0f07dc2f5e159a35b9662553c6b4e51868502f712220a202cc140a3f08a9c4149efd45643202f8bef2ad7eecf53e58951c6df6fd932004b18fc0720fc072a3095c286deb3f1657664859d59876bf1ec5a288f6e66e18b37b8a2a1e6ee4a3ef8fa50784d8b758d0c3e70a7cdfe65ab5d32144998f6ef8d999a0f36a851bfa29dbcf0364dd6560a88010a145fa8b3f3fcd4a3ea2495e11dd5dbd399b3d8d4f812220a2048e2b2f7d9a3e7b668757d9cc0bbd28cd674c34ed1c2ed75c5de3b6a8f8cad4618f60720f6072a30a4726b542012cc8023ee07b29ab3971cc999d8751bbd16f23413968afcdb070ed66ab47e6e1842bf875bef21dfc5b8af3214668a0acd8f6db5cae959a0e02132f4d6a672c4d70a88010a1455b2b6281b02e991dd9cc790bc1e6cff9db1e2c612220a2057fef603948fe010f4410a27272fcf867287b3d8421eff0afd67a157b6facf3918e90720e9072a30acf60b4cfffda7d6b120cd513bfe39e0392b1a1c433f2bb6ec1fc9200ea8f4d0c44815d4d3872e2c685371f877454284321407e201acb9f7d331a37d52ab7ad246f5c8cd1ac11291010a144b7d2bc2f5c3381c9bbf55edface8747722bc9d012220a20112b51dda2d336246bdc0cc51407ba0cb0e5087be0db5f1cdc3285bbaa8e647518d0ca1e209ed8ffffffffffffff012a30a9355ebf3c24bedac5a357a56feeb2cd8b6fed9f14cca15c3091f523b9fb21183b4bb31eb482a0321885e3f57072156432144202722cf6a34d727be762b46825b0d26b6263a0"
            ));

            let result = cometbft_light_block_validation_run(&input, 100_000, 0);
            let output = result.expect("should not return fatal error");
            assert!(output.is_halt());
        }
        // chain id mismatch
        {
            let input = Bytes::from(hex!(
                "00000000000000000000000000000000000000000000000000000000000004ec677265656e6669656c645f353630302d31000000000000000000000000000000000000000000000001c6ca63d82177ae8b4f8533bec2d4dc45a34ba3a49b720545ca48e8c6ea829a33e043df137dfc0ea134be6fae019c746c7e92bbac1641e0752557f1d30306881300000000000003e82fcbd94e02d0de3e58a4cd20dd545340b9ce990a8a9910093c8c5da624dc842eb31117b548609680bae2d1940743256c121e397ff8ead4f5582910bf329c9406ac2d83186a6ae749f288a4ce711186d2606bf4e18ae3c06fbd560d0bd2fd8ab532486e3300000000000003e8b6202c3a1620e48b9458ed75b533f37ace46398fac693b30853e3a5fa3ea18d1df5c41e94f0060cb4ecf5f3483528c525ed317d5f3441adb984431556f5d2202bf77586940affc7c57c8bf533c80569e4874de92cd406468c62b14812bbe99f3e0a14e6d00000000000003e81eb931cc754eefb3ebda912fb3bb67b32b64c1a89469c75de5a4dde7ecd8de85f1ba5acee07454cb900585354a2fda279dd115c8946dffddee2e48234e10dd785fdd7f182c6de7b46dbe53aecafcbda7c7439b6a851d9933ee8963a08335bbacd8ef710500000000000003e8bacea9f5f6f5521afc9a4d9245432f4074ce67c38de9dac5bccbfef36c3d78d743fa36c06d2ade58cfecaa8903770d7793a4bf8e9e705b4b6daf7a10b29d1190b0eef5c5ceeecd09ea4f481b37562e25264de176566582032d4c7912945fd43ae5c7f82e00000000000003e85379299a1a093c824c70629894620050ac18798e90242b34c53205728de8e81ef36b17d0e49b30f039d9c224f952410ad9a9fe1d07a001e60e92070f4082875070a4a0b688c92507b2ce3f073fe1169345544501ee11b5e495b4b2c230db9cfb25cad2de00000000000003e896931f9918e930d3e48c77aaa479349805dbcdcd85a874ce490ea46e3ed8883544496c8852de074094185ad5d881df95df71287c7a837da79b9e4e2aa5cee0d23ccff22fe17d86788d73a0d4af8a9b63d9b2b7c3bc8c3ba81871e4ba3f77be43aa8510b600000000000003e8cbe5cd96be693071413f313cc76bf085eed1806ab3f99d1a867d6710e5a9227009d45568b4a34e0355594c85d1f7330c10b706301127c39e130f02e53789e511147f550786125621c641b3344c7aa286673073cc72abac17790545ea2770f5e8be04b15500000000000003e830db54e9bd6c32580b3f01256e9ecbf97a2ea57191ef0170f22bbc666291be47e2b4ce933ceb3a4a547794ebe12097152c750068a32afeff9cfb32d89cc597e27dd6e0f20bcc4c8635134d01c17590af47838723d8246bf005ff6e74155b0a8671c1bdee00000000000003e8ebfc660e51646a77b37731c31dd46fa5ca92671eb0224a76a44eb32d778421d39726aebd29ceac9ab368aafdb51e7c19bdbf46b7e17a3f372c9b0ee842f5b37e683c47d697aa82c1183da679f69c741da4509f94490a364132958a1f0356967e6d73183300000000000003e80c2201ebc288e539dcbc8221652c719f3ee2edb38d3293a19d0e86b79ad6b5eb5bf50ca2361e5e68338db543de2551b4e1d3f48ba0e75586a5ab394b8d739ebdfd548f0a11fb80525fbe947284e9415615b58725e55876a00f0ee1d20b8b0e70dcde5bce00000000000003e8aeee47645498286d615b8850764f55985b8a643ea8c70c061865d6307e34bfd26e9b61bdcc0ae35f6cacc04eaa766f4b76ec354b99cd62fdb599e62a6cfe1e652ddf83500ad7080adf030a02080b1214677265656e6669656c645f393030302d3137343118f7fdbd01220c08dca092aa0610e79ba49c012a480a209cda416227712ec137b852d1de0fbb957045e6bf7e541bb595d7f5391b481360122408011220fbb48c3c7cbbe68becfd80e7dcdcee8b8737afdbdc2e484cdc8fd52a659215e932207ddc7495ef0d0c1229ae33348d7907d90459ecccf6dcb3415724b41c0b4d1b7c3a20e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855422003ebb95f12cb304c52484a0821ba13f4e73468f3139f45e168f8c11d9ba74ce04a2003ebb95f12cb304c52484a0821ba13f4e73468f3139f45e168f8c11d9ba74ce05220eceb4055624b9f678278a2706ca2f43e11f31a641580d5aacf2f4383d8edef7e5a206796260ff3744ac8f8a5007a23408c72abe018d65df767b71290aac5da68d5ba6220694a73ea799423215418f2390a3f56f23b31391fbbab0de7a8f5b3c4822257906a20e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b85572144b7d2bc2f5c3381c9bbf55edface8747722bc9d07a4050a45d04cd3d64ee8bc00bd5842e8ee2458001e0dcb6fc78bb1f44271ebca04ea65c689c6776582962c27492a5850e80a919a98d889d21ecfa83c54f15c4de0812f20408f7fdbd011a480a202585ab3a1bc673b3c68cc77e08b8190667641f30edffa89ae5ddfc2e1f53d60b122408011220c55ae2ca75b9705b4a486067b088c275919a6c3fc2de51cf09973232457a16042268080212144b7d2bc2f5c3381c9bbf55edface8747722bc9d01a0c08dea092aa0610eca4d6a6012240243914b3cf5be06bc69a6c34f2fc41928c1043b5a7d1bff80fd567b27398c3ce890be932958ec0d2a2083c393afbd4c75776ce84c881ec58a73b1022fcb63a0a226808031214793cee4b478e537592c40ecfb2148ebe32b8f6051a0c08dea092aa0610dcdbcfd701224000c586fe47dc655b38e7f62c913f4dd217ce16fa143e9295e146918ad361c2ed777512de15ed65eefb479864a89eebe55e38ebb8644a3887f46970bda9472203226808031214d742fa5318dc3986e075e2b050529a22c6fa3b8b1a0c08dea092aa0610ec9dd2d701224007e0f3e5a3d209a75c5db60f1502880cf25c5fddad0717e4ecaa8bbd95ebf28e78ea3229e88d741a44d534f08ac283dc587a28ced5c25b9cc64657a5bc5ce50c226808031214f0f07dc2f5e159a35b9662553c6b4e51868502f71a0c08dea092aa0610b7c186d7012240546fbdcc448af86aa932a05d8db2e4bc99816b7035b00c494ffe514d562190a68c214e5468c16d7d3a8c2fa52a6165ce870f8fc2dd93fa7cee40f3fa74e118082268080312145fa8b3f3fcd4a3ea2495e11dd5dbd399b3d8d4f81a0c08dea092aa0610f2d4efd50122400fe7c2eb23f4e2b6e57722c79ead12b2180e87bc618fc26cceccb891107be796fe8242c700fb61eeac6936659503354dc1a66874725b80f316252820e308d90f220f08011a0b088092b8c398feffffff0112df070a91010a144b7d2bc2f5c3381c9bbf55edface8747722bc9d012220a20112b51dda2d336246bdc0cc51407ba0cb0e5087be0db5f1cdc3285bbaa8e647518d0ca1e209ed8ffffffffffffff012a30a9355ebf3c24bedac5a357a56feeb2cd8b6fed9f14cca15c3091f523b9fb21183b4bb31eb482a0321885e3f57072156432144202722cf6a34d727be762b46825b0d26b6263a00a88010a14793cee4b478e537592c40ecfb2148ebe32b8f60512220a206813bfd82860d361e339bd1ae2f801b6d6ee46b8497a3d51c80b50b6160ea1cc1889082089082a308d4786703c56b300b70f085c0d0482e5d6a3c7208883f0ec8abd2de893f71d18e8f919e7ab198499201d87f92c57ebce32140dfa99423d3084c596c5e3bd6bcb4f654516517b0a88010a14d742fa5318dc3986e075e2b050529a22c6fa3b8b12220a2083ed2b763bb872e9bc148fb216fd5c93b18819670d9a946ae4b3075672d726b818fe0720fe072a308146d231a7b2051c5f7a9c07ab6e6bfe277bd5f4a94f901fe6ee7a6b6bd8479e9e5e448de4b1b33d5ddd74194c86b385321424aab6f85470ff73e3048c64083a09e980d4cb7f0a88010a14f0f07dc2f5e159a35b9662553c6b4e51868502f712220a202cc140a3f08a9c4149efd45643202f8bef2ad7eecf53e58951c6df6fd932004b18fc0720fc072a3095c286deb3f1657664859d59876bf1ec5a288f6e66e18b37b8a2a1e6ee4a3ef8fa50784d8b758d0c3e70a7cdfe65ab5d32144998f6ef8d999a0f36a851bfa29dbcf0364dd6560a88010a145fa8b3f3fcd4a3ea2495e11dd5dbd399b3d8d4f812220a2048e2b2f7d9a3e7b668757d9cc0bbd28cd674c34ed1c2ed75c5de3b6a8f8cad4618f60720f6072a30a4726b542012cc8023ee07b29ab3971cc999d8751bbd16f23413968afcdb070ed66ab47e6e1842bf875bef21dfc5b8af3214668a0acd8f6db5cae959a0e02132f4d6a672c4d70a88010a1455b2b6281b02e991dd9cc790bc1e6cff9db1e2c612220a2057fef603948fe010f4410a27272fcf867287b3d8421eff0afd67a157b6facf3918e90720e9072a30acf60b4cfffda7d6b120cd513bfe39e0392b1a1c433f2bb6ec1fc9200ea8f4d0c44815d4d3872e2c685371f877454284321407e201acb9f7d331a37d52ab7ad246f5c8cd1ac11291010a144b7d2bc2f5c3381c9bbf55edface8747722bc9d012220a20112b51dda2d336246bdc0cc51407ba0cb0e5087be0db5f1cdc3285bbaa8e647518d0ca1e209ed8ffffffffffffff012a30a9355ebf3c24bedac5a357a56feeb2cd8b6fed9f14cca15c3091f523b9fb21183b4bb31eb482a0321885e3f57072156432144202722cf6a34d727be762b46825b0d26b6263a0"
            ));

            let result = cometbft_light_block_validation_run(&input, 100_000, 0);
            let output = result.expect("should not return fatal error");
            assert!(output.is_halt());
        }
    }

    #[test]
    fn test_encode_consensus_state() {
        {
            let chain_id = "chain_9000-121".to_string();
            let height = 1;
            let next_validator_set_hash = Bytes::from(hex!(
                "0CE856B1DC9CDCF3BF2478291CF02C62AEEB3679889E9866931BF1FB05A10EDA"
            ));
            let mut validators_info = Vec::new();
            let bls_pub_key = Bytes::from(hex!("a60afe627fd78b19e07e07e19d446009dd53a18c6c8744176a5d851a762bbb51198e7e006f2a6ea7225661a61ecd832d"));
            let relayer_address = Bytes::from(hex!("B32d0723583040F3A16D1380D1e6AA874cD1bdF7"));
            validators_info.push(cometbft::validator::Info::new_with_bls_and_relayer(
                PublicKey::from_raw_ed25519(&hex!(
                    "c3d9a1082f42ca161402f8668f8e39ec9e30092affd8d3262267ac7e248a959e"
                ))
                .unwrap(),
                cometbft::vote::Power::from(10000_u32),
                bls_pub_key.to_vec(),
                relayer_address.to_vec(),
            ));
            let validator_set = validators_info;

            let cs = ConsensusState::new(chain_id, height, next_validator_set_hash, validator_set);

            let expected_output = Bytes::from(hex!("636861696e5f393030302d31323100000000000000000000000000000000000000000000000000010ce856b1dc9cdcf3bf2478291cf02c62aeeb3679889e9866931bf1fb05a10edac3d9a1082f42ca161402f8668f8e39ec9e30092affd8d3262267ac7e248a959e0000000000002710b32d0723583040f3a16d1380d1e6aa874cd1bdf7a60afe627fd78b19e07e07e19d446009dd53a18c6c8744176a5d851a762bbb51198e7e006f2a6ea7225661a61ecd832d"));
            let cs_bytes = cs.encode().unwrap();
            assert_eq!(cs_bytes, expected_output);
        }
        {
            let chain_id = "chain_9000-121".to_string();
            let height = 1;
            let next_validator_set_hash = Bytes::from(hex!(
                "A5F1AF4874227F1CDBE5240259A365AD86484A4255BFD65E2A0222D733FCDBC3"
            ));
            let mut validators_info = Vec::new();
            validators_info.push(cometbft::validator::Info::new_with_bls_and_relayer(
                PublicKey::from_raw_ed25519(&hex!(
                    "20cc466ee9412ddd49e0fff04cdb41bade2b7622f08b6bdacac94d4de03bdb97"
                ))
                .unwrap(),
                cometbft::vote::Power::from(10000_u32),
                Bytes::from(hex!("aa2d28cbcd1ea3a63479f6fb260a3d755853e6a78cfa6252584fee97b2ec84a9d572ee4a5d3bc1558bb98a4b370fb861")).to_vec(),
                Bytes::from(hex!("d5e63aeee6e6fa122a6a23a6e0fca87701ba1541")).to_vec(),
            ));
            validators_info.push(cometbft::validator::Info::new_with_bls_and_relayer(
                PublicKey::from_raw_ed25519(&hex!(
                    "6b0b523ee91ad18a63d63f21e0c40a83ef15963f4260574ca5159fd90a1c5270"
                ))
                .unwrap(),
                cometbft::vote::Power::from(10000_u32),
                Bytes::from(hex!("b31e74a881fc78681e3dfa440978d2b8be0708a1cbbca2c660866216975fdaf0e9038d9b7ccbf9731f43956dba7f2451")).to_vec(),
                Bytes::from(hex!("6fd1ceb5a48579f322605220d4325bd9ff90d5fa")).to_vec(),
            ));
            validators_info.push(cometbft::validator::Info::new_with_bls_and_relayer(
                PublicKey::from_raw_ed25519(&hex!(
                    "919606ae20bf5d248ee353821754bcdb456fd3950618fda3e32d3d0fb990eeda"
                ))
                .unwrap(),
                cometbft::vote::Power::from(10000_u32),
                Bytes::from(hex!("b32979580ea04984a2be033599c20c7a0c9a8d121b57f94ee05f5eda5b36c38f6e354c89328b92cdd1de33b64d3a0867")).to_vec(),
                Bytes::from(hex!("97376a436bbf54e0f6949b57aa821a90a749920a")).to_vec(),
            ));
            let validator_set = validators_info;
            let cs = ConsensusState::new(chain_id, height, next_validator_set_hash, validator_set);

            let expected_output = Bytes::from(hex!("636861696e5f393030302d3132310000000000000000000000000000000000000000000000000001a5f1af4874227f1cdbe5240259a365ad86484a4255bfd65e2a0222d733fcdbc320cc466ee9412ddd49e0fff04cdb41bade2b7622f08b6bdacac94d4de03bdb970000000000002710d5e63aeee6e6fa122a6a23a6e0fca87701ba1541aa2d28cbcd1ea3a63479f6fb260a3d755853e6a78cfa6252584fee97b2ec84a9d572ee4a5d3bc1558bb98a4b370fb8616b0b523ee91ad18a63d63f21e0c40a83ef15963f4260574ca5159fd90a1c527000000000000027106fd1ceb5a48579f322605220d4325bd9ff90d5fab31e74a881fc78681e3dfa440978d2b8be0708a1cbbca2c660866216975fdaf0e9038d9b7ccbf9731f43956dba7f2451919606ae20bf5d248ee353821754bcdb456fd3950618fda3e32d3d0fb990eeda000000000000271097376a436bbf54e0f6949b57aa821a90a749920ab32979580ea04984a2be033599c20c7a0c9a8d121b57f94ee05f5eda5b36c38f6e354c89328b92cdd1de33b64d3a0867"));
            let cs_bytes = cs.encode().unwrap();
            assert_eq!(cs_bytes, expected_output);
        }
    }

    #[test]
    fn test_decode_consensus_state() {
        {
            let chain_id = "chain_9000-121".to_string();
            let height = 1;
            let next_validator_set_hash = Bytes::from(hex!(
                "0CE856B1DC9CDCF3BF2478291CF02C62AEEB3679889E9866931BF1FB05A10EDA"
            ));
            let mut validators_info = Vec::new();
            validators_info.push(Validator::new_with_bls_and_relayer(
                PublicKey::from_raw_ed25519(&hex!(
                    "c3d9a1082f42ca161402f8668f8e39ec9e30092affd8d3262267ac7e248a959e"
                ))
                .unwrap(),
                cometbft::vote::Power::from(10000_u32),
                Bytes::from(hex!("a60afe627fd78b19e07e07e19d446009dd53a18c6c8744176a5d851a762bbb51198e7e006f2a6ea7225661a61ecd832d")).to_vec(),
                Bytes::from(hex!("B32d0723583040F3A16D1380D1e6AA874cD1bdF7")).to_vec(),
            ));
            let validator_set = validators_info;
            let bls_pub_key = Bytes::from(hex!("a60afe627fd78b19e07e07e19d446009dd53a18c6c8744176a5d851a762bbb51198e7e006f2a6ea7225661a61ecd832d"));
            let relayer_address = Bytes::from(hex!("B32d0723583040F3A16D1380D1e6AA874cD1bdF7"));
            let cs_bytes = Bytes::from(hex!("636861696e5f393030302d31323100000000000000000000000000000000000000000000000000010ce856b1dc9cdcf3bf2478291cf02c62aeeb3679889e9866931bf1fb05a10edac3d9a1082f42ca161402f8668f8e39ec9e30092affd8d3262267ac7e248a959e0000000000002710b32d0723583040f3a16d1380d1e6aa874cd1bdf7a60afe627fd78b19e07e07e19d446009dd53a18c6c8744176a5d851a762bbb51198e7e006f2a6ea7225661a61ecd832d"));
            let cs = match decode_consensus_state(&cs_bytes, false) {
                Ok(cs) => cs,
                Err(_) => panic!("decode consensus state failed"),
            };
            assert_eq!(cs.chain_id, chain_id);
            assert_eq!(cs.height, height);
            assert_eq!(cs.next_validator_set_hash, next_validator_set_hash);
            assert_eq!(cs.validators, validator_set);
            assert_eq!(cs.validators[0].relayer_address.as_bytes(), relayer_address.to_vec());
            assert_eq!(cs.validators[0].bls_key.as_bytes(), bls_pub_key.to_vec());
        }
        {
            let chain_id = "chain_9000-121".to_string();
            let height = 1;
            let next_validator_set_hash = Bytes::from(hex!(
                "A5F1AF4874227F1CDBE5240259A365AD86484A4255BFD65E2A0222D733FCDBC3"
            ));
            let mut validators_info = Vec::new();
            let mut bls_pub_keys = Vec::new();
            let mut relayer_addresses = Vec::new();
            validators_info.push(Validator::new_with_bls_and_relayer(
                PublicKey::from_raw_ed25519(&hex!(
                    "20cc466ee9412ddd49e0fff04cdb41bade2b7622f08b6bdacac94d4de03bdb97"
                ))
                .unwrap(),
                cometbft::vote::Power::from(10000_u32),
                Bytes::from(hex!("aa2d28cbcd1ea3a63479f6fb260a3d755853e6a78cfa6252584fee97b2ec84a9d572ee4a5d3bc1558bb98a4b370fb861")).to_vec(),
                Bytes::from(hex!("d5e63aeee6e6fa122a6a23a6e0fca87701ba1541")).to_vec(),
            ));
            bls_pub_keys.push(Bytes::from(hex!("aa2d28cbcd1ea3a63479f6fb260a3d755853e6a78cfa6252584fee97b2ec84a9d572ee4a5d3bc1558bb98a4b370fb861")));
            relayer_addresses.push(Bytes::from(hex!("d5e63aeee6e6fa122a6a23a6e0fca87701ba1541")));
            validators_info.push(cometbft::validator::Info::new_with_bls_and_relayer(
                PublicKey::from_raw_ed25519(&hex!(
                    "6b0b523ee91ad18a63d63f21e0c40a83ef15963f4260574ca5159fd90a1c5270"
                ))
                .unwrap(),
                cometbft::vote::Power::from(10000_u32),
                Bytes::from(hex!("b31e74a881fc78681e3dfa440978d2b8be0708a1cbbca2c660866216975fdaf0e9038d9b7ccbf9731f43956dba7f2451")).to_vec(),
                Bytes::from(hex!("6fd1ceb5a48579f322605220d4325bd9ff90d5fa")).to_vec(),
            ));
            bls_pub_keys.push(Bytes::from(hex!("b31e74a881fc78681e3dfa440978d2b8be0708a1cbbca2c660866216975fdaf0e9038d9b7ccbf9731f43956dba7f2451")));
            relayer_addresses.push(Bytes::from(hex!("6fd1ceb5a48579f322605220d4325bd9ff90d5fa")));
            validators_info.push(cometbft::validator::Info::new_with_bls_and_relayer(
                PublicKey::from_raw_ed25519(&hex!(
                    "919606ae20bf5d248ee353821754bcdb456fd3950618fda3e32d3d0fb990eeda"
                ))
                .unwrap(),
                cometbft::vote::Power::from(10000_u32),
                Bytes::from(hex!("b32979580ea04984a2be033599c20c7a0c9a8d121b57f94ee05f5eda5b36c38f6e354c89328b92cdd1de33b64d3a0867")).to_vec(),
                Bytes::from(hex!("97376a436bbf54e0f6949b57aa821a90a749920a")).to_vec(),
            ));
            bls_pub_keys.push(Bytes::from(hex!("b32979580ea04984a2be033599c20c7a0c9a8d121b57f94ee05f5eda5b36c38f6e354c89328b92cdd1de33b64d3a0867")));
            relayer_addresses.push(Bytes::from(hex!("97376a436bbf54e0f6949b57aa821a90a749920a")));
            let validator_set = validators_info;
            let cs_bytes = Bytes::from(hex!("636861696e5f393030302d3132310000000000000000000000000000000000000000000000000001a5f1af4874227f1cdbe5240259a365ad86484a4255bfd65e2a0222d733fcdbc320cc466ee9412ddd49e0fff04cdb41bade2b7622f08b6bdacac94d4de03bdb970000000000002710d5e63aeee6e6fa122a6a23a6e0fca87701ba1541aa2d28cbcd1ea3a63479f6fb260a3d755853e6a78cfa6252584fee97b2ec84a9d572ee4a5d3bc1558bb98a4b370fb8616b0b523ee91ad18a63d63f21e0c40a83ef15963f4260574ca5159fd90a1c527000000000000027106fd1ceb5a48579f322605220d4325bd9ff90d5fab31e74a881fc78681e3dfa440978d2b8be0708a1cbbca2c660866216975fdaf0e9038d9b7ccbf9731f43956dba7f2451919606ae20bf5d248ee353821754bcdb456fd3950618fda3e32d3d0fb990eeda000000000000271097376a436bbf54e0f6949b57aa821a90a749920ab32979580ea04984a2be033599c20c7a0c9a8d121b57f94ee05f5eda5b36c38f6e354c89328b92cdd1de33b64d3a0867"));
            let cs = match decode_consensus_state(&cs_bytes, false) {
                Ok(cs) => cs,
                Err(_) => panic!("decode consensus state failed"),
            };

            assert_eq!(cs.chain_id, chain_id);
            assert_eq!(cs.height, height);
            assert_eq!(cs.next_validator_set_hash, next_validator_set_hash);
            assert_eq!(cs.validators, validator_set);
            assert_eq!(cs.validators[0].relayer_address.as_bytes(), relayer_addresses[0].to_vec());
            assert_eq!(cs.validators[0].bls_key.as_bytes(), bls_pub_keys[0].to_vec());
            assert_eq!(cs.validators[1].relayer_address.as_bytes(), relayer_addresses[1].to_vec());
            assert_eq!(cs.validators[1].bls_key.as_bytes(), bls_pub_keys[1].to_vec());
            assert_eq!(cs.validators[2].relayer_address.as_bytes(), relayer_addresses[2].to_vec());
            assert_eq!(cs.validators[2].bls_key.as_bytes(), bls_pub_keys[2].to_vec());
        }
    }

    #[test]
    fn test_apply_light_block() {
        {
            let cs_bytes = Bytes::from(hex!("677265656e6669656c645f393030302d3132310000000000000000000000000000000000000000013c350cd55b99dc6c2b7da9bef5410fbfb869fede858e7b95bf7ca294e228bb40e33f6e876d63791ebd05ff617a1b4f4ad1aa2ce65e3c3a9cdfb33e0ffa7e8423000000000098968015154514f68ce65a0d9eecc578c0ab12da0a2a28a0805521b5b7ae56eb3fb24555efbfe59e1622bfe9f7be8c9022e9b3f2442739c1ce870b9adee169afe60f674edd7c86451c5363d89052fde8351895eeea166ce5373c36e31b518ed191d0c599aa0f5b0000000000989680432f6c4908a9aa5f3444421f466b11645235c99b831b2a2de9e504d7ea299e52a202ce529808618eb3bfc0addf13d8c5f2df821d81e18f9bc61583510b322d067d46323b0a572635c06a049c0a2a929e3c8184a50cf6a8b95708c25834ade456f399015a0000000000989680864cb9828254d712f8e59b164fc6a9402dc4e6c59065e38cff24f5323c8c5da888a0f97e5ee4ba1e11b0674b0a0d06204c1dfa247c370cd4be3e799fc4f6f48d977ac7ca"));
            let mut cs = match decode_consensus_state(&cs_bytes, false) {
                Ok(cs) => cs,
                Err(_) => panic!("decode consensus state failed"),
            };
            let light_block_bytes = Bytes::from(hex!("0aeb060adb030a02080b1213677265656e6669656c645f393030302d3132311802220c08b2d7f3a10610e8d2adb3032a480a20ec6ecb5db4ffb17fabe40c60ca7b8441e9c5d77585d0831186f3c37aa16e9c15122408011220a2ab9e1eb9ea52812f413526e424b326aff2f258a56e00d690db9f805b60fe7e32200f40aeff672e8309b7b0aefbb9a1ae3d4299b5c445b7d54e8ff398488467f0053a20e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b85542203c350cd55b99dc6c2b7da9bef5410fbfb869fede858e7b95bf7ca294e228bb404a203c350cd55b99dc6c2b7da9bef5410fbfb869fede858e7b95bf7ca294e228bb405220294d8fbd0b94b767a7eba9840f299a3586da7fe6b5dead3b7eecba193c400f935a20bc50557c12d7392b0d07d75df0b61232d48f86a74fdea6d1485d9be6317d268c6220e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b8556a20e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b85572146699336aa109d1beab3946198c8e59f3b2cbd92f7a4065e3cd89e315ca39d87dee92835b98f8b8ec0861d6d9bb2c60156df5d375b3ceb1fbe71af6a244907d62548a694165caa660fec7a9b4e7b9198191361c71be0b128a0308021a480a20726abd0fdbfb6f779b0483e6e4b4b6f12241f6ea2bf374233ab1a316692b6415122408011220159f10ff15a8b58fc67a92ffd7f33c8cd407d4ce81b04ca79177dfd00ca19a67226808021214050cff76cc632760ba9db796c046004c900967361a0c08b3d7f3a10610808cadba03224080713027ffb776a702d78fd0406205c629ba473e1f8d6af646190f6eb9262cd67d69be90d10e597b91e06d7298eb6fa4b8f1eb7752ebf352a1f51560294548042268080212146699336aa109d1beab3946198c8e59f3b2cbd92f1a0c08b3d7f3a10610b087c1c00322405e2ddb70acfe4904438be3d9f4206c0ace905ac4fc306a42cfc9e86268950a0fbfd6ec5f526d3e41a3ef52bf9f9f358e3cb4c3feac76c762fa3651c1244fe004226808021214c55765fd2d0570e869f6ac22e7f2916a35ea300d1a0c08b3d7f3a10610f0b3d492032240ca17898bd22232fc9374e1188636ee321a396444a5b1a79f7628e4a11f265734b2ab50caf21e8092c55d701248e82b2f011426cb35ba22043b497a6b4661930612a0050aa8010a14050cff76cc632760ba9db796c046004c9009673612220a20e33f6e876d63791ebd05ff617a1b4f4ad1aa2ce65e3c3a9cdfb33e0ffa7e84231880ade2042080a6bbf6ffffffffff012a30a0805521b5b7ae56eb3fb24555efbfe59e1622bfe9f7be8c9022e9b3f2442739c1ce870b9adee169afe60f674edd7c86321415154514f68ce65a0d9eecc578c0ab12da0a2a283a14ee7a2a6a44d427f6949eeb8f12ea9fbb2501da880aa2010a146699336aa109d1beab3946198c8e59f3b2cbd92f12220a20451c5363d89052fde8351895eeea166ce5373c36e31b518ed191d0c599aa0f5b1880ade2042080ade2042a30831b2a2de9e504d7ea299e52a202ce529808618eb3bfc0addf13d8c5f2df821d81e18f9bc61583510b322d067d46323b3214432f6c4908a9aa5f3444421f466b11645235c99b3a14a0a7769429468054e19059af4867da0a495567e50aa2010a14c55765fd2d0570e869f6ac22e7f2916a35ea300d12220a200a572635c06a049c0a2a929e3c8184a50cf6a8b95708c25834ade456f399015a1880ade2042080ade2042a309065e38cff24f5323c8c5da888a0f97e5ee4ba1e11b0674b0a0d06204c1dfa247c370cd4be3e799fc4f6f48d977ac7ca3214864cb9828254d712f8e59b164fc6a9402dc4e6c53a143139916d97df0c589312b89950b6ab9795f34d1a12a8010a14050cff76cc632760ba9db796c046004c9009673612220a20e33f6e876d63791ebd05ff617a1b4f4ad1aa2ce65e3c3a9cdfb33e0ffa7e84231880ade2042080a6bbf6ffffffffff012a30a0805521b5b7ae56eb3fb24555efbfe59e1622bfe9f7be8c9022e9b3f2442739c1ce870b9adee169afe60f674edd7c86321415154514f68ce65a0d9eecc578c0ab12da0a2a283a14ee7a2a6a44d427f6949eeb8f12ea9fbb2501da88"));
            let mut light_block_pb: TmLightBlock = TmLightBlock::default();
            match light_block_pb.merge(light_block_bytes.clone()) {
                Ok(_) => (),
                Err(_) => panic!("merge light block failed"),
            };
            let (light_block, validators) = match convert_light_block_from_proto(&light_block_pb) {
                Ok(v) => v,
                Err(_) => panic!("convert light block from proto failed"),
            };
            let expected_height = 2_u64;
            let expected_validator_set_changed = false;

            match cs.apply_light_block(
                &light_block,
                &decode_randao_mix(light_block_bytes.as_ref()).unwrap(),
                validator_set_hash(light_block_pb.validator_set.as_ref().unwrap()),
                validators,
            ) {
                Ok(validator_set_changed) => {
                    assert_eq!(validator_set_changed, expected_validator_set_changed);
                    assert_eq!(cs.height, expected_height);
                }
                Err(_) => panic!("apply light block failed"),
            }
        }
        {
            let cs_bytes = Bytes::from(hex!("677265656e6669656c645f393030302d313734310000000000000000000000000000000000000001af6b801dda578dddfa4da1d5d67fd1b32510db24ec271346fc573e9242b01c9a112b51dda2d336246bdc0cc51407ba0cb0e5087be0db5f1cdc3285bbaa8e647500000000000003e84202722cf6a34d727be762b46825b0d26b6263a0a9355ebf3c24bedac5a357a56feeb2cd8b6fed9f14cca15c3091f523b9fb21183b4bb31eb482a0321885e3f57072156448e2b2f7d9a3e7b668757d9cc0bbd28cd674c34ed1c2ed75c5de3b6a8f8cad4600000000000003e8668a0acd8f6db5cae959a0e02132f4d6a672c4d7a4726b542012cc8023ee07b29ab3971cc999d8751bbd16f23413968afcdb070ed66ab47e6e1842bf875bef21dfc5b8af6813bfd82860d361e339bd1ae2f801b6d6ee46b8497a3d51c80b50b6160ea1cc00000000000003e80dfa99423d3084c596c5e3bd6bcb4f654516517b8d4786703c56b300b70f085c0d0482e5d6a3c7208883f0ec8abd2de893f71d18e8f919e7ab198499201d87f92c57ebce83ed2b763bb872e9bc148fb216fd5c93b18819670d9a946ae4b3075672d726b800000000000003e824aab6f85470ff73e3048c64083a09e980d4cb7f8146d231a7b2051c5f7a9c07ab6e6bfe277bd5f4a94f901fe6ee7a6b6bd8479e9e5e448de4b1b33d5ddd74194c86b3852cc140a3f08a9c4149efd45643202f8bef2ad7eecf53e58951c6df6fd932004b00000000000003e84998f6ef8d999a0f36a851bfa29dbcf0364dd65695c286deb3f1657664859d59876bf1ec5a288f6e66e18b37b8a2a1e6ee4a3ef8fa50784d8b758d0c3e70a7cdfe65ab5d"));
            let mut cs = match decode_consensus_state(&cs_bytes, false) {
                Ok(cs) => cs,
                Err(_) => panic!("decode consensus state failed"),
            };
            let light_block_bytes = Bytes::from(hex!("0aeb070ade030a02080b1214677265656e6669656c645f393030302d3137343118e9d810220c08f2f2b6a30610af9fcc8e022a480a20315130cf3a10f78c5f7633e3941f605151a6901910713c84da0d7929898e9b9e122408011220f09b2290e56b59a7286c2144a811c780f0fd5f631614a9f7ec2dec43f14ac5d63220d15354fdbcc6c7d3e8c5ede34f4f71e896599ba67773605eb6579e10e09254773a20e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b8554220311b22582926e7833b72904605441ed602896e8aeb093bca5f2e8170cea5ed6a4a20311b22582926e7833b72904605441ed602896e8aeb093bca5f2e8170cea5ed6a5220048091bc7ddc283f77bfbf91d73c44da58c3df8a9cbc867405d8b7f3daada22f5a20ee2da802b95c55e551291d96fe6ee4fe8074ddfa2df110042d6809acb665628a6220e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b8556a20e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b8557214793cee4b478e537592c40ecfb2148ebe32b8f6057a4034248b04af30e0d302cf8cedff585d5e1c6ff8db526bcf298d665cf301ca938a874c76ba9a1fd9fae302b2ec49a335930cf0242762c92843d7f9f7963d60580a12870408e9d8101a480a20452e1984f64c79550ac23db0c408b3eb021675d678ad94f9206ad7a2dec83a181224080112205224c29260b6c220685b29f593bac728e522e3e3675ec7edd92c12251acfe4b4226808021214d742fa5318dc3986e075e2b050529a22c6fa3b8b1a0c08f4f2b6a306109898f6a70322409762b7abd4dd63bb8858673dffd5795b1a87532d3719458d12fbbd1fd2443ca76bd36c4c09fa8952a440de4904f1b6b9270037a147431892c8ace96ad43bf90b2268080212145fa8b3f3fcd4a3ea2495e11dd5dbd399b3d8d4f81a0c08f4f2b6a30610f8f2fd9e03224093f2fc21a41492a34ed3b31ff2eba571ca752ae989f2e47728740bb1eec0f20eb59f59d390ce3d67734ab49a72bc2e97e185d21a4b00f3288ea50b0f1383220a226808021214793cee4b478e537592c40ecfb2148ebe32b8f6051a0c08f4f2b6a306108e8ed7a7032240a4a3c047ca75aeb6e9a21fbc3742f4339c64ead15d117675a2757f7db965aae3e6901f81a3707a67d91c61d6c842b95009e132e7fab187965dc04861d7faa902226808021214f0f07dc2f5e159a35b9662553c6b4e51868502f71a0c08f4f2b6a30610bfed829f032240e23ddc98b0bf7cc6cd494fd8ec96d440d29193910a6eca3dc7e41cdb14efa32471feb1ea2d613bb5acdd8623e8372ed3a36e1838bc75646bdfe9d2ef96647400220f08011a0b088092b8c398feffffff0112d0060a90010a14d742fa5318dc3986e075e2b050529a22c6fa3b8b12220a2083ed2b763bb872e9bc148fb216fd5c93b18819670d9a946ae4b3075672d726b818880820abe8ffffffffffffff012a308146d231a7b2051c5f7a9c07ab6e6bfe277bd5f4a94f901fe6ee7a6b6bd8479e9e5e448de4b1b33d5ddd74194c86b385321424aab6f85470ff73e3048c64083a09e980d4cb7f0a88010a145fa8b3f3fcd4a3ea2495e11dd5dbd399b3d8d4f812220a2048e2b2f7d9a3e7b668757d9cc0bbd28cd674c34ed1c2ed75c5de3b6a8f8cad4618fc0720fc072a30a4726b542012cc8023ee07b29ab3971cc999d8751bbd16f23413968afcdb070ed66ab47e6e1842bf875bef21dfc5b8af3214668a0acd8f6db5cae959a0e02132f4d6a672c4d70a88010a14793cee4b478e537592c40ecfb2148ebe32b8f60512220a206813bfd82860d361e339bd1ae2f801b6d6ee46b8497a3d51c80b50b6160ea1cc18ec0720ec072a308d4786703c56b300b70f085c0d0482e5d6a3c7208883f0ec8abd2de893f71d18e8f919e7ab198499201d87f92c57ebce32140dfa99423d3084c596c5e3bd6bcb4f654516517b0a88010a14f0f07dc2f5e159a35b9662553c6b4e51868502f712220a202cc140a3f08a9c4149efd45643202f8bef2ad7eecf53e58951c6df6fd932004b18ec0720ec072a3095c286deb3f1657664859d59876bf1ec5a288f6e66e18b37b8a2a1e6ee4a3ef8fa50784d8b758d0c3e70a7cdfe65ab5d32144998f6ef8d999a0f36a851bfa29dbcf0364dd6560a86010a1468478c1a37bc01c3acb7470cc6a78f1009a14f7012220a20de83e10566b038855254800b5b0ebf7c21aede9883c11e5cf289979e233b3efe180120012a3089063607696a9e6dbddbe6c23b4634a7c02b80212afc7ec65fb0d379d55d2d0cb25df19c0252356ffa2e2252eedd8f57321400000000000000000000000000000000000000001290010a14d742fa5318dc3986e075e2b050529a22c6fa3b8b12220a2083ed2b763bb872e9bc148fb216fd5c93b18819670d9a946ae4b3075672d726b818880820abe8ffffffffffffff012a308146d231a7b2051c5f7a9c07ab6e6bfe277bd5f4a94f901fe6ee7a6b6bd8479e9e5e448de4b1b33d5ddd74194c86b385321424aab6f85470ff73e3048c64083a09e980d4cb7f"));
            let mut light_block_pb: TmLightBlock = TmLightBlock::default();
            match light_block_pb.merge(light_block_bytes.clone()) {
                Ok(_) => (),
                Err(_) => panic!("merge light block failed"),
            };
            let (light_block, validators) = match convert_light_block_from_proto(&light_block_pb) {
                Ok(v) => v,
                Err(_) => panic!("convert light block from proto failed"),
            };
            let expected_height = 273513_u64;
            let expected_validator_set_changed = true;

            match cs.apply_light_block(
                &light_block,
                &decode_randao_mix(light_block_bytes.as_ref()).unwrap(),
                validator_set_hash(light_block_pb.validator_set.as_ref().unwrap()),
                validators,
            ) {
                Ok(validator_set_changed) => {
                    assert_eq!(validator_set_changed, expected_validator_set_changed);
                    assert_eq!(cs.height, expected_height);
                }
                Err(_) => panic!("apply light block failed"),
            }
        }
    }

    #[test]
    fn test_cometbft_light_block_validate_before_hertz() {
        let input = Bytes::from(hex!(
            "000000000000000000000000000000000000000000000000000000000000018c677265656e6669656c645f393030302d3132310000000000000000000000000000000000000000013c350cd55b99dc6c2b7da9bef5410fbfb869fede858e7b95bf7ca294e228bb40e33f6e876d63791ebd05ff617a1b4f4ad1aa2ce65e3c3a9cdfb33e0ffa7e8423000000000098968015154514f68ce65a0d9eecc578c0ab12da0a2a28a0805521b5b7ae56eb3fb24555efbfe59e1622bfe9f7be8c9022e9b3f2442739c1ce870b9adee169afe60f674edd7c86451c5363d89052fde8351895eeea166ce5373c36e31b518ed191d0c599aa0f5b0000000000989680432f6c4908a9aa5f3444421f466b11645235c99b831b2a2de9e504d7ea299e52a202ce529808618eb3bfc0addf13d8c5f2df821d81e18f9bc61583510b322d067d46323b0a572635c06a049c0a2a929e3c8184a50cf6a8b95708c25834ade456f399015a0000000000989680864cb9828254d712f8e59b164fc6a9402dc4e6c59065e38cff24f5323c8c5da888a0f97e5ee4ba1e11b0674b0a0d06204c1dfa247c370cd4be3e799fc4f6f48d977ac7ca0aeb060adb030a02080b1213677265656e6669656c645f393030302d3132311802220c08b2d7f3a10610e8d2adb3032a480a20ec6ecb5db4ffb17fabe40c60ca7b8441e9c5d77585d0831186f3c37aa16e9c15122408011220a2ab9e1eb9ea52812f413526e424b326aff2f258a56e00d690db9f805b60fe7e32200f40aeff672e8309b7b0aefbb9a1ae3d4299b5c445b7d54e8ff398488467f0053a20e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b85542203c350cd55b99dc6c2b7da9bef5410fbfb869fede858e7b95bf7ca294e228bb404a203c350cd55b99dc6c2b7da9bef5410fbfb869fede858e7b95bf7ca294e228bb405220294d8fbd0b94b767a7eba9840f299a3586da7fe6b5dead3b7eecba193c400f935a20bc50557c12d7392b0d07d75df0b61232d48f86a74fdea6d1485d9be6317d268c6220e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b8556a20e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b85572146699336aa109d1beab3946198c8e59f3b2cbd92f7a4065e3cd89e315ca39d87dee92835b98f8b8ec0861d6d9bb2c60156df5d375b3ceb1fbe71af6a244907d62548a694165caa660fec7a9b4e7b9198191361c71be0b128a0308021a480a20726abd0fdbfb6f779b0483e6e4b4b6f12241f6ea2bf374233ab1a316692b6415122408011220159f10ff15a8b58fc67a92ffd7f33c8cd407d4ce81b04ca79177dfd00ca19a67226808021214050cff76cc632760ba9db796c046004c900967361a0c08b3d7f3a10610808cadba03224080713027ffb776a702d78fd0406205c629ba473e1f8d6af646190f6eb9262cd67d69be90d10e597b91e06d7298eb6fa4b8f1eb7752ebf352a1f51560294548042268080212146699336aa109d1beab3946198c8e59f3b2cbd92f1a0c08b3d7f3a10610b087c1c00322405e2ddb70acfe4904438be3d9f4206c0ace905ac4fc306a42cfc9e86268950a0fbfd6ec5f526d3e41a3ef52bf9f9f358e3cb4c3feac76c762fa3651c1244fe004226808021214c55765fd2d0570e869f6ac22e7f2916a35ea300d1a0c08b3d7f3a10610f0b3d492032240ca17898bd22232fc9374e1188636ee321a396444a5b1a79f7628e4a11f265734b2ab50caf21e8092c55d701248e82b2f011426cb35ba22043b497a6b4661930612a0050aa8010a14050cff76cc632760ba9db796c046004c9009673612220a20e33f6e876d63791ebd05ff617a1b4f4ad1aa2ce65e3c3a9cdfb33e0ffa7e84231880ade2042080a6bbf6ffffffffff012a30a0805521b5b7ae56eb3fb24555efbfe59e1622bfe9f7be8c9022e9b3f2442739c1ce870b9adee169afe60f674edd7c86321415154514f68ce65a0d9eecc578c0ab12da0a2a283a14ee7a2a6a44d427f6949eeb8f12ea9fbb2501da880aa2010a146699336aa109d1beab3946198c8e59f3b2cbd92f12220a20451c5363d89052fde8351895eeea166ce5373c36e31b518ed191d0c599aa0f5b1880ade2042080ade2042a30831b2a2de9e504d7ea299e52a202ce529808618eb3bfc0addf13d8c5f2df821d81e18f9bc61583510b322d067d46323b3214432f6c4908a9aa5f3444421f466b11645235c99b3a14a0a7769429468054e19059af4867da0a495567e50aa2010a14c55765fd2d0570e869f6ac22e7f2916a35ea300d12220a200a572635c06a049c0a2a929e3c8184a50cf6a8b95708c25834ade456f399015a1880ade2042080ade2042a309065e38cff24f5323c8c5da888a0f97e5ee4ba1e11b0674b0a0d06204c1dfa247c370cd4be3e799fc4f6f48d977ac7ca3214864cb9828254d712f8e59b164fc6a9402dc4e6c53a143139916d97df0c589312b89950b6ab9795f34d1a12a8010a14050cff76cc632760ba9db796c046004c9009673612220a20e33f6e876d63791ebd05ff617a1b4f4ad1aa2ce65e3c3a9cdfb33e0ffa7e84231880ade2042080a6bbf6ffffffffff012a30a0805521b5b7ae56eb3fb24555efbfe59e1622bfe9f7be8c9022e9b3f2442739c1ce870b9adee169afe60f674edd7c86321415154514f68ce65a0d9eecc578c0ab12da0a2a283a14ee7a2a6a44d427f6949eeb8f12ea9fbb2501da88"
        ));
        let except_output_after_hertz = Bytes::from(hex!(
            "000000000000000000000000000000000000000000000000000000000000018c677265656e6669656c645f393030302d3132310000000000000000000000000000000000000000023c350cd55b99dc6c2b7da9bef5410fbfb869fede858e7b95bf7ca294e228bb40e33f6e876d63791ebd05ff617a1b4f4ad1aa2ce65e3c3a9cdfb33e0ffa7e8423000000000098968015154514f68ce65a0d9eecc578c0ab12da0a2a28a0805521b5b7ae56eb3fb24555efbfe59e1622bfe9f7be8c9022e9b3f2442739c1ce870b9adee169afe60f674edd7c86451c5363d89052fde8351895eeea166ce5373c36e31b518ed191d0c599aa0f5b0000000000989680432f6c4908a9aa5f3444421f466b11645235c99b831b2a2de9e504d7ea299e52a202ce529808618eb3bfc0addf13d8c5f2df821d81e18f9bc61583510b322d067d46323b0a572635c06a049c0a2a929e3c8184a50cf6a8b95708c25834ade456f399015a0000000000989680864cb9828254d712f8e59b164fc6a9402dc4e6c59065e38cff24f5323c8c5da888a0f97e5ee4ba1e11b0674b0a0d06204c1dfa247c370cd4be3e799fc4f6f48d977ac7ca"
        ));

        let result = cometbft_light_block_validation_run_before_hertz(&input, 100_000, 0);
        let output = result.expect("cometbft_light_block_validation_run failed");
        assert_eq!(output.gas_used, 3_000);
        assert_eq!(output.bytes, except_output_after_hertz);
        assert!(output.is_success());
    }

    #[test]
    fn pasteur_rejects_duplicate_validators_in_consensus_state() {
        // Two identical validators collide on address, pubkey, bls key and relayer address.
        let pk = || {
            PublicKey::from_raw_ed25519(&hex!(
                "c3d9a1082f42ca161402f8668f8e39ec9e30092affd8d3262267ac7e248a959e"
            ))
            .unwrap()
        };
        let bls = hex!("a60afe627fd78b19e07e07e19d446009dd53a18c6c8744176a5d851a762bbb51198e7e006f2a6ea7225661a61ecd832d").to_vec();
        let relayer = hex!("b32d0723583040f3a16d1380d1e6aa874cd1bdf7").to_vec();
        let cs = ConsensusState::new(
            "chain_9000-121".to_string(),
            1,
            Bytes::from(
                hex!("0ce856b1dc9cdcf3bf2478291cf02c62aeeb3679889e9866931bf1fb05a10eda").to_vec(),
            ),
            vec![
                Validator::new_with_bls_and_relayer(
                    pk(),
                    Power::from(10000_u32),
                    bls.clone(),
                    relayer.clone(),
                ),
                Validator::new_with_bls_and_relayer(
                    pk(),
                    Power::from(10000_u32),
                    bls.clone(),
                    relayer.clone(),
                ),
            ],
        );
        let encoded = cs.encode().expect("encode consensus state");

        // Pre-Pasteur decoding tolerates duplicates; Pasteur rejects them.
        assert!(decode_consensus_state(&encoded, false).is_ok());
        match decode_consensus_state(&encoded, true) {
            Err(PrecompileHalt::Other(msg)) => assert!(msg.contains("duplicate validator")),
            Err(other) => panic!("unexpected halt: {other:?}"),
            Ok(_) => panic!("expected duplicate-validator rejection"),
        }
    }

    #[test]
    fn validate_unique_validator_set_checks_bridge_keys() {
        let pk0 = || {
            PublicKey::from_raw_ed25519(&hex!(
                "20cc466ee9412ddd49e0fff04cdb41bade2b7622f08b6bdacac94d4de03bdb97"
            ))
            .unwrap()
        };
        let pk1 = || {
            PublicKey::from_raw_ed25519(&hex!(
                "6b0b523ee91ad18a63d63f21e0c40a83ef15963f4260574ca5159fd90a1c5270"
            ))
            .unwrap()
        };
        let bls0 = hex!("aa2d28cbcd1ea3a63479f6fb260a3d755853e6a78cfa6252584fee97b2ec84a9d572ee4a5d3bc1558bb98a4b370fb861").to_vec();
        let bls1 = hex!("b31e74a881fc78681e3dfa440978d2b8be0708a1cbbca2c660866216975fdaf0e9038d9b7ccbf9731f43956dba7f2451").to_vec();
        let relayer0 = hex!("d5e63aeee6e6fa122a6a23a6e0fca87701ba1541").to_vec();
        let relayer1 = hex!("6fd1ceb5a48579f322605220d4325bd9ff90d5fa").to_vec();

        // Duplicate BLS key (pubkeys and relayer addresses distinct).
        let set = vec![
            Validator::new_with_bls_and_relayer(pk0(), Power::from(1_u32), bls0.clone(), relayer0.clone()),
            Validator::new_with_bls_and_relayer(pk1(), Power::from(1_u32), bls0.clone(), relayer1.clone()),
        ];
        match validate_unique_validator_set(&set).unwrap_err() {
            PrecompileHalt::Other(msg) => assert!(msg.contains("duplicate validator bls key")),
            other => panic!("unexpected halt: {other:?}"),
        }

        // Duplicate relayer address.
        let set = vec![
            Validator::new_with_bls_and_relayer(pk0(), Power::from(1_u32), bls0.clone(), relayer0.clone()),
            Validator::new_with_bls_and_relayer(pk1(), Power::from(1_u32), bls1.clone(), relayer0.clone()),
        ];
        match validate_unique_validator_set(&set).unwrap_err() {
            PrecompileHalt::Other(msg) => assert!(msg.contains("duplicate validator relayer address")),
            other => panic!("unexpected halt: {other:?}"),
        }

        // Unset (all-zero) bridge fields may legitimately repeat across validators.
        let set = vec![
            Validator::new_with_bls_and_relayer(pk0(), Power::from(1_u32), vec![0u8; 48], vec![0u8; 20]),
            Validator::new_with_bls_and_relayer(pk1(), Power::from(1_u32), vec![0u8; 48], vec![0u8; 20]),
        ];
        assert!(validate_unique_validator_set(&set).is_ok());
    }

    #[test]
    fn pasteur_accepts_unique_validator_set() {
        // Three distinct validators must pass the uniqueness check (no false positives).
        let cs = ConsensusState::new(
            "chain_9000-121".to_string(),
            1,
            Bytes::from(
                hex!("a5f1af4874227f1cdbe5240259a365ad86484a4255bfd65e2a0222d733fcdbc3").to_vec(),
            ),
            vec![
                Validator::new_with_bls_and_relayer(
                    PublicKey::from_raw_ed25519(&hex!(
                        "20cc466ee9412ddd49e0fff04cdb41bade2b7622f08b6bdacac94d4de03bdb97"
                    ))
                    .unwrap(),
                    Power::from(10000_u32),
                    hex!("aa2d28cbcd1ea3a63479f6fb260a3d755853e6a78cfa6252584fee97b2ec84a9d572ee4a5d3bc1558bb98a4b370fb861").to_vec(),
                    hex!("d5e63aeee6e6fa122a6a23a6e0fca87701ba1541").to_vec(),
                ),
                Validator::new_with_bls_and_relayer(
                    PublicKey::from_raw_ed25519(&hex!(
                        "6b0b523ee91ad18a63d63f21e0c40a83ef15963f4260574ca5159fd90a1c5270"
                    ))
                    .unwrap(),
                    Power::from(10000_u32),
                    hex!("b31e74a881fc78681e3dfa440978d2b8be0708a1cbbca2c660866216975fdaf0e9038d9b7ccbf9731f43956dba7f2451").to_vec(),
                    hex!("6fd1ceb5a48579f322605220d4325bd9ff90d5fa").to_vec(),
                ),
                Validator::new_with_bls_and_relayer(
                    PublicKey::from_raw_ed25519(&hex!(
                        "919606ae20bf5d248ee353821754bcdb456fd3950618fda3e32d3d0fb990eeda"
                    ))
                    .unwrap(),
                    Power::from(10000_u32),
                    hex!("b32979580ea04984a2be033599c20c7a0c9a8d121b57f94ee05f5eda5b36c38f6e354c89328b92cdd1de33b64d3a0867").to_vec(),
                    hex!("97376a436bbf54e0f6949b57aa821a90a749920a").to_vec(),
                ),
            ],
        );
        let encoded = cs.encode().expect("encode consensus state");
        assert!(decode_consensus_state(&encoded, true).is_ok());
    }

    #[test]
    fn pasteur_0x67_charges_per_byte_gas() {
        // Gas is charged before decoding, so this needs no valid light block.
        let input = vec![0u8; 1000];
        let cost = COMETBFT_LIGHT_BLOCK_VALIDATION_BASE +
            input.len() as u64 * COMETBFT_LIGHT_BLOCK_VALIDATE_PER_BYTE_GAS;

        // Pasteur prices per input byte: a gas limit one below `cost` runs out of gas.
        let pasteur = cometbft_light_block_validation_run_pasteur(&input, cost - 1, 0).unwrap();
        assert_eq!(pasteur.halt_reason(), Some(&PrecompileHalt::OutOfGas));

        // Hertz only ever charges the flat base, so the same limit clears the gas gate (it then
        // halts on the undecodable input — a halt, but not OutOfGas).
        let hertz = cometbft_light_block_validation_run(&input, cost - 1, 0).unwrap();
        assert!(hertz.is_halt());
        assert_ne!(hertz.halt_reason(), Some(&PrecompileHalt::OutOfGas));
    }

    /// Identity point encoding: a small-order public key that Go's verifier accepts.
    const IDENTITY: [u8; 32] = {
        let mut b = [0u8; 32];
        b[0] = 1;
        b
    };

    /// `A = identity, S = 0` verifies against `R = identity` for any message. Go accepts only
    /// the canonical encoding of `R`; setting the sign bit (`r_last_byte = 0x80`) gives the
    /// non-canonical encoding that ZIP-215 also accepts.
    fn identity_vote_signature(r_last_byte: u8) -> [u8; 64] {
        let mut sig = [0u8; 64];
        sig[..32].copy_from_slice(&IDENTITY);
        sig[31] = r_last_byte;
        sig
    }

    /// Rebuilds the fixture's light block around a single validator with the identity public
    /// key, re-binding `validators_hash` and the commit block ID so only the signature matters.
    fn identity_validator_light_block_input(sig: [u8; 64]) -> Vec<u8> {
        identity_validator_light_block_input_with_header(sig, |_| {})
    }

    /// As [`identity_validator_light_block_input`], applying `edit_header` before the header
    /// hash is bound into the commit.
    fn identity_validator_light_block_input_with_header(
        sig: [u8; 64],
        edit_header: impl FnOnce(&mut cometbft_proto::types::v1::Header),
    ) -> Vec<u8> {
        use cometbft_proto::crypto::v1::{public_key::Sum, PublicKey as RawPublicKey};
        use sha2::Sha256;

        let input = valid_light_block_input();
        let cs_length = u64::from_be_bytes(
            input[(CONSENSUS_STATE_LENGTH_BYTES_LENGTH - UINT64_TYPE_LENGTH) as usize..
                CONSENSUS_STATE_LENGTH_BYTES_LENGTH as usize]
                .try_into()
                .unwrap(),
        ) as usize;
        let mut light_block = TmLightBlock::decode(
            &input[CONSENSUS_STATE_LENGTH_BYTES_LENGTH as usize + cs_length..],
        )
        .expect("fixture light block decodes");

        let address = Sha256::digest(IDENTITY)[..20].to_vec();
        let mut validator_set = light_block.validator_set.take().unwrap();
        let mut validator = validator_set.validators[0].clone();
        validator.address = address.clone();
        validator.pub_key = Some(RawPublicKey { sum: Some(Sum::Ed25519(IDENTITY.to_vec())) });
        validator.voting_power = 1_000;
        validator.proposer_priority = 0;
        validator_set.validators = vec![validator.clone()];
        validator_set.proposer = Some(validator.clone());
        validator_set.total_voting_power = 1_000;
        let validators_hash = validator_set_hash(&validator_set).as_bytes().to_vec();

        let mut signed_header = light_block.signed_header.take().unwrap();
        let mut header = signed_header.header.take().unwrap();
        header.validators_hash = validators_hash.clone();
        header.next_validators_hash = validators_hash.clone();
        edit_header(&mut header);
        // The prost re-encode below drops Greenfield's RandaoMix (field 15), so bind the commit
        // to the Go header hash with an empty RandaoMix, as the precompile computes it. Go
        // hashes an omitted `time` as its zero time, which conversion normalises it to.
        let mut hashed_header = header.clone();
        hashed_header.time.get_or_insert_with(go_zero_timestamp);
        let header_hash = header_hash_with_randao_mix(
            &cometbft::block::Header::try_from(hashed_header).expect("header"),
            &[],
        )
        .as_bytes()
        .to_vec();

        let mut commit = signed_header.commit.take().unwrap();
        commit.block_id.as_mut().unwrap().hash = header_hash;
        let mut commit_sig = commit.signatures[0].clone();
        commit_sig.validator_address = address;
        commit_sig.signature = sig.to_vec();
        commit.signatures = vec![commit_sig];

        let chain_id = header.chain_id.clone();
        let height = header.height as u64;
        signed_header.header = Some(header);
        signed_header.commit = Some(commit);
        light_block.signed_header = Some(signed_header);
        light_block.validator_set = Some(validator_set);

        // Trusted state one block below, so the adjacent (`height + 1`) path is taken.
        let mut cs = vec![0u8; CHAIN_ID_LENGTH as usize];
        cs[..chain_id.len()].copy_from_slice(chain_id.as_bytes());
        cs.extend_from_slice(&(height - 1).to_be_bytes());
        cs.extend_from_slice(&validators_hash);
        cs.extend_from_slice(&IDENTITY);
        cs.extend_from_slice(&1_000u64.to_be_bytes());
        cs.extend_from_slice(&validator.relayer_address);
        cs.extend_from_slice(&validator.bls_key);

        let mut crafted = vec![0u8; CONSENSUS_STATE_LENGTH_BYTES_LENGTH as usize];
        crafted[24..32].copy_from_slice(&(cs.len() as u64).to_be_bytes());
        crafted.extend_from_slice(&cs);
        crafted.extend_from_slice(&light_block.encode_to_vec());
        crafted
    }

    /// SRC-2276: geth-bsc accepts the canonical-`R` control and rejects the same vote with a
    /// non-canonical `R`. reth used to accept both, so a contract branching on 0x67 executed
    /// differently on the two clients and reth rejected geth's block.
    #[test]
    fn non_canonical_r_vote_is_rejected_like_geth() {
        type Run = fn(&[u8], u64, u64) -> PrecompileResult;
        let runs: [Run; 3] = [
            cometbft_light_block_validation_run_before_hertz,
            cometbft_light_block_validation_run,
            cometbft_light_block_validation_run_pasteur,
        ];
        for run in runs {
            let control = identity_validator_light_block_input(identity_vote_signature(0x00));
            let output = run(&control, 10_000_000, 0).unwrap();
            assert!(!output.is_halt(), "canonical-R control must verify, as on geth");

            let forged = identity_validator_light_block_input(identity_vote_signature(0x80));
            let output = run(&forged, 10_000_000, 0).unwrap();
            let expected: PrecompileHalt = BscPrecompileError::CometBftApplyBlockFailed.into();
            assert_eq!(
                output.halt_reason(),
                Some(&expected),
                "non-canonical R must fail verification, as on geth"
            );
        }
    }

    /// SRC-1735: go-bsc models `Header.time` (protobuf field 4) as a non-nullable stdtime, so an
    /// omitted field 4 decodes to the Go zero time and is accepted; the pinned Rust model made it
    /// optional and rejected an absent value, splitting reth from canonical geth. A present
    /// (even zero) timestamp was always accepted by both clients. The two inputs differ only in
    /// the presence of field 4, and geth returns the same output for both.
    #[test]
    fn light_block_omitted_header_time_is_accepted_like_geth() {
        let with_time = |time: Option<Timestamp>| {
            identity_validator_light_block_input_with_header(identity_vote_signature(0x00), |h| {
                h.time = time
            })
        };
        let present = with_time(Some(go_zero_timestamp()));
        let omitted = with_time(None);

        let header_time = |input: &[u8]| {
            let (_, proto, _) = decode_light_block_validation_input(input, true).unwrap();
            proto.signed_header.unwrap().header.unwrap().time
        };
        assert!(header_time(&present).is_some());
        assert!(header_time(&omitted).is_none());

        type Run = fn(&[u8], u64, u64) -> PrecompileResult;
        let runs: [Run; 3] = [
            cometbft_light_block_validation_run_before_hertz,
            cometbft_light_block_validation_run,
            cometbft_light_block_validation_run_pasteur,
        ];
        for run in runs {
            let present = run(&present, 10_000_000, 0).unwrap();
            let omitted = run(&omitted, 10_000_000, 0).unwrap();
            assert!(present.is_success(), "halted with {:?}", present.halt_reason());
            assert!(omitted.is_success(), "halted with {:?}", omitted.halt_reason());
            assert_eq!(omitted.bytes, present.bytes);
        }
    }

    /// Signs `msg` as Go/RFC 8032 would, then adds `torsion` to `R` (recomputing `k` and `S`
    /// over the modified `R`, so only the cofactor separates the two verifiers).
    fn sign_with_torsion(
        secret: Scalar,
        nonce: Scalar,
        torsion: EdwardsPoint,
        msg: &[u8],
    ) -> ([u8; 32], [u8; 64]) {
        use curve25519_dalek::constants::ED25519_BASEPOINT_POINT;

        let pubkey = (ED25519_BASEPOINT_POINT * secret).compress().to_bytes();
        let r = (ED25519_BASEPOINT_POINT * nonce + torsion).compress().to_bytes();
        let k = Scalar::from_bytes_mod_order_wide(
            &Sha512::new().chain_update(r).chain_update(pubkey).chain_update(msg).finalize().into(),
        );
        let s = nonce + k * secret;
        let mut sig = [0u8; 64];
        sig[..32].copy_from_slice(&r);
        sig[32..].copy_from_slice(s.as_bytes());
        (pubkey, sig)
    }

    #[test]
    fn go_ed25519_verify_matches_go_semantics() {
        use cometbft::crypto::{
            default::signature::Verifier as Zip215Verifier, signature::Verifier,
        };
        use curve25519_dalek::constants::EIGHT_TORSION;

        let msg = b"src-2276";
        let zip215 = |pubkey: &[u8; 32], sig: &[u8; 64]| {
            let pk = PublicKey::from_raw_ed25519(pubkey).unwrap();
            Zip215Verifier::verify(pk, msg, &Signature::new(sig).unwrap().unwrap()).is_ok()
        };
        let go = |pubkey: &[u8; 32], sig: &[u8; 64]| {
            let pk = PublicKey::from_raw_ed25519(pubkey).unwrap();
            GoEd25519Verifier::verify(pk, msg, &Signature::new(sig).unwrap().unwrap()).is_ok()
        };

        // An honest signature verifies under both.
        let (pubkey, sig) =
            sign_with_torsion(Scalar::from(7u64), Scalar::from(11u64), Default::default(), msg);
        assert!(go(&pubkey, &sig) && zip215(&pubkey, &sig));

        // Reporter's vectors: small-order A with canonical R is fine for Go; non-canonical R is
        // not.
        assert!(go(&IDENTITY, &identity_vote_signature(0x00)));
        assert!(zip215(&IDENTITY, &identity_vote_signature(0x80)));
        assert!(!go(&IDENTITY, &identity_vote_signature(0x80)));

        // A canonical R with a small-order component under an ordinary key: same divergence,
        // no exotic encoding needed.
        for torsion in &EIGHT_TORSION[1..] {
            let (pubkey, sig) =
                sign_with_torsion(Scalar::from(7u64), Scalar::from(11u64), *torsion, msg);
            assert!(zip215(&pubkey, &sig));
            assert!(!go(&pubkey, &sig));
        }

        // S must be canonical (S + L is rejected).
        let (pubkey, mut sig) =
            sign_with_torsion(Scalar::from(7u64), Scalar::from(11u64), Default::default(), msg);
        let l = hex!("edd3f55c1a631258d69cf7a2def9de1400000000000000000000000000000010");
        let mut carry = 0u16;
        for i in 0..32 {
            let v = sig[32 + i] as u16 + l[i] as u16 + carry;
            sig[32 + i] = v as u8;
            carry = v >> 8;
        }
        assert!(!go(&pubkey, &sig));
    }

    /// Rebuild the valid fixture with its commit signatures changed by `f`.
    fn with_commit_sigs(
        mut f: impl FnMut(&mut Vec<cometbft_proto::types::v1::CommitSig>),
    ) -> Vec<u8> {
        modify_fixture_light_block(|proto| {
            f(&mut proto.signed_header.as_mut().unwrap().commit.as_mut().unwrap().signatures)
        })
    }

    /// SRC-1850: go-bsc verifies signature `i` under validator `i`. Swapping two complete
    /// `CommitSig`s keeps every signature valid for its embedded address, but go-bsc rejects it.
    #[test]
    fn pasteur_rejects_reordered_commit_sigs() {
        let unchanged = with_commit_sigs(|_| {});
        let output =
            cometbft_light_block_validation_run_pasteur(&unchanged, 10_000_000, 0).unwrap();
        assert!(output.is_success());

        let swapped = with_commit_sigs(|sigs| sigs.swap(0, 1));
        let output = cometbft_light_block_validation_run_pasteur(&swapped, 10_000_000, 0).unwrap();
        assert!(output.is_halt(), "reordered commit signatures must be rejected");
    }

    /// go-bsc never reads the `CommitSig` address when counting the new validator set, so a
    /// valid signature carrying a wrong or repeated address still counts.
    #[test]
    fn pasteur_ignores_commit_sig_addresses_like_go() {
        let expected =
            cometbft_light_block_validation_run_pasteur(&with_commit_sigs(|_| {}), 10_000_000, 0)
                .unwrap();

        let wrong_address = with_commit_sigs(|sigs| sigs[2].validator_address = vec![0x77; 20]);
        let duplicate_address = with_commit_sigs(|sigs| {
            sigs[1].validator_address = sigs[0].validator_address.clone();
        });
        for input in [wrong_address, duplicate_address] {
            let output =
                cometbft_light_block_validation_run_pasteur(&input, 10_000_000, 0).unwrap();
            assert!(output.is_success());
            assert_eq!(output.bytes, expected.bytes);
        }
    }

    /// go-bsc skips nil votes without verifying them, so a nil vote with a junk signature is
    /// ignored and the other three validators are still a quorum.
    #[test]
    fn pasteur_skips_nil_vote_with_junk_signature_like_go() {
        use cometbft_proto::types::v1::BlockIdFlag;

        let (input, expected) = runtime_signed_light_block(
            |_| {},
            |sigs| {
                sigs[0].block_id_flag = BlockIdFlag::Nil as i32;
                sigs[0].signature = vec![0xab; 64];
            },
        );
        let output = cometbft_light_block_validation_run_pasteur(&input, 10_000_000, 0).unwrap();
        assert!(output.is_success(), "halted with {:?}", output.halt_reason());
        assert_eq!(output.bytes.as_ref(), expected.as_slice());
    }

    /// A validator set that is not in cometbft-rs sort order: signatures bind to validators by
    /// position, and go-bsc keeps the wire order when re-encoding the consensus state.
    #[test]
    fn pasteur_keeps_unsorted_validator_set_in_wire_order() {
        let (input, expected) = runtime_signed_light_block(|signers| signers.reverse(), |_| {});
        let output = cometbft_light_block_validation_run_pasteur(&input, 10_000_000, 0).unwrap();
        assert!(output.is_success(), "halted with {:?}", output.halt_reason());
        assert_eq!(output.bytes.as_ref(), expected.as_slice());
    }

    /// Known answer from greenfield-cometbft v1.3.2 `ValidatorSet.Hash` for this decoded set, built
    /// directly in this order (not via `NewValidatorSet`, which sorts it; sorted, the hash is
    /// e57bd52c...). The public keys are filler bytes.
    #[test]
    fn trusted_validator_set_hash_matches_go_wire_order() {
        let validator = |fill: u8, power: u32| {
            Validator::new_with_bls_and_relayer(
                PublicKey::from_raw_ed25519(&[fill; 32]).unwrap(),
                Power::from(power),
                vec![fill + 0x20; 48],
                vec![fill + 0x10; 20],
            )
        };
        let validators = [validator(0xa1, 10), validator(0xa2, 30), validator(0xa3, 20)];
        assert_eq!(
            trusted_validator_set_hash(&validators),
            hex!("94394bfbba91945d64638bc905fc5c513b3f0583860dc3503160ec32f554b195"),
        );
    }
}
