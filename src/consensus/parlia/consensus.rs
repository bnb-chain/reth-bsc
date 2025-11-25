use std::sync::Arc;
use std::time::SystemTime;
use lazy_static::lazy_static;
use std::sync::RwLock;

use schnellru::LruMap;
use schnellru::ByLength;
use alloy_primitives::{Address, B256};
use secp256k1::{SECP256K1, Message, ecdsa::{RecoveryId, RecoverableSignature}};
use crate::node::evm::pre_execution::TURN_LENGTH_CACHE;
use crate::consensus::parlia::util::is_breathe_block;
use crate::consensus::parlia::vote_pool::fetch_vote_by_block_hash;
use crate::consensus::parlia::VoteData;
use crate::consensus::parlia::VoteSignature;
use crate::consensus::parlia::SYSTEM_TXS_GAS_HARD_LIMIT;
use crate::consensus::parlia::SYSTEM_TXS_GAS_SOFT_LIMIT;
use crate::hardforks::BscHardforks;
use reth_chainspec::EthChainSpec;
use alloy_consensus::{Header, BlockHeader};
use alloy_rlp::Decodable;
use super::{
    VoteAttestation, ParliaConsensusError, VoteAddress, Snapshot,
    constants::{
        EXTRA_VANITY_LEN, EXTRA_SEAL_LEN, VALIDATOR_NUMBER_SIZE, 
        VALIDATOR_BYTES_LEN_AFTER_LUBAN, VALIDATOR_BYTES_LEN_BEFORE_LUBAN, TURN_LENGTH_SIZE,
    },
    hash_with_chain_id,
    provider::ValidatorsInfo,
    util::set_millisecond_part_of_timestamp,
    BACKOFF_TIME_OF_INITIAL, BACKOFF_TIME_OF_WIGGLE, DEFAULT_TURN_LENGTH,LORENTZ_BACKOFF_TIME_OF_INITIAL,
};
use crate::consensus::parlia::go_rng::{RngSource, Shuffle};
use tracing::{trace, debug, warn};
use crate::consensus::parlia::constants::K_ANCESTOR_GENERATION_DEPTH;
use crate::consensus::parlia::provider::SnapshotProvider;

const RECOVERED_PROPOSER_CACHE_NUM: usize = 4096;
const ADDRESS_LENGTH: usize = 20; // Ethereum address length in bytes

lazy_static! {
    // recovered proposer cache map by block_number: proposer_address
    static ref RECOVERED_PROPOSER_CACHE: RwLock<LruMap<B256, Address, ByLength>> = RwLock::new(LruMap::new(ByLength::new(RECOVERED_PROPOSER_CACHE_NUM as u32)));
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Parlia<ChainSpec> {
    pub spec: Arc<ChainSpec>,
    pub epoch: u64, // The epoch number
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
        if self.spec.is_maxwell_active_at_timestamp(header.number(), header.timestamp()) {
            return crate::consensus::parlia::snapshot::MAXWELL_EPOCH_LENGTH;
        }
        if self.spec.is_lorentz_active_at_timestamp(header.number(), header.timestamp()) {
            return crate::consensus::parlia::snapshot::LORENTZ_EPOCH_LENGTH;
        }
        self.epoch
    }

    /// Get validator bytes from header extra data
    pub fn get_validator_bytes_from_header(&self, header: &Header, epoch_length: u64) -> Option<Vec<u8>> {
        let extra_len = header.extra_data.len();
        if extra_len <= EXTRA_VANITY_LEN + EXTRA_SEAL_LEN {
            return None;
        }

        let is_luban_active = self.spec.is_luban_active_at_block(header.number);
        let is_epoch = header.number.is_multiple_of(epoch_length);

        if is_luban_active {
            if !is_epoch {
                return None;
            }

            let count = header.extra_data[EXTRA_VANITY_LEN] as usize;
            let start = EXTRA_VANITY_LEN + VALIDATOR_NUMBER_SIZE;
            let end = start + count * VALIDATOR_BYTES_LEN_AFTER_LUBAN;

            let mut extra_min_len = end + EXTRA_SEAL_LEN;
            let is_bohr_active = self.spec.is_bohr_active_at_timestamp(header.number, header.timestamp);
            if is_bohr_active {
                extra_min_len += TURN_LENGTH_SIZE;
            }
            if count == 0 || extra_len < extra_min_len {
                return None
            }
            Some(header.extra_data[start..end].to_vec())
        } else {
            if is_epoch &&
                !(extra_len - EXTRA_VANITY_LEN - EXTRA_SEAL_LEN).is_multiple_of(VALIDATOR_BYTES_LEN_BEFORE_LUBAN)
            {
                return None;
            }

            Some(header.extra_data[EXTRA_VANITY_LEN..extra_len - EXTRA_SEAL_LEN].to_vec())
        }
    }

    /// Get turn length from header
    pub fn get_turn_length_from_header(&self, header: &Header, epoch_length: u64) -> Result<Option<u8>, ParliaConsensusError> {
        if !header.number.is_multiple_of(epoch_length) ||
            !self.spec.is_bohr_active_at_timestamp(header.number, header.timestamp)
        {
            return Ok(None);
        }

        if header.extra_data.len() <= EXTRA_VANITY_LEN + EXTRA_SEAL_LEN {
            return Err(ParliaConsensusError::InvalidHeaderExtraLen {
                header_extra_len: header.extra_data.len() as u64,
            });
        }

        let num = header.extra_data[EXTRA_VANITY_LEN] as usize;
        let pos = EXTRA_VANITY_LEN + 1 + num * VALIDATOR_BYTES_LEN_AFTER_LUBAN;

        if header.extra_data.len() <= pos {
            return Err(ParliaConsensusError::ExtraInvalidTurnLength);
        }

        let turn_length = header.extra_data[pos];
        Ok(Some(turn_length))
    }

    /// Get vote attestation from header
    pub fn get_vote_attestation_from_header(&self, header: &Header, epoch_length: u64) -> Result<Option<VoteAttestation>, ParliaConsensusError> {
        let extra_len = header.extra_data.len();
        if extra_len <= EXTRA_VANITY_LEN + EXTRA_SEAL_LEN {
            return Ok(None);
        }

        if !self.spec.is_luban_active_at_block(header.number()) {
            return Ok(None);
        }

        let mut raw_attestation_data = if !header.number().is_multiple_of(epoch_length) {
            &header.extra_data[EXTRA_VANITY_LEN..extra_len - EXTRA_SEAL_LEN]
        } else {
            let validator_count =
                header.extra_data[EXTRA_VANITY_LEN + VALIDATOR_NUMBER_SIZE - 1] as usize;
            let mut start =
                EXTRA_VANITY_LEN + VALIDATOR_NUMBER_SIZE + validator_count * VALIDATOR_BYTES_LEN_AFTER_LUBAN;
            let is_bohr_active = self.spec.is_bohr_active_at_timestamp(header.number, header.timestamp);
            if is_bohr_active {
                start += TURN_LENGTH_SIZE;
            }
            let end = extra_len - EXTRA_SEAL_LEN;
            if end <= start {
                return Ok(None)
            }
            &header.extra_data[start..end]
        };
        if raw_attestation_data.is_empty() {
            return Ok(None);
        }
        tracing::trace!("try debug attestation data, attestation_data_len: {:?}, header_number: {:?}", 
            raw_attestation_data.len(), header.number());

        Ok(Some(
            Decodable::decode(&mut raw_attestation_data)
                .map_err(|_| ParliaConsensusError::ExtraInvalidAttestation)?,
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
        if extra_data.len() < EXTRA_VANITY_LEN + EXTRA_SEAL_LEN {
            return Err(ParliaConsensusError::ExtraSignatureMissing);
        }

        let signature_offset = extra_data.len() - EXTRA_SEAL_LEN;
        let recovery_byte = extra_data[signature_offset + EXTRA_SEAL_LEN - 1] as i32;
        let signature_bytes = &extra_data[signature_offset..signature_offset + EXTRA_SEAL_LEN - 1];

        let recovery_id = RecoveryId::try_from(recovery_byte)
            .map_err(|err| {
                tracing::error!("Failed to create RecoveryId from recovery_byte {}: {}", recovery_byte, err);
                ParliaConsensusError::RecoverECDSAInnerError
            })?;
        let signature = RecoverableSignature::from_compact(signature_bytes, recovery_id)
            .map_err(|err| {
                tracing::error!("Failed to recover signature from signature_bytes (len={}), recovery_id={}: {}", signature_bytes.len(), recovery_byte, err);
                ParliaConsensusError::RecoverECDSAInnerError
            })?;

        let message = Message::from_digest_slice(
                            hash_with_chain_id(header, self.spec.chain().id()).as_slice(),
        )
        .map_err(|err| {
            tracing::error!("Failed to create Message from hash digest: {}", err);
            ParliaConsensusError::RecoverECDSAInnerError
        })?;
        let public = &SECP256K1
            .recover_ecdsa(&message, &signature)
            .map_err(|err| {
                tracing::error!("Failed to recover ECDSA public key from message and signature: {}", err);
                ParliaConsensusError::RecoverECDSAInnerError
            })?;

        let proposer =
            Address::from_slice(&alloy_primitives::keccak256(&public.serialize_uncompressed()[1..])[12..]);
        
        { // Update cache
            let mut cache = RECOVERED_PROPOSER_CACHE.write().unwrap();
            cache.insert(hash, proposer);
        }
        
        Ok(proposer)
    }
    
    pub fn present_millis_timestamp(&self) -> u64 {
        SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_millis() as u64
    }

    fn get_validator_len_from_header(
        &self,
        header: &Header,
    ) -> Result<usize, ParliaConsensusError> {
        if !header.number.is_multiple_of(self.get_epoch_length(header)) {
            return Ok(0);
        }

        let extra_len = header.extra_data.len();

        if !self.spec.is_luban_active_at_block(header.number) {
            return Ok(extra_len - EXTRA_VANITY_LEN - EXTRA_SEAL_LEN);
        }

        let count = header.extra_data[EXTRA_VANITY_LEN + VALIDATOR_NUMBER_SIZE - 1] as usize;
        Ok(count * VALIDATOR_BYTES_LEN_AFTER_LUBAN)
    }

    fn check_header_extra_len(&self, header: &Header) -> Result<(), ParliaConsensusError> {
        let extra_len = header.extra_data.len();
        if extra_len < EXTRA_VANITY_LEN {
            return Err(ParliaConsensusError::ExtraVanityMissing);
        }
        if extra_len < EXTRA_VANITY_LEN + EXTRA_SEAL_LEN {
            return Err(ParliaConsensusError::ExtraSignatureMissing);
        }

        if !header.number.is_multiple_of(self.get_epoch_length(header)) {
            return Ok(());
        }

        if self.spec.is_luban_active_at_block(header.number) {
            let count = header.extra_data[EXTRA_VANITY_LEN + VALIDATOR_NUMBER_SIZE - 1] as usize;
            let expect =
            EXTRA_VANITY_LEN + VALIDATOR_NUMBER_SIZE + EXTRA_SEAL_LEN + count * VALIDATOR_BYTES_LEN_AFTER_LUBAN;
            if count == 0 || extra_len < expect {
                tracing::warn!("Invalid header extra len, block_number: {}, extra_len: {}, expect: {}, count: {}, epoch_length: {}", 
                    header.number, extra_len, expect, count, self.get_epoch_length(header));
                return Err(ParliaConsensusError::InvalidHeaderExtraLen {
                    header_extra_len: extra_len as u64,
                });
            }
        } else {
            let validator_bytes_len = extra_len - EXTRA_VANITY_LEN - EXTRA_SEAL_LEN;
            if validator_bytes_len / VALIDATOR_BYTES_LEN_BEFORE_LUBAN == 0 ||
                !validator_bytes_len.is_multiple_of(VALIDATOR_BYTES_LEN_BEFORE_LUBAN)
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

        let is_epoch = header.number.is_multiple_of(self.get_epoch_length(header));
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
        epoch_length: u64,
    ) -> Result<ValidatorsInfo, ParliaConsensusError> {
        let val_bytes = self.get_validator_bytes_from_header(header, epoch_length).ok_or_else(|| {
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
        let count = validator_bytes.len() / VALIDATOR_BYTES_LEN_AFTER_LUBAN;
        let mut consensus_addrs = Vec::with_capacity(count);
        let mut vote_addrs = Vec::with_capacity(count);

        for i in 0..count {
            let consensus_start = i * VALIDATOR_BYTES_LEN_AFTER_LUBAN;
            let consensus_end = consensus_start + ADDRESS_LENGTH;
            let consensus_address =
                Address::from_slice(&validator_bytes[consensus_start..consensus_end]);
            consensus_addrs.push(consensus_address);

            let vote_start = consensus_start + ADDRESS_LENGTH;
            let vote_end = consensus_start + VALIDATOR_BYTES_LEN_AFTER_LUBAN;
            let vote_address = VoteAddress::from_slice(&validator_bytes[vote_start..vote_end]);
            vote_addrs.push(vote_address);
        }

        Ok(ValidatorsInfo { consensus_addrs, vote_addrs: Some(vote_addrs) })
    }

    fn parse_validators_before_luban(
        &self,
        validator_bytes: &[u8],
    ) -> Result<ValidatorsInfo, ParliaConsensusError> {
        let count = validator_bytes.len() / VALIDATOR_BYTES_LEN_BEFORE_LUBAN;
        let mut consensus_addrs = Vec::with_capacity(count);

        for i in 0..count {
            let start = i * VALIDATOR_BYTES_LEN_BEFORE_LUBAN;
            let end = start + VALIDATOR_BYTES_LEN_BEFORE_LUBAN;
            let address = Address::from_slice(&validator_bytes[start..end]);
            consensus_addrs.push(address);
        }

        Ok(ValidatorsInfo { consensus_addrs, vote_addrs: None })
    }

    /// return the back off milliseconds of the validator, 0 if the validator is in turn.
    pub fn back_off_time(&self, snap: &Snapshot, parent: &Header, header: &Header) -> u64 {
        let validator = header.beneficiary;
        
        tracing::trace!(
            target: "bsc::consensus::back_off_time",
            block_number = header.number,
            validator = ?validator,
            snapshot_block = snap.block_number,
            recent_proposers_count = snap.recent_proposers.len(),
            recent_proposers = ?snap.recent_proposers,
            "Calculating back_off_time"
        );
        
        if snap.is_inturn(validator) {
            return 0;
        }

        let mut delay = BACKOFF_TIME_OF_INITIAL;
        let is_parent_lorentz = self.spec.is_lorentz_active_at_timestamp(parent.number, parent.timestamp);
        if is_parent_lorentz {
            delay = LORENTZ_BACKOFF_TIME_OF_INITIAL;
        }
        let mut validators = snap.validators.clone();

        if self.spec.is_planck_active_at_block(header.number) {
            let counts = snap.count_recent_proposers();
            
            tracing::trace!(
                target: "bsc::consensus::back_off_time",
                block_number = header.number,
                counts = ?counts,
                "Counted recent proposers"
            );
            
            if snap.sign_recently_by_counts(validator, &counts) {
                // The backOffTime does not matter when a validator has signed recently.
                return 0;
            }

            let inturn_addr = snap.inturn_validator();
            if snap.sign_recently_by_counts(inturn_addr, &counts) {
                trace!(
                    "in turn validator({:?}) has recently signed, skip initialBackOffTime",
                    inturn_addr
                );
                delay = 0
            }

            // Exclude the recently signed validators and inturn validator
            validators.retain(|addr| {
                !(snap.sign_recently_by_counts(*addr, &counts) ||
                    self.spec.is_bohr_active_at_timestamp(header.number, header.timestamp) &&
                        *addr == inturn_addr)
            });
            
            tracing::trace!(
                target: "bsc::consensus::back_off_time",
                block_number = header.number,
                validators_after_filter = ?validators,
                validators_count = validators.len(),
                "Filtered validators"
            );
        }

        let is_bohr = self.spec.is_bohr_active_at_timestamp(header.number, header.timestamp);
        let mut rng = if is_bohr {
            let turn_length = snap.turn_length.unwrap_or(DEFAULT_TURN_LENGTH);
            let seed = header.number as i64 / turn_length as i64;
            tracing::trace!(
                target: "bsc::consensus::back_off_time",
                block_number = header.number,
                header_timestamp = header.timestamp,
                parent_timestamp = parent.timestamp,
                is_bohr = true,
                turn_length = turn_length,
                seed = seed,
                "RNG seed (Bohr)"
            );
            RngSource::new(seed)
        } else {
            let seed = snap.block_number as i64;
            tracing::trace!(
                target: "bsc::consensus::back_off_time",
                block_number = header.number,
                header_timestamp = header.timestamp,
                parent_timestamp = parent.timestamp,
                is_bohr = false,
                seed = seed,
                "RNG seed (pre-Bohr)"
            );
            RngSource::new(seed)
        };

        let mut back_off_steps: Vec<u64> = (0..validators.len() as u64).collect();
        tracing::trace!(
            target: "bsc::consensus::back_off_time",
            block_number = header.number,
            back_off_steps_before_shuffle = ?back_off_steps,
            "Before shuffle"
        );
        back_off_steps.shuffle(&mut rng);
        tracing::trace!(
            target: "bsc::consensus::back_off_time",
            block_number = header.number,
            back_off_steps_after_shuffle = ?back_off_steps,
            "After shuffle"
        );

        // get the index of the current validator and its shuffled backoff time.
        for (idx, val) in validators.iter().enumerate() {
            if *val == validator {
                let result = if delay == 0 && is_parent_lorentz {
                    // If the in-turn validator has signed recently, the expected backoff times are [0, 2, 3, ...].
                    if back_off_steps[idx] == 0 {
                        0
                    } else {
                        LORENTZ_BACKOFF_TIME_OF_INITIAL + (back_off_steps[idx]- 1) * BACKOFF_TIME_OF_WIGGLE
                    }
                } else {
                    delay + back_off_steps[idx] * BACKOFF_TIME_OF_WIGGLE
                };
                
                tracing::trace!(
                    target: "bsc::consensus::back_off_time",
                    block_number = header.number,
                    validator = ?validator,
                    idx = idx,
                    back_off_step = back_off_steps[idx],
                    result = result,
                    "Calculated back_off_time result"
                );
                
                return result;
            }
        }

        debug!("the validator is not authorized");
        0
    }

    /// - `snap.block_interval` is used as the period (milliseconds).
    /// - Applies `left_over_ms` reservation for finalization work.
    /// - Caps blocking time to half the period when last block in one turn (or tl == 1),
    ///   otherwise 4/5 of the period.
    pub fn delay_for_mining(
        &self,
        snap: &Snapshot,
        header: &Header,
        left_over_ms: u64,
    ) -> u64 {
        let period_ms = snap.block_interval;
        let mut delay_ms = self.delay_for_ramanujan_fork(snap, header);
        if left_over_ms >= period_ms {
            warn!("Delay invalid argument: left_over_ms={}, period_ms={}", left_over_ms, period_ms);
        } else if left_over_ms >= delay_ms {
            delay_ms = 0;
        } else {
            delay_ms -= left_over_ms;
        }

        let mut time_for_mining_ms = period_ms / 2;
        let last_block_in_turn = snap.last_block_in_one_turn(header.number);
        if !last_block_in_turn {
            time_for_mining_ms = period_ms;
        }
        if delay_ms > time_for_mining_ms {
            delay_ms = time_for_mining_ms;
        }

        delay_ms
    }

    pub fn prepare_timestamp(&self, parent_snap: &Snapshot, parent_header: &Header, new_header: &mut Header) {
        let millisecond_timestamp = self.block_time_for_ramanujan_fork(parent_snap, parent_header, new_header);
        new_header.timestamp = millisecond_timestamp / 1000;
        if self.spec.is_lorentz_active_at_timestamp(new_header.number, new_header.timestamp) {
            set_millisecond_part_of_timestamp(millisecond_timestamp, new_header);
        } else {
            new_header.mix_hash = B256::ZERO;
        }
    }

    pub fn prepare_validators(&self, snap: &Snapshot, validators: Option<(Vec<alloy_primitives::Address>, Vec<crate::consensus::parlia::VoteAddress>)>, new_header: &mut Header) {
        let epoch_length = snap.epoch_num;
        if !(new_header.number).is_multiple_of(epoch_length) {
            return;
        }
        let Some((mut new_validators, vote_addresses)) = validators else {
            return;
        };

        let mut extra_data = new_header.extra_data.to_vec();
        if !self.spec.is_luban_active_at_block(new_header.number) {
            // Pre-Luban: sort and append validator addresses directly to extra data
            new_validators.sort();
            for validator in &new_validators {
                extra_data.extend_from_slice(validator.as_slice());
            }
        } else {
            // Luban active: append validator count first, then validators with vote addresses
            extra_data.push(new_validators.len() as u8);
            
            let mut vote_map = std::collections::HashMap::new();
            if self.spec.is_luban_transition_at_block(new_header.number) {
                // On Luban transition block, use zero BLS keys for all validators
                let zero_bls_key = VoteAddress::ZERO;
                for validator in &new_validators {
                    vote_map.insert(*validator, zero_bls_key);
                }
            } else {
                for (i, validator) in new_validators.iter().enumerate() {
                    if i < vote_addresses.len() {
                        vote_map.insert(*validator, vote_addresses[i]);
                    } else {
                        vote_map.insert(*validator, VoteAddress::ZERO);
                    }
                }
            }
            
            new_validators.sort();
            for validator in &new_validators {
                extra_data.extend_from_slice(validator.as_slice());
                extra_data.extend_from_slice(vote_map.get(validator).unwrap().as_slice());
            }
        }
        new_header.extra_data = alloy_primitives::Bytes::from(extra_data);
    }

    pub fn prepare_turn_length(&self, parent_snap: &Snapshot, new_header: &mut Header) -> Result<(), ParliaConsensusError> {
        let epoch_length = parent_snap.epoch_num;
        if !new_header.number.is_multiple_of(epoch_length) || !self.spec.is_bohr_active_at_timestamp(new_header.number, new_header.timestamp) {
            return Ok(());
        }
        
        let mut cache = TURN_LENGTH_CACHE.lock().unwrap();
        let turn_length = *cache.get(&new_header.parent_hash).ok_or(ParliaConsensusError::TurnLengthNotFound {
            block_hash: new_header.parent_hash,
        })?;
        
        let mut extra_data = new_header.extra_data.to_vec();
        extra_data.push(turn_length);
        new_header.extra_data = alloy_primitives::Bytes::from(extra_data);

        Ok(())
    }

    pub fn estimate_gas_reserved_for_system_txs(&self, parent_timestamp: Option<u64>, current_number: u64, current_timestamp: u64) -> u64 {
        if let Some(parent_timestamp) = parent_timestamp {
            // Mainnet and Chapel have both passed Feynman. Now, simplify the logic before and during the Feynman hard fork.
            if self.spec.is_feynman_active_at_timestamp(current_number, current_timestamp) &&
                !self.spec.is_feynman_transition_at_timestamp(current_number, current_timestamp, parent_timestamp) &&
                 !is_breathe_block(parent_timestamp, current_timestamp) {
                // params.SystemTxsGasSoftLimit > (depositTxGas+slashTxGas+finalityRewardTxGas)*150/100
                return SYSTEM_TXS_GAS_SOFT_LIMIT;
            }
        }
        // params.SystemTxsGasHardLimit > (depositTxGas+slashTxGas+finalityRewardTxGas+updateValidatorTxGas)*150/100
        SYSTEM_TXS_GAS_HARD_LIMIT
    }
    
    pub fn assemble_vote_attestation(&self, parent_snap: &Snapshot, parent_header: &Header, current_header: &mut Header, snapshot_provider: &Arc<dyn SnapshotProvider + Send + Sync>) -> Result<(), ParliaConsensusError> {
        if !self.spec.is_luban_active_at_block(current_header.number()) || current_header.number() < 3 {
            return Ok(());
        }

        // get justified number and hash from parent snapshot
        let (justified_number, justified_hash) = (parent_snap.vote_data.target_number, parent_snap.vote_data.target_hash);
        let mut times = 1;
        if self.spec.is_fermi_active_at_timestamp(current_header.number(), current_header.timestamp()) {
            times = K_ANCESTOR_GENERATION_DEPTH;
        }
        let mut votes = Vec::new();
        let mut target_header = parent_header.clone();
        let mut target_header_parent_snap = None;
        for _ in 0..times {
            let snap = snapshot_provider.snapshot_by_hash(&target_header.parent_hash()).
                ok_or(ParliaConsensusError::SnapshotNotFound {
                    block_hash: target_header.parent_hash(),
                })?;
            votes = fetch_vote_by_block_hash(target_header.hash_slow());
            let quorum = usize::div_ceil(snap.validators.len() * 2, 3);
            if votes.len() >= quorum {
                target_header_parent_snap = Some(snap);
                break;
            }
            tracing::debug!(target: "parlia::consensus", "vote count is less than 2/3 of validators, skip assemble vote attestation, number={}, parent ={:?}, vote count={}, validators count={}", 
                target_header.number(), target_header.hash_slow(), votes.len(), parent_snap.validators.len());
            let block_hash = target_header.parent_hash();
            if let Some(header) = crate::shared::get_canonical_header_by_hash_from_provider(&block_hash) {
                target_header = header;
            } else {
                return Err(ParliaConsensusError::HeaderNotFound {
                    block_hash,
                });
            }
            if target_header.number() <= justified_number {
                break;
            }
        }
        if target_header_parent_snap.is_none() {
            tracing::warn!(target: "parlia::consensus", "cannot collect enough votes, current_block={}, target_header_number={}, justified_number={}", 
                current_header.number(), target_header.number(), justified_number);
            return Ok(());
        }

        let mut attestation = VoteAttestation::new_with_vote_data(VoteData {
            source_number: justified_number,
            source_hash: justified_hash,
            target_number: target_header.number(),
            target_hash: target_header.hash_slow(),
        });
        // Check vote data from votes
        for vote in votes.iter() {
            if vote.data.hash() != attestation.data.hash() {
                tracing::debug!(target: "parlia::consensus", "vote data hash mismatch, expected={:?}, got={:?}", attestation.data, vote);
                return Err(ParliaConsensusError::FetchVoteError {
                    address: vote.vote_address,
                });
            }
        }
        
        // Prepare aggregated vote signature and vote bitset
        let mut unique_by_addr: std::collections::HashMap<VoteAddress, VoteSignature> = std::collections::HashMap::new();
        for vote in votes.iter() {
            // Only keep votes from validators in parent snapshot
            if parent_snap.validators_map.values().any(|vi| vi.vote_addr == vote.vote_address) {
                // If the same address appears multiple times, keep the first one (stable and deterministic)
                unique_by_addr.entry(vote.vote_address).or_insert(vote.signature);
            } else {
                tracing::debug!(target: "parlia::consensus", "skip vote not in parent snapshot, vote_address={:?}", vote.vote_address);
            }
        }
        // Build a stable sequence based on the index of validators in parent snapshot (although BLS aggregation order is irrelevant, it is convenient for debugging and consistency)
        let mut ordered_unique: Vec<(u64, VoteAddress, VoteSignature)> = Vec::new();
        for (_, info) in parent_snap.validators_map.iter() {
            let vote_addr = info.vote_addr;
            if let Some(sig) = unique_by_addr.get(&vote_addr) {
                ordered_unique.push((info.index, vote_addr, *sig));
            }
        }
        // Must satisfy 2/3 threshold
        let validators_len = parent_snap.validators.len();
        let quorum = usize::div_ceil(validators_len * 2, 3);
        if ordered_unique.len() < quorum {
            tracing::debug!(target: "parlia::consensus", "not enough unique votes after filtering, have={}, need={}", ordered_unique.len(), quorum);
            return Ok(()); // If not enough unique votes, do not append attestation
        }
        // Aggregate signatures
        let sigs: Vec<blst::min_pk::Signature> = ordered_unique
            .iter()
            .map(|(_, _, sig)| blst::min_pk::Signature::from_bytes(sig.as_slice()).unwrap())
            .collect();
        let sigs_ref: Vec<&blst::min_pk::Signature> = sigs.iter().collect();
        let aggregate = blst::min_pk::AggregateSignature::aggregate(&sigs_ref, false)
            .map_err(|_| ParliaConsensusError::AggregateSignatureError)?;
        attestation.agg_signature.copy_from_slice(&aggregate.to_signature().to_bytes());
        // Build vote bitset (strictly equal to the number of aggregated signatures)
        for (index, vote_addr, _) in ordered_unique.iter() {
            // index in snapshot is 1-based, the vote bitset offset is index-1
            attestation.vote_address_set |= 1 << (index - 1);
            // Tolerance: if there are more than 64 validators in history, this needs to be changed to a larger bit set type; currently BSC limits 64
            let _ = vote_addr; // Only used for explanation
        }
        // Strict consistency check: the number of 1s in the vote bitset must be equal to the number of aggregated signatures
        let ones = attestation.vote_address_set.count_ones() as usize;
        if ones != ordered_unique.len() {
            tracing::debug!(target: "parlia::consensus", "vote bitset count mismatch, ones={}, sigs={}", ones, ordered_unique.len());
            return Err(ParliaConsensusError::InvalidAttestationVoteCount {
                got: ones as u32,
                expected: ordered_unique.len() as u32,
            });
        }
        // Append attestation to header extra field.
        let mut extra_data = current_header.extra_data.to_vec();
        let buf = alloy_rlp::encode(&attestation);
        extra_data.extend_from_slice(buf.as_ref());
        current_header.extra_data = alloy_primitives::Bytes::from(extra_data);
        
        // Update metric: successfully assembled vote attestation
        use once_cell::sync::Lazy;
        use crate::metrics::BscVoteMetrics;
        static VOTE_METRICS: Lazy<BscVoteMetrics> = Lazy::new(BscVoteMetrics::default);
        VOTE_METRICS.votes_attested_total.increment(votes.len() as u64);
        
        Ok(())
    }
}
