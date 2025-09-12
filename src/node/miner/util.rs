use std::sync::Arc;
use alloy_consensus::Header;
use alloy_primitives::{Address, Bytes};
use crate::consensus::parlia::Snapshot;
use crate::consensus::parlia::consensus::Parlia;
use crate::consensus::parlia::util::calculate_difficulty;
use crate::chainspec::BscChainSpec;
use crate::consensus::parlia::EXTRA_VANITY_LEN;

pub fn prepare_new_header(parlia: Arc<Parlia<BscChainSpec>>, parent_snap: &Snapshot, parent_header: &Header, signer: Address) -> Header {
    let mut new_header = Header::default();
    new_header.number = parent_header.number + 1;
    new_header.parent_hash = parent_header.hash_slow();
    new_header.beneficiary = signer;
    new_header.difficulty = calculate_difficulty(parent_snap, signer);
    if new_header.extra_data.len() < EXTRA_VANITY_LEN{
        new_header.extra_data = Bytes::from(vec![0u8; EXTRA_VANITY_LEN]);
    }

    parlia.prepare_timestamp(parent_snap, parent_header, &mut new_header);
    parlia.prepare_validators(parent_snap, parent_header, &mut new_header);
    parlia.prepare_turn_length(parent_snap, parent_header, &mut new_header);

    return new_header;
}