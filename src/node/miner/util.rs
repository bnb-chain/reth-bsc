use std::sync::Arc;
use alloy_consensus::Header;
use alloy_primitives::Address;
use crate::consensus::parlia::Snapshot;
use crate::consensus::parlia::consensus::Parlia;
use crate::consensus::parlia::util::calculate_difficulty;
use crate::chainspec::BscChainSpec;

pub fn prepare_new_header(parlia: Arc<Parlia<BscChainSpec>>, parent_snap: &Snapshot, parent_header: &Header, signer: Address) -> Header {
    let mut new_header = Header::default();
    new_header.number = parent_header.number + 1;
    new_header.parent_hash = parent_header.hash_slow();
    new_header.beneficiary = signer;
    new_header.difficulty = calculate_difficulty(parent_snap, signer);

    parlia.prepare_timestamp(parent_snap, parent_header, &mut new_header);


    // new_header.timestamp = parlia.block_time_for_ramanujan_fork(snap, parent, &new_header);
    // new_header.difficulty = parlia.calculate_difficulty(snap, parent, &new_header);
    // new_header.mix_hash = parlia.calculate_mix_hash(snap, parent, &new_header);
    // new_header.nonce = parlia.calculate_nonce(snap, parent, &new_header);
    // new_header.extra_data = parlia.calculate_extra_data(snap, parent, &new_header);
    // new_header.gas_limit = parlia.calculate_gas_limit(snap, parent, &new_header);
    // new_header.gas_used = parlia.calculate_gas_used(snap, parent, &new_header);
    // new_header.receipt_root = parlia.calculate_receipt_root(snap, parent, &new_header);
    // new_header.logs_bloom = parlia.calculate_logs_bloom(snap, parent, &new_header);
    // new_header.miner = parlia.calculate_miner(snap, parent, &new_header);
    // new_header.mix_digest = parlia.calculate_mix_digest(snap, parent, &new_header);


    return new_header;
}