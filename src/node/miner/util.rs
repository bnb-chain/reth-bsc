use std::sync::Arc;
use alloy_consensus::Header;
use alloy_primitives::{Address, Bytes};
use crate::consensus::parlia::Snapshot;
use crate::consensus::parlia::consensus::Parlia;
use crate::consensus::parlia::util::calculate_difficulty;
use crate::chainspec::BscChainSpec;
use crate::consensus::parlia::EXTRA_VANITY_LEN;
use reth::payload::EthPayloadBuilderAttributes;
use crate::hardforks::BscHardforks;
use alloy_primitives::B256;
use reth_chainspec::EthChainSpec;
use crate::node::evm::pre_execution::VALIDATOR_CACHE;

pub fn prepare_new_attributes(parlia: Arc<Parlia<BscChainSpec>>, parent_snap: &Snapshot, parent_header: &Header, signer: Address) -> EthPayloadBuilderAttributes {
    let new_header = prepare_new_header(parlia.clone(), parent_snap, parent_header, signer);
    let attributes = EthPayloadBuilderAttributes{
        parent: new_header.parent_hash.into(),
        timestamp: new_header.timestamp,
        suggested_fee_recipient: new_header.beneficiary,
        prev_randao: new_header.mix_hash,
        ..Default::default()
    };
    return attributes;
}

/// prepare a tmp new header for preparing attributes.
pub fn prepare_new_header<ChainSpec>(parlia: Arc<Parlia<ChainSpec>>, parent_snap: &Snapshot, parent_header: &Header, signer: Address) -> Header 
where
    ChainSpec: EthChainSpec + crate::hardforks::BscHardforks + 'static,
{
    let mut new_header = Header::default();
    new_header.number = parent_header.number + 1;
    new_header.parent_hash = parent_header.hash_slow();
    new_header.beneficiary = signer;
    parlia.prepare_timestamp(parent_snap, parent_header, &mut new_header);
    return new_header;
}

pub fn finalize_new_header<ChainSpec>(parlia: Arc<Parlia<ChainSpec>>, parent_snap: &Snapshot, parent_header: &Header, new_header: &mut Header) 
where
    ChainSpec: EthChainSpec + crate::hardforks::BscHardforks + 'static,
{
    new_header.difficulty = calculate_difficulty(parent_snap, new_header.beneficiary);
    if new_header.extra_data.len() < EXTRA_VANITY_LEN {
        new_header.extra_data = Bytes::from(vec![0u8; EXTRA_VANITY_LEN]);
    }

    {   // prepare validators
        let epoch_length = parlia.get_epoch_length(&new_header);
        if (new_header.number)% epoch_length == 0 {
            let mut validators: Option<(Vec<Address>, Vec<crate::consensus::parlia::VoteAddress>)> = None;
            let mut cache = VALIDATOR_CACHE.lock().unwrap();
            if let Some(cached_result) = cache.get(&parent_header.number) {
                tracing::debug!("Succeed to query cached validator result, block_number: {}", parent_header.number);
                validators = Some(cached_result.clone());
            }
            
            parlia.prepare_validators(validators, new_header);
        }

    }
    
    // todo: now doing
    //parlia.prepare_turn_length(parent_snap, parent_header, new_header);

    // todo: Attestation
    //todo!()
}