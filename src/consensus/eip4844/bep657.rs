use alloy_consensus::BlockHeader;
use alloy_eips::eip7840::BlobParams;

use crate::hardforks::BscHardforks;

/// Blob-eligible block interval (BEP-657). Only blocks where N % interval == 0 can include blobs.
pub const BLOB_ELIGIBLE_BLOCK_INTERVAL: u64 = 5;

/// Returns whether blob transactions are allowed in the given block.
pub fn is_blob_eligible_block<ChainSpec: BscHardforks>(
    chain_spec: &ChainSpec,
    block_number: u64,
    timestamp: u64,
) -> bool {
    if !chain_spec.is_mendel_active_at_timestamp(block_number, timestamp) {
        return true;
    }
    block_number.is_multiple_of(BLOB_ELIGIBLE_BLOCK_INTERVAL)
}

/// Returns whether the excess blob gas should be recalculated for the given block.
pub fn should_recalculate_excess_blob_gas<ChainSpec: BscHardforks>(
    chain_spec: &ChainSpec,
    block_number: u64,
    timestamp: u64,
) -> bool {
    if !chain_spec.is_mendel_active_at_timestamp(block_number, timestamp) {
        return true;
    }
    block_number % BLOB_ELIGIBLE_BLOCK_INTERVAL == 1
}

/// Calculates the expected excess blob gas for the next block, applying BEP-657 rules when Mendel
/// is active.
pub fn next_block_excess_blob_gas_with_mendel<ChainSpec, H>(
    chain_spec: &ChainSpec,
    block_number: u64,
    timestamp: u64,
    parent: &H,
    blob_params: Option<BlobParams>,
) -> Option<u64>
where
    ChainSpec: BscHardforks,
    H: BlockHeader,
{
    let blob_params = blob_params?;

    if !should_recalculate_excess_blob_gas(chain_spec, block_number, timestamp) {
        return Some(parent.excess_blob_gas().unwrap_or(0));
    }

    parent
        .maybe_next_block_excess_blob_gas(Some(blob_params))
        .or_else(|| Some(BlobParams::cancun().next_block_excess_blob_gas_osaka(0, 0, 0)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hardforks::bsc::BscHardfork;
    use alloy_consensus::Header;
    use reth_chainspec::{ChainSpecBuilder, ForkCondition, EthereumHardfork};
    use crate::chainspec::BscChainSpec;
    use std::sync::Arc;

    /// Creates a chain spec with Mendel active at timestamp 1000
    /// London must be active at block 0 for Mendel to be considered active
    fn mendel_active_chain_spec() -> Arc<BscChainSpec> {
        Arc::new(BscChainSpec::from(
            ChainSpecBuilder::mainnet()
                .with_fork(EthereumHardfork::London, ForkCondition::Block(0))
                .with_fork(BscHardfork::Mendel, ForkCondition::Timestamp(1000))
                .build(),
        ))
    }

    /// Creates a chain spec with Mendel not active (far future)
    /// London is active so we can test pre-Mendel behavior
    fn mendel_inactive_chain_spec() -> Arc<BscChainSpec> {
        Arc::new(BscChainSpec::from(
            ChainSpecBuilder::mainnet()
                .with_fork(EthereumHardfork::London, ForkCondition::Block(0))
                .with_fork(BscHardfork::Mendel, ForkCondition::Timestamp(u64::MAX))
                .build(),
        ))
    }

    // ==================== is_blob_eligible_block tests ====================

    #[test]
    fn blob_eligible_before_mendel_all_blocks_allowed() {
        let spec = mendel_inactive_chain_spec();

        // Before Mendel, all blocks should be eligible for blobs
        for block_number in 0..20 {
            assert!(
                is_blob_eligible_block(&*spec, block_number, 500),
                "block {} should be eligible before Mendel",
                block_number
            );
        }
    }

    #[test]
    fn blob_eligible_after_mendel_only_multiples_of_5() {
        let spec = mendel_active_chain_spec();
        let timestamp = 2000; // After Mendel activation (1000)

        // Blocks 0, 5, 10, 15 should be eligible
        let eligible_blocks = [0, 5, 10, 15, 20, 25, 100, 1000];
        for block_number in eligible_blocks {
            assert!(
                is_blob_eligible_block(&*spec, block_number, timestamp),
                "block {} should be eligible (multiple of 5)",
                block_number
            );
        }

        // Blocks 1, 2, 3, 4, 6, 7, 8, 9 should NOT be eligible
        let ineligible_blocks = [1, 2, 3, 4, 6, 7, 8, 9, 11, 12, 13, 14, 16, 99, 101];
        for block_number in ineligible_blocks {
            assert!(
                !is_blob_eligible_block(&*spec, block_number, timestamp),
                "block {} should NOT be eligible (not multiple of 5)",
                block_number
            );
        }
    }

    #[test]
    fn blob_eligible_at_mendel_boundary() {
        let spec = mendel_active_chain_spec();

        // Just before Mendel (timestamp 999) - all blocks eligible
        assert!(is_blob_eligible_block(&*spec, 1, 999));
        assert!(is_blob_eligible_block(&*spec, 6, 999));

        // At Mendel activation (timestamp 1000) - only multiples of 5
        assert!(is_blob_eligible_block(&*spec, 0, 1000));
        assert!(is_blob_eligible_block(&*spec, 5, 1000));
        assert!(!is_blob_eligible_block(&*spec, 1, 1000));
        assert!(!is_blob_eligible_block(&*spec, 6, 1000));
    }

    // ==================== should_recalculate_excess_blob_gas tests ====================

    #[test]
    fn recalculate_excess_gas_before_mendel_always_true() {
        let spec = mendel_inactive_chain_spec();

        // Before Mendel, should always recalculate
        for block_number in 0..20 {
            assert!(
                should_recalculate_excess_blob_gas(&*spec, block_number, 500),
                "block {} should recalculate before Mendel",
                block_number
            );
        }
    }

    #[test]
    fn recalculate_excess_gas_after_mendel_only_n_mod_5_eq_1() {
        let spec = mendel_active_chain_spec();
        let timestamp = 2000;

        // Blocks where N % 5 == 1 should recalculate: 1, 6, 11, 16, 21...
        let recalculate_blocks = [1, 6, 11, 16, 21, 26, 101, 1001];
        for block_number in recalculate_blocks {
            assert!(
                should_recalculate_excess_blob_gas(&*spec, block_number, timestamp),
                "block {} should recalculate (N %% 5 == 1)",
                block_number
            );
        }

        // All other blocks should NOT recalculate
        let no_recalculate_blocks = [0, 2, 3, 4, 5, 7, 8, 9, 10, 12, 13, 14, 15, 100, 102];
        for block_number in no_recalculate_blocks {
            assert!(
                !should_recalculate_excess_blob_gas(&*spec, block_number, timestamp),
                "block {} should NOT recalculate (N %% 5 != 1)",
                block_number
            );
        }
    }

    #[test]
    fn recalculate_pattern_aligns_with_eligibility() {
        // Recalculation happens at N%5==1, which is the block AFTER an eligible block (N%5==0)
        // This ensures the fee is calculated once after each eligible block
        let spec = mendel_active_chain_spec();
        let timestamp = 2000;

        // Block 0: eligible, no recalc (uses initial state)
        assert!(is_blob_eligible_block(&*spec, 0, timestamp));
        assert!(!should_recalculate_excess_blob_gas(&*spec, 0, timestamp));

        // Block 1: not eligible, recalculate (based on block 0's blob usage)
        assert!(!is_blob_eligible_block(&*spec, 1, timestamp));
        assert!(should_recalculate_excess_blob_gas(&*spec, 1, timestamp));

        // Blocks 2-4: not eligible, no recalc (inherit from block 1)
        for n in 2..=4 {
            assert!(!is_blob_eligible_block(&*spec, n, timestamp));
            assert!(!should_recalculate_excess_blob_gas(&*spec, n, timestamp));
        }

        // Block 5: eligible, no recalc
        assert!(is_blob_eligible_block(&*spec, 5, timestamp));
        assert!(!should_recalculate_excess_blob_gas(&*spec, 5, timestamp));

        // Block 6: not eligible, recalculate (based on block 5's blob usage)
        assert!(!is_blob_eligible_block(&*spec, 6, timestamp));
        assert!(should_recalculate_excess_blob_gas(&*spec, 6, timestamp));
    }

    // ==================== next_block_excess_blob_gas_with_mendel tests ====================

    #[test]
    fn next_excess_gas_returns_none_without_blob_params() {
        let spec = mendel_active_chain_spec();
        let parent = Header::default();

        let result = next_block_excess_blob_gas_with_mendel(
            &*spec,
            10,
            2000,
            &parent,
            None, // No blob params
        );

        assert!(result.is_none());
    }

    #[test]
    fn next_excess_gas_inherits_parent_when_no_recalculate() {
        let spec = mendel_active_chain_spec();
        let timestamp = 2000;

        // Create parent with specific excess_blob_gas
        let parent = Header { excess_blob_gas: Some(12345), ..Default::default() };

        let blob_params = Some(BlobParams::cancun());

        // Block 2 should NOT recalculate (2 % 5 != 1), so it inherits parent's value
        let result = next_block_excess_blob_gas_with_mendel(
            &*spec,
            2,
            timestamp,
            &parent,
            blob_params,
        );

        assert_eq!(result, Some(12345));
    }

    #[test]
    fn next_excess_gas_inherits_zero_when_parent_has_none() {
        let spec = mendel_active_chain_spec();
        let timestamp = 2000;

        // Parent without excess_blob_gas
        let parent = Header::default();
        let blob_params = Some(BlobParams::cancun());

        // Block 3 should NOT recalculate, inherit 0
        let result = next_block_excess_blob_gas_with_mendel(
            &*spec,
            3,
            timestamp,
            &parent,
            blob_params,
        );

        assert_eq!(result, Some(0));
    }

    #[test]
    fn next_excess_gas_recalculates_at_correct_blocks() {
        let spec = mendel_active_chain_spec();
        let timestamp = 2000;

        // Create parent with blob gas values for recalculation
        let parent = Header {
            excess_blob_gas: Some(0),
            blob_gas_used: Some(0),
            ..Default::default()
        };

        let blob_params = Some(BlobParams::cancun());

        // Block 6 should recalculate (6 % 5 == 1)
        let result = next_block_excess_blob_gas_with_mendel(
            &*spec,
            6,
            timestamp,
            &parent,
            blob_params,
        );

        // With 0 excess and 0 used, recalculation should return 0
        assert!(result.is_some());
    }

    #[test]
    fn next_excess_gas_before_mendel_always_recalculates() {
        let spec = mendel_inactive_chain_spec();
        let timestamp = 500; // Before Mendel

        let parent = Header {
            excess_blob_gas: Some(1000),
            blob_gas_used: Some(0),
            ..Default::default()
        };

        let blob_params = Some(BlobParams::cancun());

        // Any block before Mendel should recalculate
        for block_number in [2, 3, 4, 7, 8, 9] {
            let result = next_block_excess_blob_gas_with_mendel(
                &*spec,
                block_number,
                timestamp,
                &parent,
                blob_params,
            );
            // Should recalculate, not just inherit 1000
            assert!(result.is_some());
        }
    }

    // ==================== BLOB_ELIGIBLE_BLOCK_INTERVAL constant test ====================

    #[test]
    fn blob_interval_is_five() {
        assert_eq!(BLOB_ELIGIBLE_BLOCK_INTERVAL, 5);
    }
}
