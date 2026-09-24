use crate::{
    evm::precompiles::cas20::info::{token_info_at, InfoError, ProviderHost, TokenInfo},
    hardforks::BscHardforks,
};
use alloy_consensus::BlockHeader;
use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_primitives::Address;
use jsonrpsee::{core::RpcResult, proc_macros::rpc, types::ErrorObject};
use reth_chainspec::EthChainSpec;
use reth_provider::{BlockReaderIdExt, ChainSpecProvider, StateProviderFactory};
use std::sync::Arc;
use tokio::sync::Semaphore;

/// One eth_getCAS20TokenInfoBatch request reads at most this many tokens.
pub const CAS20_BATCH_LIMIT: usize = 20;

/// BSC-specific Ethereum RPC methods.
#[rpc(server, namespace = "eth")]
pub trait BscEthExtApi {
    /// Returns the validator's reward address.
    #[method(name = "coinbase")]
    async fn coinbase(&self) -> RpcResult<Address>;

    /// Returns whether the node has a canonical head.
    #[method(name = "health")]
    async fn health(&self) -> RpcResult<bool>;

    /// Returns token configuration at `block` (default: latest), or an error for a non-token.
    #[method(name = "getCAS20TokenInfo")]
    async fn cas20_token_info(
        &self,
        address: Address,
        block: Option<BlockId>,
    ) -> RpcResult<TokenInfo>;

    /// Returns token configurations, with null entries for non-token addresses.
    #[method(name = "getCAS20TokenInfoBatch")]
    async fn cas20_token_info_batch(
        &self,
        addresses: Vec<Address>,
        block: Option<BlockId>,
    ) -> RpcResult<Vec<Option<TokenInfo>>>;
}

/// BSC Ethereum RPC extension sharing the node's blocking IO limit.
pub struct BscEthExtApiImpl<P> {
    provider: P,
    blocking_io_guard: Arc<Semaphore>,
    validator_address: Address,
}

impl<P> BscEthExtApiImpl<P> {
    pub fn new(provider: P, blocking_io_guard: Arc<Semaphore>) -> Self {
        let validator_address = crate::node::miner::config::get_global_mining_config()
            .and_then(|cfg| cfg.validator_address)
            .unwrap_or(Address::ZERO);

        Self { provider, blocking_io_guard, validator_address }
    }

    async fn blocking_read<T: Send + 'static>(
        &self,
        read: impl FnOnce() -> RpcResult<T> + Send + 'static,
    ) -> RpcResult<T> {
        let permit = self
            .blocking_io_guard
            .clone()
            .acquire_owned()
            .await
            .map_err(|e| internal(format!("CAS20 query unavailable: {e}")))?;
        tokio::task::spawn_blocking(move || {
            // A cancelled request must not release capacity while its read is still running.
            let _permit = permit;
            read()
        })
        .await
        .map_err(|e| internal(format!("CAS20 query task failed: {e}")))?
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
    P: StateProviderFactory + BlockReaderIdExt + ChainSpecProvider + Clone + Send + Sync + 'static,
    P::ChainSpec: EthChainSpec + BscHardforks,
{
    async fn cas20_token_infos(
        &self,
        addresses: Vec<Address>,
        block: Option<BlockId>,
    ) -> RpcResult<Vec<Option<TokenInfo>>> {
        let provider = self.provider.clone();
        self.blocking_read(move || Self::read_cas20_token_infos(&provider, &addresses, block)).await
    }

    fn read_cas20_token_infos(
        provider: &P,
        addresses: &[Address],
        block: Option<BlockId>,
    ) -> RpcResult<Vec<Option<TokenInfo>>> {
        // Match go-bsc: pending resolves to the canonical head.
        let id = match block.unwrap_or_else(BlockId::latest) {
            BlockId::Number(BlockNumberOrTag::Pending) => BlockId::latest(),
            id => id,
        };
        let header = provider
            .sealed_header_by_id(id)
            .map_err(|e| internal(format!("failed to load header: {e}")))?
            .ok_or_else(|| invalid_params("block not found"))?;
        let spec = provider.chain_spec();
        if !spec.is_jenner_active_at_timestamp(header.number(), header.timestamp()) {
            return Err(invalid_params("CAS20 is not active at this block"));
        }
        // Pin state to the resolved header so a head update cannot mix time and state.
        let state = provider
            .state_by_block_hash(header.hash())
            .map_err(|e| internal(format!("failed to load state: {e}")))?;
        let time = header.timestamp();
        let chain_id = spec.chain().id();
        let mut host = ProviderHost { state: &*state, time, chain_id };
        addresses
            .iter()
            .map(|&addr| match token_info_at(&mut host, addr) {
                Ok(info) => Ok(Some(info)),
                Err(InfoError::NotToken) => Ok(None),
                Err(e @ InfoError::StringTooLong) => Err(invalid_params(e.to_string())),
                Err(e @ InfoError::State(_)) => Err(internal(e.to_string())),
            })
            .collect()
    }
}

#[async_trait::async_trait]
impl<P> BscEthExtApiServer for BscEthExtApiImpl<P>
where
    P: StateProviderFactory + BlockReaderIdExt + ChainSpecProvider + Clone + Send + Sync + 'static,
    P::ChainSpec: EthChainSpec + BscHardforks,
{
    async fn coinbase(&self) -> RpcResult<Address> {
        // Reflect miner_setEtherbase updates, falling back to the startup configuration.
        Ok(crate::shared::get_miner_etherbase().unwrap_or(self.validator_address))
    }

    async fn health(&self) -> RpcResult<bool> {
        Ok(crate::shared::get_best_canonical_block_number().is_some())
    }

    async fn cas20_token_info(
        &self,
        address: Address,
        block: Option<BlockId>,
    ) -> RpcResult<TokenInfo> {
        self.cas20_token_infos(vec![address], block)
            .await?
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
        self.cas20_token_infos(addresses, block).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::mpsc, thread};
    use tokio::sync::oneshot;

    #[tokio::test(flavor = "current_thread")]
    async fn blocking_read_keeps_runtime_free_and_holds_permit_after_cancellation() {
        let guard = Arc::new(Semaphore::new(1));
        let api = Arc::new(BscEthExtApiImpl::new((), guard.clone()));
        let runtime_thread = thread::current().id();
        let (started_tx, started_rx) = oneshot::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let request = tokio::spawn({
            let api = api.clone();
            async move {
                api.blocking_read(move || {
                    started_tx.send(thread::current().id()).unwrap();
                    // Bound the wait so a scheduling regression cannot hang the test suite.
                    release_rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
                    Ok(())
                })
                .await
            }
        });

        assert_ne!(started_rx.await.unwrap(), runtime_thread);
        request.abort();
        assert!(request.await.unwrap_err().is_cancelled());
        assert_eq!(guard.available_permits(), 0);

        let next = api.blocking_read(|| Ok(42));
        tokio::pin!(next);
        assert!(futures::poll!(&mut next).is_pending());
        release_tx.send(()).unwrap();
        assert_eq!(next.await.unwrap(), 42);
        assert_eq!(guard.available_permits(), 1);
    }

    #[tokio::test]
    async fn blocking_read_propagates_errors_and_releases_permits_on_panic() {
        let guard = Arc::new(Semaphore::new(1));
        let api = BscEthExtApiImpl::new((), guard.clone());
        let err =
            api.blocking_read::<()>(|| Err(invalid_params("block not found"))).await.unwrap_err();
        assert_eq!(err.code(), -32000);
        assert_eq!(err.message(), "block not found");

        let err = api.blocking_read::<()>(|| panic!("failed read")).await.unwrap_err();
        assert_eq!(err.code(), -32603);
        assert_eq!(guard.available_permits(), 1);

        guard.close();
        let err = api.blocking_read(|| Ok(())).await.unwrap_err();
        assert_eq!(err.code(), -32603);
    }
}
