use crate::{
    evm::precompiles::cas20::info::{token_info_at, InfoError, ProviderHost, TokenInfo},
    hardforks::BscHardforks,
};
use alloy_consensus::BlockHeader;
use alloy_eips::BlockId;
use alloy_primitives::Address;
use jsonrpsee::{core::RpcResult, proc_macros::rpc, types::ErrorObject};
use reth_chainspec::EthChainSpec;
use reth_provider::{BlockReaderIdExt, ChainSpecProvider, StateProviderFactory};

/// One eth_getCAS20TokenInfoBatch request reads at most this many tokens.
pub const CAS20_BATCH_LIMIT: usize = 20;

/// BSC Eth extension API - adds eth_coinbase, eth_health and the CAS20 token
/// views to match geth-bsc's EthereumAPI and BlockChainAPI.
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

    /// Returns a CAS20 token's configuration as of the given block (latest when
    /// omitted), read straight from state: in one call what would otherwise take a
    /// dozen eth_calls. Errors for an address that holds no token.
    #[method(name = "getCAS20TokenInfo")]
    async fn cas20_token_info(
        &self,
        address: Address,
        block: Option<BlockId>,
    ) -> RpcResult<TokenInfo>;

    /// `eth_getCAS20TokenInfo` over a list, answering null for an address that holds
    /// no token so one stranger does not fail the whole portfolio.
    #[method(name = "getCAS20TokenInfoBatch")]
    async fn cas20_token_info_batch(
        &self,
        addresses: Vec<Address>,
        block: Option<BlockId>,
    ) -> RpcResult<Vec<Option<TokenInfo>>>;
}

/// Implementation of the BSC Eth extension API
pub struct BscEthExtApiImpl<P> {
    provider: P,
    /// Validator address (coinbase/etherbase)
    validator_address: Address,
}

impl<P> BscEthExtApiImpl<P> {
    /// Create a new BSC Eth extension API instance
    pub fn new(provider: P) -> Self {
        // Get validator address from mining config
        let validator_address = crate::node::miner::config::get_global_mining_config()
            .and_then(|cfg| cfg.validator_address)
            .unwrap_or(Address::ZERO);

        Self { provider, validator_address }
    }
}

fn invalid_params(msg: impl Into<String>) -> ErrorObject<'static> {
    ErrorObject::owned(-32000, msg.into(), None::<()>)
}

fn internal(msg: impl Into<String>) -> ErrorObject<'static> {
    ErrorObject::owned(-32603, msg.into(), None::<()>)
}

impl<P> BscEthExtApiImpl<P>
where
    P: StateProviderFactory + BlockReaderIdExt + ChainSpecProvider,
    P::ChainSpec: EthChainSpec + BscHardforks,
{
    /// Reads the tokens at `block`, `None` for an address that holds no token.
    fn cas20_token_infos(
        &self,
        addresses: &[Address],
        block: Option<BlockId>,
    ) -> RpcResult<Vec<Option<TokenInfo>>> {
        let id = block.unwrap_or_else(BlockId::latest);
        let header = self
            .provider
            .header_by_id(id)
            .map_err(|e| internal(format!("failed to load header: {e}")))?
            .ok_or_else(|| invalid_params("block not found"))?;
        let spec = self.provider.chain_spec();
        if !spec.is_jenner_active_at_timestamp(header.number(), header.timestamp()) {
            return Err(invalid_params("CAS20 is not active at this block"));
        }
        let state = self
            .provider
            .state_by_block_id(id)
            .map_err(|e| internal(format!("failed to load state: {e}")))?;
        let time = header.timestamp();
        let mut host = ProviderHost { state: &*state, time, chain_id: spec.chain().id() };
        addresses
            .iter()
            .map(|&addr| match token_info_at(&mut host, spec.chain().id(), addr, time) {
                Ok(info) => Ok(Some(info)),
                Err(InfoError::NotToken) => Ok(None),
                Err(e) => Err(internal(e.to_string())),
            })
            .collect()
    }
}

#[async_trait::async_trait]
impl<P> BscEthExtApiServer for BscEthExtApiImpl<P>
where
    P: StateProviderFactory + BlockReaderIdExt + ChainSpecProvider + Send + Sync + 'static,
    P::ChainSpec: EthChainSpec + BscHardforks,
{
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

    async fn cas20_token_info(
        &self,
        address: Address,
        block: Option<BlockId>,
    ) -> RpcResult<TokenInfo> {
        self.cas20_token_infos(&[address], block)?
            .pop()
            .flatten()
            .ok_or_else(|| invalid_params(InfoError::NotToken.to_string()))
    }

    async fn cas20_token_info_batch(
        &self,
        addresses: Vec<Address>,
        block: Option<BlockId>,
    ) -> RpcResult<Vec<Option<TokenInfo>>> {
        if addresses.len() > CAS20_BATCH_LIMIT {
            return Err(invalid_params(format!(
                "batch of {} exceeds the limit of {CAS20_BATCH_LIMIT}",
                addresses.len()
            )));
        }
        self.cas20_token_infos(&addresses, block)
    }
}
