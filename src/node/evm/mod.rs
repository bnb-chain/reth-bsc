pub mod error;
pub mod util;

use crate::{
    evm::{
        api::{BscContext, BscEvm},
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
        result::{EVMError, HaltReason, ResultAndState},
        BlockEnv, ContextTr,
    },
    context_interface::JournalTr,
    Context, ExecuteEvm, InspectEvm, Inspector, SystemCallEvm,
};

mod assembler;
mod builder;
pub mod config;
pub use config::BscEvmConfig;
mod executor;
pub mod pre_execution;
mod post_execution;
mod factory;
mod patch;

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
    type Precompiles = PrecompilesMap;
    type Inspector = I;

    fn chain_id(&self) -> u64 {
        self.cfg.chain_id
    }

    fn block(&self) -> &BlockEnv {
        &self.block
    }

    fn transact_raw(
        &mut self,
        mut tx: Self::Tx,
    ) -> Result<ResultAndState<Self::HaltReason>, Self::Error> {
        // Detect system transactions in inspect mode (for trace APIs)
        // Normal execution: BlockExecutor filters system txs before calling transact
        // debug_traceTransaction/debug_traceCall: detect and handle system txs here

        if self.inspect {
            use crate::system_contracts::is_invoke_system_contract;
            use revm::primitives::TxKind;

            tx.is_system_transaction = matches!(tx.base.kind, TxKind::Call(to)
                if tx.base.caller == self.block.beneficiary
                    && is_invoke_system_contract(&to)
                    && tx.base.gas_price == 0);
            
            // Increase beneficiary balance for system transactions
            if tx.is_system_transaction {
                let beneficiary = self.block.beneficiary;
                if let Ok(account) = self.journal_mut().load_account(beneficiary) {
                    account.data.info.balance = tx.base.value;
                    account.data.mark_touch();
                }
            }
        }

        // Save original environment for system transactions
        let saved_env = if tx.is_system_transaction {
            Some((
                core::mem::replace(&mut self.block.gas_limit, tx.base.gas_limit),
                core::mem::replace(&mut self.block.basefee, 0),
                core::mem::replace(&mut self.cfg.disable_nonce_check, true),
            ))
        } else {
            None
        };

        // Execute transaction
        let res = if self.inspect {
            self.inspect_tx(tx)
        } else {
            ExecuteEvm::transact(self, tx)
        };

        // Restore environment for system transactions
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

    fn finish(self) -> (Self::DB, EvmEnv<Self::Spec>) {
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
    Node::Types: NodeTypes<Primitives = crate::node::primitives::BscPrimitives, ChainSpec = crate::chainspec::BscChainSpec, Payload = crate::node::engine_api::payload::BscPayloadTypes, Storage = crate::node::storage::BscStorage>,
{
    type EVM = BscEvmConfig;

    async fn build_evm(self, ctx: &BuilderContext<Node>) -> eyre::Result<Self::EVM> {
        let evm_config = BscEvmConfig::bsc(ctx.chain_spec());
        Ok(evm_config)
    }
}
