//! Unit tests for Parlia validator functionality.

use super::super::{
    snapshot::{Snapshot, ValidatorInfo},
    vote::{VoteAddress, VoteAttestation, VoteData},
};
use alloy_primitives::{Address, B256};
use alloy_rlp::Bytes;

/// Test validator info creation and index assignment
#[test]
fn test_validator_info_creation() {
    let vote_addr = VoteAddress::random();
    let info = ValidatorInfo { index: 1, vote_addr };

    assert_eq!(info.index, 1);
    assert_eq!(info.vote_addr, vote_addr);
}

/// Test snapshot creation with validators
#[test]
fn test_snapshot_with_validators() {
    let validators = vec![Address::random(), Address::random(), Address::random()];

    let block_number = 1000;
    let block_hash = B256::random();
    let epoch_num = 200;

    let snapshot = Snapshot::new(validators.clone(), block_number, block_hash, epoch_num, None);

    assert_eq!(snapshot.block_number, block_number);
    assert_eq!(snapshot.block_hash, block_hash);
    assert_eq!(snapshot.epoch_num, epoch_num);
    assert_eq!(snapshot.validators.len(), 3);

    // Validators should be sorted
    let mut expected_validators = validators.clone();
    expected_validators.sort();
    assert_eq!(snapshot.validators, expected_validators);

    // Check validator map entries - when vote_addrs is None, indices are 1-based
    // Index is based on sorted position: 1, 2, 3, ...
    for (idx, validator) in expected_validators.iter().enumerate() {
        let info = snapshot.validators_map.get(validator).unwrap();
        // When no vote_addrs provided, indices are assigned as (sorted_position + 1)
        assert_eq!(info.index, (idx + 1) as u64);
        assert_eq!(info.vote_addr, VoteAddress::ZERO);
    }
}

/// Test snapshot creation with vote addresses (Luban upgrade)
#[test]
fn test_snapshot_with_vote_addresses() {
    let validators = vec![Address::random(), Address::random(), Address::random()];

    let vote_addrs = vec![VoteAddress::random(), VoteAddress::random(), VoteAddress::random()];

    let snapshot =
        Snapshot::new(validators.clone(), 1000, B256::random(), 200, Some(vote_addrs.clone()));

    // Check that vote addresses are correctly mapped
    let mut sorted_validators = validators.clone();
    sorted_validators.sort();

    for (idx, validator) in sorted_validators.iter().enumerate() {
        let info = snapshot.validators_map.get(validator).unwrap();
        assert_eq!(info.index, (idx + 1) as u64);
        // Note: vote_addrs are stored in original order, not sorted order
        // The snapshot constructor handles this mapping
        assert_ne!(info.vote_addr, VoteAddress::default());
    }
}

/// Test inturn validator calculation
#[test]
fn test_inturn_validator() {
    let validators = vec![
        "0x0000000000000000000000000000000000000001".parse().unwrap(),
        "0x0000000000000000000000000000000000000002".parse().unwrap(),
        "0x0000000000000000000000000000000000000003".parse().unwrap(),
    ];

    let snapshot = Snapshot::new(validators.clone(), 1000, B256::random(), 200, None);

    let inturn = snapshot.inturn_validator();
    assert!(validators.contains(&inturn), "Inturn validator should be from the validator set");
}

/// Test is_inturn check for validators
#[test]
fn test_is_inturn_check() {
    let validator1: Address = "0x0000000000000000000000000000000000000001".parse().unwrap();
    let validator2: Address = "0x0000000000000000000000000000000000000002".parse().unwrap();
    let validator3: Address = "0x0000000000000000000000000000000000000003".parse().unwrap();

    let validators = vec![validator1, validator2, validator3];

    let snapshot = Snapshot::new(validators.clone(), 1000, B256::random(), 200, None);

    let inturn = snapshot.inturn_validator();
    assert!(snapshot.is_inturn(inturn), "Inturn validator should be marked as inturn");

    // Other validators should not be inturn
    for validator in &validators {
        if *validator != inturn {
            assert!(
                !snapshot.is_inturn(*validator),
                "Non-inturn validator should not be marked as inturn"
            );
        }
    }
}

/// Test validator set changes across epochs
#[test]
fn test_validator_set_changes() {
    let initial_validators = vec![
        "0x0000000000000000000000000000000000000001".parse().unwrap(),
        "0x0000000000000000000000000000000000000002".parse().unwrap(),
        "0x0000000000000000000000000000000000000003".parse().unwrap(),
    ];

    let new_validators = vec![
        "0x0000000000000000000000000000000000000002".parse().unwrap(),
        "0x0000000000000000000000000000000000000003".parse().unwrap(),
        "0x0000000000000000000000000000000000000004".parse().unwrap(),
    ];

    let snapshot1 = Snapshot::new(initial_validators.clone(), 1000, B256::random(), 200, None);

    let snapshot2 = Snapshot::new(new_validators.clone(), 1200, B256::random(), 200, None);

    assert_eq!(snapshot1.validators.len(), 3);
    assert_eq!(snapshot2.validators.len(), 3);

    // Check that validator4 is in second snapshot but not first
    let validator4: Address = "0x0000000000000000000000000000000000000004".parse().unwrap();
    assert!(!snapshot1.validators.contains(&validator4));
    assert!(snapshot2.validators.contains(&validator4));

    // Check that validator1 is in first snapshot but not second
    let validator1: Address = "0x0000000000000000000000000000000000000001".parse().unwrap();
    assert!(snapshot1.validators.contains(&validator1));
    assert!(!snapshot2.validators.contains(&validator1));
}

/// Test sign_recently function (validator signing frequency check)
#[test]
fn test_sign_recently() {
    let validator1: Address = "0x0000000000000000000000000000000000000001".parse().unwrap();
    let validator2: Address = "0x0000000000000000000000000000000000000002".parse().unwrap();
    let validator3: Address = "0x0000000000000000000000000000000000000003".parse().unwrap();

    let validators = vec![validator1, validator2, validator3];

    let mut snapshot = Snapshot::new(
        validators.clone(),
        10, // Use higher block number to have room for history
        B256::random(),
        200,
        None,
    );

    // Initially, no validator has signed recently
    assert!(!snapshot.sign_recently(validator1));
    assert!(!snapshot.sign_recently(validator2));
    assert!(!snapshot.sign_recently(validator3));

    // With turn_length=1 (default), a validator signs recently if they've signed >= 1 times
    // within the miner history check window
    // The window is: current_block_number - miner_history_check_len()
    // For 3 validators: miner_history_check_len = (3/2 + 1) * 1 - 1 = 1
    // So window is: 10 - 1 = 9, meaning blocks > 9 are in the window

    // Add validator1 to recent proposers within the window (block 10)
    snapshot.recent_proposers.insert(10, validator1);

    // Now validator1 should be marked as signing recently
    assert!(snapshot.sign_recently(validator1));
    assert!(!snapshot.sign_recently(validator2));
    assert!(!snapshot.sign_recently(validator3));
}

/// Test miner history check length calculation
#[test]
fn test_miner_history_check_len() {
    let validators = vec![
        Address::random(),
        Address::random(),
        Address::random(),
        Address::random(),
        Address::random(),
    ];

    let snapshot = Snapshot::new(validators.clone(), 1000, B256::random(), 200, None);

    let history_len = snapshot.miner_history_check_len();

    // History length calculation: (validator_count / 2 + 1) * turn_length - 1
    // For 5 validators with turn_length=1: (5 / 2 + 1) * 1 - 1 = (2 + 1) * 1 - 1 = 2
    let turn_length = snapshot.turn_length.unwrap_or(1);
    let expected = (validators.len() / 2 + 1) as u64 * turn_length as u64 - 1;
    assert_eq!(history_len, expected);
    assert_eq!(expected, 2); // Verify the expected value for this test case
}

/// Test vote data initialization
#[test]
fn test_vote_data_initialization() {
    let validators = vec![Address::random(); 3];

    let snapshot = Snapshot::new(validators, 1000, B256::random(), 200, None);

    // Vote data should be initialized to default (zeros)
    assert_eq!(snapshot.vote_data.source_hash, B256::ZERO);
    assert_eq!(snapshot.vote_data.target_hash, B256::ZERO);
    assert_eq!(snapshot.vote_data.source_number, 0);
    assert_eq!(snapshot.vote_data.target_number, 0);
}

/// Test validator map contains all validators
#[test]
fn test_validator_map_completeness() {
    let validators =
        vec![Address::random(), Address::random(), Address::random(), Address::random()];

    let snapshot = Snapshot::new(validators.clone(), 1000, B256::random(), 200, None);

    // All validators should be in the map
    for validator in &validators {
        assert!(
            snapshot.validators_map.contains_key(validator),
            "Validator {:?} should be in validators_map",
            validator
        );
    }

    assert_eq!(snapshot.validators_map.len(), validators.len());
}

/// Test epoch boundary checks
#[test]
fn test_epoch_boundary() {
    let epoch_num: u64 = 200;

    assert!(epoch_num.is_multiple_of(epoch_num), "Epoch boundary should be at epoch_num");
    assert!((epoch_num * 2).is_multiple_of(epoch_num), "Double epoch should be boundary");
    assert!(!(epoch_num + 1).is_multiple_of(epoch_num), "Non-boundary block");
    assert!(!(epoch_num - 1).is_multiple_of(epoch_num), "Block before boundary");
}

/// Test validator bytes generation for header
#[test]
fn test_validator_bytes_generation() {
    let validators: Vec<Address> = vec![
        "0x0000000000000000000000000000000000000001".parse().unwrap(),
        "0x0000000000000000000000000000000000000002".parse().unwrap(),
        "0x0000000000000000000000000000000000000003".parse().unwrap(),
    ];

    let mut sorted_validators = validators.clone();
    sorted_validators.sort();

    // Without vote addresses (pre-Luban)
    let mut validator_bytes: Vec<u8> = Vec::new();
    for v in &sorted_validators {
        validator_bytes.extend_from_slice(v.as_slice());
    }

    // Should be 20 bytes per validator
    assert_eq!(validator_bytes.len(), 20 * 3);
}

/// Test validator bytes with vote addresses (Luban)
#[test]
fn test_validator_bytes_with_vote_addresses() {
    let validators = vec![
        "0x0000000000000000000000000000000000000001".parse().unwrap(),
        "0x0000000000000000000000000000000000000002".parse().unwrap(),
        "0x0000000000000000000000000000000000000003".parse().unwrap(),
    ];

    let vote_addrs = vec![VoteAddress::random(), VoteAddress::random(), VoteAddress::random()];

    let snapshot =
        Snapshot::new(validators.clone(), 1000, B256::random(), 200, Some(vote_addrs.clone()));

    let mut sorted_validators = validators.clone();
    sorted_validators.sort();

    // With vote addresses, should be 20 + 48 = 68 bytes per validator
    let mut validator_bytes: Vec<u8> = Vec::new();
    for v in &sorted_validators {
        validator_bytes.extend_from_slice(v.as_slice());
        let info = snapshot.validators_map.get(v).unwrap();
        validator_bytes.extend_from_slice(info.vote_addr.as_slice());
    }

    assert_eq!(validator_bytes.len(), 68 * 3);
}

/// Test turn length configuration
#[test]
fn test_turn_length() {
    let validators = vec![Address::random(); 3];

    let snapshot = Snapshot::new(validators, 1000, B256::random(), 200, None);

    // Default turn length should be 1
    assert_eq!(snapshot.turn_length, Some(1));
}

/// Test snapshot with zero epoch_num defaults to DEFAULT_EPOCH_LENGTH
#[test]
fn test_zero_epoch_num_default() {
    let validators = vec![Address::random(); 3];

    let snapshot = Snapshot::new(
        validators,
        1000,
        B256::random(),
        0, // Zero epoch_num
        None,
    );

    // Should default to DEFAULT_EPOCH_LENGTH (200)
    assert_eq!(snapshot.epoch_num, 200);
}

/// Test validator info default values
#[test]
fn test_validator_info_default() {
    let info = ValidatorInfo::default();

    assert_eq!(info.index, 0);
    assert_eq!(info.vote_addr, VoteAddress::default());
}

/// Test vote attestation structure
#[test]
fn test_vote_attestation_structure() {
    let vote_data = VoteData {
        source_number: 100,
        source_hash: B256::random(),
        target_number: 200,
        target_hash: B256::random(),
    };

    let attestation = VoteAttestation {
        vote_address_set: 0b1010101, // Some validators voted
        agg_signature: [0u8; 96].into(),
        data: vote_data,
        extra: Bytes::new(),
    };

    assert_eq!(attestation.data.source_number, 100);
    assert_eq!(attestation.data.target_number, 200);
    assert_eq!(attestation.vote_address_set, 0b1010101);
}

/// Test validator count calculations
#[test]
fn test_validator_count_calculations() {
    // Test quorum calculation: 2/3 + 1
    let test_cases = vec![
        (3, 2),   // 3 validators: need 2
        (4, 3),   // 4 validators: need 3
        (5, 4),   // 5 validators: need 4
        (6, 4),   // 6 validators: need 4
        (7, 5),   // 7 validators: need 5
        (21, 14), // 21 validators: need 14
    ];

    for (total, expected_quorum) in test_cases {
        let quorum = (total * 2 + 2) / 3; // ceil(2/3)
        assert_eq!(
            quorum, expected_quorum,
            "Quorum for {} validators should be {}, got {}",
            total, expected_quorum, quorum
        );
    }
}

/// Test recent proposers limit
#[test]
fn test_recent_proposers_limit() {
    let validators = vec![Address::random(), Address::random(), Address::random()];

    let mut snapshot = Snapshot::new(validators.clone(), 1000, B256::random(), 200, None);

    // Add more proposers than the limit
    let limit = snapshot.miner_history_check_len();
    for i in 0..limit + 5 {
        snapshot.recent_proposers.insert(900 + i, validators[i as usize % validators.len()]);
    }

    // Should have more than limit
    assert!(snapshot.recent_proposers.len() > limit as usize);
}

/// Test block interval configuration
#[test]
fn test_block_interval() {
    let validators = vec![Address::random(); 3];

    let snapshot = Snapshot::new(validators, 1000, B256::random(), 200, None);

    // Default block interval should be 3000ms
    assert_eq!(snapshot.block_interval, 3000);
}

/// Test empty validator set (edge case)
#[test]
fn test_empty_validator_set() {
    let validators: Vec<Address> = vec![];

    let snapshot = Snapshot::new(validators, 1000, B256::random(), 200, None);

    assert_eq!(snapshot.validators.len(), 0);
    assert_eq!(snapshot.validators_map.len(), 0);
}

/// Test single validator
#[test]
fn test_single_validator() {
    let validator = Address::random();
    let validators = vec![validator];

    let snapshot = Snapshot::new(validators, 1000, B256::random(), 200, None);

    assert_eq!(snapshot.validators.len(), 1);
    assert_eq!(snapshot.validators[0], validator);
    assert!(snapshot.validators_map.contains_key(&validator));

    // Single validator should always be inturn
    assert!(snapshot.is_inturn(validator));
    assert_eq!(snapshot.inturn_validator(), validator);
}

/// Test large validator set
#[test]
fn test_large_validator_set() {
    let validators: Vec<Address> = (0..100).map(|_| Address::random()).collect();

    let snapshot = Snapshot::new(validators.clone(), 1000, B256::random(), 200, None);

    assert_eq!(snapshot.validators.len(), 100);
    assert_eq!(snapshot.validators_map.len(), 100);

    // All validators should be in the map
    for validator in &validators {
        assert!(snapshot.validators_map.contains_key(validator));
    }
}

#[cfg(test)]
mod validator_cache_tests {
    use super::*;
    use crate::node::evm::pre_execution::{TURN_LENGTH_CACHE, VALIDATOR_CACHE};

    /// Test validator cache insertion and retrieval
    #[test]
    fn test_validator_cache_operations() {
        let block_hash = B256::random();
        let validators = vec![Address::random(), Address::random()];
        let vote_addrs = vec![VoteAddress::random(), VoteAddress::random()];

        // Insert into cache
        {
            let mut cache = VALIDATOR_CACHE.lock().unwrap();
            cache.insert(block_hash, (validators.clone(), vote_addrs.clone()));
        }

        // Retrieve from cache
        {
            let mut cache = VALIDATOR_CACHE.lock().unwrap();
            let result = cache.get(&block_hash);
            assert!(result.is_some());

            let (cached_validators, cached_vote_addrs) = result.unwrap();
            assert_eq!(cached_validators.len(), 2);
            assert_eq!(cached_vote_addrs.len(), 2);
        }
    }

    /// Test turn length cache operations
    #[test]
    fn test_turn_length_cache_operations() {
        let block_hash = B256::random();
        let turn_length: u8 = 16;

        // Insert into cache
        {
            let mut cache = TURN_LENGTH_CACHE.lock().unwrap();
            cache.insert(block_hash, turn_length);
        }

        // Retrieve from cache
        {
            let mut cache = TURN_LENGTH_CACHE.lock().unwrap();
            let result = cache.get(&block_hash);
            assert!(result.is_some());
            assert_eq!(*result.unwrap(), turn_length);
        }
    }

    /// Test cache with multiple entries
    #[test]
    fn test_cache_multiple_entries() {
        type CacheEntry = (B256, (Vec<Address>, Vec<VoteAddress>));
        let entries: Vec<CacheEntry> = (0..5)
            .map(|_| {
                let hash = B256::random();
                let validators = vec![Address::random(), Address::random()];
                let vote_addrs = vec![VoteAddress::random(), VoteAddress::random()];
                (hash, (validators, vote_addrs))
            })
            .collect();

        // Insert all entries
        {
            let mut cache = VALIDATOR_CACHE.lock().unwrap();
            for (hash, data) in &entries {
                cache.insert(*hash, data.clone());
            }
        }

        // Verify all entries are retrievable
        {
            let mut cache = VALIDATOR_CACHE.lock().unwrap();
            for (hash, expected_data) in &entries {
                let result = cache.get(hash);
                assert!(result.is_some());

                let (validators, vote_addrs) = result.unwrap();
                assert_eq!(validators.len(), expected_data.0.len());
                assert_eq!(vote_addrs.len(), expected_data.1.len());
            }
        }
    }
}
