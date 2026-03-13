use std::ops::{Deref, DerefMut};

use crate::{evm::transaction::BscTxEnv, hardforks::bsc::BscHardfork};

use super::precompiles::BscPrecompiles;
use reth_evm::{precompiles::PrecompilesMap, Database, EvmEnv};
use revm::{
    context::{BlockEnv, CfgEnv, Evm as EvmCtx, FrameStack, JournalTr},
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
        context_interface::result::{ExecutionResult, HaltReason},
        handler::instructions::EthInstructions,
        inspector::NoOpInspector,
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
        let mut mismatched = BscEvm::new(
            env,
            make_db(caller, contract, repetitions),
            NoOpInspector,
            false,
            false,
        );
        mismatched.inner.instruction = EthInstructions::new_mainnet();

        let mismatched_result = mismatched
            .transact_one(tx)
            .expect("execution should not error");
        assert!(
            mismatched_result.is_success(),
            "latest-spec instruction table should succeed with this gas limit"
        );
    }
}
