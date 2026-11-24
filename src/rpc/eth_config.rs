use alloy_eips::eip7910::{EthForkConfig, SystemContract};
use jsonrpsee::{core::RpcResult, proc_macros::rpc};
use reth_chainspec::{ChainSpecProvider, EthChainSpec, EthereumHardforks, Hardforks};
use reth_evm::ConfigureEvm;
use reth_primitives_traits::{header::HeaderMut, NodePrimitives};
use reth_provider::BlockReaderIdExt;
use reth_rpc_eth_api::helpers::config::{EthConfigApiServer, EthConfigHandler};

use std::sync::Arc;

use crate::hardforks::BscHardforks;

/// BSC-specific `eth_config` (EIP-7910) RPC.
///
/// Returns a raw JSON value rather than alloy's [`EthConfig`](alloy_eips::eip7910::EthConfig)
/// because BSC must emit `blobSchedule: null` before blob support is active, and alloy's
/// `EthForkConfig::blob_schedule` is a required (non-optional) `BlobParams` that always serializes.
#[cfg_attr(not(feature = "client"), rpc(server, namespace = "eth"))]
#[cfg_attr(feature = "client", rpc(server, client, namespace = "eth"))]
pub trait BscEthConfigApi {
    /// Returns an object with data about recent and upcoming fork configurations.
    #[method(name = "config")]
    fn config(&self) -> RpcResult<serde_json::Value>;
}

/// BSC-specific wrapper around reth's [`EthConfigHandler`].
///
/// Adjusts the EIP-7910 output to match geth-bsc behavior:
/// - `systemContracts`: BSC only exposes `HISTORY_STORAGE_ADDRESS` (from Prague); Ethereum-specific
///   contracts (BeaconRoots, Deposit, Consolidation, Withdrawal) are dropped.
/// - `blobSchedule`: retains the chain's blob parameters once blob support is active, including
///   across later BSC forks. Before activation, the schedule is `null`.
#[derive(Debug, Clone)]
pub struct BscEthConfigHandler<Provider, Evm> {
    inner: EthConfigHandler<Provider, Evm>,
    provider: Provider,
}

impl<Provider, Evm> BscEthConfigHandler<Provider, Evm>
where
    Provider: ChainSpecProvider<ChainSpec: Hardforks + EthereumHardforks + BscHardforks>
        + BlockReaderIdExt<Header: HeaderMut>
        + Clone
        + 'static,
    Evm: ConfigureEvm<Primitives: NodePrimitives<BlockHeader = Provider::Header>> + 'static,
{
    /// Creates a new [`BscEthConfigHandler`].
    pub fn new(provider: Provider, evm_config: Evm) -> Self {
        Self { inner: EthConfigHandler::new(provider.clone(), evm_config), provider }
    }
}

impl<Provider, Evm> BscEthConfigApiServer for BscEthConfigHandler<Provider, Evm>
where
    Provider: ChainSpecProvider<ChainSpec: Hardforks + EthereumHardforks + BscHardforks>
        + BlockReaderIdExt<Header: HeaderMut>
        + Clone
        + 'static,
    Arc<<Provider as ChainSpecProvider>::ChainSpec>: BscHardforks,
    Evm: ConfigureEvm<Primitives: NodePrimitives<BlockHeader = Provider::Header>> + 'static,
{
    fn config(&self) -> RpcResult<serde_json::Value> {
        // Build the standard EIP-7910 config via reth's handler, then apply BSC adjustments.
        let mut config = EthConfigApiServer::config(&self.inner)?;
        fix_bsc_system_contracts(&mut config.current);
        if let Some(ref mut next) = config.next {
            fix_bsc_system_contracts(next);
        }
        if let Some(ref mut last) = config.last {
            fix_bsc_system_contracts(last);
        }

        // The upstream handler substitutes Cancun defaults before blob support is active.
        // Serialize to a raw value so those unsupported schedules can be returned as null.
        let mut value = serde_json::to_value(&config).map_err(|e| {
            jsonrpsee::types::ErrorObjectOwned::owned(
                jsonrpsee::types::error::INTERNAL_ERROR_CODE,
                e.to_string(),
                None::<()>,
            )
        })?;

        let chain_spec = self.provider.chain_spec();
        let mut null_if_needed = |key: &str, activation_time: u64| {
            if chain_spec.blob_params_at_timestamp(activation_time).is_none() {
                if let Some(fork_obj) = value.get_mut(key).and_then(|v| v.as_object_mut()) {
                    fork_obj.insert("blobSchedule".to_string(), serde_json::Value::Null);
                }
            }
        };

        null_if_needed("current", config.current.activation_time);
        if let Some(ref next) = config.next {
            null_if_needed("next", next.activation_time);
        }
        if let Some(ref last) = config.last {
            null_if_needed("last", last.activation_time);
        }

        Ok(value)
    }
}

/// Replaces the Ethereum system-contracts map with BSC's.
///
/// On BSC, the only system contract is `HISTORY_STORAGE_ADDRESS` (active from Prague).
/// Ethereum-specific contracts (BeaconRoots, Deposit, Consolidation, Withdrawal) do not
/// exist on BSC.
fn fix_bsc_system_contracts(fork_config: &mut EthForkConfig) {
    let has_history_storage =
        fork_config.system_contracts.contains_key(&SystemContract::HistoryStorage);
    fork_config.system_contracts.clear();
    if has_history_storage {
        fork_config
            .system_contracts
            .insert(SystemContract::HistoryStorage, alloy_eips::eip2935::HISTORY_STORAGE_ADDRESS);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        chainspec::{bsc::bsc_mainnet, BscChainSpec},
        hardforks::bsc::BscHardfork,
        node::{
            evm::config::BscEvmConfig,
            primitives::{BscBlock, BscBlockBody, BscPrimitives},
        },
    };
    use alloy_consensus::Header;
    use alloy_primitives::U256;
    use reth_chainspec::{EthereumHardfork, ForkCondition};
    use reth_provider::test_utils::MockEthProvider;
    use serde_json::{json, Value};

    fn config_at(chain_spec: BscChainSpec, timestamp: u64) -> Value {
        let chain_spec = Arc::new(chain_spec);
        let provider =
            MockEthProvider::<BscPrimitives, _>::new().with_chain_spec((*chain_spec).clone());
        let header = Header {
            number: 40_000_000,
            timestamp,
            gas_limit: 140_000_000,
            base_fee_per_gas: Some(0),
            difficulty: U256::from(2),
            excess_blob_gas: Some(0),
            blob_gas_used: Some(0),
            ..Default::default()
        };
        let hash = header.hash_slow();
        provider.add_header(hash, header.clone());
        provider.add_block(hash, BscBlock { header, body: BscBlockBody::default() });
        BscEthConfigHandler::new(provider, BscEvmConfig::new(chain_spec)).config().unwrap()
    }

    #[test]
    fn blob_schedule_survives_bsc_fork_transitions() {
        let mut spec = bsc_mainnet();
        spec.hardforks.insert(BscHardfork::Jenner, ForkCondition::Timestamp(1_800_000_000));
        let cancun = spec.fork(EthereumHardfork::Cancun).as_timestamp().unwrap();
        let mut timestamps = spec
            .forks_iter()
            .filter_map(|(_, condition)| condition.as_timestamp())
            .filter(|timestamp| *timestamp >= cancun)
            .collect::<Vec<_>>();
        timestamps.sort_unstable();
        timestamps.dedup();

        for timestamp in timestamps {
            let config = config_at(BscChainSpec::from(spec.clone()), timestamp);
            assert_eq!(config["current"]["activationTime"], timestamp);
            for key in ["current", "next", "last"] {
                if !config[key].is_null() {
                    assert_eq!(
                        config[key]["blobSchedule"],
                        json!({"target": 3, "max": 6, "baseFeeUpdateFraction": 3338477}),
                        "{key} at head timestamp {timestamp}"
                    );
                    assert_eq!(config[key]["chainId"], "0x38");
                }
            }
        }

        let config = config_at(BscChainSpec::from(spec), 1_800_000_000);
        assert!(config["next"].is_null());
        assert!(config["last"].is_null());
        assert_eq!(
            config["current"]["systemContracts"],
            json!({"HISTORY_STORAGE_ADDRESS": alloy_eips::eip2935::HISTORY_STORAGE_ADDRESS})
        );
    }

    #[test]
    fn pre_cancun_config_has_no_current_blob_schedule() {
        let spec = bsc_mainnet();
        let cancun = spec.fork(EthereumHardfork::Cancun).as_timestamp().unwrap();
        let config = config_at(BscChainSpec::from(spec), cancun - 1);
        assert!(config["current"]["blobSchedule"].is_null());
        assert_eq!(config["next"]["activationTime"], cancun);
        assert_eq!(
            config["next"]["blobSchedule"],
            json!({"target": 3, "max": 6, "baseFeeUpdateFraction": 3338477})
        );
    }

    #[test]
    fn blob_schedule_preserves_configured_parameters() {
        let mut spec = bsc_mainnet();
        spec.blob_params.cancun.target_blob_count = 2;
        spec.blob_params.cancun.max_blob_count = 4;
        spec.blob_params.cancun.max_blobs_per_tx = 4;
        spec.blob_params.cancun.update_fraction = 12345;
        let mendel = spec.fork(BscHardfork::Mendel).as_timestamp().unwrap();
        let config = config_at(BscChainSpec::from(spec), mendel);
        for key in ["current", "next", "last"] {
            assert!(config[key].is_object(), "missing {key} configuration");
            assert_eq!(
                config[key]["blobSchedule"],
                json!({"target": 2, "max": 4, "baseFeeUpdateFraction": 12345})
            );
        }
    }
}
