use std::sync::Arc;
use std::time::SystemTime;
use lazy_static::lazy_static;
use std::sync::RwLock;

use schnellru::LruMap;
use schnellru::ByLength;
use alloy_primitives::{Address, B256};
use secp256k1::{SECP256K1, Message, ecdsa::{RecoveryId, RecoverableSignature}};
use crate::hardforks::BscHardforks;
use reth_chainspec::EthChainSpec;
use alloy_consensus::{Header, BlockHeader};
use alloy_rlp::Decodable;
use super::{
    VoteAttestation, ParliaConsensusError, VoteAddress,
    constants::{
        EXTRA_VANITY, EXTRA_SEAL, VALIDATOR_NUMBER_SIZE, 
        VALIDATOR_BYTES_LEN_AFTER_LUBAN, VALIDATOR_BYTES_LEN_BEFORE_LUBAN, TURN_LENGTH_SIZE,
        EXTRA_VANITY_LEN, EXTRA_SEAL_LEN, EXTRA_VANITY_LEN_WITH_VALIDATOR_NUM,
        EXTRA_VALIDATOR_LEN, EXTRA_VALIDATOR_LEN_BEFORE_LUBAN
    },
    hash_with_chain_id,
    provider::ValidatorsInfo
};

const RECOVERED_PROPOSER_CACHE_NUM: usize = 4096;
const ADDRESS_LENGTH: usize = 20; // Ethereum address length in bytes

lazy_static! {
    // recovered proposer cache map by block_number: proposer_address
    static ref RECOVERED_PROPOSER_CACHE: RwLock<LruMap<B256, Address, ByLength>> = RwLock::new(LruMap::new(ByLength::new(RECOVERED_PROPOSER_CACHE_NUM as u32)));
}

#[derive(Debug)]
pub struct Parlia<ChainSpec> {
    pub spec: Arc<ChainSpec>,
    pub epoch: u64, // The epoch number
    // period: u64, // The period of block proposal
}

impl<ChainSpec> Parlia<ChainSpec> 
where ChainSpec: EthChainSpec + BscHardforks + 'static, 
{
    pub fn new(chain_spec: Arc<ChainSpec>, epoch: u64) -> Self {
        Self { spec: chain_spec, epoch }
    }

    /// Get chain spec
    pub fn chain_spec(&self) -> &ChainSpec {
        &self.spec
    }

    /// Get epoch length from header
    pub fn get_epoch_length(&self, header: &Header) -> u64 {
        if self.spec.is_maxwell_active_at_timestamp(header.timestamp()) {
            return crate::consensus::parlia::snapshot::MAXWELL_EPOCH_LENGTH;
        }
        if self.spec.is_lorentz_active_at_timestamp(header.timestamp()) {
            return crate::consensus::parlia::snapshot::LORENTZ_EPOCH_LENGTH;
        }
        self.epoch
    }

    /// Get validator bytes from header extra data
    pub fn get_validator_bytes_from_header(&self, header: &Header) -> Option<Vec<u8>> {
        let extra_len = header.extra_data.len();
        if extra_len <= EXTRA_VANITY + EXTRA_SEAL {
            return None;
        }

        let is_luban_active = self.spec.is_luban_active_at_block(header.number);
        let is_epoch = header.number % self.get_epoch_length(header) == 0;

        if is_luban_active {
            if !is_epoch {
                return None;
            }

            let count = header.extra_data[EXTRA_VANITY] as usize;
            let start = EXTRA_VANITY+VALIDATOR_NUMBER_SIZE;
            let end = start + count * VALIDATOR_BYTES_LEN_AFTER_LUBAN;

            let mut extra_min_len = end + EXTRA_SEAL;
            let is_bohr_active = self.spec.is_bohr_active_at_timestamp(header.timestamp);
            if is_bohr_active {
                extra_min_len += TURN_LENGTH_SIZE;
            }
            if count == 0 || extra_len < extra_min_len {
                return None
            }
            Some(header.extra_data[start..end].to_vec())
        } else {
            if is_epoch &&
                (extra_len - EXTRA_VANITY - EXTRA_SEAL) %
                VALIDATOR_BYTES_LEN_BEFORE_LUBAN !=
                    0
            {
                return None;
            }

            Some(header.extra_data[EXTRA_VANITY..extra_len - EXTRA_SEAL].to_vec())
        }
    }

    /// Get turn length from header
    pub fn get_turn_length_from_header(&self, header: &Header) -> Result<Option<u8>, ParliaConsensusError> {
        if header.number % self.get_epoch_length(header) != 0 ||
            !self.spec.is_bohr_active_at_timestamp(header.timestamp)
        {
            return Ok(None);
        }

        if header.extra_data.len() <= EXTRA_VANITY + EXTRA_SEAL {
            return Err(ParliaConsensusError::InvalidHeaderExtraLen {
                header_extra_len: header.extra_data.len() as u64,
            });
        }

        let num = header.extra_data[EXTRA_VANITY] as usize;
        let pos = EXTRA_VANITY + 1 + num * VALIDATOR_BYTES_LEN_AFTER_LUBAN;

        if header.extra_data.len() <= pos {
            return Err(ParliaConsensusError::ExtraInvalidTurnLength);
        }

        let turn_length = header.extra_data[pos];
        Ok(Some(turn_length))
    }

    /// Get vote attestation from header
    pub fn get_vote_attestation_from_header(&self, header: &Header) -> Result<Option<VoteAttestation>, ParliaConsensusError> {
        let extra_len = header.extra_data.len();
        if extra_len <= EXTRA_VANITY + EXTRA_SEAL {
            return Ok(None);
        }

        if !self.spec.is_luban_active_at_block(header.number()) {
            return Ok(None);
        }

        let mut raw_attestation_data = if header.number() % self.get_epoch_length(header) != 0 {
            &header.extra_data[EXTRA_VANITY..extra_len - EXTRA_SEAL]
        } else {
            let validator_count =
                header.extra_data[EXTRA_VANITY + VALIDATOR_NUMBER_SIZE - 1] as usize;
            let mut start =
                EXTRA_VANITY + VALIDATOR_NUMBER_SIZE + validator_count * VALIDATOR_BYTES_LEN_AFTER_LUBAN;
            let is_bohr_active = self.spec.is_bohr_active_at_timestamp(header.timestamp);
            if is_bohr_active {
                start += TURN_LENGTH_SIZE;
            }
            let end = extra_len - EXTRA_SEAL;
            if end <= start {
                return Ok(None)
            }
            &header.extra_data[start..end]
        };
        if raw_attestation_data.is_empty() {
            return Ok(None);
        }

        Ok(Some(
            Decodable::decode(&mut raw_attestation_data)
                .map_err(|_| ParliaConsensusError::ABIDecodeInnerError)?,
        ))
    }

    pub fn recover_proposer(&self, header: &Header) -> Result<Address, ParliaConsensusError> {
        let hash = header.hash_slow();
        
        { // Check cache first
            let mut cache = RECOVERED_PROPOSER_CACHE.write().unwrap();
            if let Some(proposer) = cache.get(&hash) {
                return Ok(*proposer);
            }
        }

        let extra_data = &header.extra_data;
        if extra_data.len() < EXTRA_VANITY + EXTRA_SEAL {
            return Err(ParliaConsensusError::ExtraSignatureMissing);
        }

        let signature_offset = extra_data.len() - EXTRA_SEAL;
        let recovery_byte = extra_data[signature_offset + EXTRA_SEAL - 1] as i32;
        let signature_bytes = &extra_data[signature_offset..signature_offset + EXTRA_SEAL - 1];

        let recovery_id = RecoveryId::try_from(recovery_byte)
            .map_err(|_| ParliaConsensusError::RecoverECDSAInnerError)?;
        let signature = RecoverableSignature::from_compact(signature_bytes, recovery_id)
            .map_err(|_| ParliaConsensusError::RecoverECDSAInnerError)?;

        let message = Message::from_digest_slice(
                            hash_with_chain_id(header, self.spec.chain().id()).as_slice(),
        )
        .map_err(|_| ParliaConsensusError::RecoverECDSAInnerError)?;
        let public = &SECP256K1
            .recover_ecdsa(&message, &signature)
            .map_err(|_| ParliaConsensusError::RecoverECDSAInnerError)?;

        let proposer =
            Address::from_slice(&alloy_primitives::keccak256(&public.serialize_uncompressed()[1..])[12..]);
        
        { // Update cache
            let mut cache = RECOVERED_PROPOSER_CACHE.write().unwrap();
            cache.insert(hash, proposer);
        }
        
        Ok(proposer)
    }
    
    pub fn present_timestamp(&self) -> u64 {
        SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs()
    }

    fn get_validator_len_from_header(
        &self,
        header: &Header,
    ) -> Result<usize, ParliaConsensusError> {
        if header.number % self.epoch != 0 {
            return Ok(0);
        }

        let extra_len = header.extra_data.len();

        if !self.spec.is_luban_active_at_block(header.number) {
            return Ok(extra_len - EXTRA_VANITY_LEN - EXTRA_SEAL_LEN);
        }

        let count = header.extra_data[EXTRA_VANITY_LEN_WITH_VALIDATOR_NUM - 1] as usize;
        Ok(count * EXTRA_VALIDATOR_LEN)
    }

    fn check_header_extra_len(&self, header: &Header) -> Result<(), ParliaConsensusError> {
        let extra_len = header.extra_data.len();
        if extra_len < EXTRA_VANITY_LEN {
            return Err(ParliaConsensusError::ExtraVanityMissing);
        }
        if extra_len < EXTRA_VANITY_LEN + EXTRA_SEAL_LEN {
            return Err(ParliaConsensusError::ExtraSignatureMissing);
        }

        if header.number % self.epoch != 0 {
            return Ok(());
        }

        if self.spec.is_luban_active_at_block(header.number) {
            let count = header.extra_data[EXTRA_VANITY_LEN_WITH_VALIDATOR_NUM - 1] as usize;
            let expect =
                EXTRA_VANITY_LEN_WITH_VALIDATOR_NUM + EXTRA_SEAL_LEN + count * EXTRA_VALIDATOR_LEN;
            if count == 0 || extra_len < expect {
                tracing::warn!("Invalid header extra len, block_number: {}, extra_len: {}, expect: {}, count: {}", header.number, extra_len, expect, count);
                return Err(ParliaConsensusError::InvalidHeaderExtraLen {
                    header_extra_len: extra_len as u64,
                });
            }
        } else {
            let validator_bytes_len = extra_len - EXTRA_VANITY_LEN - EXTRA_SEAL_LEN;
            if validator_bytes_len / EXTRA_VALIDATOR_LEN_BEFORE_LUBAN == 0 ||
                validator_bytes_len % EXTRA_VALIDATOR_LEN_BEFORE_LUBAN != 0
            {
                return Err(ParliaConsensusError::InvalidHeaderExtraLen {
                    header_extra_len: extra_len as u64,
                });
            }
        }

        Ok(())
    }

    pub fn check_header_extra(&self, header: &Header) -> Result<(), ParliaConsensusError> {
        self.check_header_extra_len(header)?;

        let is_epoch = header.number % self.get_epoch_length(header) == 0;
        let validator_bytes_len = self.get_validator_len_from_header(header)?;
        if (!is_epoch && validator_bytes_len != 0) || (is_epoch && validator_bytes_len == 0) {
            return Err(ParliaConsensusError::InvalidHeaderExtraValidatorBytesLen {
                is_epoch,
                validator_bytes_len,
            });
        }

        Ok(())
    }

    pub fn parse_validators_from_header(
        &self,
        header: &Header,
    ) -> Result<ValidatorsInfo, ParliaConsensusError> {
        let val_bytes = self.get_validator_bytes_from_header(header).ok_or_else(|| {
            ParliaConsensusError::InvalidHeaderExtraLen {
                header_extra_len: header.extra_data.len() as u64,
            }
        })?;

        if val_bytes.is_empty() {
            return Err(ParliaConsensusError::InvalidHeaderExtraValidatorBytesLen {
                is_epoch: true,
                validator_bytes_len: 0,
            })
        }

        if self.spec.is_luban_active_at_block(header.number) {
            self.parse_validators_after_luban(&val_bytes)
        } else {
            self.parse_validators_before_luban(&val_bytes)
        }
    }

    fn parse_validators_after_luban(
        &self,
        validator_bytes: &[u8],
    ) -> Result<ValidatorsInfo, ParliaConsensusError> {
        let count = validator_bytes.len() / EXTRA_VALIDATOR_LEN;
        let mut consensus_addrs = Vec::with_capacity(count);
        let mut vote_addrs = Vec::with_capacity(count);

        for i in 0..count {
            let consensus_start = i * EXTRA_VALIDATOR_LEN;
            let consensus_end = consensus_start + ADDRESS_LENGTH;
            let consensus_address =
                Address::from_slice(&validator_bytes[consensus_start..consensus_end]);
            consensus_addrs.push(consensus_address);

            let vote_start = consensus_start + ADDRESS_LENGTH;
            let vote_end = consensus_start + EXTRA_VALIDATOR_LEN;
            let vote_address = VoteAddress::from_slice(&validator_bytes[vote_start..vote_end]);
            vote_addrs.push(vote_address);
        }

        Ok(ValidatorsInfo { consensus_addrs, vote_addrs: Some(vote_addrs) })
    }

    fn parse_validators_before_luban(
        &self,
        validator_bytes: &[u8],
    ) -> Result<ValidatorsInfo, ParliaConsensusError> {
        let count = validator_bytes.len() / EXTRA_VALIDATOR_LEN_BEFORE_LUBAN;
        let mut consensus_addrs = Vec::with_capacity(count);

        for i in 0..count {
            let start = i * EXTRA_VALIDATOR_LEN_BEFORE_LUBAN;
            let end = start + EXTRA_VALIDATOR_LEN_BEFORE_LUBAN;
            let address = Address::from_slice(&validator_bytes[start..end]);
            consensus_addrs.push(address);
        }

        Ok(ValidatorsInfo { consensus_addrs, vote_addrs: None })
    }

}
