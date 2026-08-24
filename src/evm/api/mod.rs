use std::ops::{Deref, DerefMut};

use crate::{evm::block_env::BscBlockEnv, evm::transaction::BscTxEnv, hardforks::bsc::BscHardfork};

use super::precompiles::BscPrecompiles;
use reth_evm::{precompiles::PrecompilesMap, Database, EvmEnv};
use revm::{
    context::{CfgEnv, ContextTr, Evm as EvmCtx, FrameStack, JournalTr},
    handler::{
        evm::{ContextDbError, FrameInitResult},
        instructions::EthInstructions,
        EthFrame, EvmTr, FrameInitOrResult, FrameResult,
    },
    inspector::InspectorEvmTr,
    interpreter::{
        interpreter::EthInterpreter, interpreter_action::FrameInit, InstructionResult,
        InterpreterAction, InterpreterResult,
    },
    primitives::{hardfork::SpecId, Bytes},
    Context, Inspector, Journal,
};
use revm::context_interface::journaled_state::account::JournaledAccountTr;

mod exec;

/// Type alias for the default context type of the BscEvm.
pub type BscContext<DB> = Context<BscBlockEnv, BscTxEnv, CfgEnv<BscHardfork>, DB>;

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
    pub fn new(
        env: EvmEnv<BscHardfork, BscBlockEnv>,
        db: DB,
        inspector: I,
        inspect: bool,
        trace: bool,
    ) -> Self {
        let mut precompiles =
            PrecompilesMap::from_static(BscPrecompiles::new(env.cfg_env.spec).precompiles());
        // BEP-706 (Jenner): the 0x70 millisecond-timestamp precompile depends on the
        // block being executed, so it is registered dynamically here — never in the
        // static fork-cumulative tables. The factory captures this block's sub-second
        // remainder; the seconds are read live from the block env at call time.
        if env.cfg_env.spec >= BscHardfork::Jenner {
            precompiles.extend_precompiles([
                crate::evm::precompiles::milli_timestamp::milli_timestamp_precompile(
                    env.block_env.milli_remainder,
                ),
            ]);
        }
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
        use revm::{context_interface::Transaction as _, primitives::TxKind};

        // BSC never enforces a base fee, so pass 0. `tx.base.gas_price` alone is
        // `max_fee_per_gas` for EIP-1559 transactions, which wrongly excludes a
        // zero-priority-fee tx with a non-zero fee cap from being detected as system.
        matches!(tx.base.kind, TxKind::Call(to)
            if tx.base.caller == self.block.beneficiary
                && is_invoke_system_contract(&to)
                && tx.base.effective_gas_price(0) == 0)
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
    ) -> (
        &Self::Context,
        &Self::Instructions,
        &Self::Precompiles,
        &FrameStack<Self::Frame>,
    ) {
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
        let coinbase = self.block.beneficiary;
        let hits_coinbase = {
            let frame = self.frame_stack().get();
            frame.interpreter.input.target_address == coinbase
        };

        if hits_coinbase {
            // Ported from go-bsc (core/vm: "coinbase address cannot be used as contract
            // address"): refuse to execute any bytecode whose storage/self context is the
            // block's coinbase, whether reached via CALL/DELEGATECALL to pre-existing code
            // or freshly deployed via CREATE/CREATE2. Without this, a contract could run
            // "as" the coinbase and be mistaken for the validator itself by callers that
            // gate on `caller == coinbase` (e.g. system transaction detection), or otherwise
            // interfere with fee-distribution invariants that assume the coinbase never
            // carries code. This mirrors go-ethereum's error-code semantics: burn the
            // frame's remaining gas and revert its state changes, same as any other halt.
            let (ctx, _, _, frame_stack) = self.all_mut();
            let frame = frame_stack.get();
            let mut gas = frame.interpreter.gas;
            gas.spend_all();
            let action = InterpreterAction::Return(InterpreterResult {
                result: InstructionResult::PrecompileError,
                output: Bytes::new(),
                gas,
            });
            return frame.process_next_action(ctx, action).inspect(|i| {
                if i.is_result() {
                    frame.set_finished(true);
                }
            });
        }

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
        let env = EvmEnv::new(cfg_env, BlockEnv::default().into());

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
        let mut mismatched = BscEvm::new(
            env,
            make_db(caller, contract, repetitions),
            NoOpInspector,
            false,
            false,
        );
        mismatched.inner.instruction = EthInstructions::new_mainnet_with_spec(SpecId::default());

        let mismatched_result = mismatched
            .transact_one(tx)
            .expect("execution should not error");
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

        let block_env = BlockEnv {
            beneficiary,
            prevrandao: Some(U256::from(1).into()),
            ..Default::default()
        };
        let env = EvmEnv::new(cfg_env, block_env.into());

        let mut db = InMemoryDB::default();
        db.insert_account_info(
            beneficiary,
            AccountInfo {
                balance: U256::from(1_000_000_000_000u64),
                ..AccountInfo::default()
            },
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
    const VALIDATOR_SYSTEM_CONTRACT: Address = Address::new([
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x10, 0x00,
    ]);

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
        use revm::context::{ContextTr, JournalTr};
        use revm::context_interface::journaled_state::account::JournaledAccountTr;
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
        let block_env = BlockEnv {
            beneficiary,
            prevrandao: Some(U256::from(1).into()),
            ..Default::default()
        };
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            beneficiary,
            AccountInfo {
                balance: U256::from(TRACE_BENEFICIARY_INITIAL_BALANCE),
                ..AccountInfo::default()
            },
        );
        let mut evm = BscEvm::new(EvmEnv::new(cfg_env, block_env.into()), db, NoOpInspector, false, false);

        let initial = U256::from(TRACE_BENEFICIARY_INITIAL_BALANCE);
        evm.fund_beneficiary_for_system_tx_replay(U256::from(123u64));

        assert_eq!(read_beneficiary_balance(&mut evm, beneficiary), initial);
    }

    #[test]
    fn prepare_marks_eip1559_tx_with_zero_effective_price() {
        // BSC never enforces a base fee, so a non-zero fee cap with a zero priority fee
        // still has an effective price of 0 and must be classified as a system tx.
        let beneficiary = Address::from([0x10; 20]);
        let evm = make_trace_replay_evm(beneficiary, VALIDATOR_SYSTEM_CONTRACT);
        let mut tx = BscTxEnv::new(
            TxEnv::builder()
                .tx_type(Some(revm::context_interface::TransactionType::Eip1559 as u8))
                .caller(beneficiary)
                .chain_id(Some(56))
                .gas_limit(21_000)
                .gas_price(1_000_000_000) // non-zero fee cap
                .gas_priority_fee(Some(0)) // zero priority fee => zero effective price
                .kind(TxKind::Call(VALIDATOR_SYSTEM_CONTRACT))
                .build()
                .expect("tx env should build"),
        );

        evm.prepare_tx_for_execution(&mut tx);

        assert!(
            tx.is_system_transaction,
            "EIP-1559 tx with zero effective gas price should be marked as system, \
             even though its fee cap is non-zero"
        );
    }

    #[test]
    fn prepare_leaves_eip1559_tx_with_nonzero_effective_price_unmarked() {
        let beneficiary = Address::from([0x10; 20]);
        let evm = make_trace_replay_evm(beneficiary, VALIDATOR_SYSTEM_CONTRACT);
        let mut tx = BscTxEnv::new(
            TxEnv::builder()
                .tx_type(Some(revm::context_interface::TransactionType::Eip1559 as u8))
                .caller(beneficiary)
                .chain_id(Some(56))
                .gas_limit(21_000)
                .gas_price(1_000_000_000)
                .gas_priority_fee(Some(1)) // non-zero priority fee => non-zero effective price
                .kind(TxKind::Call(VALIDATOR_SYSTEM_CONTRACT))
                .build()
                .expect("tx env should build"),
        );

        evm.prepare_tx_for_execution(&mut tx);

        assert!(
            !tx.is_system_transaction,
            "EIP-1559 tx that actually pays a priority fee must not be classified as system"
        );
    }

    /// Ported guard (go-bsc: "coinbase address cannot be used as contract address"): a call
    /// into pre-existing bytecode deployed at the block's coinbase must fail before any of
    /// that bytecode runs.
    #[test]
    fn call_into_code_deployed_at_coinbase_is_rejected() {
        let coinbase = Address::from([0xC0; 20]);
        let caller = Address::from([0x11; 20]);

        let cfg_env = CfgEnv::new_with_spec(BscHardfork::Osaka).with_chain_id(56);
        let block_env = BlockEnv {
            beneficiary: coinbase,
            prevrandao: Some(U256::from(1).into()),
            ..Default::default()
        };
        let env = EvmEnv::new(cfg_env, block_env.into());

        let mut db = InMemoryDB::default();
        db.insert_account_info(
            caller,
            AccountInfo { balance: U256::from(1_000_000u64), ..AccountInfo::default() },
        );
        // PUSH1 0x01; PUSH1 0x00; SSTORE; STOP -- would leave storage slot 0 set to 1 if it
        // ever ran.
        let code = Bytecode::new_raw(Bytes::from(vec![0x60, 0x01, 0x60, 0x00, 0x55, 0x00]));
        db.insert_account_info(coinbase, AccountInfo::default().with_code(code));

        let mut evm = BscEvm::new(env, db, NoOpInspector, false, false);
        let tx = BscTxEnv::new(
            TxEnv::builder()
                .caller(caller)
                .chain_id(Some(56))
                .gas_limit(100_000)
                .gas_price(1)
                .kind(TxKind::Call(coinbase))
                .build()
                .expect("tx env should build"),
        );

        let result = evm.transact_one(tx).expect("execution should not error");
        match result {
            ExecutionResult::Halt { reason, .. } => {
                assert!(
                    matches!(reason, HaltReason::PrecompileError),
                    "expected the coinbase-as-contract guard to halt execution, got {reason:?}"
                );
            }
            other => panic!(
                "expected a halt when calling code deployed at the coinbase address, got {other:?}"
            ),
        }

        // The guard must fire before any bytecode runs.
        use revm::context::{ContextTr, JournalTr};
        let slot =
            evm.journal_mut().sload(coinbase, U256::ZERO).expect("sload should succeed").data;
        assert_eq!(slot, U256::ZERO, "SSTORE must not have executed");
    }

    /// Same guard, but for a `CREATE` whose computed address happens to land on the coinbase.
    #[test]
    fn create_landing_on_coinbase_is_rejected() {
        let caller = Address::from([0x11; 20]);
        // The address a `CREATE` from `caller` at nonce 0 would produce, computed up front so
        // it can be used as the block's coinbase.
        let created_address = caller.create(0);

        let cfg_env = CfgEnv::new_with_spec(BscHardfork::Osaka).with_chain_id(56);
        let block_env = BlockEnv {
            beneficiary: created_address,
            prevrandao: Some(U256::from(1).into()),
            ..Default::default()
        };
        let env = EvmEnv::new(cfg_env, block_env.into());

        let mut db = InMemoryDB::default();
        db.insert_account_info(
            caller,
            AccountInfo { balance: U256::from(1_000_000u64), ..AccountInfo::default() },
        );

        let mut evm = BscEvm::new(env, db, NoOpInspector, false, false);
        // Init code that deploys a single non-empty runtime byte if it ever runs:
        // MSTORE8(0, 0x00); RETURN(0, 1).
        let init_code = vec![0x60, 0x00, 0x60, 0x00, 0x53, 0x60, 0x01, 0x60, 0x00, 0xF3];
        let tx = BscTxEnv::new(
            TxEnv::builder()
                .caller(caller)
                .chain_id(Some(56))
                .gas_limit(200_000)
                .gas_price(1)
                .kind(TxKind::Create)
                .data(Bytes::from(init_code))
                .build()
                .expect("tx env should build"),
        );

        let result = evm.transact_one(tx).expect("execution should not error");
        assert!(
            !result.is_success(),
            "CREATE landing on the coinbase address must fail, got {result:?}"
        );
    }

    /// Regression guard: only bytecode execution at the coinbase is blocked. A plain value
    /// transfer to a coinbase with no code (tips, MEV payments, etc. — the common case on
    /// every block) never reaches the interpreter and must be unaffected.
    #[test]
    fn call_to_coinbase_without_code_still_succeeds() {
        let coinbase = Address::from([0xC0; 20]);
        let caller = Address::from([0x11; 20]);

        let cfg_env = CfgEnv::new_with_spec(BscHardfork::Osaka).with_chain_id(56);
        let block_env = BlockEnv {
            beneficiary: coinbase,
            prevrandao: Some(U256::from(1).into()),
            ..Default::default()
        };
        let env = EvmEnv::new(cfg_env, block_env.into());

        let mut db = InMemoryDB::default();
        db.insert_account_info(
            caller,
            AccountInfo { balance: U256::from(1_000_000u64), ..AccountInfo::default() },
        );
        // No account installed at `coinbase`: empty, no code.

        let mut evm = BscEvm::new(env, db, NoOpInspector, false, false);
        let tx = BscTxEnv::new(
            TxEnv::builder()
                .caller(caller)
                .chain_id(Some(56))
                .gas_limit(50_000)
                .gas_price(1)
                .value(U256::from(100u64))
                .kind(TxKind::Call(coinbase))
                .build()
                .expect("tx env should build"),
        );

        let result = evm.transact_one(tx).expect("execution should not error");
        assert!(
            result.is_success(),
            "a plain transfer to an empty coinbase account must succeed, got {result:?}"
        );
    }

    // ---- BEP-706 milliTimestamp precompile (0x70, Jenner) ----

    const MILLI_TS_ADDRESS: Address = revm::precompile::u64_to_address(0x70);
    const JENNER_TEST_SECS: u64 = 1_790_000_000;
    const JENNER_TEST_REMAINDER: u64 = 750;
    const JENNER_TEST_MS: u64 = JENNER_TEST_SECS * 1000 + JENNER_TEST_REMAINDER;

    fn make_jenner_evm(
        spec: BscHardfork,
        caller: Address,
        contract_code: Option<(Address, Vec<u8>)>,
    ) -> BscEvm<InMemoryDB, NoOpInspector> {
        let cfg_env = CfgEnv::new_with_spec(spec).with_chain_id(56);
        let block_env = BlockEnv {
            beneficiary: Address::from([0xC0; 20]),
            timestamp: U256::from(JENNER_TEST_SECS),
            prevrandao: Some(U256::from(1).into()),
            ..Default::default()
        };
        let env = EvmEnv::new(
            cfg_env,
            crate::evm::block_env::BscBlockEnv::new(block_env, JENNER_TEST_REMAINDER),
        );

        let mut db = InMemoryDB::default();
        db.insert_account_info(
            caller,
            AccountInfo { balance: U256::from(1_000_000u64), ..AccountInfo::default() },
        );
        if let Some((address, code)) = contract_code {
            db.insert_account_info(
                address,
                AccountInfo::default().with_code(Bytecode::new_raw(Bytes::from(code))),
            );
        }
        BscEvm::new(env, db, NoOpInspector, false, false)
    }

    fn call_output(result: ExecutionResult) -> Bytes {
        match result {
            ExecutionResult::Success { output, .. } => output.into_data(),
            other => panic!("expected success, got {other:?}"),
        }
    }

    /// Mirrors go-bsc `TestMilliTimestamp_PreActivation`: before Jenner, `0x70` is a
    /// plain empty account — the call succeeds with empty return data, no revert, no
    /// "invalid precompile" error.
    #[test]
    fn milli_timestamp_pre_activation_behaves_as_empty_account() {
        let caller = Address::from([0x11; 20]);
        let mut evm = make_jenner_evm(BscHardfork::Pasteur, caller, None);
        let tx = BscTxEnv::new(
            TxEnv::builder()
                .caller(caller)
                .chain_id(Some(56))
                .gas_limit(60_000)
                .gas_price(1)
                .data(Bytes::from(vec![0xde, 0xad]))
                .kind(TxKind::Call(MILLI_TS_ADDRESS))
                .build()
                .expect("tx env should build"),
        );

        let result = evm.transact_one(tx).expect("execution should not error");
        assert!(result.is_success(), "pre-Jenner call to 0x70 must succeed, got {result:?}");
        assert!(call_output(result).is_empty(), "empty account returns no data");
    }

    /// Mirrors go-bsc `TestMilliTimestamp_PostActivation` + `_GasCost` + `_IgnoresInput`
    /// end-to-end: from Jenner, `0x70` returns the block's millisecond timestamp for a
    /// flat 20 gas (21000 intrinsic + 20 for an empty-calldata direct call — the same
    /// observable go's devnet run showed), and calldata does not affect the output.
    #[test]
    fn milli_timestamp_post_activation_returns_value_for_twenty_gas() {
        let caller = Address::from([0x11; 20]);
        let mut evm = make_jenner_evm(BscHardfork::Jenner, caller, None);
        let tx = BscTxEnv::new(
            TxEnv::builder()
                .caller(caller)
                .chain_id(Some(56))
                .gas_limit(60_000)
                .gas_price(1)
                .kind(TxKind::Call(MILLI_TS_ADDRESS))
                .build()
                .expect("tx env should build"),
        );
        let result = evm.transact_one(tx).expect("execution should not error");
        let gas_used = result.tx_gas_used();
        let output = call_output(result);
        assert_eq!(U256::from_be_slice(&output), U256::from(JENNER_TEST_MS));
        assert_eq!(output.len(), 32);
        assert_eq!(gas_used, 21_000 + 20, "flat 20 gas on top of the intrinsic cost");

        // Garbage calldata changes only the intrinsic gas, never the output.
        let tx = BscTxEnv::new(
            TxEnv::builder()
                .caller(caller)
                .chain_id(Some(56))
                .nonce(1)
                .gas_limit(60_000)
                .gas_price(1)
                .data(Bytes::from(vec![0xde, 0xad, 0xbe, 0xef]))
                .kind(TxKind::Call(MILLI_TS_ADDRESS))
                .build()
                .expect("tx env should build"),
        );
        let result = evm.transact_one(tx).expect("execution should not error");
        assert_eq!(U256::from_be_slice(&call_output(result)), U256::from(JENNER_TEST_MS));
    }

    /// Mirrors go-bsc `TestMilliTimestamp_AllCallKindsUseBlockContext` (and
    /// `_StaticCall`): CALL, CALLCODE, DELEGATECALL and STATICCALL all reach the
    /// dynamically registered precompile and return the real millisecond timestamp.
    #[test]
    fn milli_timestamp_serves_all_call_kinds() {
        let caller = Address::from([0x11; 20]);
        let contract = Address::from([0x22; 20]);

        // (opcode, has value argument): bytecode calls 0x70 with the given kind and
        // returns the first 32 bytes of the returned data.
        for (name, opcode, pushes_value) in [
            ("CALL", 0xF1u8, true),
            ("CALLCODE", 0xF2, true),
            ("DELEGATECALL", 0xF4, false),
            ("STATICCALL", 0xFA, false),
        ] {
            // PUSH1 0x20 (retSize) PUSH1 0x00 (retOffset) PUSH1 0x00 (argsSize)
            // PUSH1 0x00 (argsOffset) [PUSH1 0x00 (value)] PUSH1 0x70 (addr) GAS <op>
            // POP PUSH1 0x20 PUSH1 0x00 RETURN
            let mut code =
                vec![0x60, 0x20, 0x60, 0x00, 0x60, 0x00, 0x60, 0x00];
            if pushes_value {
                code.extend([0x60, 0x00]);
            }
            code.extend([0x60, 0x70, 0x5A, opcode, 0x50, 0x60, 0x20, 0x60, 0x00, 0xF3]);

            let mut evm =
                make_jenner_evm(BscHardfork::Jenner, caller, Some((contract, code)));
            let tx = BscTxEnv::new(
                TxEnv::builder()
                    .caller(caller)
                    .chain_id(Some(56))
                    .gas_limit(200_000)
                    .gas_price(1)
                    .kind(TxKind::Call(contract))
                    .build()
                    .expect("tx env should build"),
            );
            let result = evm.transact_one(tx).expect("execution should not error");
            let output = call_output(result);
            assert_eq!(
                U256::from_be_slice(&output),
                U256::from(JENNER_TEST_MS),
                "{name} must reach the millisecond-timestamp precompile"
            );
        }
    }

    /// The go-side `TestPrecompiles_FreshMap` equivalent, asserted on the live EVM's
    /// `PrecompilesMap` (0x70 is registered dynamically, never in the static tables):
    /// the Jenner set is exactly the Pasteur set plus `0x70`, and no earlier spec
    /// contains `0x70` — i.e. the precompile cannot activate early.
    #[test]
    fn jenner_precompile_set_is_pasteur_set_plus_milli_timestamp() {
        use std::collections::BTreeSet;

        let caller = Address::from([0x11; 20]);
        let addresses = |spec: BscHardfork| -> BTreeSet<Address> {
            let evm = make_jenner_evm(spec, caller, None);
            evm.inner.precompiles.addresses().copied().collect()
        };

        let pasteur = addresses(BscHardfork::Pasteur);
        let jenner = addresses(BscHardfork::Jenner);

        assert!(!pasteur.contains(&MILLI_TS_ADDRESS), "0x70 must not exist before Jenner");
        assert!(jenner.contains(&MILLI_TS_ADDRESS));
        let mut expected = pasteur.clone();
        expected.insert(MILLI_TS_ADDRESS);
        assert_eq!(jenner, expected, "Jenner set == Pasteur set + 0x70, nothing else");

        // Every pre-Jenner spec stays 0x70-free (spot-check the fork ladder).
        for spec in [BscHardfork::Mendel, BscHardfork::Haber, BscHardfork::Hertz] {
            assert!(
                !addresses(spec).contains(&MILLI_TS_ADDRESS),
                "{spec:?} must not contain 0x70"
            );
        }
    }
}
