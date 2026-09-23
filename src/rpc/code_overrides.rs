//! RPC code-override support for CAS20 prefix routing.
//!
//! Reth c55455b applies overrides to the temporary database, but does not pass
//! overridden addresses to dynamic precompile lookups. Keep its simulation and
//! callMany execution flow here until the upstream helpers expose that hook.

use crate::evm::{block_env::BscBlockEnv, precompiles::cas20::is_cas20_precompile};
use alloy_consensus::BlockHeader;
use alloy_evm::overrides::{apply_block_overrides, apply_state_overrides};
use alloy_network::TransactionBuilder;
use alloy_primitives::{Address, Bytes, B256, U256};
use alloy_rpc_types_eth::{
    simulate::{SimBlock, SimulatePayload, SimulatedBlock},
    state::{EvmOverrides, StateOverride},
    BlockId, BlockOverrides, Bundle, EthCallResponse, StateContext, TransactionRequest,
};
use jsonrpsee::{core::RpcResult, proc_macros::rpc};
use reth_errors::RethError;
use reth_evm::{
    block::BlockExecutor, env::BlockEnvironment, execute::BlockBuilder, ConfigureEvm, Evm,
    EvmFactory,
};
use reth_primitives_traits::BlockBody;
use reth_provider::{BlockIdReader, ProviderError};
use reth_revm::cancelled::CancelOnDrop;
use reth_rpc_convert::RpcTypes;
use reth_rpc_eth_api::{
    helpers::{estimate::EstimateCall, EthCall},
    FromEvmError, RpcBlock, RpcNodeCore,
};
use reth_rpc_eth_types::{
    error::{AsEthApiError, FromEthApiError},
    simulate::{self, EthSimulateError},
    BlockOverridesExt, EthApiError,
};
use revm::{context::Block, DatabaseCommit};
use revm_inspectors::transfer::TransferInspector;
use std::collections::BTreeSet;

pub(crate) fn code_overridden_addresses(overrides: Option<&StateOverride>) -> BTreeSet<Address> {
    overrides
        .into_iter()
        .flat_map(|state| state.iter())
        .filter_map(|(address, account)| {
            (account.code.is_some() && is_cas20_precompile(*address)).then_some(*address)
        })
        .collect()
}

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
        let disabled = code_overridden_addresses(state.as_ref());
        let _permit = self.0.acquire_owned_blocking_io().await;
        let guard = CancelOnDrop::default();
        let cancel = guard.clone();
        let eth = self.0.clone();
        let result = self
            .0
            .spawn_with_call_at(
                request,
                block.unwrap_or_default(),
                EvmOverrides::new(state, overrides),
                move |db, mut env, tx| {
                    if cancel.is_cancelled() {
                        return Err(EthApiError::InternalEthError.into());
                    }
                    env.block_env.disabled_cas20 = disabled;
                    eth.transact(db, env, tx)
                },
            )
            .await
            .map_err(Into::into)?;
        drop(guard);
        Eth::Error::ensure_success(result.result).map_err(Into::into)
    }

    async fn estimate_gas(
        &self,
        request: TransactionRequest,
        block: Option<BlockId>,
        state: Option<StateOverride>,
    ) -> RpcResult<U256> {
        let (mut env, at) =
            self.0.evm_env_at(block.unwrap_or_default()).await.map_err(Into::into)?;
        env.block_env.disabled_cas20 = code_overridden_addresses(state.as_ref());
        self.0
            .spawn_with_state(Some(at), move |eth, provider| {
                EstimateCall::estimate_gas_with(&eth, env, request, provider, state)
            })
            .await
            .map_err(Into::into)
    }

    async fn simulate_v1(
        &self,
        payload: SimulatePayload<TransactionRequest>,
        block: Option<BlockId>,
    ) -> RpcResult<Vec<SimulatedBlock<RpcBlock<Eth::NetworkTypes>>>> {
        let _permit = self.0.tracing_task_guard().clone().acquire_owned().await;
        self.simulate(payload, block).await.map_err(Into::into)
    }

    async fn call_many(
        &self,
        bundles: Vec<Bundle<TransactionRequest>>,
        state_context: Option<StateContext>,
        state: Option<StateOverride>,
    ) -> RpcResult<Vec<Vec<EthCallResponse>>> {
        self.many(bundles, state_context, state).await.map_err(Into::into)
    }
}

impl<Eth> BscCodeOverridesApiImpl<Eth>
where
    Eth: EthCall,
    Eth::NetworkTypes: RpcTypes<TransactionRequest = TransactionRequest>,
    reth_evm::EvmFactoryFor<Eth::Evm>: EvmFactory<BlockEnv = BscBlockEnv>,
{
    async fn simulate(
        &self,
        payload: SimulatePayload<TransactionRequest>,
        block: Option<BlockId>,
    ) -> Result<Vec<SimulatedBlock<RpcBlock<Eth::NetworkTypes>>>, Eth::Error> {
        if payload.block_state_calls.len() > self.0.max_simulate_blocks() as usize {
            return Err(EthApiError::other(EthSimulateError::TooManyBlocks).into());
        }

        let block = block.unwrap_or_default();

        let SimulatePayload {
            block_state_calls,
            trace_transfers,
            validation,
            return_full_transactions,
        } = payload;

        if block_state_calls.is_empty() {
            return Err(EthApiError::InvalidParams(String::from("calls are empty.")).into());
        }

        let base_block =
            self.0.recovered_block(block).await?.ok_or(EthApiError::HeaderNotFound(block))?;
        let mut parent = base_block.sealed_header().clone();

        self.0.spawn_with_state_at_block(block, move |this, mut db| {
                let mut blocks: Vec<SimulatedBlock<RpcBlock<Eth::NetworkTypes>>> =
                    Vec::with_capacity(block_state_calls.len());

                let mut prev_block_number = parent.number();
                let mut prev_timestamp = parent.timestamp();

                for block in block_state_calls {
                    if let Some(number) = block.block_overrides.as_ref().and_then(|o| o.number) {
                        let number: u64 = number.try_into().unwrap_or(u64::MAX);
                        if number <= prev_block_number {
                            return Err(EthApiError::other(EthSimulateError::BlockNumberInvalid {
                                got: number,
                                parent: prev_block_number,
                            })
                            .into());
                        }
                    }
                    if let Some(time) = block
                        .block_overrides
                        .as_ref()
                        .and_then(|o| o.time)
                        .filter(|&t| t <= prev_timestamp)
                    {
                        return Err(EthApiError::other(EthSimulateError::BlockTimestampInvalid {
                            got: time,
                            parent: prev_timestamp,
                        })
                        .into());
                    }

                    let attributes = this.next_env_attributes(&parent)?;

                    let mut evm_env = this
                        .evm_config()
                        .next_evm_env(&parent, &attributes)
                        .map_err(RethError::other)
                        .map_err(Eth::Error::from_eth_err)?;

                    evm_env.cfg_env.disable_eip3607 = true;

                    if !validation {
                        evm_env.cfg_env.disable_nonce_check = true;
                        evm_env.cfg_env.disable_base_fee = true;
                        evm_env.cfg_env.tx_gas_limit_cap = Some(u64::MAX);
                        evm_env.block_env.inner_mut().basefee = 0;
                    }

                    let SimBlock { block_overrides, state_overrides, calls } = block;

                    evm_env.block_env.inner_mut().prevrandao = Some(B256::ZERO);

                    if let Some(block_overrides) = block_overrides {
                        if block_overrides.gas_limit.is_some_and(|limit|
                            limit > evm_env.block_env.gas_limit() && limit > this.call_gas_limit())
                        {
                            return Err(EthApiError::other(EthSimulateError::GasLimitReached).into())
                        }
                        apply_block_overrides(
                            block_overrides.clone(),
                            &mut db,
                            evm_env.block_env.inner_mut(),
                        );
                        evm_env
                            .block_env
                            .apply_block_overrides_ext(&block_overrides)
                            .map_err(EthApiError::InvalidParams)?;
                    }
                    if let Some(ref state_overrides) = state_overrides {
                        apply_state_overrides(state_overrides.clone(), &mut db)
                            .map_err(Eth::Error::from_eth_err)?;
                    }

                    // go-bsc rebuilds its precompile map per simulated block. Only
                    // explicit code overrides in this block suppress native routing.
                    evm_env.block_env.disabled_cas20 =
                        code_overridden_addresses(state_overrides.as_ref());

                    let block_gas_limit = evm_env.block_env.gas_limit();
                    let chain_id = evm_env.cfg_env.chain_id;

                    let default_gas_limit = {
                        let total_specified_gas =
                            calls.iter().filter_map(|tx| tx.as_ref().gas_limit()).sum::<u64>();
                        let txs_without_gas_limit =
                            calls.iter().filter(|tx| tx.as_ref().gas_limit().is_none()).count();

                        if total_specified_gas > block_gas_limit {
                            return Err(EthApiError::Other(Box::new(
                                EthSimulateError::BlockGasLimitExceeded,
                            ))
                            .into())
                        }

                        if txs_without_gas_limit > 0 {
                            let gas_per_tx = (block_gas_limit - total_specified_gas) /
                                txs_without_gas_limit as u64;
                            let call_gas_limit = this.call_gas_limit();
                            if call_gas_limit > 0 {
                                gas_per_tx.min(call_gas_limit)
                            } else {
                                gas_per_tx
                            }
                        } else {
                            0
                        }
                    };

                    let ctx = this
                        .evm_config()
                        .context_for_next_block(&parent, attributes)
                        .map_err(RethError::other)
                        .map_err(Eth::Error::from_eth_err)?;
                    let map_err = |e: EthApiError| -> Eth::Error {
                        match e.as_simulate_error() {
                            Some(sim_err) => Eth::Error::from_eth_err(EthApiError::other(sim_err)),
                            None => Eth::Error::from_eth_err(e),
                        }
                    };

                    let (result, results) = if trace_transfers {
                        let inspector = TransferInspector::new(false).with_logs(true);
                        let evm = this
                            .evm_config()
                            .evm_with_env_and_inspector(&mut db, evm_env, inspector);
                        let mut builder = this.evm_config().create_block_builder(evm, &parent, ctx);

                        if let Some(ref state_overrides) = state_overrides {
                            simulate::apply_precompile_overrides(
                                state_overrides,
                                builder.evm_mut().precompiles_mut(),
                            )
                            .map_err(|e| Eth::Error::from_eth_err(EthApiError::other(e)))?;
                        }

                        simulate::execute_transactions(
                            builder,
                            calls,
                            default_gas_limit,
                            chain_id,
                            this.converter(),
                        )
                        .map_err(map_err)?
                    } else {
                        let evm = this.evm_config().evm_with_env(&mut db, evm_env);
                        let mut builder = this.evm_config().create_block_builder(evm, &parent, ctx);

                        if let Some(ref state_overrides) = state_overrides {
                            simulate::apply_precompile_overrides(
                                state_overrides,
                                builder.evm_mut().precompiles_mut(),
                            )
                            .map_err(|e| Eth::Error::from_eth_err(EthApiError::other(e)))?;
                        }

                        simulate::execute_transactions(
                            builder,
                            calls,
                            default_gas_limit,
                            chain_id,
                            this.converter(),
                        )
                        .map_err(map_err)?
                    };

                    parent = result.block.clone_sealed_header();

                    prev_block_number = parent.number();
                    prev_timestamp = parent.timestamp();

                    let block = simulate::build_simulated_block::<Eth::Error, _>(
                        result.block,
                        results,
                        return_full_transactions.into(),
                        this.converter(),
                    )?;

                    blocks.push(block);
                }

                Ok(blocks)
            })
            .await
    }
    async fn many(
        &self,
        bundles: Vec<Bundle<TransactionRequest>>,
        state_context: Option<StateContext>,
        mut state_override: Option<StateOverride>,
    ) -> Result<Vec<Vec<EthCallResponse>>, Eth::Error> {
        let disabled = code_overridden_addresses(state_override.as_ref());
        if bundles.is_empty() {
            return Err(EthApiError::InvalidParams(String::from("bundles are empty.")).into());
        }

        let StateContext { transaction_index, block_number } = state_context.unwrap_or_default();
        let transaction_index = transaction_index.unwrap_or_default();

        let mut target_block = block_number.unwrap_or_default();
        let is_block_target_pending = target_block.is_pending();

        if !is_block_target_pending {
            let Some(block_hash) = self
                .0
                .provider()
                .block_hash_for_id(target_block)
                .map_err(Eth::Error::from_eth_err::<ProviderError>)?
            else {
                return Err(EthApiError::HeaderNotFound(target_block).into());
            };
            target_block = block_hash.into();
        }

        let block = self
            .0
            .recovered_block(target_block)
            .await?
            .ok_or(EthApiError::HeaderNotFound(target_block))?;
        let evm_env = self.0.evm_env_for_header(block.sealed_block().sealed_header())?;

        let mut at = block.parent_hash();
        let mut replay_block_txs = true;

        let num_txs =
            transaction_index.index().unwrap_or_else(|| block.body().transactions().len());
        if !is_block_target_pending && num_txs == block.body().transactions().len() {
            at = block.hash();
            replay_block_txs = false;
        }

        self.0
            .spawn_with_state_at_block(at, move |this, mut db| {
                let mut all_results = Vec::with_capacity(bundles.len());

                if replay_block_txs {
                    let mut executor = RpcNodeCore::evm_config(&this)
                        .executor_for_block(&mut db, block.sealed_block())
                        .map_err(RethError::other)
                        .map_err(Eth::Error::from_eth_err)?;
                    executor.apply_pre_execution_changes().map_err(Eth::Error::from_eth_err)?;
                    for tx in block.transactions_recovered().take(num_txs) {
                        executor.execute_transaction(tx).map_err(Eth::Error::from_eth_err)?;
                    }
                }

                for (bundle_index, bundle) in bundles.into_iter().enumerate() {
                    let Bundle { transactions, block_override } = bundle;
                    if transactions.is_empty() {
                        continue;
                    }

                    let mut bundle_results = Vec::with_capacity(transactions.len());
                    let block_overrides = block_override.map(Box::new);

                    for (tx_index, tx) in transactions.into_iter().enumerate() {
                        let overrides =
                            EvmOverrides::new(state_override.take(), block_overrides.clone());

                        let (mut current_evm_env, prepared_tx) = this
                            .prepare_call_env(evm_env.clone(), tx, &mut db, overrides)
                            .map_err(|err| {
                                Eth::Error::from_eth_err(EthApiError::call_many_error(
                                    bundle_index,
                                    tx_index,
                                    err.into(),
                                ))
                            })?;
                        current_evm_env.block_env.disabled_cas20 = disabled.clone();
                        let res = this.transact(&mut db, current_evm_env, prepared_tx).map_err(
                            |err| {
                                Eth::Error::from_eth_err(EthApiError::call_many_error(
                                    bundle_index,
                                    tx_index,
                                    err.into(),
                                ))
                            },
                        )?;

                        match Eth::Error::ensure_success(res.result) {
                            Ok(output) => {
                                bundle_results
                                    .push(EthCallResponse { value: Some(output), error: None });
                            }
                            Err(err) => {
                                bundle_results.push(EthCallResponse {
                                    value: None,
                                    error: Some(err.to_string()),
                                });
                            }
                        }

                        db.commit(res.state);
                    }

                    all_results.push(bundle_results);
                }

                Ok(all_results)
            })
            .await
    }
}

#[cfg(test)]
mod tests;
