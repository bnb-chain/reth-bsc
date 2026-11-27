use super::*;
use alloy_primitives::B256;
use revm::context::Block;

#[test]
fn pending_difficulty_attributes_and_environment_are_deterministic() {
    let config =
        BscEvmConfig::new(Arc::new(BscChainSpec::from(crate::chainspec::bsc::bsc_mainnet())));
    for timestamp in [1_700_000_000, 1_780_000_000] {
        for difficulty in [1, 2] {
            let parent = SealedHeader::seal_slow(Header {
                number: 50_000_000,
                timestamp,
                difficulty: U256::from(difficulty),
                gas_limit: 30_000_000,
                base_fee_per_gas: Some(0),
                excess_blob_gas: Some(0),
                blob_gas_used: Some(0),
                ..Default::default()
            });
            for _ in 0..3 {
                let attrs = BscNextBlockEnvAttributes::build_pending_env(&parent, None);
                assert_eq!(attrs.inner.prev_randao, B256::from(parent.difficulty));
                let env = config.next_evm_env(&parent, &attrs).unwrap();
                let env = config.with_block_rules(env);
                assert_eq!(env.block_env.difficulty(), parent.difficulty);
                assert_eq!(env.block_env.prevrandao(), Some(parent.difficulty.into()));
                // The assembler reads raw fields: synthetic headers retain difficulty,
                // but mixHash stays zero rather than accidentally encoding milliseconds.
                assert_eq!(env.block_env.inner.difficulty, parent.difficulty);
                assert_eq!(env.block_env.inner.prevrandao, Some(B256::ZERO));
                assert_eq!(env.block_env.milli_remainder, 0);
            }
        }
    }
}

#[test]
fn pending_difficulty_fix_preserves_producer_and_import_environments() {
    let config =
        BscEvmConfig::new(Arc::new(BscChainSpec::from(crate::chainspec::bsc::bsc_mainnet())));
    for difficulty in [1, 2] {
        let parent = SealedHeader::seal_slow(Header {
            number: 50_000_000,
            timestamp: 1_780_000_000,
            difficulty: U256::from(difficulty),
            gas_limit: 30_000_000,
            base_fee_per_gas: Some(0),
            excess_blob_gas: Some(0),
            blob_gas_used: Some(0),
            ..Default::default()
        });
        let imported = config.evm_env(&parent).unwrap();
        assert_eq!(imported.block_env.prevrandao(), Some(parent.difficulty.into()));
        assert_eq!(imported.block_env.opcode_44_override, None);
        // The next producer can have a different turn/difficulty from the parent.
        let producer_difficulty = U256::from(3 - difficulty);
        for mode in [BscExecutionMode::Mining, BscExecutionMode::BidSimulation] {
            let mut attrs = BscNextBlockEnvAttributes::build_pending_env(&parent, None);
            attrs.mode = mode;
            attrs.inner.prev_randao = producer_difficulty.into();
            attrs.milli_remainder = 450;
            let env = config.next_evm_env(&parent, &attrs).unwrap();
            assert_eq!(env.block_env.prevrandao(), Some(producer_difficulty.into()));
            assert_eq!(env.block_env.opcode_44_override, None);
            assert_eq!(env.block_env.milli_remainder, 450);
        }
    }
}
