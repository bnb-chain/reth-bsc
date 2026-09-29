//! Public RPC adapters preserving BSC's named parameters.
//!
//! Execution and code-override routing use the shared Reth helpers and
//! BscBlockEnv's state-override hook, including authenticated calls and traces.

use crate::evm::block_env::BscBlockEnv;
use alloy_primitives::{Bytes, U256};
use alloy_rpc_types_eth::{
    simulate::{SimulatePayload, SimulatedBlock},
    state::{EvmOverrides, StateOverride},
    BlockId, BlockOverrides, Bundle, EthCallResponse, StateContext, TransactionRequest,
};
use jsonrpsee::{core::RpcResult, proc_macros::rpc};
use reth_evm::EvmFactory;
use reth_rpc_convert::RpcTypes;
use reth_rpc_eth_api::{helpers::EthCall, RpcBlock};

#[rpc(server, namespace = "eth")]
pub trait BscCodeOverridesApi<B> {
    #[method(name = "call")]
    async fn call(
        &self,
        request: TransactionRequest,
        block_number: Option<BlockId>,
        state_overrides: Option<StateOverride>,
        block_overrides: Option<Box<BlockOverrides>>,
    ) -> RpcResult<Bytes>;
    #[method(name = "estimateGas")]
    async fn estimate_gas(
        &self,
        request: TransactionRequest,
        block_number: Option<BlockId>,
        state_override: Option<StateOverride>,
    ) -> RpcResult<U256>;
    #[method(name = "simulateV1")]
    async fn simulate_v1(
        &self,
        opts: SimulatePayload<TransactionRequest>,
        block_number: Option<BlockId>,
    ) -> RpcResult<Vec<SimulatedBlock<B>>>;
    #[method(name = "callMany")]
    async fn call_many(
        &self,
        bundles: Vec<Bundle<TransactionRequest>>,
        state_context: Option<StateContext>,
        state_override: Option<StateOverride>,
    ) -> RpcResult<Vec<Vec<EthCallResponse>>>;
}

pub struct BscCodeOverridesApiImpl<Eth>(pub Eth);

#[async_trait::async_trait]
impl<Eth> BscCodeOverridesApiServer<RpcBlock<Eth::NetworkTypes>> for BscCodeOverridesApiImpl<Eth>
where
    Eth: EthCall,
    Eth::NetworkTypes: RpcTypes<TransactionRequest = TransactionRequest>,
    reth_evm::EvmFactoryFor<Eth::Evm>: EvmFactory<BlockEnv = BscBlockEnv>,
{
    async fn call(
        &self,
        request: TransactionRequest,
        block: Option<BlockId>,
        state: Option<StateOverride>,
        overrides: Option<Box<BlockOverrides>>,
    ) -> RpcResult<Bytes> {
        self.0.call(request, block, EvmOverrides::new(state, overrides)).await.map_err(Into::into)
    }

    async fn estimate_gas(
        &self,
        request: TransactionRequest,
        block: Option<BlockId>,
        state: Option<StateOverride>,
    ) -> RpcResult<U256> {
        EthCall::estimate_gas_at(&self.0, request, block.unwrap_or_default(), state)
            .await
            .map_err(Into::into)
    }

    async fn simulate_v1(
        &self,
        payload: SimulatePayload<TransactionRequest>,
        block: Option<BlockId>,
    ) -> RpcResult<Vec<SimulatedBlock<RpcBlock<Eth::NetworkTypes>>>> {
        let _permit = self.0.tracing_task_guard().clone().acquire_owned().await;
        self.0.simulate_v1(payload, block).await.map_err(Into::into)
    }

    async fn call_many(
        &self,
        bundles: Vec<Bundle<TransactionRequest>>,
        state_context: Option<StateContext>,
        state: Option<StateOverride>,
    ) -> RpcResult<Vec<Vec<EthCallResponse>>> {
        self.0.call_many(bundles, state_context, state).await.map_err(Into::into)
    }
}

#[cfg(test)]
mod tests;
