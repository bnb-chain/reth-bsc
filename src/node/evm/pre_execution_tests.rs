//! Unit tests for validator pre-execution functionality.

#[cfg(test)]
mod tests {
    use crate::consensus::parlia::vote::VoteAddress;
    use crate::node::evm::pre_execution::{TURN_LENGTH_CACHE, VALIDATOR_CACHE};
    use alloy_primitives::{Address, B256};

    /// Test validator cache basic operations
    #[test]
    fn test_validator_cache_insert_and_get() {
        let block_hash = B256::random();
        let validator1 = Address::random();
        let validator2 = Address::random();
        let validators = vec![validator1, validator2];

        let vote_addr1 = VoteAddress::random();
        let vote_addr2 = VoteAddress::random();
        let vote_addrs = vec![vote_addr1, vote_addr2];

        // Insert into cache
        {
            let mut cache = VALIDATOR_CACHE.lock().unwrap();
            cache.insert(block_hash, (validators.clone(), vote_addrs.clone()));
        }

        // Retrieve and verify
        {
            let mut cache = VALIDATOR_CACHE.lock().unwrap();
            let result = cache.get(&block_hash);
            assert!(result.is_some());

            let (cached_validators, cached_vote_addrs) = result.unwrap();
            assert_eq!(cached_validators.len(), 2);
            assert_eq!(cached_vote_addrs.len(), 2);
            assert_eq!(cached_validators[0], validator1);
            assert_eq!(cached_validators[1], validator2);
        }
    }

    /// Test validator cache with empty vectors
    #[test]
    fn test_validator_cache_empty_vectors() {
        let block_hash = B256::random();
        let validators: Vec<Address> = vec![];
        let vote_addrs: Vec<VoteAddress> = vec![];

        {
            let mut cache = VALIDATOR_CACHE.lock().unwrap();
            cache.insert(block_hash, (validators.clone(), vote_addrs.clone()));
        }

        {
            let mut cache = VALIDATOR_CACHE.lock().unwrap();
            let result = cache.get(&block_hash);
            assert!(result.is_some());

            let (cached_validators, cached_vote_addrs) = result.unwrap();
            assert_eq!(cached_validators.len(), 0);
            assert_eq!(cached_vote_addrs.len(), 0);
        }
    }

    /// Test validator cache miss
    #[test]
    fn test_validator_cache_miss() {
        let block_hash = B256::random();

        let mut cache = VALIDATOR_CACHE.lock().unwrap();
        let result = cache.get(&block_hash);
        assert!(result.is_none());
    }

    /// Test validator cache with multiple different block hashes
    #[test]
    fn test_validator_cache_multiple_blocks() {
        let mut block_data = vec![];

        // Create test data
        for _ in 0..5 {
            let block_hash = B256::random();
            let validators = vec![Address::random(), Address::random()];
            let vote_addrs = vec![VoteAddress::random(), VoteAddress::random()];
            block_data.push((block_hash, validators, vote_addrs));
        }

        // Insert all
        {
            let mut cache = VALIDATOR_CACHE.lock().unwrap();
            for (hash, validators, vote_addrs) in &block_data {
                cache.insert(*hash, (validators.clone(), vote_addrs.clone()));
            }
        }

        // Verify all
        {
            let mut cache = VALIDATOR_CACHE.lock().unwrap();
            for (hash, expected_validators, expected_vote_addrs) in &block_data {
                let result = cache.get(hash);
                assert!(result.is_some());

                let (validators, vote_addrs) = result.unwrap();
                assert_eq!(validators.len(), expected_validators.len());
                assert_eq!(vote_addrs.len(), expected_vote_addrs.len());
            }
        }
    }

    /// Test validator cache overwrite
    #[test]
    fn test_validator_cache_overwrite() {
        let block_hash = B256::random();

        // First insert
        let validators1 = vec![Address::random()];
        let vote_addrs1 = vec![VoteAddress::random()];

        {
            let mut cache = VALIDATOR_CACHE.lock().unwrap();
            cache.insert(block_hash, (validators1.clone(), vote_addrs1.clone()));
        }

        // Second insert (overwrite)
        let validators2 = vec![Address::random(), Address::random()];
        let vote_addrs2 = vec![VoteAddress::random(), VoteAddress::random()];

        {
            let mut cache = VALIDATOR_CACHE.lock().unwrap();
            cache.insert(block_hash, (validators2.clone(), vote_addrs2.clone()));
        }

        // Verify second data is retrieved
        {
            let mut cache = VALIDATOR_CACHE.lock().unwrap();
            let result = cache.get(&block_hash);
            assert!(result.is_some());

            let (validators, vote_addrs) = result.unwrap();
            assert_eq!(validators.len(), 2);
            assert_eq!(vote_addrs.len(), 2);
        }
    }

    /// Test turn length cache basic operations
    #[test]
    fn test_turn_length_cache_insert_and_get() {
        let block_hash = B256::random();
        let turn_length: u8 = 16;

        // Insert
        {
            let mut cache = TURN_LENGTH_CACHE.lock().unwrap();
            cache.insert(block_hash, turn_length);
        }

        // Retrieve
        {
            let mut cache = TURN_LENGTH_CACHE.lock().unwrap();
            let result = cache.get(&block_hash);
            assert!(result.is_some());
            assert_eq!(*result.unwrap(), turn_length);
        }
    }

    /// Test turn length cache with different values
    #[test]
    fn test_turn_length_cache_different_values() {
        let test_cases = vec![
            (B256::random(), 1u8),
            (B256::random(), 8u8),
            (B256::random(), 16u8),
            (B256::random(), 32u8),
        ];

        // Insert all
        {
            let mut cache = TURN_LENGTH_CACHE.lock().unwrap();
            for (hash, length) in &test_cases {
                cache.insert(*hash, *length);
            }
        }

        // Verify all
        {
            let mut cache = TURN_LENGTH_CACHE.lock().unwrap();
            for (hash, expected_length) in &test_cases {
                let result = cache.get(hash);
                assert!(result.is_some());
                assert_eq!(*result.unwrap(), *expected_length);
            }
        }
    }

    /// Test turn length cache miss
    #[test]
    fn test_turn_length_cache_miss() {
        let block_hash = B256::random();

        let mut cache = TURN_LENGTH_CACHE.lock().unwrap();
        let result = cache.get(&block_hash);
        assert!(result.is_none());
    }

    /// Test turn length cache overwrite
    #[test]
    fn test_turn_length_cache_overwrite() {
        let block_hash = B256::random();

        // First insert
        {
            let mut cache = TURN_LENGTH_CACHE.lock().unwrap();
            cache.insert(block_hash, 8);
        }

        // Second insert (overwrite)
        {
            let mut cache = TURN_LENGTH_CACHE.lock().unwrap();
            cache.insert(block_hash, 16);
        }

        // Verify second value
        {
            let mut cache = TURN_LENGTH_CACHE.lock().unwrap();
            let result = cache.get(&block_hash);
            assert!(result.is_some());
            assert_eq!(*result.unwrap(), 16);
        }
    }

    /// Test validator cache with large validator set
    #[test]
    fn test_validator_cache_large_set() {
        let block_hash = B256::random();
        let validators: Vec<Address> = (0..100).map(|_| Address::random()).collect();
        let vote_addrs: Vec<VoteAddress> = (0..100).map(|_| VoteAddress::random()).collect();

        {
            let mut cache = VALIDATOR_CACHE.lock().unwrap();
            cache.insert(block_hash, (validators.clone(), vote_addrs.clone()));
        }

        {
            let mut cache = VALIDATOR_CACHE.lock().unwrap();
            let result = cache.get(&block_hash);
            assert!(result.is_some());

            let (cached_validators, cached_vote_addrs) = result.unwrap();
            assert_eq!(cached_validators.len(), 100);
            assert_eq!(cached_vote_addrs.len(), 100);
        }
    }

    /// Test validator cache with single validator
    #[test]
    fn test_validator_cache_single_validator() {
        let block_hash = B256::random();
        let validators = vec![Address::random()];
        let vote_addrs = vec![VoteAddress::random()];

        {
            let mut cache = VALIDATOR_CACHE.lock().unwrap();
            cache.insert(block_hash, (validators.clone(), vote_addrs.clone()));
        }

        {
            let mut cache = VALIDATOR_CACHE.lock().unwrap();
            let result = cache.get(&block_hash);
            assert!(result.is_some());

            let (cached_validators, cached_vote_addrs) = result.unwrap();
            assert_eq!(cached_validators.len(), 1);
            assert_eq!(cached_vote_addrs.len(), 1);
        }
    }

    /// Test concurrent cache access
    #[test]
    fn test_validator_cache_concurrent_access() {
        use std::thread;

        let mut handles = vec![];

        // Spawn multiple threads to insert data
        for i in 0..10 {
            let handle = thread::spawn(move || {
                let block_hash = B256::random();
                let validators = vec![Address::random(); i % 5 + 1];
                let vote_addrs = vec![VoteAddress::random(); i % 5 + 1];

                let mut cache = VALIDATOR_CACHE.lock().unwrap();
                cache.insert(block_hash, (validators.clone(), vote_addrs.clone()));
                block_hash
            });
            handles.push(handle);
        }

        // Wait for all threads
        let mut hashes = vec![];
        for handle in handles {
            let hash = handle.join().unwrap();
            hashes.push(hash);
        }

        // Verify all data is accessible
        {
            let mut cache = VALIDATOR_CACHE.lock().unwrap();
            for hash in hashes {
                let result = cache.get(&hash);
                assert!(result.is_some());
            }
        }
    }

    /// Test turn length cache concurrent access
    #[test]
    fn test_turn_length_cache_concurrent_access() {
        use std::thread;

        let mut handles = vec![];

        for i in 0..10 {
            let handle = thread::spawn(move || {
                let block_hash = B256::random();
                let turn_length = ((i % 4) + 1) as u8 * 8;

                let mut cache = TURN_LENGTH_CACHE.lock().unwrap();
                cache.insert(block_hash, turn_length);
                (block_hash, turn_length)
            });
            handles.push(handle);
        }

        let mut data = vec![];
        for handle in handles {
            let result = handle.join().unwrap();
            data.push(result);
        }

        // Verify all data
        {
            let mut cache = TURN_LENGTH_CACHE.lock().unwrap();
            for (hash, expected_length) in data {
                let result = cache.get(&hash);
                assert!(result.is_some());
                assert_eq!(*result.unwrap(), expected_length);
            }
        }
    }

    /// Test validator cache with mismatched lengths (edge case)
    #[test]
    fn test_validator_cache_mismatched_lengths() {
        let block_hash = B256::random();
        let validators = vec![Address::random(), Address::random(), Address::random()];
        let vote_addrs = vec![VoteAddress::random()]; // Mismatched length

        // Cache allows this, but application logic should validate
        {
            let mut cache = VALIDATOR_CACHE.lock().unwrap();
            cache.insert(block_hash, (validators.clone(), vote_addrs.clone()));
        }

        {
            let mut cache = VALIDATOR_CACHE.lock().unwrap();
            let result = cache.get(&block_hash);
            assert!(result.is_some());

            let (cached_validators, cached_vote_addrs) = result.unwrap();
            assert_eq!(cached_validators.len(), 3);
            assert_eq!(cached_vote_addrs.len(), 1);
        }
    }

    /// Test turn length with boundary values
    #[test]
    fn test_turn_length_boundary_values() {
        let test_cases =
            vec![(B256::random(), 0u8), (B256::random(), 1u8), (B256::random(), 255u8)];

        {
            let mut cache = TURN_LENGTH_CACHE.lock().unwrap();
            for (hash, length) in &test_cases {
                cache.insert(*hash, *length);
            }
        }

        {
            let mut cache = TURN_LENGTH_CACHE.lock().unwrap();
            for (hash, expected_length) in &test_cases {
                let result = cache.get(hash);
                assert!(result.is_some());
                assert_eq!(*result.unwrap(), *expected_length);
            }
        }
    }

    /// Test validator cache clearing behavior (if needed)
    #[test]
    fn test_validator_cache_multiple_operations() {
        let block_hash1 = B256::random();
        let block_hash2 = B256::random();

        // Insert first entry
        {
            let mut cache = VALIDATOR_CACHE.lock().unwrap();
            cache.insert(block_hash1, (vec![Address::random()], vec![VoteAddress::random()]));
        }

        // Insert second entry
        {
            let mut cache = VALIDATOR_CACHE.lock().unwrap();
            cache.insert(block_hash2, (vec![Address::random()], vec![VoteAddress::random()]));
        }

        // Both should be accessible
        {
            let mut cache = VALIDATOR_CACHE.lock().unwrap();
            assert!(cache.get(&block_hash1).is_some());
            assert!(cache.get(&block_hash2).is_some());
        }
    }
}
