//! EVM Handler related to Bsc chain

use crate::evm::api::{BscContext, BscEvm};

use alloy_primitives::{address, Address, U256};
use reth_evm::Database;
use revm::{
    context::{
        result::{EVMError, ExecutionResult, FromStringError, HaltReason},
        Cfg, ContextError, ContextTr, LocalContextTr, Transaction,
    },
    context_interface::JournalTr,
    handler::{EthFrame, EvmTr, FrameResult, Handler, MainnetHandler},
    inspector::{Inspector, InspectorHandler},
    interpreter::{interpreter::EthInterpreter, Host, InitialAndFloorGas, SuccessOrHalt},
    primitives::hardfork::SpecId,
};

const SYSTEM_ADDRESS: Address = address!("fffffffffffffffffffffffffffffffffffffffe");

pub struct BscHandler<DB: revm::database::Database, INSP> {
    pub mainnet: MainnetHandler<BscEvm<DB, INSP>, EVMError<DB::Error>, EthFrame>,
}

impl<DB: revm::database::Database, INSP> BscHandler<DB, INSP> {
    pub fn new() -> Self {
        Self { mainnet: MainnetHandler::default() }
    }
}

impl<DB: revm::database::Database, INSP> Default for BscHandler<DB, INSP> {
    fn default() -> Self {
        Self::new()
    }
}

impl<DB: Database, INSP> Handler for BscHandler<DB, INSP> {
    type Evm = BscEvm<DB, INSP>;
    type Error = EVMError<DB::Error>;
    type HaltReason = HaltReason;

    fn validate_initial_tx_gas(
        &self,
        evm: &Self::Evm,
    ) -> Result<revm::interpreter::InitialAndFloorGas, Self::Error> {
        let ctx = evm.ctx_ref();
        let tx = ctx.tx();

        if tx.is_system_transaction {
            return Ok(InitialAndFloorGas { initial_gas: 0, floor_gas: 0 });
        }

        self.mainnet.validate_initial_tx_gas(evm)
    }

    fn reward_beneficiary(
        &self,
        evm: &mut Self::Evm,
        exec_result: &mut FrameResult,
    ) -> Result<(), Self::Error> {
        let ctx = evm.ctx();
        let tx = ctx.tx();

        if tx.is_system_transaction {
            return Ok(());
        }

        let effective_gas_price = ctx.effective_gas_price();
        let gas = exec_result.gas();
        let mut tx_fee = U256::from(gas.spent() - gas.refunded() as u64) * effective_gas_price;

        // EIP-4844
        let is_cancun = SpecId::from(ctx.cfg().spec()).is_enabled_in(SpecId::CANCUN);
        if is_cancun {
            let data_fee = U256::from(tx.total_blob_gas()) * ctx.blob_gasprice();
            tx_fee = tx_fee.saturating_add(data_fee);
        }

        let system_account = ctx.journal_mut().load_account(SYSTEM_ADDRESS)?;
        system_account.data.mark_touch();
        system_account.data.info.balance = system_account.data.info.balance.saturating_add(tx_fee);
        Ok(())
    }

    fn execution_result(
        &mut self,
        evm: &mut Self::Evm,
        result: FrameResult,
    ) -> Result<ExecutionResult<Self::HaltReason>, Self::Error> {
        match core::mem::replace(evm.ctx().error(), Ok(())) {
            Err(ContextError::Db(e)) => return Err(e.into()),
            Err(ContextError::Custom(e)) => return Err(Self::Error::from_string(e)),
            Ok(_) => (),
        }

        // used gas with refund calculated.
        let gas_refunded =
            if evm.ctx().tx().is_system_transaction { 0 } else { result.gas().refunded() as u64 };
        let final_gas_used = result.gas().spent() - gas_refunded;
        let output = result.output();
        let instruction_result = result.into_interpreter_result();

        // Reset journal and return present state.
        let logs = evm.ctx().journal_mut().take_logs();

        let result = match SuccessOrHalt::from(instruction_result.result) {
            SuccessOrHalt::Success(reason) => ExecutionResult::Success {
                reason,
                gas_used: final_gas_used,
                gas_refunded,
                logs,
                output,
            },
            SuccessOrHalt::Revert => {
                ExecutionResult::Revert { gas_used: final_gas_used, output: output.into_data() }
            }
            SuccessOrHalt::Halt(reason) => {
                ExecutionResult::Halt { reason, gas_used: final_gas_used }
            }
            // Only two internal return flags.
            flag @ (SuccessOrHalt::FatalExternalError | SuccessOrHalt::Internal(_)) => {
                panic!(
                "Encountered unexpected internal return flag: {flag:?} with instruction result: {instruction_result:?}"
            )
            }
        };

        evm.ctx().journal_mut().commit_tx();
        evm.ctx().local_mut().clear();
        evm.frame_stack().clear();

        Ok(result)
    }
}

impl<DB, INSP> InspectorHandler for BscHandler<DB, INSP>
where
    DB: Database,
    INSP: Inspector<BscContext<DB>>,
{
    type IT = EthInterpreter;
}
