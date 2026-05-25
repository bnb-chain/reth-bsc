use crate::evm::{
    api::{BscContext, BscEvm},
    handler::BscHandler,
    transaction::BscTxEnv,
};

use reth_evm::Database;
use revm::{
    context::{BlockEnv, ContextSetters},
    context_interface::{
        result::{EVMError, ExecutionResult, ResultAndState},
        ContextTr,
    },
    handler::Handler,
    inspector::{InspectCommitEvm, InspectEvm, Inspector, InspectorHandler},
    state::EvmState,
    DatabaseCommit, ExecuteCommitEvm, ExecuteEvm,
};

impl<DB, INSP> ExecuteEvm for BscEvm<DB, INSP>
where
    DB: Database,
{
    type ExecutionResult = ExecutionResult;
    type State = EvmState;
    type Error = EVMError<DB::Error>;
    type Tx = BscTxEnv;
    type Block = BlockEnv;

    fn set_block(&mut self, block: Self::Block) {
        self.inner.set_block(block);
    }

    fn transact_one(&mut self, tx: Self::Tx) -> Result<Self::ExecutionResult, Self::Error> {
        let mut tx = tx;
        self.prepare_tx_for_execution(&mut tx);
        self.maybe_bump_beneficiary_balance_for_trace(tx.is_system_transaction, tx.base.value);
        self.inner.ctx.set_tx(tx);
        BscHandler::new().run(self)
    }

    fn finalize(&mut self) -> Self::State {
        self.inner.finalize()
    }

    fn replay(&mut self) -> Result<ResultAndState, Self::Error> {
        self.prepare_current_tx_for_execution();
        let is_sys = self.inner.ctx.tx.is_system_transaction;
        let value = self.inner.ctx.tx.base.value;
        self.maybe_bump_beneficiary_balance_for_trace(is_sys, value);
        BscHandler::new().run(self).map(|result| {
            let state = self.finalize();
            ResultAndState::new(result, state)
        })
    }
}

impl<DB, INSP> ExecuteCommitEvm for BscEvm<DB, INSP>
where
    DB: Database + DatabaseCommit,
{
    fn commit(&mut self, state: Self::State) {
        self.inner.ctx.db_mut().commit(state);
    }
}

impl<DB, INSP> InspectEvm for BscEvm<DB, INSP>
where
    DB: Database,
    INSP: Inspector<BscContext<DB>>,
{
    type Inspector = INSP;

    fn set_inspector(&mut self, inspector: Self::Inspector) {
        self.inner.set_inspector(inspector);
    }

    fn inspect_one_tx(&mut self, tx: Self::Tx) -> Result<Self::ExecutionResult, Self::Error> {
        let mut tx = tx;
        self.prepare_tx_for_execution(&mut tx);
        self.maybe_bump_beneficiary_balance_for_trace(tx.is_system_transaction, tx.base.value);
        self.inner.ctx.set_tx(tx);
        BscHandler::new().inspect_run(self)
    }
}

impl<DB, INSP> InspectCommitEvm for BscEvm<DB, INSP>
where
    DB: Database + DatabaseCommit,
    INSP: Inspector<BscContext<DB>>,
{
}
