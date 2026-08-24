pub mod error;
pub mod util;

#[cfg(test)]
mod pre_execution_tests;
use crate::{
    evm::{
        api::{BscContext, BscEvm},
        block_env::BscBlockEnv,
        transaction::BscTxEnv,
    },
    hardforks::bsc::BscHardfork,
};
use alloy_primitives::{Address, Bytes};

use reth::{
    api::{FullNodeTypes, NodeTypes},
    builder::{components::ExecutorBuilder, BuilderContext},
};
use reth_evm::{precompiles::PrecompilesMap, Database, Evm, EvmEnv};
use revm::{
    context::{
        result::{EVMError, HaltReason, ResultAndState}, CfgEnv,
    },
    Context, ExecuteEvm, InspectEvm, Inspector, SystemCallEvm,
};

mod assembler;
mod builder;
pub mod config;
pub use config::BscEvmConfig;
mod executor;
pub use executor::BscBlockExecutor;
mod factory;
pub use factory::BscEvmFactory;
mod patch;
mod post_execution;
pub mod pre_execution;

impl<DB, I> Evm for BscEvm<DB, I>
where
    DB: Database,
    I: Inspector<BscContext<DB>>,
{
    type DB = DB;
    type Tx = BscTxEnv;
    type Error = EVMError<DB::Error>;
    type HaltReason = HaltReason;
    type Spec = BscHardfork;
    type BlockEnv = BscBlockEnv;
    type Precompiles = PrecompilesMap;
    type Inspector = I;

    fn cfg_env(&self) -> &CfgEnv<Self::Spec> {
        &self.inner.ctx.cfg
    }

    fn chain_id(&self) -> u64 {
        self.cfg.chain_id
    }

    fn block(&self) -> &BscBlockEnv {
        &self.inner.ctx.block
    }

    fn transact_raw(
        &mut self,
        mut tx: Self::Tx,
    ) -> Result<ResultAndState<Self::HaltReason>, Self::Error> {
        // BlockExecutor filters mined system txs out before reaching here; trace
        // RPCs do not — let `prepare` mark them idempotently for `BscHandler`.
        self.prepare_tx_for_execution(&mut tx);

        let saved_env = if tx.is_system_transaction {
            self.fund_beneficiary_for_system_tx_replay(tx.base.value);
            Some((
                core::mem::replace(&mut self.block.gas_limit, tx.base.gas_limit),
                core::mem::replace(&mut self.block.basefee, 0),
                core::mem::replace(&mut self.cfg.disable_nonce_check, true),
            ))
        } else {
            None
        };

        let res = if self.inspect { self.inspect_tx(tx) } else { ExecuteEvm::transact(self, tx) };

        if let Some((gas_limit, basefee, disable_nonce_check)) = saved_env {
            self.block.gas_limit = gas_limit;
            self.block.basefee = basefee;
            self.cfg.disable_nonce_check = disable_nonce_check;
        }

        res
    }

    fn transact_system_call(
        &mut self,
        caller: Address,
        contract: Address,
        data: Bytes,
    ) -> Result<ResultAndState<Self::HaltReason>, Self::Error> {
        let result = self.inner.system_call_one_with_caller(caller, contract, data)?;
        let state = self.finalize();
        Ok(ResultAndState::new(result, state))
    }

    fn finish(self) -> (Self::DB, EvmEnv<Self::Spec, Self::BlockEnv>) {
        let Context { block: block_env, cfg: cfg_env, journaled_state, .. } = self.inner.ctx;

        (journaled_state.database, EvmEnv { block_env, cfg_env })
    }

    fn set_inspector_enabled(&mut self, enabled: bool) {
        self.inspect = enabled;
    }

    fn components(&self) -> (&Self::DB, &Self::Inspector, &Self::Precompiles) {
        (&self.journaled_state.database, &self.inner.inspector, &self.inner.precompiles)
    }

    fn components_mut(&mut self) -> (&mut Self::DB, &mut Self::Inspector, &mut Self::Precompiles) {
        (
            &mut self.inner.ctx.journaled_state.database,
            &mut self.inner.inspector,
            &mut self.inner.precompiles,
        )
    }
}

/// A regular bsc evm and executor builder.
#[derive(Debug, Default, Clone, Copy)]
#[non_exhaustive]
pub struct BscExecutorBuilder;

impl<Node> ExecutorBuilder<Node> for BscExecutorBuilder
where
    Node: FullNodeTypes,
    Node::Types: NodeTypes<
        Primitives = crate::node::primitives::BscPrimitives,
        ChainSpec = crate::chainspec::BscChainSpec,
        Payload = crate::node::engine_api::payload::BscPayloadTypes,
        Storage = crate::node::storage::BscStorage,
    >,
{
    type EVM = BscEvmConfig;

    async fn build_evm(self, ctx: &BuilderContext<Node>) -> eyre::Result<Self::EVM> {
        let evm_config = BscEvmConfig::bsc(ctx.chain_spec());
        Ok(evm_config)
    }
}
