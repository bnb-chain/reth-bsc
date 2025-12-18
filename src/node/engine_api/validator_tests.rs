//! Unit tests for BSC validator engine API functionality.

#[cfg(test)]
mod tests {
    use crate::node::engine_api::validator::{BscEngineValidator, BscExecutionData};
    use crate::chainspec::bsc::bsc_mainnet;
    use crate::{BscBlock, BscBlockBody};
    use crate::chainspec::BscChainSpec;
    use alloy_consensus::Header;
    use alloy_primitives::{Address, B256, U256};
    use std::sync::Arc;
    use reth_ethereum_primitives::BlockBody;
    use reth_engine_primitives::ExecutionPayload;

    /// Helper function to create a test block
    fn create_test_block(number: u64, parent_hash: B256) -> BscBlock {
        let header = Header {
            number,
            parent_hash,
            timestamp: 1234567890 + number * 3,
            gas_limit: 30_000_000,
            gas_used: 0,
            beneficiary: Address::random(),
            difficulty: U256::from(2),
            ..Default::default()
        };
        
        BscBlock {
            header,
            body: BscBlockBody {
                inner: BlockBody::default(),
                sidecars: None,
            },
        }
    }

    #[test]
    fn test_bsc_engine_validator_creation() {
        let chain_spec = Arc::new(BscChainSpec::from(bsc_mainnet()));
        let validator = BscEngineValidator::new(chain_spec);
        
        // Validator should be created successfully
        assert!(std::mem::size_of_val(&validator) > 0);
    }

    #[test]
    fn test_execution_data_parent_hash() {
        let parent_hash = B256::random();
        let block = create_test_block(1, parent_hash);
        let execution_data = BscExecutionData(block);
        
        assert_eq!(execution_data.parent_hash(), parent_hash);
    }

    #[test]
    fn test_execution_data_block_hash() {
        let parent_hash = B256::random();
        let block = create_test_block(1, parent_hash);
        let expected_hash = block.header.hash_slow();
        let execution_data = BscExecutionData(block);
        
        assert_eq!(execution_data.block_hash(), expected_hash);
    }

    #[test]
    fn test_execution_data_block_number() {
        let block_number = 12345;
        let block = create_test_block(block_number, B256::random());
        let execution_data = BscExecutionData(block);
        
        assert_eq!(execution_data.block_number(), block_number);
    }

    #[test]
    fn test_execution_data_timestamp() {
        let block = create_test_block(1, B256::random());
        let expected_timestamp = block.header.timestamp;
        let execution_data = BscExecutionData(block);
        
        assert_eq!(execution_data.timestamp(), expected_timestamp);
    }

    #[test]
    fn test_execution_data_gas_used() {
        let mut block = create_test_block(1, B256::random());
        block.header.gas_used = 1_000_000;
        let execution_data = BscExecutionData(block);
        
        assert_eq!(execution_data.gas_used(), 1_000_000);
    }

    #[test]
    fn test_execution_data_withdrawals_none() {
        let block = create_test_block(1, B256::random());
        let execution_data = BscExecutionData(block);
        
        // BSC doesn't support withdrawals
        assert!(execution_data.withdrawals().is_none());
    }

    #[test]
    fn test_execution_data_parent_beacon_block_root_none() {
        let block = create_test_block(1, B256::random());
        let execution_data = BscExecutionData(block);
        
        // BSC doesn't use parent beacon block root
        assert!(execution_data.parent_beacon_block_root().is_none());
    }

    #[test]
    fn test_engine_validator_debug_format() {
        let chain_spec = Arc::new(BscChainSpec::from(bsc_mainnet()));
        let validator = BscEngineValidator::new(chain_spec);
        
        let debug_str = format!("{:?}", validator);
        assert!(!debug_str.is_empty());
    }

    #[test]
    fn test_engine_validator_clone() {
        let chain_spec = Arc::new(BscChainSpec::from(bsc_mainnet()));
        let validator = BscEngineValidator::new(chain_spec);
        
        let cloned_validator = validator.clone();
        
        // Both validators should be usable
        assert!(std::mem::size_of_val(&validator) > 0);
        assert!(std::mem::size_of_val(&cloned_validator) > 0);
    }

    #[test]
    fn test_execution_data_serialization() {
        let block = create_test_block(1, B256::random());
        let execution_data = BscExecutionData(block);
        
        // Test that BscExecutionData can be serialized
        let serialized = serde_json::to_string(&execution_data);
        assert!(serialized.is_ok(), "ExecutionData should be serializable");
    }

    #[test]
    fn test_execution_data_deserialization() {
        let block = create_test_block(1, B256::random());
        let execution_data = BscExecutionData(block);
        
        // Serialize and then deserialize
        let serialized = serde_json::to_string(&execution_data).unwrap();
        let deserialized: Result<BscExecutionData, _> = serde_json::from_str(&serialized);
        
        assert!(deserialized.is_ok(), "ExecutionData should be deserializable");
    }

    #[test]
    fn test_execution_data_with_zero_gas() {
        let mut block = create_test_block(1, B256::random());
        block.header.gas_used = 0;
        let execution_data = BscExecutionData(block);
        
        assert_eq!(execution_data.gas_used(), 0);
    }

    #[test]
    fn test_execution_data_with_max_gas() {
        let mut block = create_test_block(1, B256::random());
        let gas_limit = block.header.gas_limit;
        block.header.gas_used = gas_limit;
        let execution_data = BscExecutionData(block);
        
        assert_eq!(execution_data.gas_used(), gas_limit);
    }

    #[test]
    fn test_execution_data_with_different_difficulties() {
        let difficulties = vec![
            U256::from(1),  // DIFF_NOTURN
            U256::from(2),  // DIFF_INTURN
        ];
        
        for difficulty in difficulties {
            let mut block = create_test_block(1, B256::random());
            block.header.difficulty = difficulty;
            let execution_data = BscExecutionData(block);
            
            assert_eq!(execution_data.block_number(), 1);
        }
    }

    #[test]
    fn test_execution_data_with_different_timestamps() {
        let timestamps = vec![
            1234567890u64,
            1640000000u64,  // ~2022
            1700000000u64,  // ~2023
            1740000000u64,  // ~2025
        ];
        
        for timestamp in timestamps {
            let mut block = create_test_block(1, B256::random());
            block.header.timestamp = timestamp;
            let execution_data = BscExecutionData(block);
            
            assert_eq!(execution_data.timestamp(), timestamp);
        }
    }
}
