use crate::chainspec::BscChainSpec;
use crate::consensus::eip4844::next_block_excess_blob_gas_with_mendel;
use crate::consensus::parlia::consensus::Parlia;
use crate::consensus::parlia::constants::{
    TURN_LENGTH_SIZE, VALIDATOR_BYTES_LEN_AFTER_LUBAN, VALIDATOR_NUMBER_SIZE,
};
use crate::consensus::parlia::provider::SnapshotProvider;
use crate::consensus::parlia::util::{
    calculate_difficulty, debug_header, set_millisecond_part_of_timestamp,
};
use crate::consensus::parlia::Snapshot;
use crate::consensus::parlia::{EXTRA_SEAL_LEN, EXTRA_VANITY_LEN};
use crate::hardforks::BscHardforks;
use crate::node::evm::pre_execution::{
    validators_at_parent, EpochValidators, VALIDATOR_CACHE,
};
use crate::node::miner::bsc_miner::MiningContext;
use crate::node::miner::signer::{seal_header_with_global_signer, SignerError};
use alloy_consensus::{BlockHeader, Header};
use alloy_primitives::{Address, Bytes, B256};
use reth_node_ethereum::engine::EthPayloadAttributes;
use reth_chainspec::EthChainSpec;
use reth_primitives_traits::SealedHeader;
use reth_provider::StateProviderFactory;
use std::sync::Arc;

/// Returns the validator set for `parent_header.number + 1`, or `None` off the epoch boundary.
///
/// Reads under the parent's state/env and caches by `parent.hash()`.
pub(crate) fn epoch_validators_for_next_block<C>(
    client: &C,
    chain_spec: &Arc<BscChainSpec>,
    parent_snap: &Snapshot,
    parent_header: &SealedHeader,
) -> Result<Option<EpochValidators>, SignerError>
where
    C: StateProviderFactory,
{
    if !(parent_header.number() + 1).is_multiple_of(parent_snap.epoch_num) {
        return Ok(None);
    }
    let parent_hash = parent_header.hash();
    // Bind the lookup so the guard drops here: under edition 2021 an `if let` on the guard itself
    // would hold the lock across its whole body.
    let cached = VALIDATOR_CACHE.lock().unwrap().get(&parent_hash).cloned();
    if let Some(validators) = cached {
        tracing::debug!(
            target: "bsc::miner",
            block_number = parent_header.number() + 1,
            validators = ?validators.0,
            "Epoch validators from cache"
        );
        return Ok(Some(validators));
    }

    let failed = |e: &dyn std::fmt::Display| {
        SignerError::SigningFailed(format!(
            "Failed to read epoch validators from parent block {} ({}): {e}",
            parent_header.number(),
            parent_hash,
        ))
    };
    let state = client.state_by_block_hash(parent_hash).map_err(|e| failed(&e))?;
    let validators = validators_at_parent(&state, chain_spec.clone(), parent_header)
        .map_err(|e| failed(&e))?;
    // Worth a `warn!`: the parent should have filled the cache, so a miss here means this process
    // did not execute it — the restart window #465 stalls in.
    tracing::warn!(
        target: "bsc::miner",
        block_number = parent_header.number() + 1,
        %parent_hash,
        validators = ?validators.0,
        "Epoch validators read from parent state after a cache miss"
    );
    VALIDATOR_CACHE.lock().unwrap().insert(parent_hash, validators.clone());
    Ok(Some(validators))
}

/// Prepare a new block's header and derive the payload builder attributes.
///
/// Populates on `ctx`:
/// - `header`: the freshly constructed unsealed header for this block.
/// - `block_timestamp_ms`: block timestamp in ms fixed by `prepare_timestamp`, reused
///   verbatim by `finalize_new_header` to avoid seconds/ms drift at sealing time.
/// - `end_mining_timestamp_ms`: wall-clock deadline for this mining job, computed as
///   `now_ms + parlia.delay_for_ramanujan_fork(...)`.
pub fn prepare_new_attributes(
    ctx: &mut MiningContext,
    parlia: Arc<Parlia<BscChainSpec>>,
    parent_header: &SealedHeader,
    signer: Address,
) -> EthPayloadAttributes {
    let mut new_header = prepare_new_header(parlia.clone(), parent_header, signer);
    // Cache the planned millisecond timestamp so finalize_new_header can reuse it verbatim.
    ctx.block_timestamp_ms =
        parlia.prepare_timestamp(&ctx.parent_snapshot, parent_header.header(), &mut new_header);

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let end_mining_delay_ms =
        parlia.delay_for_ramanujan_fork(&ctx.parent_snapshot, &new_header);
    ctx.end_mining_timestamp_ms = now_ms + end_mining_delay_ms as u128;

    // BSC uses the PREVRANDAO opcode to return difficulty (not a random value like
    // Ethereum PoS). The validation path in BscEvmConfig::evm_env sets
    // `prevrandao = header.difficulty()`, so the building path must match.
    let difficulty = calculate_difficulty(&ctx.parent_snapshot, signer);
    let mut attributes = EthPayloadAttributes {
        timestamp: new_header.timestamp,
        suggested_fee_recipient: new_header.beneficiary,
        prev_randao: difficulty.into(),
        withdrawals: None,
        parent_beacon_block_root: None,
        slot_number: None,
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

/// Finalize a new header and seal it.
///
/// Epoch blocks require an explicit validator set from the caller.
pub fn finalize_new_header<ChainSpec>(
    parlia: Arc<Parlia<ChainSpec>>,
    parent_snap: &Snapshot,
    parent_header: &SealedHeader,
    new_header: &mut Header,
    snapshot_provider: &Arc<dyn SnapshotProvider + Send + Sync>,
    block_timestamp_ms: u64,
    epoch_validators: Option<EpochValidators>,
) -> Result<(), crate::node::miner::signer::SignerError>
where
    ChainSpec: EthChainSpec + crate::hardforks::BscHardforks + 'static,
{
    new_header.difficulty = calculate_difficulty(parent_snap, new_header.beneficiary);
    if parlia.spec.is_lorentz_active_at_timestamp(new_header.number, new_header.timestamp) {
        set_millisecond_part_of_timestamp(block_timestamp_ms, new_header);
    } else {
        new_header.mix_hash = B256::ZERO;
    }

    if new_header.extra_data.len() < EXTRA_VANITY_LEN {
        let mut padded = new_header.extra_data.to_vec();
        padded.resize(EXTRA_VANITY_LEN, 0u8);
        new_header.extra_data = Bytes::from(padded);
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
            let validators = epoch_validators.ok_or_else(|| {
                SignerError::SigningFailed(format!(
                    "Epoch block {} needs epoch validators, but none were supplied (parent {})",
                    new_header.number,
                    parent_header.hash(),
                ))
            })?;
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
    use crate::chainspec::bsc::bsc_mainnet;
    use crate::consensus::parlia::VoteAddress;
    use reth_provider::test_utils::MockEthProvider;
    use alloy_primitives::B256;

    fn unique_parent_header(number: u64) -> Header {
        Header { number, parent_hash: B256::random(), ..Default::default() }
    }

    /// The gate: a non-epoch child needs no set, and deciding that must not touch state.
    #[test]
    fn epoch_validators_are_only_resolved_for_epoch_blocks() {
        let parent = SealedHeader::seal_slow(Header { number: 5, ..Default::default() });
        // `epoch_num = 2` makes 6 an epoch block and 7 not one.
        let at_boundary = Snapshot::new(vec![Address::with_last_byte(1)], 5, parent.hash(), 2, None);
        let off_boundary =
            Snapshot::new(vec![Address::with_last_byte(1)], 5, parent.hash(), 4, None);
        let spec = Arc::new(BscChainSpec::from(bsc_mainnet()));
        // A provider that knows nothing: reaching it at all is a failure, not a `None`.
        let client = MockEthProvider::default();

        assert_eq!(
            epoch_validators_for_next_block(&client, &spec, &off_boundary, &parent).unwrap(),
            None,
            "block 6 is not a multiple of 4, so no state should be read"
        );
        epoch_validators_for_next_block(&client, &spec, &at_boundary, &parent)
            .expect_err("block 6 is a multiple of 2, so the empty provider must surface an error");
    }

    /// The memo: an entry the parent's own execution already left is used as-is, so sealing an
    /// epoch block reads no state. Its key and value are [`VALIDATOR_CACHE`]'s (see its invariant).
    #[test]
    fn epoch_validators_come_from_the_cache_when_the_parent_filled_it() {
        let parent = SealedHeader::seal_slow(Header {
            number: 5,
            parent_hash: B256::random(), // unique, so this test owns its cache entry
            ..Default::default()
        });
        let snap = Snapshot::new(vec![Address::with_last_byte(1)], 5, parent.hash(), 2, None);
        let expected = (vec![Address::with_last_byte(9)], vec![VoteAddress::with_last_byte(99)]);
        VALIDATOR_CACHE.lock().unwrap().insert(parent.hash(), expected.clone());

        let resolved = epoch_validators_for_next_block(
            &MockEthProvider::default(),
            &Arc::new(BscChainSpec::from(bsc_mainnet())),
            &snap,
            &parent,
        )
        .expect("cache hit must not need the provider");

        assert_eq!(resolved, Some(expected));
    }

    /// A single-validator epoch block at height 2: `epoch_num = 2` makes it an epoch boundary
    /// while staying under the height-3 short-circuit in `assemble_vote_attestation`. Bohr is not
    /// enabled by `lorentz_chain_spec`, so no turn-length byte follows the validator records.
    fn epoch_block_fixture() -> (Arc<Parlia<BscChainSpec>>, Snapshot, SealedHeader, Header) {
        let parlia = Arc::new(Parlia::new(lorentz_chain_spec(), 200));
        let parent = SealedHeader::seal_slow(Header {
            number: 1,
            timestamp: 1_776_727_552,
            ..Default::default()
        });
        let validator = Address::with_last_byte(1);
        let parent_snap = Snapshot::new(vec![validator], 1, parent.hash(), 2, None);
        let header = Header {
            number: 2,
            parent_hash: parent.hash(),
            beneficiary: validator,
            timestamp: parent.timestamp() + 1,
            extra_data: Bytes::from(vec![0u8; EXTRA_VANITY_LEN]),
            ..Default::default()
        };
        (parlia, parent_snap, parent, header)
    }

    /// Fail-closed at an epoch block: there is no fallback, because the only set reachable without
    /// the parent's state (the snapshot's) is the *outgoing* one — sealing that would produce a
    /// block the rest of the network rejects (bnb-chain/reth-bsc#465).
    #[test]
    fn finalize_new_header_rejects_an_epoch_block_without_validators() {
        let (parlia, parent_snap, parent, mut header) = epoch_block_fixture();
        let sp: Arc<dyn SnapshotProvider + Send + Sync> =
            Arc::new(MockSnapshotProvider { snapshot: parent_snap.clone() });
        let planned_ms = header.timestamp * 1000;

        let err = finalize_new_header(
            parlia,
            &parent_snap,
            &parent,
            &mut header,
            &sp,
            planned_ms,
            None,
        )
        .unwrap_err();

        match err {
            SignerError::SigningFailed(msg) => {
                assert!(msg.contains("needs epoch validators"), "unexpected message: {msg}");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    /// Whatever set the caller supplies is what the epoch block's extra data carries —
    /// `finalize_new_header` no longer sources it itself.
    #[test]
    fn finalize_new_header_writes_the_supplied_epoch_validators() {
        ensure_test_signer();
        let (parlia, parent_snap, parent, mut header) = epoch_block_fixture();
        let sp: Arc<dyn SnapshotProvider + Send + Sync> =
            Arc::new(MockSnapshotProvider { snapshot: parent_snap.clone() });
        let planned_ms = header.timestamp * 1000;
        let mut validators = vec![Address::with_last_byte(9), Address::with_last_byte(7)];

        finalize_new_header(
            parlia,
            &parent_snap,
            &parent,
            &mut header,
            &sp,
            planned_ms,
            Some((validators.clone(), vec![VoteAddress::ZERO; validators.len()])),
        )
        .expect("finalize_new_header should succeed");

        // Assert the layout, not mere presence: these addresses are 19 zero bytes plus one
        // significant byte and sit among long zero runs, so a substring search matches by chance.
        let payload = &header.extra_data[EXTRA_VANITY_LEN..];
        assert_eq!(payload[0] as usize, validators.len(), "validator count");
        validators.sort(); // prepare_validators sorts before writing
        for (i, validator) in validators.iter().enumerate() {
            let at = VALIDATOR_NUMBER_SIZE + i * VALIDATOR_BYTES_LEN_AFTER_LUBAN;
            assert_eq!(&payload[at..at + 20], validator.as_slice(), "validator {i}");
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
        crate::node::miner::signer::init_test_signer();
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

    // --- Tests for finalize_new_header millisecond-timestamp invariant ---

    /// Post-Luban, post-Lorentz chainspec so `finalize_new_header` takes the Lorentz branch
    /// that writes the millisecond part into `mix_hash`. Lorentz activation is gated on
    /// `is_london_active_at_block`, so London is also forced to block 0.
    fn lorentz_chain_spec() -> Arc<BscChainSpec> {
        use reth_chainspec::EthereumHardfork;
        Arc::new(BscChainSpec::from(
            ChainSpecBuilder::mainnet()
                .with_fork(EthereumHardfork::London, ForkCondition::Block(0))
                .with_fork(BscHardfork::Luban, ForkCondition::Block(0))
                .with_fork(BscHardfork::Lorentz, ForkCondition::Timestamp(0))
                .build(),
        ))
    }

    /// Regression for the bsc-qanet stall of 2026-04-20. When `finalize_new_header` ran
    /// slightly past the planned block timestamp, the wall-clock ceiling branch in
    /// `block_time_for_ramanujan_fork` recomputed a millisecond_timestamp that crossed a
    /// second boundary, so `set_millisecond_part_of_timestamp` wrote a stale ms onto the
    /// already-fixed `header.timestamp` (seconds). The sealed header's effective
    /// millisecond timestamp was 1 second behind the required floor and every peer
    /// rejected it with "Block time is too early" / "timestamp in the past".
    ///
    /// Fix: `finalize_new_header` takes the planned ms cached by `prepare_timestamp` and
    /// uses it directly; it never re-runs `block_time_for_ramanujan_fork`. This matches
    /// go-bsc where `Prepare()` decides the timestamp once and `Seal()` never recomputes.
    #[test]
    fn finalize_new_header_preserves_planned_ms_regardless_of_wall_clock() {
        use crate::consensus::parlia::util::calculate_millisecond_timestamp;
        ensure_test_signer();
        let parlia = Arc::new(Parlia::new(lorentz_chain_spec(), 200));

        // Parent: seconds 1_776_727_552, ms 500 → parent_ms = 1_776_727_552_500
        let mut parent_hdr =
            Header { number: 1, timestamp: 1_776_727_552, ..Default::default() };
        set_millisecond_part_of_timestamp(1_776_727_552_500, &mut parent_hdr);
        let parent_sealed = SealedHeader::seal_slow(parent_hdr);

        let validator = Address::with_last_byte(1);
        let mut parent_snap =
            Snapshot::new(vec![validator], 1, parent_sealed.hash(), 500, None);
        parent_snap.block_interval = 450; // Fermi

        // Single-validator snapshot ⇒ sole validator is always in-turn ⇒ back_off_time = 0.
        // Planned ms = parent_ms + block_interval + back_off_time.
        let planned_ms: u64 = 1_776_727_552_500 + 450; // 1_776_727_552_950

        // Simulate the header state right before finalize runs: prepare_timestamp has
        // fixed header.timestamp (seconds) and mix_hash (ms), then the EVM assembler
        // overwrote mix_hash with BlockEnv.prevrandao (= difficulty in BSC).
        let mut header = Header {
            number: 2, // <3: assemble_vote_attestation short-circuits to Ok(())
            parent_hash: parent_sealed.hash(),
            beneficiary: validator,
            timestamp: planned_ms / 1000,
            mix_hash: B256::repeat_byte(0xAB), // prevrandao garbage from assembler
            extra_data: Bytes::from(vec![0u8; EXTRA_VANITY_LEN]),
            ..Default::default()
        };

        let sp: Arc<dyn SnapshotProvider + Send + Sync> =
            Arc::new(MockSnapshotProvider { snapshot: parent_snap.clone() });

        finalize_new_header(
            parlia,
            &parent_snap,
            &parent_sealed,
            &mut header,
            &sp,
            planned_ms,
            None, // block 2 is not an epoch block at epoch_num 500
        )
        .expect("finalize_new_header should succeed");

        assert_eq!(
            calculate_millisecond_timestamp(&header),
            planned_ms,
            "finalize_new_header must preserve the planned millisecond timestamp exactly — \
             the wall-clock ceiling path that caused the 2026-04-20 qanet stall must not apply"
        );
        assert_eq!(header.timestamp, planned_ms / 1000);
    }
}
