//! Public RPC code-override support for CAS20 prefix routing.

use crate::{evm::precompiles::cas20::is_cas20_precompile, node::evm::config::BscEvmConfig};
use alloy_consensus::BlockHeader;
use alloy_primitives::{Address, Bytes, U256};
use alloy_rpc_types_eth::{
    simulate::{SimBlock, SimulatePayload, SimulatedBlock},
    state::{EvmOverrides, StateOverride},
    BlockId, BlockOverrides, Bundle, EthCallResponse, StateContext, TransactionRequest,
};
use evm2::evm::CacheDB;
use evm2_inspectors::transfer::TransferInspector;
use jsonrpsee::{core::RpcResult, proc_macros::rpc};
use reth_chainspec::{ChainSpecProvider, EthChainSpec, EthereumHardforks};
use reth_errors::RethError;
use reth_evm::{
    database::StateProviderDatabase, execute::BlockBuilder, BlockExecutor, BundleState,
    ConfigureEvm, Evm, EvmEnv, EvmTypesFor,
};
use reth_execution_types::BundleSource;
use reth_provider::{EvmStateProviderAdapter, StateProvider};
use reth_rpc_convert::RpcTypes;
use reth_rpc_eth_api::{
    helpers::{estimate::EstimateCall, EthCall},
    FromEvmError, RpcBlock, RpcNodeCore,
};
use reth_rpc_eth_types::{
    cache::db::{apply_block_overrides, apply_state_overrides_with_state},
    error::{AsEthApiError, FromEthApiError},
    simulate::{self, EthSimulateError, SimulatedState},
    BlockOverridesExt, EthApiError,
};
use reth_tasks::CancelOnDrop;
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
    Eth: EthCall
        + reth_rpc_eth_api::helpers::Trace
        + RpcNodeCore<Evm = BscEvmConfig, Primitives = crate::BscPrimitives>,
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
                    env.code_overrides = disabled;
                    eth.transact(db, env, tx)
                },
            )
            .await
            .map_err(Into::into)?;
        drop(guard);
        Eth::Error::ensure_success::<reth_bsc_evm::types::BscEvmTypes>(result.result)
            .map_err(Into::into)
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
        env.code_overrides = code_overridden_addresses(state.as_ref());
        if let Some(overrides) = overrides {
            // blockHash overrides are ignored, as in go-bsc.
            apply_block_overrides(
                (*overrides).clone(),
                &mut CacheDB::new(evm2::evm::Db::new(evm2::evm::EmptyDB::default())),
                &mut env,
            );
            env.apply_block_overrides_ext(&overrides).map_err(EthApiError::InvalidParams)?;
            // The estimator sizes its search from the env, before any EVM is created.
            env = self.0.evm_config().with_block_rules(env);
        }
        self.0
            .spawn_blocking_io_with_state(at, move |eth, provider| {
                // Block overrides are already in `env`; blockHash overrides are ignored, as in
                // go-bsc.
                EstimateCall::estimate_gas_with(
                    &eth,
                    env,
                    request,
                    EvmStateProviderAdapter(provider),
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
    Eth: EthCall
        + reth_rpc_eth_api::helpers::Trace
        + RpcNodeCore<Evm = BscEvmConfig, Primitives = crate::BscPrimitives>,
    Eth::NetworkTypes: RpcTypes<TransactionRequest = TransactionRequest>,
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

        let permit =
            self.0.acquire_owned_blocking_io().await.map_err(|_| EthApiError::InternalEthError)?;

        let base_block = self
            .0
            .recovered_block(block)
            .await?
            .ok_or_else(|| EthApiError::other(EthSimulateError::BlockNotFound { block }))?;
        let parent = base_block.sealed_header().clone();
        let max_simulate_blocks = self.0.max_simulate_blocks();
        // Load the state of the block resolved above: resolving a tag such as `latest` again
        // could return a newer block than `parent`.
        let state_at = if block.is_pending() { block } else { parent.hash().into() };

        self.0
            .spawn_with_state_at_block(state_at, move |this, db| {
                let _permit = permit;
                let state_provider = db.db.into_inner().into_inner().0;
                let mut db = CacheDB::new(evm2::evm::Db::new(StateProviderDatabase::new(
                    (&state_provider).into_evm_state_provider(),
                )));
                let mut parent = parent;
                let mut simulation_state =
                    this.compute_state_root_for_eth_simulate().then(SimulatedState::default);

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
                    evm_env.version_mut().features.remove(evm2::EvmFeatures::EIP3607);

                    // EIP-7825's transaction gas cap is only active with Amsterdam's
                    // regular/state-gas accounting, and only in validation mode. Without
                    // validation the whole gas limit is execution gas.
                    if !validation || !evm_env.version().feature(evm2::EvmFeatures::EIP8037) {
                        evm_env.version_mut().tx_gas_limit_cap = u64::MAX;
                    }

                    if !validation {
                        // If not explicitly required, we disable nonce check <https://github.com/paradigmxyz/reth/issues/16108>
                        evm_env.version_mut().features.remove(evm2::EvmFeatures::NONCE_CHECK);
                        evm_env.version_mut().features.remove(evm2::EvmFeatures::BASE_FEE_CHECK);
                        evm_env.block_env_mut().basefee = U256::ZERO;
                    }

                    // Set prevrandao to zero for simulated blocks by default,
                    // matching spec behavior where MixDigest is zero-initialized.
                    // If user provides an override, it will be applied by apply_block_overrides.
                    evm_env.block_env_mut().prevrandao = U256::ZERO;
                    evm_env.block_env_mut().difficulty = U256::ZERO;
                    // BSC synthetic headers keep the requested difficulty instead of inheriting it.

                    if let Some(block_overrides) = block_overrides {
                        // ensure we don't allow uncapped gas limit per block
                        if block_overrides.gas_limit.is_some_and(|limit| {
                            limit > evm_env.block_env().gas_limit.to::<u64>() &&
                                limit > this.call_gas_limit()
                        }) {
                            return Err(EthApiError::other(EthSimulateError::GasLimitReached).into())
                        }
                        apply_block_overrides(block_overrides.clone(), &mut db, &mut evm_env);
                        evm_env
                            .apply_block_overrides_ext(&block_overrides)
                            .map_err(EthApiError::InvalidParams)?;
                    }
                    if let Some(ref state_overrides) = state_overrides {
                        evm_env
                            .apply_state_overrides_ext(state_overrides)
                            .map_err(EthApiError::InvalidParams)?;
                        let mut changes = simulation_state.as_ref().map(|_| BundleState::default());
                        apply_state_overrides_with_state(
                            state_overrides.clone(),
                            &mut db,
                            changes.as_mut(),
                        )
                        .map_err(Eth::Error::from_eth_err)?;
                        if let (Some(simulation_state), Some(changes)) =
                            (simulation_state.as_mut(), changes)
                        {
                            simulation_state
                                .extend(&state_provider, &changes)
                                .map_err(Eth::Error::from_eth_err)?;
                        }
                    }

                    let chain_id = evm_env.chain_id();
                    let build_bal = this.provider().chain_spec().is_amsterdam_active_at_timestamp(
                        evm_env.block_env().timestamp.to::<u64>(),
                    );

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

                    // EIP-7708 already emits transfer logs: https://eips.ethereum.org/EIPS/eip-7708
                    let trace_transfers =
                        trace_transfers && !evm_env.version().feature(evm2::EvmFeatures::EIP7708);
                    let mut evm = this.evm_config().evm_with_env(&mut db, evm_env.clone());
                    if trace_transfers {
                        // prepare inspector to capture transfer inside the evm so they are recorded
                        // and included in logs
                        evm.set_inspector(TransferInspector::new(false).with_logs(true));
                    }
                    let mut builder =
                        this.evm_config().create_block_builder(evm, evm_env, &parent, ctx);
                    if build_bal {
                        builder.executor_mut().enable_block_access_list_builder();
                    }

                    if let Some(ref state_overrides) = state_overrides {
                        simulate::apply_precompile_overrides(state_overrides, builder.evm_mut())
                            .map_err(|e| Eth::Error::from_eth_err(EthApiError::other(e)))?;
                    }

                    let (result, results) = simulate::execute_transactions(
                        builder,
                        &*state_provider,
                        calls,
                        &mut remaining_call_gas_limit,
                        chain_id,
                        simulation_state.as_mut(),
                        this.converter(),
                    )
                    .map_err(map_err)?;

                    db.commit_source(&BundleSource(&result.execution_state));
                    let simulated_header = result.block.clone_sealed_header();
                    db.insert_block_hash(
                        &U256::from(simulated_header.number()),
                        &simulated_header.hash(),
                    );
                    parent = simulated_header;

                    let block =
                        simulate::build_simulated_block::<Eth::Error, _, EvmTypesFor<Eth::Evm>>(
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
        state_override: Option<StateOverride>,
    ) -> Result<Vec<Vec<EthCallResponse>>, Eth::Error> {
        EthCall::call_many(&self.0, bundles, state_context, state_override).await
    }
}

#[cfg(test)]
mod tests;
