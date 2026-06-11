use alloy_eips::eip7910::{EthConfig, EthForkConfig, SystemContract};
use jsonrpsee::core::RpcResult;
use reth_chainspec::{ChainSpecProvider, EthereumHardforks, Hardforks};
use reth_evm::ConfigureEvm;
use reth_primitives_traits::NodePrimitives;
use reth_primitives_traits::header::HeaderMut;
use reth_provider::BlockReaderIdExt;
use reth_rpc_eth_api::helpers::config::{EthConfigApiServer, EthConfigHandler};

/// BSC-specific wrapper around reth's [`EthConfigHandler`].
///
/// Adjusts `systemContracts` to match geth-bsc behavior:
/// - BSC has no Beacon Chain, so `BEACON_ROOTS_ADDRESS` is not included.
/// - BSC only includes `HISTORY_STORAGE_ADDRESS` when Prague is active.
/// - Other Ethereum-specific system contracts (ConsolidationRequest, Deposit,
///   WithdrawalRequest) are not used on BSC.
#[derive(Debug, Clone)]
pub struct BscEthConfigHandler<Provider, Evm> {
    inner: EthConfigHandler<Provider, Evm>,
}

impl<Provider, Evm> BscEthConfigHandler<Provider, Evm>
where
    Provider: ChainSpecProvider<ChainSpec: Hardforks + EthereumHardforks>
        + BlockReaderIdExt<Header: HeaderMut>
        + 'static,
    Evm: ConfigureEvm<Primitives: NodePrimitives<BlockHeader = Provider::Header>> + 'static,
{
    /// Creates a new [`BscEthConfigHandler`].
    pub fn new(provider: Provider, evm_config: Evm) -> Self {
        Self { inner: EthConfigHandler::new(provider, evm_config) }
    }
}

impl<Provider, Evm> EthConfigApiServer for BscEthConfigHandler<Provider, Evm>
where
    Provider: ChainSpecProvider<ChainSpec: Hardforks + EthereumHardforks>
        + BlockReaderIdExt<Header: HeaderMut>
        + 'static,
    Evm: ConfigureEvm<Primitives: NodePrimitives<BlockHeader = Provider::Header>> + 'static,
{
    fn config(&self) -> RpcResult<EthConfig> {
        let mut config = EthConfigApiServer::config(&self.inner)?;
        fix_bsc_system_contracts(&mut config.current);
        if let Some(ref mut next) = config.next {
            fix_bsc_system_contracts(next);
        }
        if let Some(ref mut last) = config.last {
            fix_bsc_system_contracts(last);
        }
        Ok(config)
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
