use alloy_eips::eip7910::{EthForkConfig, SystemContract};
use jsonrpsee::core::RpcResult;
use jsonrpsee::proc_macros::rpc;
use reth_chainspec::{ChainSpecProvider, EthereumHardforks, Hardforks};
use reth_evm::ConfigureEvm;
use reth_primitives_traits::NodePrimitives;
use reth_primitives_traits::header::HeaderMut;
use reth_provider::BlockReaderIdExt;
use reth_rpc_eth_api::helpers::config::{EthConfigApiServer, EthConfigHandler};

use std::sync::Arc;

use crate::hardforks::BscHardforks;
use crate::hardforks::bsc::BscHardfork;
use crate::node::evm::config::revm_spec_by_timestamp_and_block_number;

/// BSC-specific `eth_config` (EIP-7910) RPC.
///
/// Returns a raw JSON value rather than alloy's [`EthConfig`](alloy_eips::eip7910::EthConfig)
/// because BSC must be able to emit `blobSchedule: null` to match go-bsc, and alloy's
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
/// - `blobSchedule`: mirrors go-bsc's `ChainConfig.BlobConfig(LatestFork(t))` — only the forks that
///   go-bsc reports a blob config for keep a value; all others (incl. Mendel/Pasteur) become `null`.
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

        // Serialize to a raw value so `blobSchedule` can be nulled per go-bsc parity.
        let mut value = serde_json::to_value(&config).map_err(|e| {
            jsonrpsee::types::ErrorObjectOwned::owned(
                jsonrpsee::types::error::INTERNAL_ERROR_CODE,
                e.to_string(),
                None::<()>,
            )
        })?;

        let chain_spec = self.provider.chain_spec();
        let mut null_if_needed = |key: &str, activation_time: u64| {
            // block_number is irrelevant for the post-merge (timestamp-gated) forks that carry
            // blob params; u64::MAX keeps all block-based forks active so the timestamp decides.
            let fork = revm_spec_by_timestamp_and_block_number(
                chain_spec.clone(),
                activation_time,
                u64::MAX,
            );
            if !go_bsc_reports_blob_schedule(fork) {
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

/// Mirrors go-bsc `params.ChainConfig.BlobConfig`: only these forks return a blob config; every
/// other fork (including Mendel and Pasteur) yields `nil`, which serializes as `blobSchedule: null`.
///
/// go-bsc keeps: `Cancun`, `Prague`, `Fermi`, `Maxwell`, `Lorentz`, `Osaka`. The reth-bsc spec
/// resolver never emits a standalone `Prague` (it is always superseded by a later BSC fork), so the
/// observable set here is the same.
const fn go_bsc_reports_blob_schedule(fork: BscHardfork) -> bool {
    matches!(
        fork,
        BscHardfork::Cancun
            | BscHardfork::Fermi
            | BscHardfork::Maxwell
            | BscHardfork::Lorentz
            | BscHardfork::Osaka
    )
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
