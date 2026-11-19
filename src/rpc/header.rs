//! Go-BSC fields for explicit block and header queries.

use alloy_eips::BlockNumberOrTag;
use alloy_primitives::B256;
use alloy_rpc_types::serde_helpers::WithOtherFields;
use alloy_rpc_types_eth::Header;
use jsonrpsee::{core::RpcResult, proc_macros::rpc};
use reth_rpc_convert::RpcTypes;
use reth_rpc_eth_api::{helpers::EthBlocks, FullEthApiTypes, RpcBlock};

pub const METHODS: [&str; 6] = [
    "eth_getBlockByNumber",
    "eth_getBlockByHash",
    "eth_getHeaderByNumber",
    "eth_getHeaderByHash",
    "eth_getFinalizedBlock",
    "eth_getFinalizedHeader",
];

/// Go-BSC adds milliseconds to explicit queries, but not subscriptions or simulations.
pub struct BscBlockApiImpl<Eth>(pub Eth);

impl<Eth> BscBlockApiImpl<Eth>
where
    Eth: EthBlocks + FullEthApiTypes + 'static,
    Eth::NetworkTypes: RpcTypes<Header = Header>,
{
    /// Return only the block queries already exposed by the authenticated Engine RPC server.
    pub fn into_auth_rpc(self) -> jsonrpsee::RpcModule<Self> {
        let mut module = self.into_rpc();
        let excluded = module
            .method_names()
            .filter(|method| !matches!(*method, "eth_getBlockByHash" | "eth_getBlockByNumber"))
            .collect::<Vec<_>>();
        for method in excluded {
            module.remove_method(method);
        }
        module
    }
}

fn with_milliseconds<T>(value: T, header: &Header) -> WithOtherFields<T> {
    let milliseconds =
        crate::consensus::parlia::util::calculate_millisecond_timestamp(&header.inner);
    let mut response = WithOtherFields::new(value);
    response.other.insert("milliTimestamp".into(), format!("0x{milliseconds:x}").into());
    response
}

#[rpc(server, namespace = "eth")]
pub trait BscBlockApi<B> {
    #[method(name = "getBlockByNumber")]
    async fn block_by_number(
        &self,
        number: BlockNumberOrTag,
        full: bool,
    ) -> RpcResult<Option<WithOtherFields<B>>>;
    #[method(name = "getBlockByHash")]
    async fn block_by_hash(&self, hash: B256, full: bool) -> RpcResult<Option<WithOtherFields<B>>>;
    #[method(name = "getHeaderByNumber")]
    async fn header_by_number(
        &self,
        block_number: BlockNumberOrTag,
    ) -> RpcResult<Option<WithOtherFields<Header>>>;
    #[method(name = "getHeaderByHash")]
    async fn header_by_hash(&self, hash: B256) -> RpcResult<Option<WithOtherFields<Header>>>;
    #[method(name = "getFinalizedBlock")]
    async fn finalized_block(
        &self,
        verified_validator_num: i64,
        full: bool,
    ) -> RpcResult<Option<WithOtherFields<B>>>;
    #[method(name = "getFinalizedHeader")]
    async fn finalized_header(
        &self,
        verified_validator_num: i64,
    ) -> RpcResult<Option<WithOtherFields<Header>>>;
}

#[async_trait::async_trait]
impl<Eth> BscBlockApiServer<RpcBlock<Eth::NetworkTypes>> for BscBlockApiImpl<Eth>
where
    Eth: EthBlocks + FullEthApiTypes + 'static,
    Eth::NetworkTypes: RpcTypes<Header = Header>,
{
    async fn block_by_number(
        &self,
        number: BlockNumberOrTag,
        full: bool,
    ) -> RpcResult<Option<WithOtherFields<RpcBlock<Eth::NetworkTypes>>>> {
        let result = self.0.rpc_block(number.into(), full).await.map_err(Into::into)?;
        Ok(result.map(|value| {
            let header = value.header.clone();
            with_milliseconds(value, &header)
        }))
    }
    async fn block_by_hash(
        &self,
        hash: B256,
        full: bool,
    ) -> RpcResult<Option<WithOtherFields<RpcBlock<Eth::NetworkTypes>>>> {
        let result = self.0.rpc_block(hash.into(), full).await.map_err(Into::into)?;
        Ok(result.map(|value| {
            let header = value.header.clone();
            with_milliseconds(value, &header)
        }))
    }
    async fn header_by_number(
        &self,
        block_number: BlockNumberOrTag,
    ) -> RpcResult<Option<WithOtherFields<Header>>> {
        let result = self.0.rpc_block_header(block_number.into()).await.map_err(Into::into)?;
        Ok(result.map(|value| {
            let header = value.clone();
            with_milliseconds(value, &header)
        }))
    }
    async fn header_by_hash(&self, hash: B256) -> RpcResult<Option<WithOtherFields<Header>>> {
        let result = self.0.rpc_block_header(hash.into()).await.map_err(Into::into)?;
        Ok(result.map(|value| {
            let header = value.clone();
            with_milliseconds(value, &header)
        }))
    }
    async fn finalized_block(
        &self,
        verified_validator_num: i64,
        full: bool,
    ) -> RpcResult<Option<WithOtherFields<RpcBlock<Eth::NetworkTypes>>>> {
        let result =
            self.0.rpc_finalized_block(verified_validator_num, full).await.map_err(Into::into)?;
        Ok(result.map(|value| {
            let header = value.header.clone();
            with_milliseconds(value, &header)
        }))
    }
    async fn finalized_header(
        &self,
        verified_validator_num: i64,
    ) -> RpcResult<Option<WithOtherFields<Header>>> {
        let result =
            self.0.rpc_finalized_header(verified_validator_num).await.map_err(Into::into)?;
        Ok(result.map(|value| {
            let header = value.clone();
            with_milliseconds(value, &header)
        }))
    }
}

#[cfg(test)]
mod tests;
