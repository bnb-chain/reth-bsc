//! EVM Handler related to Bsc chain

use crate::evm::{
    api::{BscContext, BscEvm},
    blacklist,
};

use alloy_primitives::{U256};
use reth_evm::Database;
use revm::{bytecode::Bytecode, primitives::eip7702};

use alloy_consensus::constants::KECCAK_EMPTY;
use revm::{
    context::{
        result::{EVMError, ExecutionResult, FromStringError, HaltReason},
        transaction::TransactionType,
        Cfg, ContextError, ContextTr, LocalContextTr, Transaction,
    },
    context_interface::{transaction::eip7702::AuthorizationTr, JournalTr},
    handler::{EthFrame, EvmTr, FrameResult, Handler, MainnetHandler},
    inspector::{Inspector, InspectorHandler},
    interpreter::{interpreter::EthInterpreter, Host, InitialAndFloorGas, SuccessOrHalt},
    primitives::hardfork::SpecId,
};

use crate::consensus::SYSTEM_ADDRESS;
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

    // This function is based on the implementation of the EIP-7702.
    // https://github.com/bluealloy/revm/blob/df467931c4b1b8b620ff2cb9f62501c7abc3ea03/crates/handler/src/pre_execution.rs#L186
    // with slight modifications to support BSC specific validation.
    // https://github.com/bnb-chain/bsc/blob/develop/core/state_transition.go#L593
    fn apply_eip7702_auth_list(&self, evm: &mut Self::Evm) -> Result<u64, Self::Error> {
        let ctx = evm.ctx_ref();
        let tx = ctx.tx();

        if tx.tx_type() != TransactionType::Eip7702 {
            return Ok(0);
        }

        let chain_id = evm.ctx().cfg().chain_id();
        let (tx, journal) = evm.ctx().tx_journal_mut();

        let mut refunded_accounts = 0;
        for authorization in tx.authorization_list() {
            // 1. Verify the chain id is either 0 or the chain's current ID.
            let auth_chain_id = authorization.chain_id();
            if !auth_chain_id.is_zero() && auth_chain_id != U256::from(chain_id) {
                continue;
            }

            // 2. Verify the `nonce` is less than `2**64 - 1`.
            if authorization.nonce() == u64::MAX {
                continue;
            }

            // recover authority and authorized addresses.
            // 3. `authority = ecrecover(keccak(MAGIC || rlp([chain_id, address, nonce])), y_parity,
            //    r, s]`
            let Some(authority) = authorization.authority() else {
                continue;
            };

            // check if authority is blacklisted.
            if blacklist::is_blacklisted(&authority) {
                continue;
            }

            // warm authority account and check nonce.
            // 4. Add `authority` to `accessed_addresses` (as defined in [EIP-2929](./eip-2929.md).)
            let mut authority_acc = journal.load_account_code(authority)?;

            // 5. Verify the code of `authority` is either empty or already delegated.
            if let Some(bytecode) = &authority_acc.info.code {
                // if it is not empty and it is not eip7702
                if !bytecode.is_empty() && !bytecode.is_eip7702() {
                    continue;
                }
            }

            // 6. Verify the nonce of `authority` is equal to `nonce`. In case `authority` does not
            //    exist in the trie, verify that `nonce` is equal to `0`.
            if authorization.nonce() != authority_acc.info.nonce {
                continue;
            }

            // 7. Add `PER_EMPTY_ACCOUNT_COST - PER_AUTH_BASE_COST` gas to the global refund counter
            //    if `authority` exists in the trie.
            if !(authority_acc.is_empty() && authority_acc.is_loaded_as_not_existing_not_touched())
            {
                refunded_accounts += 1;
            }

            // 8. Set the code of `authority` to be `0xef0100 || address`. This is a delegation
            //    designation.
            //  * As a special case, if `address` is `0x0000000000000000000000000000000000000000` do
            //    not write the designation. Clear the accounts code and reset the account's code
            //    hash to the empty hash
            //    `0xc5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470`.
            let address = authorization.address();
            let (bytecode, hash) = if address.is_zero() {
                (Bytecode::default(), KECCAK_EMPTY)
            } else {
                let bytecode = Bytecode::new_eip7702(address);
                let hash = bytecode.hash_slow();
                (bytecode, hash)
            };
            authority_acc.info.code_hash = hash;
            authority_acc.info.code = Some(bytecode);

            // 9. Increase the nonce of `authority` by one.
            authority_acc.info.nonce = authority_acc.info.nonce.saturating_add(1);
            authority_acc.mark_touch();
        }

        let refunded_gas =
            refunded_accounts * (eip7702::PER_EMPTY_ACCOUNT_COST - eip7702::PER_AUTH_BASE_COST);

        Ok(refunded_gas)
    }

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
