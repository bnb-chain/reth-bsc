//! eth_createAccessList with CAS20 native accesses in addition to opcode accesses.
//! The opcode inspector cannot observe storage accessed by native precompiles.

use crate::{evm::precompiles::cas20::access_list::capture, node::evm::config::BscEvmConfig};
use alloy_eips::{eip2930::AccessListResult, BlockId};
use alloy_primitives::U256;
use alloy_rpc_types_eth::{
    state::{EvmOverrides, StateOverride},
    TransactionRequest,
};
use evm2_inspectors::access_list::AccessListInspector;
use jsonrpsee::{core::RpcResult, proc_macros::rpc};
use reth_bsc_evm::types::BscEvmTypes;
use reth_evm::{ConfigureEvm, Evm, EvmEnv, TransactionEnvMut};
use reth_rpc_convert::RpcTypes;
use reth_rpc_eth_api::{
    helpers::{EthCall, Trace},
    FromEvmError, RpcNodeCore,
};

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
    Eth: EthCall + Trace + RpcNodeCore<Evm = BscEvmConfig>,
    Eth::NetworkTypes: RpcTypes<TransactionRequest = TransactionRequest>,
{
    async fn create_access_list(
        &self,
        request: TransactionRequest,
        block: Option<BlockId>,
        state_override: Option<StateOverride>,
    ) -> RpcResult<AccessListResult> {
        let (env, at) = self.0.evm_env_at(block.unwrap_or_default()).await.map_err(Into::into)?;
        self.0
            .spawn_with_state_at_block(at, move |eth, mut db| {
                let initial = request.access_list.clone().unwrap_or_default();
                let (env, mut tx) = eth.prepare_call_env(
                    env,
                    request,
                    &mut db,
                    EvmOverrides::state(state_override),
                )?;
                let chain_id = env.chain_id();
                let mut evm = eth.evm_config().evm_with_env(&mut db, env);
                let inspector =
                    AccessListInspector::new(initial).with_excluded_from_tx(tx.inner(), chain_id);
                let (result, native) = capture(|| evm.transact_with_inspector(&tx, inspector));
                let (inspector, result) = result.map_err(Eth::Error::from_evm_err)?;
                let access_list = native
                    .merge(inspector.access_list(), |addr| inspector.excluded().contains(addr));
                let gas_used = result.result.tx_gas_used();
                if let Err(err) = Eth::Error::ensure_success::<BscEvmTypes>(result.result) {
                    return Ok(AccessListResult {
                        access_list,
                        gas_used: U256::from(gas_used),
                        error: Some(err.to_string()),
                    });
                }
                tx.inner_mut().set_access_list(access_list.clone());
                let result = evm.transact_result(&tx).map_err(Eth::Error::from_evm_err)?;
                let gas_used = result.tx_gas_used();
                let error =
                    Eth::Error::ensure_success::<BscEvmTypes>(result).err().map(|e| e.to_string());
                Ok(AccessListResult { access_list, gas_used: U256::from(gas_used), error })
            })
            .await
            .map_err(Into::into)
    }
}
