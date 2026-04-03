//! BSC-specific implementation of EIP-7910 `eth_config` RPC endpoint.
//!
//! This overrides the upstream `EthConfigHandler` to include BSC system contracts
//! and correctly handle BSC's chain-specific configuration (e.g., no deposit contract,
//! BSC-specific blob params, BSC system contracts from genesis).

use crate::{
    hardforks::BscHardforks,
    system_contracts::{
        CROSS_CHAIN_CONTRACT, GOV_HUB_CONTRACT, GOV_TOKEN_CONTRACT, GOVERNOR_CONTRACT,
        LIGHT_CLIENT_CONTRACT, RELAYER_HUB_CONTRACT, RELAYER_INCENTIVIZE_CONTRACT,
        SLASH_CONTRACT, STAKE_CREDIT_CONTRACT, STAKE_HUB_CONTRACT, STAKING_CONTRACT,
        SYSTEM_REWARD_CONTRACT, TIMELOCK_CONTRACT, TOKEN_HUB_CONTRACT,
        TOKEN_MANAGER_CONTRACT, TOKEN_RECOVER_PORTAL_CONTRACT, VALIDATOR_CONTRACT,
    },
};
use alloy_consensus::BlockHeader;
use alloy_eips::{
    eip7840::BlobParams,
    eip7910::{EthConfig, EthForkConfig, SystemContract},
};
use alloy_evm::precompiles::Precompile;
use alloy_primitives::Address;
use jsonrpsee::core::RpcResult;
use reth_chainspec::{ChainSpecProvider, EthChainSpec, EthereumHardforks, Hardforks, Head};
use reth_errors::{ProviderError, RethError};
use reth_evm::{precompiles::PrecompilesMap, ConfigureEvm, Evm};
use reth_node_api::NodePrimitives;
use reth_primitives_traits::header::HeaderMut;
use reth_revm::db::EmptyDB;
use reth_rpc_eth_api::helpers::config::EthConfigApiServer;
use reth_rpc_eth_types::EthApiError;
use reth_storage_api::BlockReaderIdExt;
use std::collections::BTreeMap;

/// BSC-specific handler for the `eth_config` RPC endpoint.
///
/// Extends the upstream EIP-7910 implementation with:
/// - BSC system contracts (ValidatorSet, Slash, StakeHub, etc.)
/// - Correct blob param handling (BSC doesn't modify params at Prague)
/// - BSC hardfork-aware system contract activation
///
/// Ref: <https://eips.ethereum.org/EIPS/eip-7910>
#[derive(Debug, Clone)]
pub struct BscEthConfigHandler<Provider, Evm> {
    provider: Provider,
    evm_config: Evm,
}

impl<Provider, EvmConfig> BscEthConfigHandler<Provider, EvmConfig>
where
    Provider: ChainSpecProvider<ChainSpec: Hardforks + EthereumHardforks + BscHardforks>
        + BlockReaderIdExt<Header: HeaderMut>
        + 'static,
    EvmConfig:
        ConfigureEvm<Primitives: NodePrimitives<BlockHeader = Provider::Header>> + 'static,
{
    /// Creates a new [`BscEthConfigHandler`].
    pub const fn new(provider: Provider, evm_config: EvmConfig) -> Self {
        Self { provider, evm_config }
    }

    /// Returns BSC system contracts active at the given timestamp.
    ///
    /// BSC has a rich set of system contracts deployed at genesis and extended
    /// at specific hardforks. Unlike Ethereum, BSC does not have beacon/deposit
    /// contracts but instead has validator management, staking, and cross-chain
    /// bridge contracts.
    ///
    /// We use `u64::MAX` for block_number when checking hardfork activation because
    /// all timestamp-based BSC forks also require London to be active (block-based),
    /// and `u64::MAX` always satisfies the block condition. This is consistent with
    /// the upstream fork_id calculation: `Head { timestamp, number: u64::MAX, .. }`.
    fn bsc_system_contracts_at(&self, timestamp: u64) -> BTreeMap<SystemContract, Address> {
        let chain_spec = self.provider.chain_spec();
        let mut contracts = BTreeMap::new();

        // Core system contracts - present from genesis
        contracts.insert(
            SystemContract::Other("ValidatorSet".to_string()),
            VALIDATOR_CONTRACT,
        );
        contracts.insert(
            SystemContract::Other("SlashIndicator".to_string()),
            SLASH_CONTRACT,
        );
        contracts.insert(
            SystemContract::Other("SystemReward".to_string()),
            SYSTEM_REWARD_CONTRACT,
        );
        contracts.insert(
            SystemContract::Other("LightClient".to_string()),
            LIGHT_CLIENT_CONTRACT,
        );
        contracts.insert(
            SystemContract::Other("TokenHub".to_string()),
            TOKEN_HUB_CONTRACT,
        );
        contracts.insert(
            SystemContract::Other("RelayerIncentivize".to_string()),
            RELAYER_INCENTIVIZE_CONTRACT,
        );
        contracts.insert(
            SystemContract::Other("RelayerHub".to_string()),
            RELAYER_HUB_CONTRACT,
        );
        contracts.insert(
            SystemContract::Other("GovHub".to_string()),
            GOV_HUB_CONTRACT,
        );
        contracts.insert(
            SystemContract::Other("TokenManager".to_string()),
            TOKEN_MANAGER_CONTRACT,
        );
        contracts.insert(
            SystemContract::Other("CrossChain".to_string()),
            CROSS_CHAIN_CONTRACT,
        );
        contracts.insert(
            SystemContract::Other("Staking".to_string()),
            STAKING_CONTRACT,
        );

        // Staking v2 contracts - activated at Feynman hardfork.
        // Use u64::MAX for block_number since all timestamp-based BSC forks
        // require London to be active (block-based), and u64::MAX always
        // satisfies that block condition.
        if chain_spec.is_feynman_active_at_timestamp(u64::MAX, timestamp) {
            contracts.insert(
                SystemContract::Other("StakeHub".to_string()),
                STAKE_HUB_CONTRACT,
            );
            contracts.insert(
                SystemContract::Other("StakeCredit".to_string()),
                STAKE_CREDIT_CONTRACT,
            );
            contracts.insert(
                SystemContract::Other("Governor".to_string()),
                GOVERNOR_CONTRACT,
            );
            contracts.insert(
                SystemContract::Other("GovToken".to_string()),
                GOV_TOKEN_CONTRACT,
            );
            contracts.insert(
                SystemContract::Other("Timelock".to_string()),
                TIMELOCK_CONTRACT,
            );
            contracts.insert(
                SystemContract::Other("TokenRecoverPortal".to_string()),
                TOKEN_RECOVER_PORTAL_CONTRACT,
            );
        }

        contracts
    }

    /// Builds fork config for a specific timestamp, including BSC system contracts.
    fn build_fork_config_at(
        &self,
        timestamp: u64,
        precompiles: BTreeMap<String, Address>,
    ) -> EthForkConfig {
        let chain_spec = self.provider.chain_spec();

        let system_contracts = self.bsc_system_contracts_at(timestamp);

        // Fork config only exists for timestamp-based hardforks.
        let fork_id = chain_spec
            .fork_id(&Head { timestamp, number: u64::MAX, ..Default::default() })
            .hash
            .0
            .into();

        EthForkConfig {
            activation_time: timestamp,
            blob_schedule: chain_spec
                .blob_params_at_timestamp(timestamp)
                // no blob support, so we set this to original cancun values as defined in eip-4844
                .unwrap_or_else(BlobParams::cancun),
            chain_id: chain_spec.chain().id(),
            fork_id,
            precompiles,
            system_contracts,
        }
    }

    /// Main config method - builds current, next, and last fork configurations.
    fn config(&self) -> Result<EthConfig, RethError> {
        let chain_spec = self.provider.chain_spec();
        let latest = self
            .provider
            .latest_header()?
            .ok_or_else(|| ProviderError::BestBlockNotFound)?
            .into_header();

        let current_precompiles = evm_to_precompiles_map(
            self.evm_config
                .evm_for_block(EmptyDB::default(), &latest)
                .map_err(RethError::other)?,
        );

        let mut fork_timestamps = chain_spec
            .forks_iter()
            .filter_map(|(_, cond)| cond.as_timestamp())
            .collect::<Vec<_>>();
        fork_timestamps.sort_unstable();
        fork_timestamps.dedup();

        let current_fork_idx =
            match fork_timestamps.iter().position(|ts| &latest.timestamp() < ts) {
                // All forks are in the past, use the last one.
                None => fork_timestamps.len().checked_sub(1),
                // First fork hasn't activated yet — no active timestamp fork.
                Some(0) => None,
                // Found a future fork; current is the one right before it.
                Some(idx) => Some(idx - 1),
            };
        let (current_fork_idx, current_fork_timestamp) = current_fork_idx
            .and_then(|idx| fork_timestamps.get(idx).map(|ts| (idx, *ts)))
            .ok_or_else(|| RethError::msg("no active timestamp fork found"))?;

        let current = self.build_fork_config_at(current_fork_timestamp, current_precompiles);

        let mut config = EthConfig { current, next: None, last: None };

        if let Some(next_fork_timestamp) = fork_timestamps.get(current_fork_idx + 1).copied() {
            let fake_header = {
                let mut header = latest.clone();
                header.set_timestamp(next_fork_timestamp);
                header
            };
            let next_precompiles = evm_to_precompiles_map(
                self.evm_config
                    .evm_for_block(EmptyDB::default(), &fake_header)
                    .map_err(RethError::other)?,
            );

            config.next =
                Some(self.build_fork_config_at(next_fork_timestamp, next_precompiles));
        } else {
            // If there is no fork scheduled, there is no "last" or "final" fork scheduled.
            return Ok(config);
        }

        let last_fork_timestamp = fork_timestamps.last().copied().unwrap();
        let fake_header = {
            let mut header = latest;
            header.set_timestamp(last_fork_timestamp);
            header
        };
        let last_precompiles = evm_to_precompiles_map(
            self.evm_config
                .evm_for_block(EmptyDB::default(), &fake_header)
                .map_err(RethError::other)?,
        );

        config.last =
            Some(self.build_fork_config_at(last_fork_timestamp, last_precompiles));

        Ok(config)
    }
}

impl<Provider, EvmConfig> EthConfigApiServer for BscEthConfigHandler<Provider, EvmConfig>
where
    Provider: ChainSpecProvider<ChainSpec: Hardforks + EthereumHardforks + BscHardforks>
        + BlockReaderIdExt<Header: HeaderMut>
        + 'static,
    EvmConfig:
        ConfigureEvm<Primitives: NodePrimitives<BlockHeader = Provider::Header>> + 'static,
{
    fn config(&self) -> RpcResult<EthConfig> {
        Ok(self.config().map_err(EthApiError::from)?)
    }
}

/// Converts EVM precompile addresses into a name→address map for the RPC response.
fn evm_to_precompiles_map(
    evm: impl Evm<Precompiles = PrecompilesMap>,
) -> BTreeMap<String, Address> {
    let precompiles = evm.precompiles();
    precompiles
        .addresses()
        .filter_map(|address| {
            Some((
                precompiles.get(address)?.precompile_id().name().to_string(),
                *address,
            ))
        })
        .collect()
}
