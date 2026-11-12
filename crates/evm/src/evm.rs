//! Native EVM adapter used by the BSC executor and RPCs.

use crate::{
    env::BscEvmEnv,
    system::bsc_tx_registry,
    types::{BscEvmTypes, BscRuntime},
};
use alloy_consensus::transaction::Recovered;
use alloy_primitives::{Address, Log, U256};
use evm2::{
    ethereum::TxEnvelope,
    evm::{
        precompile::PrecompileProvider, AccountInfo, Cache, DynDatabase, InterpreterRunner,
        PendingState,
    },
    interpreter::{InstrStop, Interpreter, Message, MessageResult},
    precompiles::{MovePrecompileError, PrecompileId},
    ExecutedTx, ExecutionConfig, Inspector, TxResult, TxResultWithState,
};
use reth_evm::{execute::map_handler_error, BlockExecutionError, Evm};

/// BSC EVM with consensus guards retained when RPC installs an inspector.
#[derive(Debug)]
pub struct BscEvm<'a> {
    /// Request-local metadata used by block finalization.
    pub env: BscEvmEnv,
    /// Native execution state. Install inspectors through this wrapper's [`Evm`] implementation.
    pub inner: evm2::Evm<'a, BscEvmTypes>,
}

impl<'a> BscEvm<'a> {
    /// Constructs the EVM with the chain's fork-specific precompile provider.
    pub fn new(
        env: BscEvmEnv,
        db: impl DynDatabase + 'a,
        precompiles: impl PrecompileProvider<BscEvmTypes> + 'a,
        is_system_contract: fn(&Address) -> bool,
    ) -> Self {
        let mut inner = evm2::Evm::new_with_execution_config_and_ext(
            ExecutionConfig::for_spec_and_version(env.spec, env.version),
            env.spec,
            env.block,
            bsc_tx_registry(env.spec.into()),
            db,
            precompiles,
            BscRuntime { is_system_contract, ..Default::default() },
        );
        inner.set_interpreter_runner(CoinbaseGuard);
        Self { inner, env }
    }

    /// Returns the opcode-visible block environment.
    pub fn block(&self) -> &evm2::env::BlockEnv<BscEvmTypes> {
        self.inner.block()
    }

    /// Executes an explicitly constructed Parlia system transaction with a detached result.
    pub fn transact_system_transaction(
        &mut self,
        tx: &Recovered<TxEnvelope>,
    ) -> Result<TxResultWithState<BscEvmTypes>, BlockExecutionError> {
        let previous = core::mem::replace(&mut self.inner.ext_mut().system_transaction, true);
        let result = Evm::transact(&mut self.inner, tx);
        self.inner.ext_mut().system_transaction = previous;
        result
    }
}

impl Evm for BscEvm<'_> {
    type EvmTypes = BscEvmTypes;
    type Transaction = TxEnvelope;

    fn transact(
        &mut self,
        tx: &Recovered<TxEnvelope>,
    ) -> Result<TxResultWithState<BscEvmTypes>, BlockExecutionError> {
        Evm::transact(&mut self.inner, tx)
    }
    fn transact_commit(
        &mut self,
        tx: &Recovered<TxEnvelope>,
    ) -> Result<TxResult<BscEvmTypes>, BlockExecutionError> {
        Evm::transact_commit(&mut self.inner, tx)
    }
    fn transact_result(
        &mut self,
        tx: &Recovered<TxEnvelope>,
    ) -> Result<TxResult<BscEvmTypes>, BlockExecutionError> {
        Evm::transact_result(&mut self.inner, tx)
    }
    fn transact_with_inspector<I: Inspector<BscEvmTypes> + 'static>(
        &mut self,
        tx: &Recovered<TxEnvelope>,
        inspector: I,
    ) -> Result<(I, TxResultWithState<BscEvmTypes>), BlockExecutionError> {
        let previous = core::mem::replace(&mut self.inner.ext_mut().trace_replay, true);
        let result = Evm::transact_with_inspector(&mut self.inner, tx, GuardedInspector(inspector));
        self.inner.ext_mut().trace_replay = previous;
        result.map(|(GuardedInspector(inspector), result)| (inspector, result))
    }
    fn set_inspector<I: Inspector<BscEvmTypes> + 'static>(&mut self, inspector: I) {
        self.inner.ext_mut().trace_replay = true;
        self.inner.set_inspector(GuardedInspector(inspector));
    }
    fn state_db_mut(&mut self) -> &mut dyn DynDatabase {
        Evm::state_db_mut(&mut self.inner)
    }
    fn commit_state(&mut self, state: &PendingState) {
        Evm::commit_state(&mut self.inner, state)
    }
    fn take_state_cache(&mut self) -> Cache {
        Evm::take_state_cache(&mut self.inner)
    }
    fn precompile_ids(&self) -> Vec<(Address, PrecompileId)> {
        Evm::precompile_ids(&self.inner)
    }
    fn has_precompile(&self, address: &Address) -> bool {
        Evm::has_precompile(&self.inner, address)
    }
    fn account_info(
        &mut self,
        address: &Address,
    ) -> Result<Option<AccountInfo>, BlockExecutionError> {
        Evm::account_info(&mut self.inner, address)
    }
    fn move_precompiles(
        &mut self,
        moves: impl IntoIterator<Item = (Address, Address)>,
    ) -> Result<(), MovePrecompileError> {
        Evm::move_precompiles(&mut self.inner, moves)
    }
    fn transact_and_discard<R>(
        &mut self,
        tx: &Recovered<TxEnvelope>,
        observe: impl FnOnce(&ExecutedTx<'_, '_, BscEvmTypes>) -> R,
    ) -> Result<R, BlockExecutionError> {
        let executed = self.inner.transact(tx).map_err(map_handler_error)?;
        Ok(observe(&executed))
    }
}

/// Only bytecode execution at the beneficiary is forbidden; value transfers and precompiles
/// retain their normal behavior. The runner avoids per-opcode inspection on the import path.
#[derive(Debug)]
struct CoinbaseGuard;

impl InterpreterRunner<BscEvmTypes> for CoinbaseGuard {
    fn run<'frame, 'host>(
        &self,
        interpreter: &mut Interpreter<'frame, 'host, BscEvmTypes>,
        host: &mut evm2::Evm<'host, BscEvmTypes>,
    ) -> Option<InstrStop> {
        (interpreter.message().destination == host.block().beneficiary)
            .then_some(InstrStop::PrecompileError)
    }
}

struct GuardedInspector<I>(I);

impl<I: Inspector<BscEvmTypes>> Inspector<BscEvmTypes> for GuardedInspector<I> {
    fn initialize_interp(&mut self, interp: &mut Interpreter<'_, '_, BscEvmTypes>) {
        self.0.initialize_interp(interp);
        if interp.message().destination == interp.host().block().beneficiary {
            interp.set_stop(InstrStop::PrecompileError);
        }
    }
    fn step(&mut self, interp: &mut Interpreter<'_, '_, BscEvmTypes>) {
        self.0.step(interp);
    }
    fn step_end(&mut self, interp: &mut Interpreter<'_, '_, BscEvmTypes>) {
        self.0.step_end(interp);
    }
    fn log(&mut self, log: &Log, host: &mut evm2::Evm<'_, BscEvmTypes>) {
        self.0.log(log, host);
    }
    fn call(
        &mut self,
        interp: &mut Interpreter<'_, '_, BscEvmTypes>,
        message: &mut Message<BscEvmTypes>,
    ) -> Option<MessageResult<BscEvmTypes>> {
        self.0.call(interp, message)
    }
    fn call_end(
        &mut self,
        interp: &mut Interpreter<'_, '_, BscEvmTypes>,
        message: &Message<BscEvmTypes>,
        result: &mut MessageResult<BscEvmTypes>,
    ) {
        self.0.call_end(interp, message, result);
    }
    fn create(
        &mut self,
        interp: &mut Interpreter<'_, '_, BscEvmTypes>,
        message: &mut Message<BscEvmTypes>,
    ) -> Option<MessageResult<BscEvmTypes>> {
        self.0.create(interp, message)
    }
    fn create_end(
        &mut self,
        interp: &mut Interpreter<'_, '_, BscEvmTypes>,
        message: &Message<BscEvmTypes>,
        result: &mut MessageResult<BscEvmTypes>,
    ) {
        self.0.create_end(interp, message, result);
    }
    fn selfdestruct(
        &mut self,
        contract: &Address,
        target: &Address,
        value: &U256,
        host: &mut evm2::Evm<'_, BscEvmTypes>,
    ) {
        self.0.selfdestruct(contract, target, value, host);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{hardfork::BscHardfork, SYSTEM_ADDRESS};
    use alloy_consensus::TxLegacy;
    use alloy_primitives::{address, Bytes, TxKind};
    use evm2::{bytecode::Bytecode, env::BlockEnv, evm::InMemoryDB, Precompiles};

    const VALIDATOR: Address = address!("1111111111111111111111111111111111111111");
    const CONTRACT: Address = address!("0000000000000000000000000000000000001000");
    const USER: Address = address!("2222222222222222222222222222222222222222");

    fn evm(db: InMemoryDB) -> BscEvm<'static> {
        BscEvm::new(
            BscEvmEnv::new(
                BscHardfork::Osaka,
                BlockEnv::<BscEvmTypes> {
                    beneficiary: VALIDATOR,
                    gas_limit: U256::from(30_000_000),
                    ..Default::default()
                },
                56,
                0,
            ),
            db,
            Precompiles::base(evm2::SpecId::OSAKA),
            |to| *to == CONTRACT,
        )
    }

    fn db() -> InMemoryDB {
        let mut db = InMemoryDB::default();
        for who in [VALIDATOR, USER] {
            db.insert_account_info(
                &who,
                AccountInfo::default().with_balance(U256::from(1_000_000)),
            );
        }
        db
    }

    fn system_tx() -> Recovered<TxEnvelope> {
        Recovered::new_unchecked(
            TxEnvelope::Legacy(TxLegacy {
                gas_limit: i64::MAX as u64,
                chain_id: Some(999),
                nonce: 999,
                to: TxKind::Call(CONTRACT),
                value: U256::from(7),
                ..Default::default()
            }),
            VALIDATOR,
        )
    }

    #[derive(Default)]
    struct Noop;
    impl Inspector<BscEvmTypes> for Noop {}

    #[test]
    fn parlia_transfer_bypasses_tx_checks_but_increments_actual_nonce() {
        let mut evm = evm(db());
        let result = evm.transact_commit(&system_tx()).unwrap();
        assert!(result.status);
        assert_eq!(result.tx_gas_used(), 0);
        assert_eq!(result.refunded, 0);
        let validator = evm.account_info(&VALIDATOR).unwrap().unwrap();
        assert_eq!(validator.nonce, 1);
        assert_eq!(validator.balance, U256::from(999_993));
        assert_eq!(evm.account_info(&CONTRACT).unwrap().unwrap().balance, U256::from(7));
        assert!(evm.account_info(&SYSTEM_ADDRESS).unwrap().is_none());
    }

    #[test]
    fn system_revert_keeps_nonce_without_value_or_fees() {
        let mut db = db();
        db.insert_account_info(
            &CONTRACT,
            AccountInfo::default()
                .with_code(Bytecode::new_legacy(Bytes::from_static(&[0x60, 0, 0x60, 0, 0xfd]))),
        );
        let mut evm = evm(db);
        let result = evm.transact_commit(&system_tx()).unwrap();
        assert!(!result.status);
        assert_eq!(result.refunded, 0);
        let validator = evm.account_info(&VALIDATOR).unwrap().unwrap();
        assert_eq!(validator.nonce, 1);
        assert_eq!(validator.balance, U256::from(1_000_000));
        assert_eq!(evm.account_info(&CONTRACT).unwrap().unwrap().balance, U256::ZERO);
        assert!(evm.account_info(&SYSTEM_ADDRESS).unwrap().is_none());
    }

    #[test]
    fn system_storage_clear_receives_no_refund() {
        let mut db = db();
        db.insert_account_info(
            &CONTRACT,
            AccountInfo::default()
                .with_code(Bytecode::new_legacy(Bytes::from_static(&[0x60, 0, 0x60, 0, 0x55, 0]))),
        );
        db.insert_account_storage(&CONTRACT, &U256::ZERO, &U256::from(1));
        let mut evm = evm(db);
        let result = evm.transact_commit(&system_tx()).unwrap();
        assert!(result.status);
        assert_eq!(result.refunded, 0);
        assert_eq!(result.tx_gas_used(), 5006);
    }

    #[test]
    fn explicit_system_mode_does_not_leak_to_next_transaction() {
        let mut evm = evm(db());
        let tx = Recovered::new_unchecked(system_tx().into_inner(), USER);
        let result = evm.transact_system_transaction(&tx).unwrap();
        evm.commit_state(&result.pending_state);
        assert!(!evm.inner.ext().system_transaction);
        assert!(evm.transact(&tx).is_err(), "ordinary oversized transaction must fail");
    }

    #[test]
    fn trace_replay_credits_value_without_destroying_previous_balance() {
        let mut evm = evm(db());
        let (_, result) = evm.transact_with_inspector(&system_tx(), Noop).unwrap();
        assert!(result.result.status);
        evm.commit_state(&result.pending_state);
        let validator = evm.account_info(&VALIDATOR).unwrap().unwrap();
        assert_eq!(validator.balance, U256::from(1_000_000));
        assert_eq!(validator.nonce, 1);
        assert!(!evm.inner.ext().trace_replay);
    }

    #[test]
    fn coinbase_bytecode_guard_applies_with_and_without_inspection() {
        for inspected in [false, true] {
            let mut db = db();
            db.insert_account_info(
                &VALIDATOR,
                AccountInfo::default().with_code(Bytecode::new_legacy(Bytes::from_static(&[
                    0x60, 1, 0x60, 0, 0x55, 0,
                ]))),
            );
            let mut evm = evm(db);
            let tx = Recovered::new_unchecked(
                TxEnvelope::Legacy(TxLegacy {
                    chain_id: Some(56),
                    gas_limit: 100_000,
                    gas_price: 1,
                    to: TxKind::Call(VALIDATOR),
                    ..Default::default()
                }),
                USER,
            );
            let result = if inspected {
                evm.transact_with_inspector(&tx, Noop).unwrap().1
            } else {
                evm.transact(&tx).unwrap()
            };
            assert_eq!(result.result.stop, InstrStop::PrecompileError, "inspected={inspected}");
            assert_eq!(result.result.tx_gas_used(), 100_000);
            evm.commit_state(&result.pending_state);
            assert_eq!(
                evm.inner.overlay_db_mut().get_storage(&VALIDATOR, &U256::ZERO).unwrap(),
                U256::ZERO
            );
        }
    }
}
