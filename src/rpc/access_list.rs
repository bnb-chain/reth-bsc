//! eth_createAccessList with CAS20 native accesses in addition to opcode accesses.
//! The opcode inspector cannot observe storage accessed by native precompiles.

use crate::evm::precompiles::cas20::access_list::capture;
use alloy_eips::{eip2930::AccessListResult, BlockId};
use alloy_primitives::U256;
use alloy_rpc_types_eth::{
    state::{EvmOverrides, StateOverride},
    TransactionRequest,
};
use jsonrpsee::{core::RpcResult, proc_macros::rpc};
use reth_evm::{BlockEnvFor, EvmFactory, TransactionEnvMut};
use reth_provider::EvmStateProviderAdapter;
use reth_revm::{database::StateProviderDatabase, db::State};
use reth_rpc_convert::RpcTypes;
use reth_rpc_eth_api::{
    helpers::{EthCall, Trace},
    FromEvmError,
};
use reth_rpc_eth_types::BlockOverridesExt;
use revm_inspectors::access_list::AccessListInspector;

#[cfg(test)]
mod tests;

#[rpc(server, namespace = "eth")]
pub trait BscAccessListApi {
    #[method(name = "createAccessList")]
    async fn create_access_list(
        &self,
        request: TransactionRequest,
        block: Option<BlockId>,
        state_override: Option<StateOverride>,
    ) -> RpcResult<AccessListResult>;
}

pub struct BscAccessListApiImpl<Eth>(pub Eth);

#[async_trait::async_trait]
impl<Eth> BscAccessListApiServer for BscAccessListApiImpl<Eth>
where
    Eth: EthCall + Trace,
    reth_evm::EvmFactoryFor<Eth::Evm>: EvmFactory<BlockEnv = crate::evm::block_env::BscBlockEnv>,
    Eth::NetworkTypes: RpcTypes<TransactionRequest = TransactionRequest>,
    BlockEnvFor<Eth::Evm>: BlockOverridesExt,
{
    async fn create_access_list(
        &self,
        request: TransactionRequest,
        block: Option<BlockId>,
        state_override: Option<StateOverride>,
    ) -> RpcResult<AccessListResult> {
        let (mut env, at) =
            self.0.evm_env_at(block.unwrap_or_default()).await.map_err(Into::into)?;
        env.block_env.disabled_cas20 =
            super::code_overrides::code_overridden_addresses(state_override.as_ref());
        self.0
            .spawn_blocking_io_with_state(at, move |eth, state| {
                let mut db = State::builder()
                    .with_database(StateProviderDatabase::new(EvmStateProviderAdapter(state)))
                    .build();
                let initial = request.access_list.clone().unwrap_or_default();
                // Apply the RPC gas and memory limits to both executions, including
                // omitted-gas requests whose caller allowance exceeds the RPC cap.
                let (env, mut tx) = eth.prepare_call_env(
                    env,
                    request,
                    &mut db,
                    EvmOverrides::state(state_override),
                )?;

                let mut inspector = AccessListInspector::new(initial);
                let (result, native) =
                    capture(|| eth.inspect(&mut db, env.clone(), tx.clone(), &mut inspector));
                let result = result?;
                let access_list = native
                    .merge(inspector.access_list(), |addr| inspector.excluded().contains(addr));
                let gas_used = result.result.tx_gas_used();
                if let Err(err) = Eth::Error::ensure_success(result.result) {
                    return Ok(AccessListResult {
                        access_list,
                        gas_used: U256::from(gas_used),
                        error: Some(err.to_string()),
                    });
                }
                tx.set_access_list(access_list.clone());
                let result = eth.transact(&mut db, env, tx)?;
                let gas_used = result.result.tx_gas_used();
                let error = Eth::Error::ensure_success(result.result).err().map(|e| e.to_string());
                Ok(AccessListResult { access_list, gas_used: U256::from(gas_used), error })
            })
            .await
            .map_err(Into::into)
    }
}
