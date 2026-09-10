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

/// Helper-level regression tests for parent-vs-current validator reads.
#[cfg(test)]
mod parent_block_env {
    use crate::chainspec::{bsc::bsc_mainnet, BscChainSpec};
    use crate::consensus::parlia::snapshot::MAXWELL_EPOCH_LENGTH;
    use crate::evm::api::BscEvm;
    use crate::node::evm::config::{
        evm_env_for_header, BscBlockExecutionCtx, BscExecutionMode, BscExecutionSharedCtx,
    };
    use crate::node::evm::executor::BscBlockExecutor;
    use crate::node::evm::pre_execution::{CallBlockEnv, VALIDATOR_CACHE};
    use crate::system_contracts::{SystemContract, VALIDATOR_CONTRACT};
    use alloy_consensus::Header;
    use alloy_evm::eth::EthBlockExecutionCtx;
    use alloy_primitives::{hex, Address, Bytes, U160};
    use reth_evm_ethereum::RethReceiptBuilder;
    use revm::bytecode::Bytecode;
    use revm::database::InMemoryDB;
    use revm::inspector::NoOpInspector;
    use revm::state::AccountInfo;
    use std::sync::Arc;

    /// Hard-coded interval used by the mock validator contract below.
    const SHUFFLE_INTERVAL: u64 = 200;

    /// Epoch block used by the fixture.
    const EPOCH_BLOCK: u64 = 115_596_000;

    /// Stand-in for the `ValidatorSet` contract. The returned validator address is
    /// `block.number / 200`, so the decoded set reflects the observed block env.
    ///
    /// ```text
    /// PUSH1 0x40   PUSH1 0x00 MSTORE   head[0] = 0x40   -> address[] at 0x40
    /// PUSH1 0x80   PUSH1 0x20 MSTORE   head[1] = 0x80   -> bytes[]   at 0x80
    /// PUSH1 0x01   PUSH1 0x40 MSTORE   address[].len = 1
    /// PUSH1 0xc8   NUMBER DIV
    ///              PUSH1 0x60 MSTORE   address[0] = NUMBER / 200
    /// PUSH1 0x01   PUSH1 0x80 MSTORE   bytes[].len = 1
    /// PUSH1 0x20   PUSH1 0xa0 MSTORE   bytes[0] data offset = 0x20, relative to 0xa0
    /// PUSH1 0x30   PUSH1 0xc0 MSTORE   bytes[0].len = 48; its 0xe0..0x120 payload stays zero
    /// PUSH2 0x0120 PUSH1 0x00 RETURN
    /// ```
    const WINDOW_REPORTING_VALIDATOR_SET: &[u8] = &hex!(
        "6040600052" // head[0]
        "6080602052" // head[1]
        "6001604052" // address[].len
        "60c84304606052" // address[0] = NUMBER / 200
        "6001608052" // bytes[].len
        "602060a052" // bytes[0] data offset
        "603060c052" // bytes[0].len
        "6101206000f3" // return mem[0x00..0x120]
    );

    /// The address the mock reports for a callee that observed `block_number`.
    fn window_address(block_number: u64) -> Address {
        Address::from(U160::from(block_number / SHUFFLE_INTERVAL))
    }

    fn header_at(number: u64) -> Header {
        Header {
            number,
            timestamp: 1_786_578_821, // real timestamp of EPOCH_BLOCK
            gas_limit: 140_000_000,
            // Cancun is long active at this timestamp, and a Cancun block env without it is
            // rejected outright.
            excess_blob_gas: Some(0),
            blob_gas_used: Some(0),
            ..Default::default()
        }
    }

    /// Provider-backed twin of the executor path.
    mod standalone {
        use super::*;
        use crate::node::evm::pre_execution::validators_at_parent;
        use alloy_primitives::{B256, U256};
        use reth_primitives_traits::{Account, Bytecode as PrimitivesBytecode, SealedHeader};
        use reth_provider::ProviderResult;
        use reth_revm::database::EvmStateProvider;

        /// Serves [`WINDOW_REPORTING_VALIDATOR_SET`] as the code of `VALIDATOR_CONTRACT`, or an
        /// empty state when `code` is `None` — i.e. the contract not deployed.
        struct ParentState {
            code: Option<PrimitivesBytecode>,
        }

        impl ParentState {
            fn code_hash(&self) -> B256 {
                self.code.as_ref().map(|c| c.hash_slow()).unwrap_or_default()
            }
        }

        impl EvmStateProvider for ParentState {
            fn basic_account(&self, address: &Address) -> ProviderResult<Option<Account>> {
                Ok((self.code.is_some() && *address == VALIDATOR_CONTRACT).then_some(Account {
                    nonce: 0,
                    balance: U256::ZERO,
                    bytecode_hash: Some(self.code_hash()),
                }))
            }

            fn block_hash(&self, _: u64) -> ProviderResult<Option<B256>> {
                Ok(None)
            }

            fn bytecode_by_hash(
                &self,
                code_hash: &B256,
            ) -> ProviderResult<Option<PrimitivesBytecode>> {
                Ok((code_hash == &self.code_hash()).then(|| self.code.clone()).flatten())
            }

            fn storage(&self, _: Address, _: B256) -> ProviderResult<Option<U256>> {
                Ok(None)
            }
        }

        fn parent_state() -> ParentState {
            let code = PrimitivesBytecode(Bytecode::new_raw(Bytes::from_static(
                WINDOW_REPORTING_VALIDATOR_SET,
            )));
            ParentState { code: Some(code) }
        }

        #[test]
        fn observes_the_parent_shuffle_window() {
            let spec = Arc::new(BscChainSpec::from(bsc_mainnet()));
            let parent = SealedHeader::seal_slow(header_at(EPOCH_BLOCK - 1));

            let (set, _) = validators_at_parent(parent_state(), spec, &parent).unwrap();

            assert_eq!(
                set,
                vec![window_address(EPOCH_BLOCK - 1)],
                "must draw from the parent env"
            );
        }

        /// A call to an address with no code succeeds with empty returndata, which the ABI unpack
        /// would `unwrap()`-panic on — inside a payload job or the bid loop, where a panic is
        /// permanent. It must surface as an error instead.
        #[test]
        fn missing_validator_set_contract_errors_rather_than_panicking() {
            let spec = Arc::new(BscChainSpec::from(bsc_mainnet()));
            let parent = SealedHeader::seal_slow(header_at(EPOCH_BLOCK - 1));

            let err = validators_at_parent(ParentState { code: None }, spec, &parent).unwrap_err();
            assert!(err.to_string().contains("no data"), "unexpected error: {err}");
        }

    }

    type TestExecutor = BscBlockExecutor<
        'static,
        BscEvm<InMemoryDB, NoOpInspector>,
        Arc<BscChainSpec>,
        RethReceiptBuilder,
    >;

    /// An executor positioned on `EPOCH_BLOCK`, with the mock installed at `ValidatorSet` and
    /// `parent_header` set as `check_new_block` sets it.
    fn executor() -> TestExecutor {
        let spec = Arc::new(BscChainSpec::from(bsc_mainnet()));
        let header = header_at(EPOCH_BLOCK);

        let mut db = InMemoryDB::default();
        db.insert_account_info(
            VALIDATOR_CONTRACT,
            AccountInfo::default()
                .with_code(Bytecode::new_raw(Bytes::from_static(WINDOW_REPORTING_VALIDATOR_SET))),
        );

        let evm =
            BscEvm::new(evm_env_for_header(&spec, &header), db, NoOpInspector {}, false, false);

        let ctx = BscBlockExecutionCtx {
            base: EthBlockExecutionCtx {
                parent_hash: header.parent_hash,
                parent_beacon_block_root: None,
                ommers: &[],
                withdrawals: None,
                extra_data: Bytes::new(),
                tx_count_hint: None,
                slot_number: None,
            },
            header: Some(header),
            header_hash: None,
            mode: BscExecutionMode::Import,
            validator_cache_sink: None,
            turn_length_sink: None,
            state_root_precomputed_sink: None,
            trie_handle: None,
            state_root_deadline_ms: None,
        };

        let mut executor = BscBlockExecutor::new(
            evm,
            ctx,
            BscExecutionSharedCtx::default(),
            spec.clone(),
            RethReceiptBuilder::default(),
            SystemContract::new(spec),
        );
        executor.inner_ctx.parent_header = Some(header_at(EPOCH_BLOCK - 1));
        executor
    }

    /// `Parent` reads the parent env; `Current` reads the current env.
    #[test]
    fn parent_env_call_observes_the_parent_shuffle_window() {
        assert_ne!(
            window_address(EPOCH_BLOCK - 1),
            window_address(EPOCH_BLOCK),
            "fixture is useless unless the two windows differ"
        );

        let mut executor = executor();

        let (validators, _) = executor
            .get_current_validators(EPOCH_BLOCK - 1, CallBlockEnv::Parent)
            .expect("parent-env call");
        assert_eq!(validators, vec![window_address(EPOCH_BLOCK - 1)], "must observe the parent");

        let (validators, _) = executor
            .get_current_validators(EPOCH_BLOCK - 1, CallBlockEnv::Current)
            .expect("current-env call");
        assert_eq!(validators, vec![window_address(EPOCH_BLOCK)], "must observe the current env");
    }

    /// On a cache miss, the miner resolves validators from parent state and env.
    #[test]
    fn the_miner_reads_the_parents_window_on_a_cache_miss() {
        use reth_provider::test_utils::{ExtendedAccount, MockEthProvider};

        let client = MockEthProvider::default();
        client.add_account(
            VALIDATOR_CONTRACT,
            ExtendedAccount::new(0, alloy_primitives::U256::ZERO)
                .with_bytecode(Bytes::from_static(WINDOW_REPORTING_VALIDATOR_SET)),
        );
        // A fresh hash forces the miss; `epoch_num` makes EPOCH_BLOCK the next epoch block.
        let parent = reth_primitives_traits::SealedHeader::new(
            header_at(EPOCH_BLOCK - 1),
            alloy_primitives::B256::random(),
        );
        let snap = crate::consensus::parlia::Snapshot::new(
            vec![Address::with_last_byte(1)],
            EPOCH_BLOCK - 1,
            parent.hash(),
            MAXWELL_EPOCH_LENGTH,
            None,
        );

        let resolved = crate::node::miner::util::epoch_validators_for_next_block(
            &client,
            &Arc::new(BscChainSpec::from(bsc_mainnet())),
            &snap,
            &parent,
        )
        .expect("state read on cache miss");

        assert_eq!(
            resolved.map(|(validators, _)| validators),
            Some(vec![window_address(EPOCH_BLOCK - 1)]),
            "the miner must evaluate at the parent, not at the epoch block"
        );
        // And file it under the parent, per VALIDATOR_CACHE's invariant.
        assert_eq!(
            VALIDATOR_CACHE.lock().unwrap().get(&parent.hash()).unwrap().0,
            vec![window_address(EPOCH_BLOCK - 1)],
        );
    }

    #[test]
    fn parent_env_call_requires_a_parent_header() {
        let mut executor = executor();
        executor.inner_ctx.parent_header = None;
        let err = executor
            .get_current_validators(EPOCH_BLOCK - 1, CallBlockEnv::Parent)
            .expect_err("must not silently fall back to the current env");
        assert!(err.to_string().contains("parent header"), "unexpected error: {err}");
    }
}
