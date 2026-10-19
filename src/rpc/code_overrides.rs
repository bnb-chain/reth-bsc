//! Public RPC code-override support for CAS20 prefix routing.

use crate::{
    evm::{block_env::BscBlockEnv, precompiles::cas20::is_cas20_precompile},
    node::evm::config::BscEvmConfig,
};
use alloy_consensus::BlockHeader;
use alloy_evm::overrides::{apply_block_overrides, apply_state_overrides, OverrideBlockHashes};
use alloy_primitives::{Address, Bytes, B256, U256};
use alloy_rpc_types_eth::{
    simulate::{SimBlock, SimulatePayload, SimulatedBlock},
    state::{EvmOverrides, StateOverride},
    BlockId, BlockOverrides, Bundle, EthCallResponse, StateContext, TransactionRequest,
};
use jsonrpsee::{core::RpcResult, proc_macros::rpc};
use reth_chainspec::{ChainSpecProvider, EthChainSpec};
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
use reth_revm::{database::StateProviderDatabase, db::State};
use revm::{context::Block, context_interface::Cfg, database::InMemoryDB, DatabaseCommit};
use revm_inspectors::transfer::TransferInspector;
use std::collections::{BTreeMap, BTreeSet};

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
        block_overrides: Option<Box<BlockOverrides>>,
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
    Eth: EthCall + RpcNodeCore<Evm = BscEvmConfig>,
    Eth::NetworkTypes: RpcTypes<TransactionRequest = TransactionRequest>,
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
        overrides: Option<Box<BlockOverrides>>,
    ) -> RpcResult<U256> {
        let (mut env, at) =
            self.0.evm_env_at(block.unwrap_or_default()).await.map_err(Into::into)?;
        env.block_env.disabled_cas20 = code_overridden_addresses(state.as_ref());
        if let Some(overrides) = overrides {
            // blockHash overrides are ignored, as in go-bsc.
            apply_block_overrides(
                (*overrides).clone(),
                &mut InMemoryDB::default(),
                env.block_env.inner_mut(),
            );
            env.block_env
                .apply_block_overrides_ext(&overrides)
                .map_err(EthApiError::InvalidParams)?;
            // The estimator sizes its search from the env, before any EVM is created.
            env = self.0.evm_config().with_block_rules(env);
        }
        self.0
            .spawn_with_state(Some(at), move |eth, provider| {
                // Block overrides are already in `env`; blockHash overrides are ignored, as in go-bsc.
                EstimateCall::estimate_gas_with(
                    &eth,
                    env,
                    request,
                    provider,
                    EvmOverrides::state(state),
                )
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
            return Err(EthApiError::other(EthSimulateError::TooManyBlocks).into())
        }

        let block = block.unwrap_or_default();

        let SimulatePayload {
            block_state_calls,
            trace_transfers,
            validation,
            return_full_transactions,
        } = payload;

        if block_state_calls.is_empty() {
            return Err(EthApiError::InvalidParams(String::from("calls are empty.")).into())
        }

        let _permit = self.0.acquire_owned_blocking_io().await;

        let base_block = self
            .0
            .recovered_block(block)
            .await?
            .ok_or_else(|| EthApiError::other(EthSimulateError::BlockNotFound { block }))?;
        let parent = base_block.sealed_header().clone();
        let max_simulate_blocks = self.0.max_simulate_blocks();

        self.0.spawn_with_state_at_block(block, move |this, db| {
            let state_provider = db.database.0 .0;
            let mut db = State::builder()
                .with_database(StateProviderDatabase::new(&state_provider))
                .with_bundle_update()
                .build();
            let mut parent = parent;

            let chain_id = this.provider().chain_spec().chain_id();

            // Validate block ordering and fill gaps with empty blocks so every entry has an
            // explicit `number` and `time` override and the chain is contiguous (see the
            // execution-apis spec note: "If the block number is increased more than 1 compared
            // to the previous block, new empty blocks are generated in between.").
            let block_state_calls = simulate::sanitize_chain(
                block_state_calls,
                &parent,
                chain_id,
                max_simulate_blocks,
            )?;

            let mut blocks: Vec<SimulatedBlock<RpcBlock<Eth::NetworkTypes>>> =
                Vec::with_capacity(block_state_calls.len());

            let call_gas_limit = this.call_gas_limit();
            let mut remaining_call_gas_limit = (call_gas_limit > 0).then_some(call_gas_limit);

            for block in block_state_calls {
                let SimBlock { block_overrides, state_overrides, calls } = block;

                let attributes = this
                    .pending_env_builder()
                    .pending_env_attributes(&parent, block_overrides.as_ref())
                    .map_err(Eth::Error::from_eth_err)?;

                let mut evm_env = this
                    .evm_config()
                    .next_evm_env(&parent, &attributes)
                    .map_err(RethError::other)
                    .map_err(Eth::Error::from_eth_err)?;

                // Always disable EIP-3607
                evm_env.cfg_env.disable_eip3607 = true;

                // EIP-7825's transaction gas cap is only active with Amsterdam's
                // regular/state-gas accounting.
                if !evm_env.cfg_env.is_amsterdam_eip8037_enabled() {
                    evm_env.cfg_env.tx_gas_limit_cap = Some(u64::MAX);
                }

                if !validation {
                    // If not explicitly required, we disable nonce check <https://github.com/paradigmxyz/reth/issues/16108>
                    evm_env.cfg_env.disable_nonce_check = true;
                    evm_env.cfg_env.disable_base_fee = true;
                    evm_env.block_env.inner_mut().basefee = 0;
                }

                // Set prevrandao to zero for simulated blocks by default,
                // matching spec behavior where MixDigest is zero-initialized.
                // If user provides an override, it will be applied by apply_block_overrides.
                evm_env.block_env.inner_mut().prevrandao = Some(B256::ZERO);
                // Unlike upstream, the parent difficulty is not carried over: BSC never
                // activates Paris, and the synthetic header keeps the requested difficulty.

                if let Some(block_overrides) = block_overrides {
                    // ensure we don't allow uncapped gas limit per block
                    if block_overrides.gas_limit.is_some_and(|limit| {
                        limit > evm_env.block_env.gas_limit() && limit > this.call_gas_limit()
                    }) {
                        return Err(EthApiError::other(EthSimulateError::GasLimitReached).into())
                    }
                    apply_block_overrides(
                        block_overrides.clone(),
                        &mut db,
                        evm_env.block_env.inner_mut(),
                    );
                    // Let chain-specific block environments observe the overrides
                    // (no-op for the standard `BlockEnv`).
                    evm_env
                        .block_env
                        .apply_block_overrides_ext(&block_overrides)
                        .map_err(EthApiError::InvalidParams)?;
                }
                if let Some(ref state_overrides) = state_overrides {
                    evm_env
                        .block_env
                        .apply_state_overrides_ext(state_overrides)
                        .map_err(EthApiError::InvalidParams)?;
                    apply_state_overrides(state_overrides.clone(), &mut db)
                        .map_err(Eth::Error::from_eth_err)?;
                }

                // go-bsc rebuilds its precompile map per simulated block. Only
                // explicit code overrides in this block suppress native routing.
                evm_env.block_env.disabled_cas20 =
                    code_overridden_addresses(state_overrides.as_ref());

                let chain_id = evm_env.cfg_env.chain_id;

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
                    // prepare inspector to capture transfer inside the evm so they are recorded
                    // and included in logs
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
                        &state_provider,
                        calls,
                        &mut remaining_call_gas_limit,
                        chain_id,
                        this.compute_state_root_for_eth_simulate(),
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
                        &state_provider,
                        calls,
                        &mut remaining_call_gas_limit,
                        chain_id,
                        this.compute_state_root_for_eth_simulate(),
                        this.converter(),
                    )
                    .map_err(map_err)?
                };

                let simulated_header = result.block.clone_sealed_header();
                db.override_block_hashes(BTreeMap::from([(
                    simulated_header.number(),
                    simulated_header.hash(),
                )]));
                parent = simulated_header;

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
