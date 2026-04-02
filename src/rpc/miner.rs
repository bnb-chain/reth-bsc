use alloy_primitives::Address;
use jsonrpsee::core::RpcResult;
use jsonrpsee::proc_macros::rpc;

/// BSC Miner RPC API - matches geth-bsc's MinerAPI
/// Provides control over mining operations, gas settings, and MEV configuration.
#[rpc(server, namespace = "miner")]
pub trait BscMinerApi {
    /// Starts the miner.
    #[method(name = "start")]
    async fn start(&self) -> RpcResult<()>;

    /// Terminates the miner, both at the consensus engine level as well as at
    /// the block creation level.
    #[method(name = "stop")]
    async fn stop(&self) -> RpcResult<()>;

    /// Sets the extra data string that is included when this miner mines a block.
    #[method(name = "setExtra")]
    async fn set_extra(&self, extra: String) -> RpcResult<bool>;

    /// Sets the minimum accepted gas price for the miner.
    #[method(name = "setGasPrice")]
    async fn set_gas_price(&self, gas_price: alloy_primitives::U256) -> RpcResult<bool>;

    /// Sets the gaslimit to target towards during mining.
    #[method(name = "setGasLimit")]
    async fn set_gas_limit(&self, gas_limit: alloy_primitives::U256) -> RpcResult<bool>;

    /// Sets the etherbase of the miner.
    #[method(name = "setEtherbase")]
    async fn set_etherbase(&self, etherbase: Address) -> RpcResult<bool>;

    /// Updates the interval for miner sealing work recommitting.
    /// interval is in milliseconds.
    #[method(name = "setRecommitInterval")]
    async fn set_recommit_interval(&self, interval: u64) -> RpcResult<()>;

    /// Returns true if the validator accepts bids from builders.
    #[method(name = "mevRunning")]
    async fn mev_running(&self) -> RpcResult<bool>;

    /// Starts MEV. Notifies the miner to start receiving bids from builders.
    #[method(name = "startMev")]
    async fn start_mev(&self) -> RpcResult<()>;

    /// Stops MEV. Notifies the miner to stop receiving bids, but previously
    /// received bids are still considered.
    #[method(name = "stopMev")]
    async fn stop_mev(&self) -> RpcResult<()>;

    /// Adds a builder to the bid simulator.
    /// url is the endpoint of the builder (e.g., "https://mev-builder.amazonaws.com").
    /// If the validator is equipped with a sentry, the url can be ignored.
    #[method(name = "addBuilder")]
    async fn add_builder(&self, builder: Address, url: String) -> RpcResult<()>;

    /// Removes a builder from the bid simulator.
    #[method(name = "removeBuilder")]
    async fn remove_builder(&self, builder: Address) -> RpcResult<()>;
}

/// Implementation of the BSC Miner RPC API
#[derive(Default)]
pub struct BscMinerApiImpl;

impl BscMinerApiImpl {
    /// Create a new BSC Miner API instance
    pub fn new() -> Self {
        Self
    }
}

#[async_trait::async_trait]
impl BscMinerApiServer for BscMinerApiImpl {
    async fn start(&self) -> RpcResult<()> {
        tracing::info!(target: "bsc::rpc", "miner_start called");
        crate::shared::set_mining_enabled(true);
        Ok(())
    }

    async fn stop(&self) -> RpcResult<()> {
        tracing::info!(target: "bsc::rpc", "miner_stop called");
        crate::shared::set_mining_enabled(false);
        Ok(())
    }

    async fn set_extra(&self, extra: String) -> RpcResult<bool> {
        tracing::info!(target: "bsc::rpc", "miner_setExtra called with: {}", extra);
        let bytes = alloy_primitives::Bytes::from(extra.into_bytes());
        if bytes.len() > 32 {
            return Err(jsonrpsee::types::ErrorObject::owned(
                -32000,
                "extra data too long (max 32 bytes)",
                None::<()>,
            ));
        }
        crate::shared::set_miner_extra(bytes);
        Ok(true)
    }

    async fn set_gas_price(&self, gas_price: alloy_primitives::U256) -> RpcResult<bool> {
        tracing::info!(target: "bsc::rpc", "miner_setGasPrice called with: {}", gas_price);
        // Convert U256 to u64 (gas tip in wei); saturate if it exceeds u64::MAX
        let tip: u64 = gas_price.try_into().unwrap_or(u64::MAX);
        crate::shared::set_miner_gas_tip(tip);
        Ok(true)
    }

    async fn set_gas_limit(&self, gas_limit: alloy_primitives::U256) -> RpcResult<bool> {
        tracing::info!(target: "bsc::rpc", "miner_setGasLimit called with: {}", gas_limit);
        let limit: u64 = gas_limit.try_into().unwrap_or(u64::MAX);
        crate::shared::set_miner_gas_limit(limit);
        Ok(true)
    }

    async fn set_etherbase(&self, etherbase: Address) -> RpcResult<bool> {
        tracing::info!(target: "bsc::rpc", "miner_setEtherbase called with: {}", etherbase);
        crate::shared::set_miner_etherbase(etherbase);
        Ok(true)
    }

    async fn set_recommit_interval(&self, interval: u64) -> RpcResult<()> {
        tracing::info!(target: "bsc::rpc", "miner_setRecommitInterval called with: {}ms", interval);
        crate::shared::set_miner_recommit_interval_ms(interval);
        Ok(())
    }

    async fn mev_running(&self) -> RpcResult<bool> {
        Ok(crate::shared::is_mev_running())
    }

    async fn start_mev(&self) -> RpcResult<()> {
        tracing::info!(target: "bsc::rpc", "miner_startMev called");
        crate::shared::start_mev();
        Ok(())
    }

    async fn stop_mev(&self) -> RpcResult<()> {
        tracing::info!(target: "bsc::rpc", "miner_stopMev called");
        crate::shared::stop_mev();
        Ok(())
    }

    async fn add_builder(&self, builder: Address, url: String) -> RpcResult<()> {
        tracing::info!(target: "bsc::rpc", "miner_addBuilder: builder={}, url={}", builder, url);
        crate::shared::add_builder(builder);
        Ok(())
    }

    async fn remove_builder(&self, builder: Address) -> RpcResult<()> {
        tracing::info!(target: "bsc::rpc", "miner_removeBuilder: builder={}", builder);
        crate::shared::remove_builder(&builder);
        Ok(())
    }
}
