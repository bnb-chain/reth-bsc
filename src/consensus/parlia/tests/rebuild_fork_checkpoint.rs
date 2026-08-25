//! Regression: the epoch checkpoint must be reached by `parent_hash`, not by block number
//! (geth `consensus/parlia/snapshot.go:384` `FindAncientHeader`). The by-number header cache
//! can hold a rejected sibling, which on a fork loads the other branch's validators.

use super::super::{
    provider::{EnhancedDbSnapshotProvider, SnapshotProvider},
    snapshot::Snapshot,
};
use super::snapshot_persistence::TestCleanup;
use crate::chainspec::{bsc_testnet, BscChainSpec};
use crate::consensus::parlia::constants::{EXTRA_SEAL_LEN, EXTRA_VANITY_LEN};
use crate::hardforks::BscHardforks;
use crate::node::evm::util::insert_header_to_cache_with_hash;
use alloy_consensus::Header;
use alloy_primitives::{Address, B256};
use reth_db::{init_db, mdbx::DatabaseArguments};
use std::sync::Arc;
use uuid::Uuid;

const EPOCH: u64 = 200;
const CHECKPOINT: u64 = 200;
/// +2 because 5 validators, turn_length 1 => miner_history_check_len == 2. Two hops, so an
/// off-by-one in the walk is caught.
const TRANSITION: u64 = CHECKPOINT + 2;

fn addrs(seed: u8) -> Vec<Address> {
    (0..5u8).map(|i| Address::repeat_byte(seed + i)).collect()
}

/// Empty `validators` -> plain header; otherwise a pre-Luban epoch checkpoint.
fn header(number: u64, parent: B256, validators: &[Address]) -> Header {
    let mut extra = vec![0u8; EXTRA_VANITY_LEN];
    validators.iter().for_each(|v| extra.extend_from_slice(v.as_slice()));
    extra.extend_from_slice(&[0u8; EXTRA_SEAL_LEN]);
    Header {
        number,
        parent_hash: parent,
        beneficiary: Address::repeat_byte(0x10),
        extra_data: extra.into(),
        ..Default::default()
    }
}

#[test]
fn epoch_checkpoint_is_read_from_the_branch_being_rebuilt() -> eyre::Result<()> {
    let db_path = std::env::temp_dir().join(format!("bsc_fork_checkpoint_{}", Uuid::new_v4()));
    std::fs::create_dir_all(&db_path)?;
    let _cleanup = TestCleanup { path: db_path.clone() };
    let db = init_db(&db_path, DatabaseArguments::new(Default::default()))?;
    let chain_spec = Arc::new(BscChainSpec::from(bsc_testnet()));
    // Both matter: Luban changes the validator layout, Bohr makes `extra[32]` a validator
    // count and the hand-built extra_data would parse as garbage.
    assert!(!chain_spec.is_luban_active_at_block(TRANSITION), "extra_data above is pre-Luban");
    assert!(!chain_spec.is_bohr_active_at_timestamp(TRANSITION, 0), "no turn_length byte");
    let provider = EnhancedDbSnapshotProvider::new(db, 256, chain_spec);

    let base = addrs(0x10);
    let (canon_v, fork_v) = (addrs(0x20), addrs(0x30));
    let grandparent = B256::repeat_byte(0xaa);

    // Insert order matters: the by-number map keeps only the last writer, so after this loop
    // by-number[CHECKPOINT] is the *fork* checkpoint. Any fall back to height therefore makes
    // the canonical branch load the fork's validators, and the first assertion below fails.
    let mut tips = Vec::new();
    for validators in [&canon_v, &fork_v] {
        let cp = header(CHECKPOINT, grandparent, validators);
        let mid = header(CHECKPOINT + 1, cp.hash_slow(), &[]);
        let mid_hash = mid.hash_slow();
        let next = header(TRANSITION, mid_hash, &[]);
        tips.push(next.hash_slow());
        // Seed at the intermediate block so the rebuild replays exactly one header.
        provider.insert(Snapshot::new(base.clone(), CHECKPOINT + 1, mid_hash, EPOCH, None));
        for h in [cp, mid, next] {
            insert_header_to_cache_with_hash(h, None);
        }
    }

    for (tip, mut expected) in tips.iter().zip([canon_v, fork_v]) {
        expected.sort();
        let snap = provider.snapshot_by_hash(tip).expect("branch snapshot should rebuild");
        assert_eq!(snap.validators, expected, "branch must load its own checkpoint ancestor");
    }
    Ok(())
}
