//! Block and request-local execution data for the native BSC EVM.

use crate::{hardfork::BscHardfork, types::BscEvmTypes};
use alloy_primitives::{Address, B256, U256};
use alloy_rpc_types_eth::{state::StateOverride, BlockOverrides};
use evm2::{env::BlockEnv, EvmFeatures, Version};
use reth_evm::{
    BlockOverridesExt, EvmEnv, EvmTransactionValidationGasRules, EvmTransactionValidationLimits,
};
use std::collections::BTreeSet;

/// BSC execution environment with owned, request-local override metadata.
///
/// Precompile providers must capture `milli_remainder` and `code_overrides` when creating the
/// EVM. These fields belong to the full environment; evm2's opcode-visible block is `Copy`.
#[derive(Clone, Debug)]
pub struct BscEvmEnv {
    /// Inherited Ethereum instruction specification, selected by the BSC fork schedule.
    pub spec: BscHardfork,
    /// Runtime gas rules and validation flags.
    pub version: Version,
    /// Opcode-visible block fields.
    pub block: BlockEnv<BscEvmTypes>,
    /// Header fields remain distinct from the opcode representation selected by fork rules.
    pub header_difficulty: U256,
    /// Raw mix hash used when assembling synthetic headers.
    pub header_mix_hash: B256,
    /// Sub-second timestamp component carried by the BSC header's mix hash.
    pub milli_remainder: u64,
    /// Addresses whose explicit code overrides take precedence over dynamic precompiles.
    pub code_overrides: BTreeSet<Address>,
    /// Whether RPC supplied the blob fee explicitly, including zero.
    pub blob_base_fee_overridden: bool,
}

impl BscEvmEnv {
    /// Resolves a BSC environment from already validated block data.
    pub fn new(
        spec: BscHardfork,
        block: BlockEnv<BscEvmTypes>,
        chain_id: u64,
        milli_remainder: u64,
    ) -> Self {
        let mut version = Version::new(spec.into());
        version.chain_id = chain_id;
        Self {
            spec,
            version,
            header_difficulty: block.difficulty,
            header_mix_hash: B256::from(U256::from(milli_remainder)),
            block,
            milli_remainder,
            code_overrides: BTreeSet::new(),
            blob_base_fee_overridden: false,
        }
    }

    /// Computes milliseconds from the live seconds field with go-bsc's uint64 wrapping behavior.
    pub fn milli_timestamp(&self) -> u64 {
        self.block
            .timestamp
            .saturating_to::<u64>()
            .wrapping_mul(1000)
            .wrapping_add(self.milli_remainder)
    }
}

impl BlockOverridesExt for BscEvmEnv {
    fn apply_block_overrides_ext(&mut self, overrides: &BlockOverrides) -> Result<(), String> {
        // Validate before modifying metadata so an invalid override cannot leave a partial update.
        let remainder = overrides.random.map(|v| U256::from_be_bytes(v.0));
        if let Some(value) = remainder {
            if value >= U256::from(1000) {
                return Err(format!("block override \"prevRandao\" on BSC carries the millisecond remainder of the block timestamp (BEP-520/BEP-706) and must be less than 1000, got {value}"));
            }
        }
        self.blob_base_fee_overridden |= overrides.blob_base_fee.is_some();
        if overrides.time.is_some() {
            self.milli_remainder = 0;
            self.header_mix_hash = B256::ZERO;
        }
        if let Some(value) = remainder {
            self.milli_remainder = value.to();
            self.header_mix_hash = B256::from(value);
        }
        if let Some(difficulty) = overrides.difficulty {
            self.header_difficulty = difficulty;
        }
        // The selected instruction table reads difficulty before Paris and prevrandao after it.
        // Explicit overrides must have the same opcode-0x44 meaning in both cases.
        if let Some(value) = remainder.or(overrides.difficulty) {
            self.block.difficulty = value;
            self.block.prevrandao = value;
        }
        Ok(())
    }

    fn apply_state_overrides_ext(&mut self, overrides: &StateOverride) -> Result<(), String> {
        self.code_overrides.extend(
            overrides
                .iter()
                .filter_map(|(address, account)| account.code.is_some().then_some(*address)),
        );
        Ok(())
    }
}

impl EvmEnv for BscEvmEnv {
    type EvmTypes = BscEvmTypes;

    fn spec_id(&self) -> BscHardfork {
        self.spec
    }
    fn chain_id(&self) -> u64 {
        self.version.chain_id
    }
    fn block_env(&self) -> &BlockEnv<BscEvmTypes> {
        &self.block
    }
    fn block_env_mut(&mut self) -> &mut BlockEnv<BscEvmTypes> {
        &mut self.block
    }
    fn version(&self) -> &Version {
        &self.version
    }
    fn version_mut(&mut self) -> &mut Version {
        &mut self.version
    }
    fn block_base_fee(&self) -> u64 {
        self.block.basefee.saturating_to()
    }
    fn block_blob_base_fee(&self) -> u64 {
        self.block.blob_basefee.saturating_to()
    }

    fn transaction_validation_limits(&self) -> EvmTransactionValidationLimits {
        EvmTransactionValidationLimits {
            max_initcode_size: self.version.max_initcode_size,
            tx_gas_limit_cap: if self.version.feature(EvmFeatures::EIP8037) ||
                self.version.tx_gas_limit_cap == u64::MAX
            {
                0
            } else {
                self.version.tx_gas_limit_cap
            },
        }
    }
    fn transaction_validation_gas_rules(&self) -> EvmTransactionValidationGasRules {
        EvmTransactionValidationGasRules { version: self.version }
    }
    fn uses_separate_block_gas(&self) -> bool {
        self.version.feature(EvmFeatures::EIP8037)
    }
    fn regular_gas_limit_cap(&self) -> u64 {
        self.version.tx_gas_limit_cap
    }
    fn with_nonce_check_disabled(mut self) -> Self {
        self.version.features.remove(EvmFeatures::NONCE_CHECK);
        self
    }
    fn with_balance_check_disabled(mut self) -> Self {
        self.version.features.remove(EvmFeatures::BALANCE_CHECK);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, Bytes, B256};
    use alloy_rpc_types_eth::state::AccountOverride;

    fn env() -> BscEvmEnv {
        BscEvmEnv::new(
            BscHardfork::Osaka,
            BlockEnv::<BscEvmTypes> {
                timestamp: U256::from(1_790_000_000u64),
                ..Default::default()
            },
            56,
            750,
        )
    }

    #[test]
    fn live_seconds_and_override_precedence() {
        let mut env = env();
        assert_eq!(env.milli_timestamp(), 1_790_000_000_750);
        env.block_env_mut().timestamp = U256::from(1_800_000_000u64);
        assert_eq!(env.milli_timestamp(), 1_800_000_000_750);
        env.apply_block_overrides_ext(&BlockOverrides {
            time: Some(1_800_000_000),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(env.milli_timestamp(), 1_800_000_000_000);
        env.apply_block_overrides_ext(&BlockOverrides {
            time: Some(1_800_000_000),
            random: Some(B256::from(U256::from(999))),
            difficulty: Some(U256::from(42)),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(env.milli_timestamp(), 1_800_000_000_999);
        assert_eq!(env.block.difficulty, U256::from(999));
        assert_eq!(env.block.prevrandao, U256::from(999));
    }

    #[test]
    fn invalid_full_width_remainder_does_not_mutate_metadata() {
        for value in [U256::from(1000), U256::from(1) << 200, U256::MAX] {
            let mut env = env();
            assert!(env
                .apply_block_overrides_ext(&BlockOverrides {
                    time: Some(1),
                    random: Some(B256::from(value)),
                    ..Default::default()
                })
                .is_err());
            assert_eq!(env.milli_remainder, 750);
        }
    }

    #[test]
    fn code_override_is_owned_idempotent_and_includes_empty_code() {
        let cas20 = address!("ca52000000000000000000000000000000000001");
        let balance_only = Address::with_last_byte(3);
        let mut overrides = StateOverride::default();
        overrides.insert(cas20, AccountOverride { code: Some(Bytes::new()), ..Default::default() });
        overrides.insert(
            balance_only,
            AccountOverride { balance: Some(U256::from(1)), ..Default::default() },
        );
        let original = env();
        let mut request = original.clone();
        request.apply_state_overrides_ext(&overrides).unwrap();
        request.apply_state_overrides_ext(&overrides).unwrap();
        assert_eq!(request.code_overrides, BTreeSet::from([cas20]));
        assert!(original.code_overrides.is_empty());
        let mut next_request = request.clone();
        next_request.code_overrides.clear();
        assert_eq!(request.code_overrides, BTreeSet::from([cas20]));
    }

    #[test]
    fn timestamp_wraps_like_go_and_validation_flags_are_independent() {
        let mut env = env();
        env.block.timestamp = U256::from(u64::MAX);
        assert_eq!(env.milli_timestamp(), u64::MAX.wrapping_mul(1000).wrapping_add(750));
        let original = env.clone();
        let without_nonce = env.with_nonce_check_disabled();
        assert!(!without_nonce.version.feature(EvmFeatures::NONCE_CHECK));
        assert!(without_nonce.version.feature(EvmFeatures::BALANCE_CHECK));
        assert!(original.version.feature(EvmFeatures::NONCE_CHECK));
        let without_balance = original.with_balance_check_disabled();
        assert!(without_balance.version.feature(EvmFeatures::NONCE_CHECK));
        assert!(!without_balance.version.feature(EvmFeatures::BALANCE_CHECK));
    }
}
