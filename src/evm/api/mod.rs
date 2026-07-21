use std::ops::{Deref, DerefMut};

use crate::{evm::transaction::BscTxEnv, hardforks::bsc::BscHardfork};

use super::precompiles::BscPrecompiles;
use reth_evm::{precompiles::PrecompilesMap, Database, EvmEnv};
use revm::{
    context::{BlockEnv, CfgEnv, ContextTr, Evm as EvmCtx, FrameStack, JournalTr},
    context_interface::journaled_state::account::JournaledAccountTr,
    handler::{
        evm::{ContextDbError, FrameInitResult},
        instructions::EthInstructions,
        EthFrame, EvmTr, FrameInitOrResult, FrameResult,
    },
    inspector::InspectorEvmTr,
    interpreter::{interpreter::EthInterpreter, interpreter_action::FrameInit},
    primitives::hardfork::SpecId,
    Context, Inspector, Journal,
};

mod exec;

/// Type alias for the default context type of the BscEvm.
pub type BscContext<DB> = Context<BlockEnv, BscTxEnv, CfgEnv<BscHardfork>, DB>;

/// BSC EVM implementation.
///
/// This is a wrapper type around the `revm` evm with optional [`Inspector`] (tracing)
/// support. [`Inspector`] support is configurable at runtime because it's part of the underlying
#[allow(missing_debug_implementations)]
pub struct BscEvm<DB: revm::Database, I> {
    pub inner: EvmCtx<
        BscContext<DB>,
        I,
        EthInstructions<EthInterpreter, BscContext<DB>>,
        PrecompilesMap,
        EthFrame,
    >,
    pub inspect: bool,
    pub trace: bool,
}

impl<DB: Database, I> BscEvm<DB, I> {
    /// Creates a new [`BscEvm`].
    pub fn new(env: EvmEnv<BscHardfork>, db: DB, inspector: I, inspect: bool, trace: bool) -> Self {
        let precompiles =
            PrecompilesMap::from_static(BscPrecompiles::new(env.cfg_env.spec).precompiles());
        // Ensure the instruction table matches the configured spec. `new_mainnet()` defaults to
        // the latest spec (Prague), which undercharges pre-Berlin SLOAD in early blocks.
        let spec_id = SpecId::from(env.cfg_env.spec);

        Self {
            inner: EvmCtx {
                ctx: Context {
                    block: env.block_env,
                    cfg: env.cfg_env,
                    journaled_state: Journal::new(db),
                    tx: Default::default(),
                    chain: Default::default(),
                    local: Default::default(),
                    error: Ok(()),
                },
                inspector,
                instruction: EthInstructions::new_mainnet_with_spec(spec_id),
                precompiles,
                frame_stack: Default::default(),
            },
            inspect,
            trace,
        }
    }
}

impl<DB: Database, I> BscEvm<DB, I> {
    /// Provides a reference to the EVM context.
    pub const fn ctx(&self) -> &BscContext<DB> {
        &self.inner.ctx
    }

    /// Provides a mutable reference to the EVM context.
    pub fn ctx_mut(&mut self) -> &mut BscContext<DB> {
        &mut self.inner.ctx
    }

    /// `BscTxEnv` counterpart of `system_contracts::is_system_transaction`.
    fn detect_system_transaction(&self, tx: &BscTxEnv) -> bool {
        use crate::system_contracts::is_invoke_system_contract;
        use revm::primitives::TxKind;

        matches!(tx.base.kind, TxKind::Call(to)
            if tx.base.caller == self.block.beneficiary
                && is_invoke_system_contract(&to)
                && tx.base.gas_price == 0)
    }

    /// Mark `tx` if it looks like a system tx. Idempotent: never downgrades an
    /// already-true flag, because `pre/post_execution` hand-set it with
    /// `caller = 0x00..00` which the predicate does not match.
    pub(crate) fn prepare_tx_for_execution(&self, tx: &mut BscTxEnv) {
        if !tx.is_system_transaction {
            tx.is_system_transaction = self.detect_system_transaction(tx);
        }
    }

    /// `prepare_tx_for_execution` for the `replay()` path (tx already on ctx).
    pub(crate) fn prepare_current_tx_for_execution(&mut self) {
        if !self.inner.ctx.tx.is_system_transaction {
            let detected = self.detect_system_transaction(&self.inner.ctx.tx);
            self.inner.ctx.tx.is_system_transaction = detected;
        }
    }

    /// Stand in for `post_execution::distribute_incoming`'s SYSTEM_ADDRESS →
    /// validator credit, which trace replay skips. Use `incr_balance`, not
    /// `set_balance`, so the archive-state balance survives the deposit's
    /// `B + V → B` round-trip. Runs before the handler's tx checkpoint, so a
    /// (very unlikely) system-tx revert would leave the top-up in place.
    pub(crate) fn fund_beneficiary_for_system_tx_replay(&mut self, value: revm::primitives::U256) {
        if !self.trace || value.is_zero() {
            return;
        }
        let beneficiary = self.block.beneficiary;
        if let Ok(mut account) = self.journal_mut().load_account_mut(beneficiary) {
            let _ = account.incr_balance(value);
        }
    }
}

impl<DB: Database, I> Deref for BscEvm<DB, I> {
    type Target = BscContext<DB>;

    #[inline]
    fn deref(&self) -> &Self::Target {
        self.ctx()
    }
}

impl<DB: Database, I> DerefMut for BscEvm<DB, I> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.ctx_mut()
    }
}

impl<DB, INSP> EvmTr for BscEvm<DB, INSP>
where
    DB: Database,
{
    type Context = BscContext<DB>;
    type Instructions = EthInstructions<EthInterpreter, BscContext<DB>>;
    type Precompiles = PrecompilesMap;
    type Frame = EthFrame;

    fn all(
        &self,
    ) -> (&Self::Context, &Self::Instructions, &Self::Precompiles, &FrameStack<Self::Frame>) {
        self.inner.all()
    }

    fn all_mut(
        &mut self,
    ) -> (
        &mut Self::Context,
        &mut Self::Instructions,
        &mut Self::Precompiles,
        &mut FrameStack<Self::Frame>,
    ) {
        self.inner.all_mut()
    }

    fn ctx(&mut self) -> &mut Self::Context {
        self.all_mut().0
    }

    fn ctx_ref(&self) -> &Self::Context {
        self.all().0
    }

    fn ctx_instructions(&mut self) -> (&mut Self::Context, &mut Self::Instructions) {
        let (ctx, instructions, _, _) = self.all_mut();
        (ctx, instructions)
    }

    fn ctx_precompiles(&mut self) -> (&mut Self::Context, &mut Self::Precompiles) {
        let (ctx, _, precompiles, _) = self.all_mut();
        (ctx, precompiles)
    }

    /// Returns a mutable reference to the frame stack.
    fn frame_stack(&mut self) -> &mut FrameStack<Self::Frame> {
        self.all_mut().3
    }

    fn frame_init(
        &mut self,
        frame_input: FrameInit,
    ) -> Result<FrameInitResult<'_, Self::Frame>, ContextDbError<Self::Context>> {
        self.inner.frame_init(frame_input)
    }

    fn frame_run(
        &mut self,
    ) -> Result<FrameInitOrResult<Self::Frame>, ContextDbError<Self::Context>> {
        self.inner.frame_run()
    }

    fn frame_return_result(
        &mut self,
        result: FrameResult,
    ) -> Result<Option<FrameResult>, ContextDbError<Self::Context>> {
        self.inner.frame_return_result(result)
    }
}

impl<DB, INSP> InspectorEvmTr for BscEvm<DB, INSP>
where
    DB: Database,
    INSP: Inspector<BscContext<DB>>,
{
    type Inspector = INSP;

    fn all_inspector(
        &self,
    ) -> (
        &Self::Context,
        &Self::Instructions,
        &Self::Precompiles,
        &FrameStack<Self::Frame>,
        &Self::Inspector,
    ) {
        self.inner.all_inspector()
    }

    fn all_mut_inspector(
        &mut self,
    ) -> (
        &mut Self::Context,
        &mut Self::Instructions,
        &mut Self::Precompiles,
        &mut FrameStack<Self::Frame>,
        &mut Self::Inspector,
    ) {
        self.inner.all_mut_inspector()
    }

    fn inspector(&mut self) -> &mut Self::Inspector {
        self.all_mut_inspector().4
    }

    fn ctx_inspector(&mut self) -> (&mut Self::Context, &mut Self::Inspector) {
        let (ctx, _, _, _, inspector) = self.all_mut_inspector();
        (ctx, inspector)
    }

    fn ctx_inspector_frame(
        &mut self,
    ) -> (&mut Self::Context, &mut Self::Inspector, &mut Self::Frame) {
        let (ctx, _, _, frame_stack, inspector) = self.all_mut_inspector();
        (ctx, inspector, frame_stack.get())
    }

    fn ctx_inspector_frame_instructions(
        &mut self,
    ) -> (&mut Self::Context, &mut Self::Inspector, &mut Self::Frame, &mut Self::Instructions) {
        let (ctx, instructions, _, frame_stack, inspector) = self.all_mut_inspector();
        (ctx, inspector, frame_stack.get(), instructions)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)] // tests assert by panicking

    use super::BscEvm;
    use crate::{evm::transaction::BscTxEnv, hardforks::bsc::BscHardfork};
    use reth_evm::EvmEnv;
    use revm::{
        context::{BlockEnv, CfgEnv, TxEnv},
        context_interface::result::{EVMError, ExecutionResult, HaltReason, InvalidTransaction},
        handler::instructions::EthInstructions,
        inspector::{InspectEvm, NoOpInspector},
        primitives::{hardfork::SpecId, Address, Bytes, TxKind, U256},
        state::{AccountInfo, Bytecode},
        ExecuteEvm,
    };
    use revm_database::InMemoryDB;

    /// Builds bytecode that repeatedly loads the same storage slot.
    ///
    /// Under pre-Berlin rules each `SLOAD` is charged the full cost, while post-Berlin rules
    /// heavily discount warm reads. This makes it a good regression test for instruction tables
    /// that accidentally default to the latest spec.
    fn repeated_sload_bytecode(repetitions: usize) -> Bytecode {
        let mut code = Vec::with_capacity(repetitions * 3 + 1);
        for _ in 0..repetitions {
            // PUSH1 0x00; SLOAD
            code.extend([0x60, 0x00, 0x54]);
        }
        // STOP
        code.push(0x00);
        Bytecode::new_raw(Bytes::from(code))
    }

    fn make_db(caller: Address, contract: Address, repetitions: usize) -> InMemoryDB {
        let mut db = InMemoryDB::default();

        // Fund the caller so initial gas validation succeeds.
        db.insert_account_info(
            caller,
            AccountInfo { balance: U256::from(1_000_000u64), ..AccountInfo::default() },
        );

        // Install the contract code and a value at storage slot 0.
        let contract_code = repeated_sload_bytecode(repetitions);
        db.insert_account_info(contract, AccountInfo::default().with_code(contract_code));
        db.insert_account_storage(contract, U256::ZERO, U256::from(1u64))
            .expect("storage insert should succeed");

        db
    }

    #[test]
    fn instruction_table_respects_configured_spec_for_early_blocks() {
        // Use a pre-Berlin BSC hardfork which maps to Muir Glacier rules.
        let spec = BscHardfork::Bruno;
        let cfg_env = CfgEnv::new_with_spec(spec).with_chain_id(56);
        let env = EvmEnv::new(cfg_env, BlockEnv::default());

        let caller = Address::from([0x11; 20]);
        let contract = Address::from([0x22; 20]);
        let repetitions = 30;

        // Pick a gas limit that should be sufficient under post-Berlin warm access rules but
        // insufficient under pre-Berlin `SLOAD` pricing.
        let gas_limit = 40_000u64;

        let tx = BscTxEnv::new(
            TxEnv::builder()
                .caller(caller)
                .chain_id(Some(56))
                .gas_limit(gas_limit)
                .gas_price(1)
                .kind(TxKind::Call(contract))
                .build()
                .expect("tx env should build"),
        );

        // Correct instruction table: should respect the pre-Berlin pricing and run out of gas.
        let mut evm = BscEvm::new(
            env.clone(),
            make_db(caller, contract, repetitions),
            NoOpInspector,
            false,
            false,
        );
        let expected_spec_id = SpecId::from(spec);
        assert_eq!(evm.inner.instruction.spec, expected_spec_id);

        let result = evm.transact_one(tx.clone()).expect("execution should not error");
        match result {
            ExecutionResult::Halt { reason, .. } => {
                assert!(
                    matches!(reason, HaltReason::OutOfGas(_)),
                    "expected out-of-gas under pre-Berlin pricing, got {reason:?}"
                );
            }
            other => panic!("expected halt due to out-of-gas, got {other:?}"),
        }

        // Mismatched instruction table (defaults to the latest spec): should undercharge and
        // succeed with the same gas limit.
        let mut mismatched =
            BscEvm::new(env, make_db(caller, contract, repetitions), NoOpInspector, false, false);
        mismatched.inner.instruction = EthInstructions::new_mainnet_with_spec(SpecId::default());

        let mismatched_result = mismatched.transact_one(tx).expect("execution should not error");
        assert!(
            mismatched_result.is_success(),
            "latest-spec instruction table should succeed with this gas limit"
        );
    }

    /// `BscEvm` with Osaka + EIP-7825 cap enabled and a funded beneficiary,
    /// shaped for trace-replay regression tests.
    fn make_trace_replay_evm(
        beneficiary: Address,
        system_contract: Address,
    ) -> BscEvm<InMemoryDB, NoOpInspector> {
        let spec = BscHardfork::Osaka;
        let mut cfg_env = CfgEnv::new_with_spec(spec).with_chain_id(56);
        cfg_env.tx_gas_limit_cap = Some(2u64.pow(24));

        let block_env =
            BlockEnv { beneficiary, prevrandao: Some(U256::from(1).into()), ..Default::default() };
        let env = EvmEnv::new(cfg_env, block_env);

        let mut db = InMemoryDB::default();
        db.insert_account_info(
            beneficiary,
            AccountInfo { balance: U256::from(1_000_000_000_000u64), ..AccountInfo::default() },
        );
        db.insert_account_info(system_contract, AccountInfo::default());

        // trace = true enables `fund_beneficiary_for_system_tx_replay`.
        BscEvm::new(env, db, NoOpInspector, false, true)
    }

    /// Mined-system-tx shape: signer = miner, to = system contract, fee = 0,
    /// gas = `i64::MAX` (the value that trips the EIP-7825 cap).
    fn system_tx_shape(beneficiary: Address, system_contract: Address) -> BscTxEnv {
        BscTxEnv::new(
            TxEnv::builder()
                .caller(beneficiary)
                .chain_id(Some(56))
                .gas_limit(i64::MAX as u64)
                .gas_price(0)
                .kind(TxKind::Call(system_contract))
                .build()
                .expect("tx env should build"),
        )
    }

    /// BSC `VALIDATOR_CONTRACT` (target of `deposit` / `distributeFinalityReward`).
    const VALIDATOR_SYSTEM_CONTRACT: Address =
        Address::new([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x10, 0x00]);

    #[test]
    fn prepare_marks_system_shape_tx() {
        let beneficiary = Address::from([0x10; 20]);
        let evm = make_trace_replay_evm(beneficiary, VALIDATOR_SYSTEM_CONTRACT);
        let mut tx = system_tx_shape(beneficiary, VALIDATOR_SYSTEM_CONTRACT);
        assert!(!tx.is_system_transaction, "precondition: unmarked");

        evm.prepare_tx_for_execution(&mut tx);

        assert!(
            tx.is_system_transaction,
            "tx matching (caller == beneficiary, to ∈ system contracts, gas_price == 0) should be marked"
        );
    }

    #[test]
    fn prepare_is_idempotent_and_does_not_overwrite_pre_marked() {
        // Regression guard for `pre/post_execution` helpers that hand-set
        // is_system_transaction = true with caller = 0x00..00 (which the
        // predicate would reject).
        let beneficiary = Address::from([0x10; 20]);
        let evm = make_trace_replay_evm(beneficiary, VALIDATOR_SYSTEM_CONTRACT);
        let mut tx = BscTxEnv {
            base: TxEnv::builder()
                .caller(Address::ZERO) // != beneficiary
                .chain_id(Some(56))
                .gas_limit(30_000_000)
                .gas_price(0)
                .kind(TxKind::Call(VALIDATOR_SYSTEM_CONTRACT))
                .build()
                .expect("tx env should build"),
            is_system_transaction: true,
        };

        evm.prepare_tx_for_execution(&mut tx);

        assert!(
            tx.is_system_transaction,
            "prepare must not overwrite an explicitly set is_system_transaction = true"
        );
    }

    #[test]
    fn prepare_leaves_normal_user_tx_unmarked() {
        let beneficiary = Address::from([0x10; 20]);
        let evm = make_trace_replay_evm(beneficiary, VALIDATOR_SYSTEM_CONTRACT);
        let mut tx = BscTxEnv::new(
            TxEnv::builder()
                .caller(Address::from([0x11; 20])) // not beneficiary
                .chain_id(Some(56))
                .gas_limit(21_000)
                .gas_price(1) // not zero
                .kind(TxKind::Call(Address::from([0x22; 20])))
                .build()
                .expect("tx env should build"),
        );

        evm.prepare_tx_for_execution(&mut tx);

        assert!(
            !tx.is_system_transaction,
            "ordinary user tx must not be misclassified as a system tx"
        );
    }

    #[test]
    fn replay_marks_system_tx_before_validation() {
        let beneficiary = Address::from([0x10; 20]);
        let mut evm = make_trace_replay_evm(beneficiary, VALIDATOR_SYSTEM_CONTRACT);
        evm.inner.ctx.tx = system_tx_shape(beneficiary, VALIDATOR_SYSTEM_CONTRACT);

        let result = evm.replay();
        if let Err(EVMError::Transaction(InvalidTransaction::TxGasLimitGreaterThanCap { .. })) =
            result
        {
            panic!("system transaction was not classified before replay validation");
        }
        assert!(
            evm.inner.ctx.tx.is_system_transaction,
            "system transaction should be marked after replay()"
        );
    }

    #[test]
    fn transact_one_marks_system_tx_before_validation() {
        let beneficiary = Address::from([0x10; 20]);
        let mut evm = make_trace_replay_evm(beneficiary, VALIDATOR_SYSTEM_CONTRACT);
        let tx = system_tx_shape(beneficiary, VALIDATOR_SYSTEM_CONTRACT);

        let result = evm.transact_one(tx);
        if let Err(EVMError::Transaction(InvalidTransaction::TxGasLimitGreaterThanCap { .. })) =
            result
        {
            panic!("system transaction was not classified before transact_one validation");
        }
        assert!(
            evm.inner.ctx.tx.is_system_transaction,
            "system transaction should be marked after transact_one()"
        );
    }

    #[test]
    fn inspect_one_tx_marks_system_tx_before_validation() {
        let beneficiary = Address::from([0x10; 20]);
        let mut evm = make_trace_replay_evm(beneficiary, VALIDATOR_SYSTEM_CONTRACT);
        let tx = system_tx_shape(beneficiary, VALIDATOR_SYSTEM_CONTRACT);

        let result = evm.inspect_one_tx(tx);
        if let Err(EVMError::Transaction(InvalidTransaction::TxGasLimitGreaterThanCap { .. })) =
            result
        {
            panic!("system transaction was not classified before inspect_one_tx validation");
        }
        assert!(
            evm.inner.ctx.tx.is_system_transaction,
            "system transaction should be marked after inspect_one_tx()"
        );
    }

    fn read_beneficiary_balance(
        evm: &mut BscEvm<InMemoryDB, NoOpInspector>,
        beneficiary: Address,
    ) -> U256 {
        use revm::{
            context::{ContextTr, JournalTr},
            context_interface::journaled_state::account::JournaledAccountTr,
        };
        *evm.journal_mut()
            .load_account_mut(beneficiary)
            .expect("beneficiary account should load")
            .balance()
    }

    const TRACE_BENEFICIARY_INITIAL_BALANCE: u64 = 1_000_000_000_000u64;

    #[test]
    fn fund_uses_incr_semantics_not_set() {
        let beneficiary = Address::from([0x10; 20]);
        let mut evm = make_trace_replay_evm(beneficiary, VALIDATOR_SYSTEM_CONTRACT);

        let initial = U256::from(TRACE_BENEFICIARY_INITIAL_BALANCE);
        let top_up = U256::from(42_000u64);
        evm.fund_beneficiary_for_system_tx_replay(top_up);

        assert_eq!(read_beneficiary_balance(&mut evm, beneficiary), initial + top_up);
    }

    #[test]
    fn fund_is_noop_for_zero_value() {
        let beneficiary = Address::from([0x10; 20]);
        let mut evm = make_trace_replay_evm(beneficiary, VALIDATOR_SYSTEM_CONTRACT);

        let initial = U256::from(TRACE_BENEFICIARY_INITIAL_BALANCE);
        evm.fund_beneficiary_for_system_tx_replay(U256::ZERO);

        assert_eq!(read_beneficiary_balance(&mut evm, beneficiary), initial);
    }

    #[test]
    fn fund_is_noop_when_trace_disabled() {
        let beneficiary = Address::from([0x10; 20]);
        let mut cfg_env = CfgEnv::new_with_spec(BscHardfork::Osaka).with_chain_id(56);
        cfg_env.tx_gas_limit_cap = Some(2u64.pow(24));
        let block_env =
            BlockEnv { beneficiary, prevrandao: Some(U256::from(1).into()), ..Default::default() };
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            beneficiary,
            AccountInfo {
                balance: U256::from(TRACE_BENEFICIARY_INITIAL_BALANCE),
                ..AccountInfo::default()
            },
        );
        let mut evm = BscEvm::new(EvmEnv::new(cfg_env, block_env), db, NoOpInspector, false, false);

        let initial = U256::from(TRACE_BENEFICIARY_INITIAL_BALANCE);
        evm.fund_beneficiary_for_system_tx_replay(U256::from(123u64));

        assert_eq!(read_beneficiary_balance(&mut evm, beneficiary), initial);
    }
}
