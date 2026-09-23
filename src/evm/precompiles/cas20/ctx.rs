//! CAS20 journal access, gas accounting and frame-wide errors.
//! Mirrors go-bsc's contracts_stateful.go and cas20_gas.go.

use super::observer::CallStats;
use alloy_evm::EvmInternals;
use alloy_primitives::{Address, Bytes, Log, B256, U256};
use revm::{
    bytecode::Bytecode,
    interpreter::{SStoreResult, StateLoad},
};

// Every charge mirrors an existing EVM cost, so nothing a CAS20 call does is cheaper
// than the same work through bytecode (BEP-702 3.14). The values are go-bsc's `params`.
pub(crate) const WARM_STORAGE_READ_COST: u64 = 100;
pub(crate) const COLD_SLOAD_COST: u64 = 2100;
pub(crate) const COLD_ACCOUNT_ACCESS_COST: u64 = 2600;
pub(crate) const COPY_GAS: u64 = 3;
pub(crate) const MEMORY_GAS: u64 = 3;
pub(crate) const KECCAK256_GAS: u64 = 30;
pub(crate) const KECCAK256_WORD_GAS: u64 = 6;
pub(crate) const LOG_GAS: u64 = 375;
pub(crate) const LOG_TOPIC_GAS: u64 = 375;
pub(crate) const LOG_DATA_GAS: u64 = 8;
pub(crate) const CREATE_DATA_GAS: u64 = 200;
pub(crate) const CREATE_GAS: u64 = 32000;
pub(crate) const SSTORE_SENTRY_GAS: u64 = 2300;
pub(crate) const SSTORE_SET_GAS: u64 = 20000;
pub(crate) const SSTORE_RESET_GAS: u64 = 5000;
pub(crate) const SSTORE_CLEARS_SCHEDULE_REFUND: i64 = 4800;
pub(crate) const ECRECOVER_GAS: u64 = 3000;

/// A warm CALL plus the copy of one calldata word into the callee's input.
pub(crate) const CALLDATA_WORD_GAS: u64 = COPY_GAS + MEMORY_GAS;

/// What a CAS20 call needs from the state: the journal, so every read warms and
/// every write is checkpointed with the frame, plus the block fields it consults.
pub trait Cas20State {
    fn sload(&mut self, address: Address, key: U256) -> Result<StateLoad<U256>, String>;
    fn sstore(
        &mut self,
        address: Address,
        key: U256,
        value: U256,
    ) -> Result<StateLoad<SStoreResult>, String>;
    /// Loads the account, warming it, and answers its code hash. A missing account
    /// answers the empty-code hash, which is all the callers distinguish.
    fn code_hash(&mut self, address: Address) -> Result<StateLoad<B256>, String>;
    fn set_code(&mut self, address: Address, code: Bytecode) -> Result<(), String>;
    fn log(&mut self, log: Log);
    fn block_timestamp(&self) -> u64;
    fn chain_id(&self) -> u64;
}

impl Cas20State for EvmInternals<'_> {
    fn sload(&mut self, address: Address, key: U256) -> Result<StateLoad<U256>, String> {
        EvmInternals::sload(self, address, key).map_err(|e| e.to_string())
    }

    fn sstore(
        &mut self,
        address: Address,
        key: U256,
        value: U256,
    ) -> Result<StateLoad<SStoreResult>, String> {
        EvmInternals::sstore(self, address, key, value).map_err(|e| e.to_string())
    }

    fn code_hash(&mut self, address: Address) -> Result<StateLoad<B256>, String> {
        let acc = self.load_account(address).map_err(|e| e.to_string())?;
        Ok(StateLoad::new(acc.data.info.code_hash, acc.is_cold))
    }

    fn set_code(&mut self, address: Address, code: Bytecode) -> Result<(), String> {
        self.touch_account(address).map_err(|e| e.to_string())?;
        EvmInternals::set_code(self, address, code).map_err(|e| e.to_string())
    }

    fn log(&mut self, log: Log) {
        EvmInternals::log(self, log)
    }

    fn block_timestamp(&self) -> u64 {
        EvmInternals::block_timestamp(self).saturating_to()
    }

    fn chain_id(&self) -> u64 {
        EvmInternals::chain_id(self)
    }
}

/// Frame gas budget; an unaffordable charge exhausts it.
#[derive(Debug)]
pub(crate) struct Meter {
    limit: u64,
    used: u64,
    pub(crate) refund: i64,
}

impl Meter {
    pub(crate) fn new(limit: u64) -> Self {
        Self { limit, used: 0, refund: 0 }
    }

    pub(crate) fn used(&self) -> u64 {
        self.used
    }

    pub(crate) fn left(&self) -> u64 {
        self.limit - self.used
    }

    /// What an exhausted frame hands back: nothing.
    pub(crate) fn exhaust(&mut self) {
        self.used = self.limit;
    }

    /// Exhausts the budget when the charge cannot be covered, as the EVM does.
    fn charge(&mut self, cost: u64) -> bool {
        if self.left() < cost {
            self.used = self.limit;
            return false;
        }
        self.used += cost;
        true
    }
}

/// Shared accounting and errors for all contexts within one EVM frame.
pub(crate) struct Frame<'a> {
    pub(crate) state: &'a mut dyn Cas20State,
    pub(crate) gas: Meter,
    /// What the call has done so far, for the observer; never consulted by the logic.
    pub(crate) stats: CallStats,
    out_of_gas: bool,
    /// Outranks out_of_gas at the exit: the frame had no business writing at all,
    /// whatever it could afford.
    write_protected: bool,
    /// A database failure. It is treated as out-of-gas for control flow and turned
    /// into a fatal precompile error at the exit, which aborts the transaction the
    /// way a database error inside the interpreter would.
    pub(crate) fatal: Option<String>,
}

impl<'a> Frame<'a> {
    pub(crate) fn new(state: &'a mut dyn Cas20State, gas_limit: u64) -> Self {
        Self {
            state,
            gas: Meter::new(gas_limit),
            stats: CallStats::default(),
            out_of_gas: false,
            write_protected: false,
            fatal: None,
        }
    }

    pub(crate) fn is_out_of_gas(&self) -> bool {
        self.out_of_gas
    }

    pub(crate) fn is_write_protected(&self) -> bool {
        self.write_protected
    }

    fn fail(&mut self, err: String) {
        self.fatal.get_or_insert(err);
        self.out_of_gas = true;
    }

    pub(crate) fn sload(&mut self, address: Address, key: U256) -> Option<StateLoad<U256>> {
        self.stats.sloads += 1;
        match self.state.sload(address, key) {
            Ok(v) => Some(v),
            Err(e) => {
                self.fail(e);
                None
            }
        }
    }

    pub(crate) fn sstore(
        &mut self,
        address: Address,
        key: U256,
        value: U256,
    ) -> Option<StateLoad<SStoreResult>> {
        self.stats.sstores += 1;
        match self.state.sstore(address, key, value) {
            Ok(v) => Some(v),
            Err(e) => {
                self.fail(e);
                None
            }
        }
    }

    pub(crate) fn code_hash(&mut self, address: Address) -> Option<StateLoad<B256>> {
        match self.state.code_hash(address) {
            Ok(v) => Some(v),
            Err(e) => {
                self.fail(e);
                None
            }
        }
    }

    pub(crate) fn set_code(&mut self, address: Address, code: Bytecode) {
        if let Err(e) = self.state.set_code(address, code) {
            self.fail(e);
        }
    }
}

/// The environment handed to one CAS20 entry point.
pub(crate) struct Ctx<'f, 'a> {
    pub(crate) frame: &'f mut Frame<'a>,
    /// The callee address and the storage root of its state.
    pub(crate) self_addr: Address,
    /// msg.sender for this frame.
    pub(crate) caller: Address,
    /// True inside a STATICCALL or any read-only ancestor frame.
    pub(crate) read_only: bool,
    /// False for CALLCODE and DELEGATECALL, which would make self_addr something
    /// other than the genuine callee.
    pub(crate) direct_call: bool,
    /// The wei attached to the frame.
    pub(crate) value: U256,
    /// Per-frame, not stored: outside the bootstrap window the zero admin count
    /// freezes role mutation on its own (BEP-702 3.4).
    pub(crate) admin_renounced: bool,
}

impl<'f, 'a> Ctx<'f, 'a> {
    /// read_only is carried across: dropping it would let initCalls write during a
    /// STATICCALL.
    pub(crate) fn spawn_bootstrap(&mut self, self_addr: Address, caller: Address) -> Ctx<'_, 'a> {
        Ctx {
            frame: &mut *self.frame,
            self_addr,
            caller,
            read_only: self.read_only,
            direct_call: true,
            value: U256::ZERO,
            admin_renounced: false,
        }
    }

    /// An exhausted frame hands nothing back, whatever it had left when the
    /// charge it could not cover arrived; the meter says so from here on.
    pub(crate) fn mark_out_of_gas(&mut self) {
        self.frame.out_of_gas = true;
        self.frame.gas.exhaust();
    }

    pub(crate) fn out_of_gas(&self) -> bool {
        self.frame.out_of_gas
    }

    pub(crate) fn gas_left(&self) -> u64 {
        self.frame.gas.left()
    }

    pub(crate) fn block_time(&self) -> u64 {
        self.frame.state.block_timestamp()
    }

    pub(crate) fn chain_id(&self) -> u64 {
        self.frame.state.chain_id()
    }

    /// The handlers each check read_only first; this is what makes a missing check
    /// fail closed instead of writing inside a STATICCALL.
    pub(crate) fn mark_write_protected(&mut self) {
        self.frame.write_protected = true;
    }

    /// Where every CAS20 charge arrives. False means stop before the operation the
    /// charge pays for, as the interpreter checks gas before an opcode.
    pub(crate) fn charge_gas(&mut self, cost: u64) -> bool {
        if self.out_of_gas() {
            return false;
        }
        if !self.frame.gas.charge(cost) {
            self.mark_out_of_gas();
            return false;
        }
        true
    }

    /// The result must be honoured: an Approval log fits inside the 2,300 gas the
    /// EIP-2200 sentry leaves behind, so a refused write could still emit its event.
    pub(crate) fn add_log(&mut self, topics: Vec<B256>, data: Vec<u8>) -> bool {
        if self.read_only {
            self.mark_write_protected();
            return false;
        }
        if !self.charge_log(topics.len(), data.len()) {
            return false;
        }
        self.frame.state.log(Log::new_unchecked(self.self_addr, topics, Bytes::from(data)));
        true
    }

    /// What the reference contract pays to route one entry of an announce bundle or
    /// an initCalls array: a warm CALL plus the copy of that entry into the callee's
    /// input. Without it a shared tail could be dispatched N×M times for the price
    /// of one.
    pub(crate) fn charge_internal_dispatch(&mut self, call: &[u8]) -> bool {
        self.frame.stats.internal_calls += 1;
        self.frame.stats.internal_call_bytes += call.len() as u64;
        let words = (call.len() as u64).div_ceil(32);
        self.charge_gas(WARM_STORAGE_READ_COST + words * CALLDATA_WORD_GAS)
    }

    pub(crate) fn charge_calldata(&mut self, input: &[u8]) -> bool {
        let words = (input.len() as u64).div_ceil(32);
        if words == 0 {
            return true;
        }
        self.charge_gas(words * CALLDATA_WORD_GAS)
    }

    pub(crate) fn charge_keccak(&mut self, size: usize) -> bool {
        let words = (size as u64).div_ceil(32);
        let paid = self.charge_gas(KECCAK256_GAS + KECCAK256_WORD_GAS * words);
        if paid {
            self.frame.stats.keccaks += 1;
        }
        paid
    }

    pub(crate) fn charge_log(&mut self, topics: usize, data_len: usize) -> bool {
        self.charge_gas(LOG_GAS + LOG_TOPIC_GAS * topics as u64 + LOG_DATA_GAS * data_len as u64)
    }

    /// Warms the account, in the interpreter's order, then charges for the state it
    /// was found in. Answers the code hash so a caller does not load twice.
    pub(crate) fn charge_account_access(&mut self, addr: Address) -> Option<B256> {
        if self.out_of_gas() {
            return None;
        }
        let load = self.frame.code_hash(addr)?;
        let cost = if load.is_cold { COLD_ACCOUNT_ACCESS_COST } else { WARM_STORAGE_READ_COST };
        if !self.charge_gas(cost) {
            return None;
        }
        Some(load.data)
    }

    /// The creation cost is owed even at a prefunded address: balance alone does not
    /// make an account a contract.
    pub(crate) fn charge_code_write(&mut self, addr: Address, code: &[u8]) -> bool {
        if self.read_only {
            self.mark_write_protected();
            return false;
        }
        let mut cost = CREATE_DATA_GAS * code.len() as u64;
        let Some(hash) = self.frame.code_hash(addr) else { return false };
        if had_no_code(hash.data) {
            cost += CREATE_GAS;
        }
        self.charge_gas(cost) && self.charge_keccak(code.len())
    }

    /// EIP-2200's reentrancy guard, which is what makes transfer()/send() safe: a
    /// CAS20 token writes state without SSTORE, so it applies the check itself.
    pub(crate) fn sstore_sentry(&self) -> bool {
        self.gas_left() > SSTORE_SENTRY_GAS
    }
}

/// No code at all is the condition under which writing code owes the
/// account-creation cost.
pub(crate) fn had_no_code(code_hash: B256) -> bool {
    code_hash == B256::ZERO || code_hash == alloy_primitives::KECCAK256_EMPTY
}
