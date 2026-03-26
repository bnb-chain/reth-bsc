use crate::chainspec::BscChainSpec;
use crate::consensus::eip4844::next_block_excess_blob_gas_with_mendel;
use crate::consensus::parlia::consensus::Parlia;
use crate::consensus::parlia::constants::{
    TURN_LENGTH_SIZE, VALIDATOR_BYTES_LEN_AFTER_LUBAN, VALIDATOR_NUMBER_SIZE,
};
use crate::consensus::parlia::provider::SnapshotProvider;
use crate::consensus::parlia::util::{calculate_difficulty, debug_header};
use crate::consensus::parlia::{Snapshot, VoteAddress};
use crate::consensus::parlia::{EXTRA_SEAL_LEN, EXTRA_VANITY_LEN};
use crate::hardforks::BscHardforks;
use crate::node::evm::pre_execution::VALIDATOR_CACHE;
use crate::node::miner::bsc_miner::MiningContext;
use crate::node::miner::signer::{seal_header_with_global_signer, SignerError};
use alloy_consensus::{BlockHeader, Header};
use alloy_primitives::{Address, Bytes, B256};
use reth::payload::EthPayloadBuilderAttributes;
use reth_chainspec::EthChainSpec;
use reth_primitives::SealedHeader;
use std::sync::Arc;

fn resolve_epoch_validators(
    parent_snap: &Snapshot,
    parent_header: &SealedHeader,
) -> Result<(Vec<Address>, Vec<VoteAddress>), SignerError> {
    let parent_hash = parent_header.hash();
    let mut cache = VALIDATOR_CACHE.lock().unwrap();
    if let Some(cached_result) = cache.get(&parent_hash) {
        tracing::debug!(
            "Succeed to query cached validator result, block_number: {}, block_hash: {}",
            parent_header.number(),
            parent_hash
        );
        return Ok(cached_result.clone());
    }

    if parent_snap.validators.is_empty() {
        return Err(SignerError::SigningFailed(format!(
            "Missing epoch validators for parent block {} ({})",
            parent_header.number(), parent_hash
        )));
    }

    let validators = parent_snap.validators.clone();
    let vote_addresses = validators
        .iter()
        .map(|validator| {
            parent_snap
                .validators_map
                .get(validator)
                .map(|info| info.vote_addr)
                .unwrap_or(VoteAddress::ZERO)
        })
        .collect::<Vec<_>>();

    tracing::warn!(
        target: "bsc::miner",
        block_number = parent_header.number(),
        block_hash = %parent_hash,
        "Validator cache miss on epoch boundary, falling back to parent snapshot validators"
    );
    cache.insert(parent_hash, (validators.clone(), vote_addresses.clone()));
    Ok((validators, vote_addresses))
}

pub fn prepare_new_attributes(
    ctx: &mut MiningContext,
    parlia: Arc<Parlia<BscChainSpec>>,
    parent_header: &SealedHeader,
    signer: Address,
) -> EthPayloadBuilderAttributes {
    let mut new_header = prepare_new_header(parlia.clone(), parent_header, signer);
    parlia.prepare_timestamp(&ctx.parent_snapshot, parent_header.header(), &mut new_header);
    let mut attributes = EthPayloadBuilderAttributes {
        parent: new_header.parent_hash,
        timestamp: new_header.timestamp,
        suggested_fee_recipient: new_header.beneficiary,
        prev_randao: new_header.mix_hash,
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
    parent_header: &SealedHeader,
    signer: Address,
) -> Header
where
    ChainSpec: EthChainSpec + BscHardforks + 'static,
{
    let mut timestamp = parlia.present_millis_timestamp() / 1000;
    if parent_header.timestamp() >= timestamp {
        timestamp = parent_header.timestamp() + 1;
    }
    let mut new_header = Header {
        number: parent_header.number() + 1,
        parent_hash: parent_header.hash(),
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
        new_header.excess_blob_gas = next_block_excess_blob_gas_with_mendel(
            parlia.spec.as_ref(),
            new_header.number,
            new_header.timestamp,
            parent_header.header(),
            blob_params,
        );
    }

    new_header
}

/// finalize a new header and seal it.
pub fn finalize_new_header<ChainSpec>(
    parlia: Arc<Parlia<ChainSpec>>,
    parent_snap: &Snapshot,
    parent_header: &SealedHeader,
    new_header: &mut Header,
    snapshot_provider: &Arc<dyn SnapshotProvider + Send + Sync>,
) -> Result<(), crate::node::miner::signer::SignerError>
where
    ChainSpec: EthChainSpec + crate::hardforks::BscHardforks + 'static,
{
    new_header.difficulty = calculate_difficulty(parent_snap, new_header.beneficiary);

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
            let validators = resolve_epoch_validators(parent_snap, parent_header)?;
            parlia.prepare_validators(parent_snap, Some(validators), new_header);
        }
    }

    parlia
        .prepare_turn_length(parent_snap, new_header)
        .map_err(|e| SignerError::SigningFailed(format!("Failed to prepare turn length: {}", e)))?;

    if let Err(e) =
        parlia.assemble_vote_attestation(parent_snap, parent_header.header(), new_header, snapshot_provider)
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

/// Refresh the vote attestation in an already-finalized header with the latest votes
/// from the pool, then re-seal (re-sign) the header.
///
/// In Go BSC, vote attestation is assembled inside `Seal()` AFTER the full sealing delay,
/// giving maximum time for votes to propagate across the network. In reth-bsc, the block
/// is built (including vote attestation) at the start of the payload job, so for empty
/// blocks this happens almost immediately—before votes have had time to arrive.
///
/// This function fixes that by re-assembling the vote attestation with the latest votes
/// from the pool right before the payload is submitted, matching Go BSC's behavior.
pub fn refresh_vote_attestation_and_seal<ChainSpec>(
    parlia: Arc<Parlia<ChainSpec>>,
    parent_snap: &Snapshot,
    parent_header: &Header,
    header: &mut Header,
    snapshot_provider: &Arc<dyn SnapshotProvider + Send + Sync>,
) -> Result<(), SignerError>
where
    ChainSpec: EthChainSpec + crate::hardforks::BscHardforks + 'static,
{
    if !parlia.spec.is_luban_active_at_block(header.number) {
        // Pre-Luban: no vote attestation support
        return Ok(());
    }

    let extra_len = header.extra_data.len();
    if extra_len < EXTRA_VANITY_LEN + EXTRA_SEAL_LEN {
        return Ok(());
    }

    // Calculate where the vote attestation starts in extra_data.
    // Structure (post-Luban):
    //   Vanity (32) + [ValidatorNum (1) + Validators (N*68) + TurnLength (1, if Bohr)] + [Attestation (RLP)] + Seal (65)
    // On non-epoch blocks, there are no validator/turnLength fields.
    let epoch_length = parent_snap.epoch_num;
    let attestation_start = if header.number.is_multiple_of(epoch_length) {
        let count = header.extra_data[EXTRA_VANITY_LEN] as usize;
        let mut start =
            EXTRA_VANITY_LEN + VALIDATOR_NUMBER_SIZE + count * VALIDATOR_BYTES_LEN_AFTER_LUBAN;
        if parlia.spec.is_bohr_active_at_timestamp(header.number, header.timestamp) {
            start += TURN_LENGTH_SIZE;
        }
        start
    } else {
        EXTRA_VANITY_LEN
    };

    // Safety: attestation_start must not exceed extra_data length
    if attestation_start > extra_len {
        tracing::warn!(
            target: "bsc::miner",
            block_number = header.number,
            attestation_start,
            extra_len,
            "Unexpected extra_data layout, skipping vote refresh"
        );
        return Ok(());
    }

    // Save the original extra_data so we can restore it if re-assembly fails.
    // This prevents silently producing a block with fewer votes than the original.
    let original_extra = header.extra_data.clone();

    // Strip old attestation + seal, keeping vanity + validators + turn_length
    let base_extra = header.extra_data[..attestation_start].to_vec();
    header.extra_data = Bytes::from(base_extra);

    // Re-assemble vote attestation with fresh votes from pool
    if let Err(e) =
        parlia.assemble_vote_attestation(parent_snap, parent_header, header, snapshot_provider)
    {
        tracing::warn!(
            target: "bsc::miner",
            error = %e,
            block_number = header.number,
            "Failed to refresh vote attestation, restoring original"
        );
        header.extra_data = original_extra;
        return Ok(());
    }

    // Re-seal the header
    {
        let mut extra_data = header.extra_data.to_vec();
        extra_data.extend_from_slice(&[0u8; EXTRA_SEAL_LEN]);
        header.extra_data = Bytes::from(extra_data);

        match seal_header_with_global_signer(header, parlia.spec.chain().id()) {
            Ok(seal_data) => {
                let mut extra_data = header.extra_data.to_vec();
                let start = extra_data.len() - EXTRA_SEAL_LEN;
                extra_data[start..].copy_from_slice(&seal_data);
                header.extra_data = Bytes::from(extra_data);
            }
            Err(e) => {
                tracing::warn!(
                    target: "bsc::miner",
                    error = %e,
                    block_number = header.number,
                    "Failed to re-seal header after vote refresh, restoring original"
                );
                header.extra_data = original_extra;
                return Ok(());
            }
        }

        debug_header(header, parlia.spec.chain().id(), "refresh_vote_attestation_and_seal");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::consensus::parlia::constants::{
        VALIDATOR_BYTES_LEN_AFTER_LUBAN, VALIDATOR_NUMBER_SIZE,
    };
    use crate::consensus::parlia::vote::{VoteAttestation, VoteData};
    use alloy_primitives::B256;

    fn unique_parent_header(number: u64) -> Header {
        Header { number, parent_hash: B256::random(), ..Default::default() }
    }

    #[test]
    fn resolve_epoch_validators_falls_back_to_snapshot_on_cache_miss() {
        let parent_header = unique_parent_header(499);
        let parent_hash = parent_header.hash_slow();

        let validators = vec![
            Address::with_last_byte(2),
            Address::with_last_byte(1),
            Address::with_last_byte(3),
        ];
        let vote_addresses = vec![
            VoteAddress::with_last_byte(22),
            VoteAddress::with_last_byte(11),
            VoteAddress::with_last_byte(33),
        ];
        let parent_snap =
            Snapshot::new(validators, 499, B256::random(), 500, Some(vote_addresses.clone()));

        let resolved =
            resolve_epoch_validators(&parent_snap, &SealedHeader::seal_slow(parent_header)).unwrap();

        assert_eq!(resolved.0, parent_snap.validators);
        assert_eq!(resolved.1.len(), parent_snap.validators.len());
        for (i, validator) in parent_snap.validators.iter().enumerate() {
            assert_eq!(resolved.1[i], parent_snap.validators_map.get(validator).unwrap().vote_addr);
        }

        let mut cache = VALIDATOR_CACHE.lock().unwrap();
        let cached = cache.get(&parent_hash).unwrap();
        assert_eq!(cached.0, resolved.0);
        assert_eq!(cached.1, resolved.1);
    }

    #[test]
    fn resolve_epoch_validators_prefers_cache() {
        let parent_header = unique_parent_header(799);
        let parent_hash = parent_header.hash_slow();

        let cached_validators = vec![Address::with_last_byte(9), Address::with_last_byte(7)];
        let cached_vote_addresses =
            vec![VoteAddress::with_last_byte(99), VoteAddress::with_last_byte(77)];
        {
            let mut cache = VALIDATOR_CACHE.lock().unwrap();
            cache.insert(parent_hash, (cached_validators.clone(), cached_vote_addresses.clone()));
        }

        let parent_snap =
            Snapshot::new(vec![Address::with_last_byte(1)], 799, B256::random(), 500, None);
        let resolved =
            resolve_epoch_validators(&parent_snap, &SealedHeader::seal_slow(parent_header)).unwrap();

        assert_eq!(resolved.0, cached_validators);
        assert_eq!(resolved.1, cached_vote_addresses);
    }

    #[test]
    fn resolve_epoch_validators_errors_when_no_cache_or_snapshot_validators() {
        let parent_header = unique_parent_header(999);
        let parent_snap = Snapshot::default();

        let err =
            resolve_epoch_validators(&parent_snap, &SealedHeader::seal_slow(parent_header))
                .unwrap_err();
        match err {
            SignerError::SigningFailed(msg) => {
                assert!(msg.contains("Missing epoch validators"), "unexpected message: {}", msg);
            }
            other => panic!("unexpected error: {:?}", other),
        }
    }

    // --- Tests for refresh_vote_attestation_and_seal ---

    use crate::chainspec::BscChainSpec;
    use crate::consensus::parlia::provider::SnapshotProvider;
    use crate::hardforks::bsc::BscHardfork;
    use alloy_primitives::BlockHash;
    use reth_chainspec::{ChainSpecBuilder, ForkCondition};

    /// Mock SnapshotProvider that returns a fixed snapshot for any hash.
    struct MockSnapshotProvider {
        snapshot: Snapshot,
    }

    impl SnapshotProvider for MockSnapshotProvider {
        fn snapshot_by_hash(&self, _block_hash: &BlockHash) -> Option<Snapshot> {
            Some(self.snapshot.clone())
        }
        fn insert(&self, _snapshot: Snapshot) {}
    }

    /// Initialize the global signer for tests (ignores AlreadyInitialized errors).
    fn ensure_test_signer() {
        let raw = alloy_primitives::hex::decode(
            "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80",
        )
        .unwrap();
        let _ = crate::node::miner::signer::init_global_signer(B256::from_slice(&raw));
    }

    /// Build a post-Luban chain spec with Luban active at block 0.
    fn luban_chain_spec() -> Arc<BscChainSpec> {
        Arc::new(BscChainSpec::from(
            ChainSpecBuilder::mainnet()
                .with_fork(BscHardfork::Luban, ForkCondition::Block(0))
                .build(),
        ))
    }

    /// Build a non-epoch header with a fake attestation in extra_data.
    /// Structure: Vanity (32) + Attestation (RLP) + Seal (65)
    fn header_with_fake_attestation(number: u64) -> Header {
        let att = VoteAttestation {
            vote_address_set: 0xFF,
            agg_signature: Default::default(),
            data: VoteData {
                source_number: 1,
                source_hash: B256::ZERO,
                target_number: number - 1,
                target_hash: B256::ZERO,
            },
            extra: bytes::Bytes::new(),
        };
        let mut extra = vec![0u8; EXTRA_VANITY_LEN];
        extra.extend_from_slice(alloy_rlp::encode(&att).as_ref());
        extra.extend_from_slice(&[0u8; EXTRA_SEAL_LEN]);
        Header {
            number,
            extra_data: alloy_primitives::Bytes::from(extra),
            ..Default::default()
        }
    }

    /// Build a non-epoch header with no attestation (just vanity + seal).
    fn header_without_attestation(number: u64) -> Header {
        let mut extra = vec![0u8; EXTRA_VANITY_LEN];
        extra.extend_from_slice(&[0u8; EXTRA_SEAL_LEN]);
        Header {
            number,
            extra_data: alloy_primitives::Bytes::from(extra),
            ..Default::default()
        }
    }

    #[test]
    fn refresh_skips_pre_luban_header() {
        ensure_test_signer();
        // Use mainnet spec where Luban is NOT active at low block numbers
        let chain_spec = Arc::new(BscChainSpec::from(ChainSpecBuilder::mainnet().build()));
        let parlia = Arc::new(Parlia::new(chain_spec, 200));

        let parent_snap = Snapshot::new(
            vec![Address::with_last_byte(1)],
            0,
            B256::random(),
            200,
            None,
        );
        let parent_header = unique_parent_header(0);
        let mut header = header_without_attestation(1);
        let original_extra = header.extra_data.clone();

        let sp: Arc<dyn SnapshotProvider + Send + Sync> = Arc::new(MockSnapshotProvider {
            snapshot: parent_snap.clone(),
        });

        let result = refresh_vote_attestation_and_seal(
            parlia,
            &parent_snap,
            &parent_header,
            &mut header,
            &sp,
        );

        assert!(result.is_ok());
        // Extra data should be unchanged (function returned early)
        assert_eq!(header.extra_data, original_extra);
    }

    // Use block number < 3 so assemble_vote_attestation returns Ok(()) immediately
    // (it skips assembly for blocks < 3). This lets us test the pure stripping +
    // re-sealing logic without needing the full global header provider.

    #[test]
    fn refresh_strips_old_attestation_on_non_epoch_block() {
        ensure_test_signer();
        let chain_spec = luban_chain_spec();
        let parlia = Arc::new(Parlia::new(chain_spec, 200));

        let parent_snap = Snapshot::new(
            vec![Address::with_last_byte(1)],
            0,
            B256::random(),
            200,
            None,
        );
        let parent_header = unique_parent_header(0);
        // Block 1: non-epoch, number < 3 so assemble skips → tests pure stripping
        let mut header = header_with_fake_attestation(1);
        let original_extra_len = header.extra_data.len();

        let sp: Arc<dyn SnapshotProvider + Send + Sync> = Arc::new(MockSnapshotProvider {
            snapshot: parent_snap.clone(),
        });

        let result = refresh_vote_attestation_and_seal(
            parlia,
            &parent_snap,
            &parent_header,
            &mut header,
            &sp,
        );

        assert!(result.is_ok());
        // The old attestation was stripped and no new one was added (block < 3).
        // Final extra = Vanity (32) + Seal (65) = 97 bytes
        assert_eq!(
            header.extra_data.len(),
            EXTRA_VANITY_LEN + EXTRA_SEAL_LEN,
            "expected vanity+seal only, original_len={}, new_len={}",
            original_extra_len,
            header.extra_data.len()
        );
        assert!(
            header.extra_data.len() < original_extra_len,
            "refreshed header should be shorter than original with fake attestation"
        );
    }

    #[test]
    fn refresh_preserves_vanity_only_header() {
        ensure_test_signer();
        let chain_spec = luban_chain_spec();
        let parlia = Arc::new(Parlia::new(chain_spec, 200));

        let parent_snap = Snapshot::new(
            vec![Address::with_last_byte(1)],
            0,
            B256::random(),
            200,
            None,
        );
        let parent_header = unique_parent_header(0);
        // Block 1, no attestation (vanity + seal only)
        let mut header = header_without_attestation(1);

        let sp: Arc<dyn SnapshotProvider + Send + Sync> = Arc::new(MockSnapshotProvider {
            snapshot: parent_snap.clone(),
        });

        let result = refresh_vote_attestation_and_seal(
            parlia,
            &parent_snap,
            &parent_header,
            &mut header,
            &sp,
        );

        assert!(result.is_ok());
        // Should still be vanity + seal (no attestation added, block < 3)
        assert_eq!(header.extra_data.len(), EXTRA_VANITY_LEN + EXTRA_SEAL_LEN);
    }

    #[test]
    fn refresh_handles_epoch_block_with_validators() {
        ensure_test_signer();
        // epoch_num=2 so block 2 is an epoch block and also < 3 for assembly skip
        let chain_spec = luban_chain_spec();
        let parlia = Arc::new(Parlia::new(chain_spec, 2));

        let validators = vec![
            Address::with_last_byte(1),
            Address::with_last_byte(2),
            Address::with_last_byte(3),
        ];
        let vote_addrs = vec![
            VoteAddress::with_last_byte(11),
            VoteAddress::with_last_byte(22),
            VoteAddress::with_last_byte(33),
        ];
        let parent_snap = Snapshot::new(
            validators.clone(),
            1,
            B256::random(),
            2, // epoch_num = 2
            Some(vote_addrs.clone()),
        );
        let parent_header = unique_parent_header(1);

        // Build an epoch block (number=2, divisible by epoch_num=2)
        // Structure: Vanity (32) + Count (1) + 3*68 validators + fake attestation + Seal (65)
        let mut extra = vec![0u8; EXTRA_VANITY_LEN];
        extra.push(3u8);
        let mut sorted_validators = validators.clone();
        sorted_validators.sort();
        for (i, v) in sorted_validators.iter().enumerate() {
            extra.extend_from_slice(v.as_slice());
            extra.extend_from_slice(vote_addrs[i].as_slice());
        }
        let att = VoteAttestation {
            vote_address_set: 0x7,
            agg_signature: Default::default(),
            data: VoteData {
                source_number: 1,
                source_hash: B256::ZERO,
                target_number: 1,
                target_hash: B256::ZERO,
            },
            extra: bytes::Bytes::new(),
        };
        extra.extend_from_slice(alloy_rlp::encode(&att).as_ref());
        extra.extend_from_slice(&[0u8; EXTRA_SEAL_LEN]);

        let mut header = Header {
            number: 2,
            extra_data: alloy_primitives::Bytes::from(extra),
            ..Default::default()
        };

        let expected_base_len = EXTRA_VANITY_LEN
            + VALIDATOR_NUMBER_SIZE
            + 3 * VALIDATOR_BYTES_LEN_AFTER_LUBAN;
        let original_extra_len = header.extra_data.len();

        let sp: Arc<dyn SnapshotProvider + Send + Sync> = Arc::new(MockSnapshotProvider {
            snapshot: parent_snap.clone(),
        });

        let result = refresh_vote_attestation_and_seal(
            parlia,
            &parent_snap,
            &parent_header,
            &mut header,
            &sp,
        );

        assert!(result.is_ok());
        // After refresh: base (vanity + count + validators) + seal, no attestation
        assert_eq!(
            header.extra_data.len(),
            expected_base_len + EXTRA_SEAL_LEN,
            "epoch block should preserve validators but strip attestation, \
             original_len={}, new_len={}, expected={}",
            original_extra_len,
            header.extra_data.len(),
            expected_base_len + EXTRA_SEAL_LEN
        );
        // Verify validator count byte is preserved
        assert_eq!(header.extra_data[EXTRA_VANITY_LEN], 3u8);
    }

    #[test]
    fn refresh_returns_ok_for_too_short_extra_data() {
        ensure_test_signer();
        let chain_spec = luban_chain_spec();
        let parlia = Arc::new(Parlia::new(chain_spec, 200));

        let parent_snap = Snapshot::new(
            vec![Address::with_last_byte(1)],
            0,
            B256::random(),
            200,
            None,
        );
        let parent_header = unique_parent_header(0);
        let mut header = Header {
            number: 1,
            extra_data: alloy_primitives::Bytes::from(vec![0u8; 10]),
            ..Default::default()
        };
        let original_extra = header.extra_data.clone();

        let sp: Arc<dyn SnapshotProvider + Send + Sync> = Arc::new(MockSnapshotProvider {
            snapshot: parent_snap.clone(),
        });

        let result = refresh_vote_attestation_and_seal(
            parlia,
            &parent_snap,
            &parent_header,
            &mut header,
            &sp,
        );

        assert!(result.is_ok());
        // Extra data should be unchanged
        assert_eq!(header.extra_data, original_extra);
    }

    #[test]
    fn refresh_produces_valid_seal() {
        ensure_test_signer();
        let chain_spec = luban_chain_spec();
        let parlia = Arc::new(Parlia::new(chain_spec.clone(), 200));

        let parent_snap = Snapshot::new(
            vec![Address::with_last_byte(1)],
            0,
            B256::random(),
            200,
            None,
        );
        let parent_header = unique_parent_header(0);
        // Block 1 with fake attestation
        let mut header = header_with_fake_attestation(1);

        let sp: Arc<dyn SnapshotProvider + Send + Sync> = Arc::new(MockSnapshotProvider {
            snapshot: parent_snap.clone(),
        });

        let result = refresh_vote_attestation_and_seal(
            parlia.clone(),
            &parent_snap,
            &parent_header,
            &mut header,
            &sp,
        );

        assert!(result.is_ok());
        // Verify the seal is valid by recovering the proposer address
        let recovered = parlia.recover_proposer(&header);
        assert!(
            recovered.is_ok(),
            "seal should be valid and proposer recoverable, err={:?}",
            recovered.err()
        );
    }

    #[test]
    fn refresh_restores_original_when_reassembly_fails() {
        // When assemble_vote_attestation fails (e.g., missing header provider for
        // blocks >= 3), the function must restore the original extra_data to avoid
        // silently producing a block with fewer votes.
        ensure_test_signer();
        let chain_spec = luban_chain_spec();
        let parlia = Arc::new(Parlia::new(chain_spec, 200));

        let parent_snap = Snapshot::new(
            vec![Address::with_last_byte(1)],
            2,
            B256::random(),
            200,
            None,
        );
        let parent_header = unique_parent_header(2);
        // Block 3: assemble_vote_attestation will fail (no global header provider)
        let mut header = header_with_fake_attestation(3);
        let original_extra = header.extra_data.clone();

        let sp: Arc<dyn SnapshotProvider + Send + Sync> = Arc::new(MockSnapshotProvider {
            snapshot: parent_snap.clone(),
        });

        let result = refresh_vote_attestation_and_seal(
            parlia,
            &parent_snap,
            &parent_header,
            &mut header,
            &sp,
        );

        assert!(result.is_ok());
        // Original extra_data should be preserved since re-assembly failed
        assert_eq!(
            header.extra_data, original_extra,
            "original extra_data must be preserved when vote re-assembly fails"
        );
    }
}
