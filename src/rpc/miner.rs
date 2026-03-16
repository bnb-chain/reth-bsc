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
    async fn set_gas_limit(&self, gas_limit: u64) -> RpcResult<bool>;

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
    /// Start mining
    /// Note: In reth-bsc, mining is controlled by the node configuration and Parlia consensus.
    /// This method is provided for API compatibility with geth-bsc.
    async fn start(&self) -> RpcResult<()> {
        tracing::warn!(target: "bsc::rpc", "miner_start called - mining lifecycle is managed by node configuration in reth-bsc");
        Err(jsonrpsee::types::ErrorObject::owned(
            -32000,
            "miner_start is not supported: mining lifecycle is managed by node configuration",
            None::<()>,
        ))
    }

    /// Stop mining
    /// Note: In reth-bsc, mining is controlled by the node configuration and Parlia consensus.
    /// This method is provided for API compatibility with geth-bsc.
    async fn stop(&self) -> RpcResult<()> {
        tracing::warn!(target: "bsc::rpc", "miner_stop called - mining lifecycle is managed by node configuration in reth-bsc");
        Err(jsonrpsee::types::ErrorObject::owned(
            -32000,
            "miner_stop is not supported: mining lifecycle is managed by node configuration",
            None::<()>,
        ))
    }

    /// Set extra data for mined blocks
    /// Note: Extra data is determined by Parlia consensus in reth-bsc.
    async fn set_extra(&self, extra: String) -> RpcResult<bool> {
        tracing::warn!(target: "bsc::rpc", "miner_setExtra called with: {} - extra data is managed by Parlia consensus", extra);
        Err(jsonrpsee::types::ErrorObject::owned(
            -32000,
            "miner_setExtra is not supported: extra data is managed by Parlia consensus",
            None::<()>,
        ))
    }

    /// Set minimum accepted gas price
    /// Note: Gas price is configured via environment variables in reth-bsc.
    async fn set_gas_price(&self, gas_price: alloy_primitives::U256) -> RpcResult<bool> {
        tracing::warn!(target: "bsc::rpc", "miner_setGasPrice called with: {} - use BSC_MIN_GAS_TIP env var", gas_price);
        Err(jsonrpsee::types::ErrorObject::owned(
            -32000,
            "miner_setGasPrice is not supported: use BSC_MIN_GAS_TIP environment variable",
            None::<()>,
        ))
    }

    /// Set gas limit target for mining
    /// Note: Gas limit is configured via environment variables in reth-bsc.
    async fn set_gas_limit(&self, gas_limit: u64) -> RpcResult<bool> {
        tracing::warn!(target: "bsc::rpc", "miner_setGasLimit called with: {} - use BSC_GAS_LIMIT env var", gas_limit);
        Err(jsonrpsee::types::ErrorObject::owned(
            -32000,
            "miner_setGasLimit is not supported: use BSC_GAS_LIMIT environment variable",
            None::<()>,
        ))
    }

    /// Set etherbase (validator address)
    /// Note: Validator address is configured via keystore/private key in reth-bsc.
    async fn set_etherbase(&self, etherbase: Address) -> RpcResult<bool> {
        tracing::warn!(target: "bsc::rpc", "miner_setEtherbase called with: {} - use BSC_PRIVATE_KEY or BSC_KEYSTORE_PATH env var", etherbase);
        Err(jsonrpsee::types::ErrorObject::owned(
            -32000,
            "miner_setEtherbase is not supported: validator address is derived from the configured private key",
            None::<()>,
        ))
    }

    /// Set recommit interval for miner sealing work
    /// Note: Not currently supported in reth-bsc.
    async fn set_recommit_interval(&self, interval: u64) -> RpcResult<()> {
        tracing::warn!(target: "bsc::rpc", "miner_setRecommitInterval called with: {}ms - not supported", interval);
        Err(jsonrpsee::types::ErrorObject::owned(
            -32000,
            "miner_setRecommitInterval is not supported",
            None::<()>,
        ))
    }

    /// Check if MEV is running (validator accepting bids from builders)
    async fn mev_running(&self) -> RpcResult<bool> {
        Ok(crate::shared::is_mev_running())
    }

    /// Start MEV - begin accepting bids from builders
    async fn start_mev(&self) -> RpcResult<()> {
        tracing::info!(target: "bsc::rpc", "miner_startMev called - enabling MEV bid acceptance");
        crate::shared::start_mev();
        Ok(())
    }

    /// Stop MEV - stop accepting new bids from builders
    /// Previously received bids are still considered.
    async fn stop_mev(&self) -> RpcResult<()> {
        tracing::info!(target: "bsc::rpc", "miner_stopMev called - disabling MEV bid acceptance");
        crate::shared::stop_mev();
        Ok(())
    }

    /// Add a builder to the bid simulator whitelist
    async fn add_builder(&self, builder: Address, url: String) -> RpcResult<()> {
        tracing::info!(target: "bsc::rpc", "miner_addBuilder called: builder={}, url={}", builder, url);
        // Note: url is accepted for API compatibility with geth-bsc but not used currently
        // as reth-bsc's bid simulator doesn't connect to builder endpoints
        let added = crate::shared::add_builder(builder);
        if added {
            tracing::info!(target: "bsc::rpc", "Builder {} added to whitelist", builder);
        } else {
            tracing::info!(target: "bsc::rpc", "Builder {} was already in whitelist", builder);
        }
        Ok(())
    }

    /// Remove a builder from the bid simulator whitelist
    async fn remove_builder(&self, builder: Address) -> RpcResult<()> {
        tracing::info!(target: "bsc::rpc", "miner_removeBuilder called: builder={}", builder);
        let removed = crate::shared::remove_builder(&builder);
        if removed {
            tracing::info!(target: "bsc::rpc", "Builder {} removed from whitelist", builder);
        } else {
            tracing::info!(target: "bsc::rpc", "Builder {} was not in whitelist", builder);
        }
        Ok(())
    }
}
