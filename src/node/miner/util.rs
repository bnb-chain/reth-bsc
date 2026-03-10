use crate::chainspec::BscChainSpec;
use crate::consensus::parlia::consensus::Parlia;
use crate::consensus::parlia::provider::SnapshotProvider;
use crate::consensus::parlia::util::{
    calculate_difficulty, debug_header, set_millisecond_part_of_timestamp,
};
use crate::consensus::parlia::Snapshot;
use crate::consensus::parlia::{EXTRA_SEAL_LEN, EXTRA_VANITY_LEN};
use crate::hardforks::BscHardforks;
use crate::node::evm::pre_execution::VALIDATOR_CACHE;
use crate::node::miner::bsc_miner::MiningContext;
use crate::node::miner::signer::{seal_header_with_global_signer, SignerError};
use alloy_consensus::{BlockHeader, Header};
use alloy_primitives::{Address, Bytes, B256};
use reth::payload::EthPayloadBuilderAttributes;
use reth_chainspec::EthChainSpec;
use std::sync::Arc;

pub fn prepare_new_attributes(
    ctx: &mut MiningContext,
    parlia: Arc<Parlia<BscChainSpec>>,
    parent_header: &Header,
    signer: Address,
) -> EthPayloadBuilderAttributes {
    let mut new_header = prepare_new_header(parlia.clone(), parent_header, signer);
    parlia.prepare_timestamp(&ctx.parent_snapshot, parent_header, &mut new_header);
    // BSC uses the PREVRANDAO opcode to return difficulty (not a random value like
    // Ethereum PoS). The validation path in BscEvmConfig::evm_env sets
    // `prevrandao = header.difficulty()`, so the building path must match.
    let difficulty = calculate_difficulty(&ctx.parent_snapshot, signer);
    let mut attributes = EthPayloadBuilderAttributes {
        parent: new_header.parent_hash,
        timestamp: new_header.timestamp,
        suggested_fee_recipient: new_header.beneficiary,
        prev_randao: difficulty.into(),
        ..Default::default()
    };
    if BscHardforks::is_bohr_active_at_timestamp(
        &parlia.spec,
        new_header.number,
        new_header.timestamp,
    ) {
        attributes.parent_beacon_block_root = Some(B256::default());
    }
    ctx.header = Some(new_header);
    attributes
}

/// prepare a tmp new header for preparing attributes.
pub fn prepare_new_header<ChainSpec>(
    parlia: Arc<Parlia<ChainSpec>>,
    parent_header: &Header,
    signer: Address,
) -> Header
where
    ChainSpec: EthChainSpec + BscHardforks + 'static,
{
    let mut timestamp = parlia.present_millis_timestamp() / 1000;
    if parent_header.timestamp >= timestamp {
        timestamp = parent_header.timestamp + 1;
    }
    let mut new_header = Header {
        number: parent_header.number + 1,
        parent_hash: parent_header.hash_slow(),
        beneficiary: signer,
        // Set timestamp to present time (or parent + 1 if present time is not greater)
        // This avoids header.timestamp = 0 when back_off_time is called inside prepare_timestamp
        timestamp,
        ..Default::default()
    };
    if BscHardforks::is_cancun_active_at_timestamp(
        parlia.spec.as_ref(),
        new_header.number,
        new_header.timestamp,
    ) {
        let blob_params = parlia.spec.blob_params_at_timestamp(new_header.timestamp);
        new_header.excess_blob_gas = parent_header.maybe_next_block_excess_blob_gas(blob_params);
    }

    new_header
}

/// finalize a new header and seal it.
pub fn finalize_new_header<ChainSpec>(
    parlia: Arc<Parlia<ChainSpec>>,
    parent_snap: &Snapshot,
    parent_header: &Header,
    new_header: &mut Header,
    snapshot_provider: &Arc<dyn SnapshotProvider + Send + Sync>,
) -> Result<(), crate::node::miner::signer::SignerError>
where
    ChainSpec: EthChainSpec + crate::hardforks::BscHardforks + 'static,
{
    new_header.difficulty = calculate_difficulty(parent_snap, new_header.beneficiary);

    // Restore correct mix_hash for the sealed header. The assembler may have set
    // mix_hash from BlockEnv.prevrandao (which is the difficulty value for EVM
    // execution). BSC encodes the millisecond timestamp part in mix_hash post-Lorentz,
    // or uses B256::ZERO pre-Lorentz.
    let millisecond_timestamp =
        parlia.block_time_for_ramanujan_fork(parent_snap, parent_header, new_header);
    if parlia.spec.is_lorentz_active_at_timestamp(new_header.number, new_header.timestamp) {
        set_millisecond_part_of_timestamp(millisecond_timestamp, new_header);
    } else {
        new_header.mix_hash = B256::ZERO;
    }

    if new_header.extra_data.len() < EXTRA_VANITY_LEN {
        new_header.extra_data = Bytes::from(vec![0u8; EXTRA_VANITY_LEN]);
    }
    // TODO: add vanity data, and fork hash.
    // set default header extra with Reth version.
    // extra, _ = rlp.EncodeToBytes([]interface{}{
    // 	uint(gethversion.Major<<16 | gethversion.Minor<<8 | gethversion.Patch),
    // 	"geth",
    // 	runtime.Version(),
    // 	runtime.GOOS,
    // })

    {
        // prepare validators
        // Use epoch_num from parent snapshot for epoch boundary check
        let epoch_length = parent_snap.epoch_num;
        if (new_header.number).is_multiple_of(epoch_length) {
            let mut validators: Option<(Vec<Address>, Vec<crate::consensus::parlia::VoteAddress>)> =
                None;
            let mut cache = VALIDATOR_CACHE.lock().unwrap();
            if let Some(cached_result) = cache.get(&parent_header.hash_slow()) {
                tracing::debug!(
                    "Succeed to query cached validator result, block_number: {}, block_hash: {}",
                    parent_header.number,
                    parent_header.hash_slow()
                );
                validators = Some(cached_result.clone());
            }

            parlia.prepare_validators(parent_snap, validators, new_header);
        }
    }

    parlia
        .prepare_turn_length(parent_snap, new_header)
        .map_err(|e| SignerError::SigningFailed(format!("Failed to prepare turn length: {}", e)))?;

    if let Err(e) =
        parlia.assemble_vote_attestation(parent_snap, parent_header, new_header, snapshot_provider)
    {
        tracing::warn!(
            target: "bsc::miner",
            error = %e,
            "Failed to assemble vote attestation, continuing without it"
        );
    }

    {
        // seal header
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
