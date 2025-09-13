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
pub fn prepare_new_header(parlia: Arc<Parlia<BscChainSpec>>, parent_snap: &Snapshot, parent_header: &Header, signer: Address) -> Header {
    let mut new_header = Header::default();
    new_header.number = parent_header.number + 1;
    new_header.parent_hash = parent_header.hash_slow();
    new_header.beneficiary = signer;
    parlia.prepare_timestamp(parent_snap, parent_header, &mut new_header);
    return new_header;
}

pub fn finalize_new_header(_parlia: Arc<Parlia<BscChainSpec>>, _parent_snap: &Snapshot, _parent_header: &Header, _new_header: &mut Header) {
    //new_header.difficulty = calculate_difficulty(parent_snap, signer);
    // if new_header.extra_data.len() < EXTRA_VANITY_LEN {
    //     new_header.extra_data = Bytes::from(vec![0u8; EXTRA_VANITY_LEN]);
    // }
    //parlia.prepare_timestamp(parent_snap, parent_header, new_header);
    //parlia.prepare_validators(parent_snap, parent_header, new_header);
    //parlia.prepare_turn_length(parent_snap, parent_header, new_header);

    // todo: Attestation
    todo!()
}