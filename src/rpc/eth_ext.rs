use alloy_primitives::Address;
use jsonrpsee::core::RpcResult;
use jsonrpsee::proc_macros::rpc;

/// BSC Eth extension API - adds eth_coinbase and eth_health
/// to match geth-bsc's EthereumAPI and BlockChainAPI.
///
/// Note: eth_config (EIP-7910) is already provided by reth's EthConfigHandler.
#[rpc(server, namespace = "eth")]
pub trait BscEthExtApi {
    /// Returns the client coinbase address (alias for etherbase).
    /// This is the validator address that mining rewards will be sent to.
    #[method(name = "coinbase")]
    async fn coinbase(&self) -> RpcResult<Address>;

    /// Returns true if the node is healthy.
    /// Matches geth-bsc's Health() which checks RPC serving latency.
    #[method(name = "health")]
    async fn health(&self) -> RpcResult<bool>;
}

/// Implementation of the BSC Eth extension API
#[derive(Default)]
pub struct BscEthExtApiImpl {
    /// Validator address (coinbase/etherbase)
    validator_address: Address,
}

impl BscEthExtApiImpl {
    /// Create a new BSC Eth extension API instance
    pub fn new() -> Self {
        // Get validator address from mining config
        let validator_address = crate::node::miner::config::get_global_mining_config()
            .and_then(|cfg| cfg.validator_address)
            .unwrap_or(Address::ZERO);

        Self { validator_address }
    }
}

#[async_trait::async_trait]
impl BscEthExtApiServer for BscEthExtApiImpl {
    /// Returns the validator address (coinbase/etherbase).
    /// In geth-bsc, Coinbase() is an alias for Etherbase() which returns
    /// the address that mining rewards will be sent to.
    /// Reflects updates from miner_setEtherbase if called.
    async fn coinbase(&self) -> RpcResult<Address> {
        // Prefer dynamic value (set by miner_setEtherbase), fall back to startup value
        let addr = crate::shared::get_miner_etherbase().unwrap_or(self.validator_address);
        Ok(addr)
    }

    /// Returns true if the node is healthy.
    /// In geth-bsc, this checks if the 75th percentile of RPC serving time
    /// is below the unhealthy timeout threshold. For reth-bsc, we check
    /// if the node can provide a best block number as a basic health indicator.
    async fn health(&self) -> RpcResult<bool> {
        let healthy = crate::shared::get_best_canonical_block_number().is_some();
        Ok(healthy)
    }
}
