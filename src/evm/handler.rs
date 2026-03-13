//! EVM Handler related to Bsc chain

use crate::evm::{
    api::{BscContext, BscEvm},
    blacklist,
    precompiles,
};

use alloy_primitives::{hex, TxKind, U256};
use reth_evm::Database;
use revm::{bytecode::Bytecode, primitives::eip7702};

use alloy_consensus::constants::KECCAK_EMPTY;
use revm::{
    context::{
        result::{EVMError, ExecutionResult, FromStringError, HaltReason},
        transaction::TransactionType,
        Cfg, ContextError, ContextTr, LocalContextTr, Transaction,
    },
    context_interface::{transaction::eip7702::AuthorizationTr, Block, JournalTr},
    handler::{EthFrame, EvmTr, FrameResult, Handler, MainnetHandler},
    inspector::{Inspector, InspectorHandler},
    interpreter::{interpreter::EthInterpreter, Host, InitialAndFloorGas, SuccessOrHalt},
    primitives::hardfork::SpecId,
};
use revm_context_interface::journaled_state::account::JournaledAccountTr;

use crate::consensus::SYSTEM_ADDRESS;
pub struct BscHandler<DB: revm::Database, INSP> {
    pub mainnet: MainnetHandler<BscEvm<DB, INSP>, EVMError<DB::Error>, EthFrame>,
}

impl<DB: revm::Database, INSP> BscHandler<DB, INSP> {
    pub fn new() -> Self {
        Self { mainnet: MainnetHandler::default() }
    }
}

impl<DB: revm::Database, INSP> Default for BscHandler<DB, INSP> {
    fn default() -> Self {
        Self::new()
    }
}

impl<DB: Database, INSP> Handler for BscHandler<DB, INSP> {
    type Evm = BscEvm<DB, INSP>;
    type Error = EVMError<DB::Error>;
    type HaltReason = HaltReason;

    /// Override validate_env to skip gas limit cap validation for system transactions.
    /// System transactions in BSC use i64::MAX as gas limit, which would fail the
    /// EIP-7825 gas limit cap check. This is consistent with go-bsc behavior where
    /// system transactions set SkipTransactionChecks=true.
    fn validate_env(&self, evm: &mut Self::Evm) -> Result<(), Self::Error> {
        let is_system_tx = evm.ctx_ref().tx().is_system_transaction;

        if is_system_tx {
            // For system transactions, only validate block-level requirements
            // Skip transaction-level validation including gas limit cap
            let ctx = evm.ctx_ref();
            let spec: SpecId = (*ctx.cfg().spec()).into();

            // `prevrandao` is required for the merge
            if spec.is_enabled_in(SpecId::MERGE) && ctx.block().prevrandao().is_none() {
                return Err(revm::context_interface::result::InvalidHeader::PrevrandaoNotSet.into());
            }
            // `excess_blob_gas` is required for Cancun
            if spec.is_enabled_in(SpecId::CANCUN) && ctx.block().blob_excess_gas_and_price().is_none() {
                return Err(revm::context_interface::result::InvalidHeader::ExcessBlobGasNotSet.into());
            }
            Ok(())
        } else {
            // For non-system transactions, use default validation
            self.mainnet.validate_env(evm)
        }
    }

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
            let mut authority_acc = journal.load_account_with_code_mut(authority)?;

            // 5. Verify the code of `authority` is either empty or already delegated.
            if let Some(bytecode) = authority_acc.code() {
                // if it is not empty and it is not eip7702
                if !bytecode.is_empty() && !bytecode.is_eip7702() {
                    continue;
                }
            }

            // 6. Verify the nonce of `authority` is equal to `nonce`. In case `authority` does not
            //    exist in the trie, verify that `nonce` is equal to `0`.
            if authorization.nonce() != authority_acc.nonce() {
                continue;
            }

            // 7. Add `PER_EMPTY_ACCOUNT_COST - PER_AUTH_BASE_COST` gas to the global refund counter
            //    if `authority` exists in the trie.
            let account = authority_acc.account();
            if !(account.is_empty() && account.is_loaded_as_not_existing_not_touched()) {
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
            authority_acc.set_code(hash, bytecode);
            // 9. Increase the nonce of `authority` by one.
            authority_acc.bump_nonce();
        }

        let refunded_gas =
            refunded_accounts * (eip7702::PER_EMPTY_ACCOUNT_COST - eip7702::PER_AUTH_BASE_COST);

        Ok(refunded_gas)
    }

    fn validate_initial_tx_gas(
        &self,
        evm: &mut Self::Evm,
    ) -> Result<revm::interpreter::InitialAndFloorGas, Self::Error> {
        // Extract trace context parts without holding a borrow across the validation call.
        let (block_number, spec, is_system_tx, to, selector, input_len) = {
            let ctx = evm.ctx_ref();
            let tx = ctx.tx();
            let to = match tx.base.kind {
                TxKind::Call(addr) => Some(addr),
                _ => None,
            };
            let input = tx.base.data.as_ref();
            let selector = if input.len() >= 4 {
                Some(hex::encode(&input[..4]))
            } else {
                None
            };
            (
                ctx.block().number.to::<u64>(),
                *ctx.cfg().spec(),
                tx.is_system_transaction,
                to,
                selector,
                input.len(),
            )
        };

        precompiles::push_precompile_trace_context(precompiles::PrecompileTraceContext::from_parts(
            block_number,
            spec,
            is_system_tx,
            None,
            to,
            selector,
            input_len,
        ));

        let res = if is_system_tx {
            Ok(InitialAndFloorGas { initial_gas: 0, floor_gas: 0 })
        } else {
            self.mainnet.validate_initial_tx_gas(evm)
        };

        // If validation fails, `execution_result` may not run, so we must pop here.
        if res.is_err() {
            precompiles::pop_precompile_trace_context();
        }

        res
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
        let is_cancun = SpecId::from(*ctx.cfg().spec()).is_enabled_in(SpecId::CANCUN);
        if is_cancun {
            let data_fee = U256::from(tx.total_blob_gas()) * ctx.blob_gasprice();
            tx_fee = tx_fee.saturating_add(data_fee);
        }

        let mut system_account = ctx.journal_mut().load_account_mut(SYSTEM_ADDRESS)?;
        let _ = system_account.incr_balance(tx_fee);
        Ok(())
    }

    fn execution_result(
        &mut self,
        evm: &mut Self::Evm,
        result: FrameResult,
    ) -> Result<ExecutionResult<Self::HaltReason>, Self::Error> {
        // Ensure we always pop the trace context, even on early returns.
        struct PrecompileTracePopGuard;
        impl Drop for PrecompileTracePopGuard {
            fn drop(&mut self) {
                precompiles::pop_precompile_trace_context();
            }
        }
        let _precompile_trace_pop_guard = PrecompileTracePopGuard;

        match core::mem::replace(evm.ctx().error(), Ok(())) {
            Err(ContextError::Db(e)) => return Err(e.into()),
            Err(ContextError::Custom(e)) => return Err(Self::Error::from_string(e)),
            Ok(_) => (),
        }

        // used gas with refund calculated.
        let raw_gas_refunded = result.gas().refunded() as u64;
        let raw_gas_spent = result.gas().spent();
        let is_system_tx = {
            let ctx = evm.ctx_ref();
            ctx.tx().is_system_transaction
        };
        let gas_refunded = if is_system_tx { 0 } else { raw_gas_refunded };
        let final_gas_used = raw_gas_spent - gas_refunded;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{evm::transaction::BscTxEnv, hardforks::bsc::BscHardfork};
    use reth_evm::EvmEnv;
    use revm::{
        context::{BlockEnv, CfgEnv, TxEnv},
        context_interface::result::InvalidTransaction,
        handler::Handler,
        inspector::NoOpInspector,
        primitives::{Address, TxKind, U256},
        state::AccountInfo,
    };
    use revm_database::InMemoryDB;

    use crate::evm::api::BscEvm;

    /// The EIP-7825 gas limit cap (2^24 = 16777216)
    const MAX_TX_GAS_LIMIT_OSAKA: u64 = 2u64.pow(24);

    fn make_db_with_funded_caller(caller: Address) -> InMemoryDB {
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            caller,
            AccountInfo { balance: U256::from(1_000_000_000_000u64), ..AccountInfo::default() },
        );
        db
    }

    #[test]
    fn test_system_tx_bypasses_gas_limit_cap_validation() {
        // Use Osaka spec which has the gas limit cap enabled
        let spec = BscHardfork::Osaka;
        let mut cfg_env = CfgEnv::new_with_spec(spec).with_chain_id(56);
        // Set the gas limit cap
        cfg_env.tx_gas_limit_cap = Some(MAX_TX_GAS_LIMIT_OSAKA);

        let block_env = BlockEnv {
            prevrandao: Some(U256::from(1).into()), // Required for post-merge
            ..Default::default()
        };
        let env = EvmEnv::new(cfg_env, block_env);

        let caller = Address::from([0x11; 20]);
        let contract = Address::from([0x22; 20]);

        // Create a system transaction with gas limit exceeding the cap (i64::MAX)
        let system_tx_gas_limit = i64::MAX as u64;
        assert!(system_tx_gas_limit > MAX_TX_GAS_LIMIT_OSAKA);

        let mut tx = BscTxEnv::new(
            TxEnv::builder()
                .caller(caller)
                .chain_id(Some(56))
                .gas_limit(system_tx_gas_limit)
                .gas_price(0) // System txs have 0 gas price
                .kind(TxKind::Call(contract))
                .build()
                .expect("tx env should build"),
        );
        // Mark as system transaction
        tx.is_system_transaction = true;

        let mut evm = BscEvm::new(
            env,
            make_db_with_funded_caller(caller),
            NoOpInspector,
            false,
            false,
        );

        // Set the transaction directly on the inner context
        evm.inner.ctx.tx = tx;

        // Create handler and validate env
        let handler = BscHandler::<InMemoryDB, NoOpInspector>::new();
        let result = handler.validate_env(&mut evm);

        // System transaction should pass validation despite exceeding gas limit cap
        assert!(
            result.is_ok(),
            "System transaction should bypass gas limit cap validation, got error: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_non_system_tx_fails_gas_limit_cap_validation() {
        // Use Osaka spec which has the gas limit cap enabled
        let spec = BscHardfork::Osaka;
        let mut cfg_env = CfgEnv::new_with_spec(spec).with_chain_id(56);
        // Set the gas limit cap
        cfg_env.tx_gas_limit_cap = Some(MAX_TX_GAS_LIMIT_OSAKA);

        let block_env = BlockEnv {
            prevrandao: Some(U256::from(1).into()), // Required for post-merge
            ..Default::default()
        };
        let env = EvmEnv::new(cfg_env, block_env);

        let caller = Address::from([0x11; 20]);
        let contract = Address::from([0x22; 20]);

        // Create a non-system transaction with gas limit exceeding the cap
        let excessive_gas_limit = MAX_TX_GAS_LIMIT_OSAKA + 1;

        let tx = BscTxEnv::new(
            TxEnv::builder()
                .caller(caller)
                .chain_id(Some(56))
                .gas_limit(excessive_gas_limit)
                .gas_price(1) // Non-zero gas price
                .kind(TxKind::Call(contract))
                .build()
                .expect("tx env should build"),
        );
        // is_system_transaction defaults to false

        let mut evm = BscEvm::new(
            env,
            make_db_with_funded_caller(caller),
            NoOpInspector,
            false,
            false,
        );

        // Set the transaction directly on the inner context
        evm.inner.ctx.tx = tx;

        // Create handler and validate env
        let handler = BscHandler::<InMemoryDB, NoOpInspector>::new();
        let result = handler.validate_env(&mut evm);

        // Non-system transaction should fail validation
        assert!(
            result.is_err(),
            "Non-system transaction exceeding gas limit cap should fail validation"
        );

        // Verify it's the correct error type
        if let Err(e) = result {
            match e {
                revm::context::result::EVMError::Transaction(InvalidTransaction::TxGasLimitGreaterThanCap { gas_limit, cap }) => {
                    assert_eq!(gas_limit, excessive_gas_limit);
                    assert_eq!(cap, MAX_TX_GAS_LIMIT_OSAKA);
                }
                _ => panic!("Expected TxGasLimitGreaterThanCap error, got: {:?}", e),
            }
        }
    }

    #[test]
    fn test_non_system_tx_within_cap_passes_validation() {
        // Use Osaka spec which has the gas limit cap enabled
        let spec = BscHardfork::Osaka;
        let mut cfg_env = CfgEnv::new_with_spec(spec).with_chain_id(56);
        // Set the gas limit cap
        cfg_env.tx_gas_limit_cap = Some(MAX_TX_GAS_LIMIT_OSAKA);

        let block_env = BlockEnv {
            prevrandao: Some(U256::from(1).into()), // Required for post-merge
            ..Default::default()
        };
        let env = EvmEnv::new(cfg_env, block_env);

        let caller = Address::from([0x11; 20]);
        let contract = Address::from([0x22; 20]);

        // Create a non-system transaction with gas limit within the cap
        let valid_gas_limit = MAX_TX_GAS_LIMIT_OSAKA - 1;

        let tx = BscTxEnv::new(
            TxEnv::builder()
                .caller(caller)
                .chain_id(Some(56))
                .gas_limit(valid_gas_limit)
                .gas_price(1)
                .kind(TxKind::Call(contract))
                .build()
                .expect("tx env should build"),
        );

        let mut evm = BscEvm::new(
            env,
            make_db_with_funded_caller(caller),
            NoOpInspector,
            false,
            false,
        );

        // Set the transaction directly on the inner context
        evm.inner.ctx.tx = tx;

        // Create handler and validate env
        let handler = BscHandler::<InMemoryDB, NoOpInspector>::new();
        let result = handler.validate_env(&mut evm);

        // Non-system transaction within cap should pass validation
        assert!(
            result.is_ok(),
            "Non-system transaction within gas limit cap should pass validation, got error: {:?}",
            result.err()
        );
    }
}
