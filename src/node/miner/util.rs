use std::sync::Arc;
use alloy_consensus::Header;
use alloy_primitives::{Address, Bytes, B256};
use crate::consensus::parlia::Snapshot;
use crate::consensus::parlia::consensus::Parlia;
use crate::consensus::parlia::util::{calculate_difficulty, debug_header};
use crate::chainspec::BscChainSpec;
use crate::consensus::parlia::{EXTRA_VANITY_LEN, EXTRA_SEAL_LEN};
use reth::payload::EthPayloadBuilderAttributes;
use crate::hardforks::BscHardforks;
use reth_chainspec::EthChainSpec;
use crate::node::evm::pre_execution::VALIDATOR_CACHE;
use crate::node::miner::signer::seal_header_with_global_signer;
use crate::node::miner::bsc_miner::MiningContext;

pub fn prepare_new_attributes(ctx: &mut MiningContext, parlia: Arc<Parlia<BscChainSpec>>, parent_snap: &Snapshot, parent_header: &Header, signer: Address) -> EthPayloadBuilderAttributes {
    let new_header = prepare_new_header(parlia.clone(), parent_snap, parent_header, signer);
    let mut attributes = EthPayloadBuilderAttributes{
        parent: new_header.parent_hash,
        timestamp: new_header.timestamp,
        suggested_fee_recipient: new_header.beneficiary,
        prev_randao: new_header.mix_hash,
        ..Default::default()
    };
    if BscHardforks::is_bohr_active_at_timestamp(&parlia.spec, new_header.number, new_header.timestamp) {
        attributes.parent_beacon_block_root = Some(B256::default());
    }
    ctx.header = Some(new_header);
    attributes
}

/// prepare a tmp new header for preparing attributes.
pub fn prepare_new_header<ChainSpec>(parlia: Arc<Parlia<ChainSpec>>, parent_snap: &Snapshot, parent_header: &Header, signer: Address) -> Header 
where
    ChainSpec: EthChainSpec + BscHardforks + 'static,
{
    let mut new_header = Header { 
        number: parent_header.number + 1, 
        parent_hash: parent_header.hash_slow(), 
        beneficiary: signer, 
        ..Default::default() 
    };
    parlia.prepare_timestamp(parent_snap, parent_header, &mut new_header);
    new_header
}

/// finalize a new header and seal it.
pub fn finalize_new_header<ChainSpec>(
    parlia: Arc<Parlia<ChainSpec>>, 
    parent_snap: &Snapshot, 
    parent_header: &Header, 
    turn_length: Option<u8>,
    new_header: &mut Header) -> Result<(), crate::node::miner::signer::SignerError>
where
    ChainSpec: EthChainSpec + crate::hardforks::BscHardforks + 'static,
{
    new_header.difficulty = calculate_difficulty(parent_snap, new_header.beneficiary);
    if new_header.extra_data.len() < EXTRA_VANITY_LEN {
        new_header.extra_data = Bytes::from(vec![0u8; EXTRA_VANITY_LEN]);
    }

    {   // prepare validators
        let epoch_length = parlia.get_epoch_length(new_header);
        if (new_header.number).is_multiple_of(epoch_length) {
            let mut validators: Option<(Vec<Address>, Vec<crate::consensus::parlia::VoteAddress>)> = None;
            let mut cache = VALIDATOR_CACHE.lock().unwrap();
            if let Some(cached_result) = cache.get(&parent_header.number) {
                tracing::debug!("Succeed to query cached validator result, block_number: {}", parent_header.number);
                validators = Some(cached_result.clone());
            }
            
            parlia.prepare_validators(validators, new_header);
        }
    }

    {   // prepare turn length
        parlia.prepare_turn_length(parent_snap, turn_length, new_header);
    }

    // todo: assembleVoteAttestation

    {   // seal header
        let mut extra_data = new_header.extra_data.to_vec();
        extra_data.extend_from_slice(&[0u8; EXTRA_SEAL_LEN]);
        new_header.extra_data = Bytes::from(extra_data);
        
        let seal_data = seal_header_with_global_signer(new_header, parlia.spec.chain().id())?;
        let mut extra_data = new_header.extra_data.to_vec();
        let start = extra_data.len() - EXTRA_SEAL_LEN;
        extra_data[start..].copy_from_slice(&seal_data);
        new_header.extra_data = Bytes::from(extra_data);

        debug_header(new_header, parlia.spec.chain().id(), "finalize_new_header");
    }

    Ok(())
}