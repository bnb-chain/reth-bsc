//! A host for driving the CAS20 family without an EVM: a state with the
//! access-list semantics of one transaction, checkpoints that undo a failed call
//! the way a frame revert does, and a runner that closes a call the way the entry
//! point does. Shared by the behavioural tests and the golden replay.

use super::{
    activation::{act_slot, SLOT_ADMIN, SLOT_FEATURES},
    ctx::{Cas20State, Ctx, Frame},
    errors::{complete, Outcome},
    execute, resolve,
    sigs::{FEATURE_ASSET, FEATURE_POLICY_REGISTRY, FEATURE_STABLECOIN, MARKER_CODE_HASH},
    storage::mapping_slot,
    CallStats, ACTIVATION_REGISTRY_ADDRESS, POLICY_REGISTRY_ADDRESS,
};
use alloy_primitives::{Address, Log, B256, KECCAK256_EMPTY, U256};
use revm::{
    bytecode::Bytecode,
    interpreter::{SStoreResult, StateLoad},
};
use std::collections::{HashMap, HashSet};

/// The mutable part of the host, snapshotted by a checkpoint.
#[derive(Clone, Default)]
struct Snapshot {
    storage: HashMap<(Address, U256), U256>,
    warm_slots: HashSet<(Address, U256)>,
    warm_addrs: HashSet<Address>,
    code_hash: HashMap<Address, B256>,
    logs: Vec<Log>,
}

/// A slot or account is cold until its first touch; original values are those at
/// the last transaction boundary.
#[derive(Clone, Default)]
pub(crate) struct MockHost {
    now: Snapshot,
    original: HashMap<(Address, U256), U256>,
    checkpoints: Vec<Snapshot>,
    pub(crate) time: u64,
    pub(crate) chain_id: u64,
}

impl MockHost {
    pub(crate) fn new(chain_id: u64, time: u64) -> Self {
        Self { chain_id, time, ..Default::default() }
    }

    /// What the fork plants: the two registries' account sentinels.
    pub(crate) fn seed_sentinels(&mut self) {
        for a in [ACTIVATION_REGISTRY_ADDRESS, POLICY_REGISTRY_ADDRESS] {
            self.now.code_hash.insert(a, MARKER_CODE_HASH);
        }
    }

    /// What the fork leaves to governance: an activation admin, and every feature
    /// open (BEP-702 3.15). Written as uncommitted state, as go-bsc's harness does.
    pub(crate) fn seed_activation(&mut self, admin: Address) {
        self.seed_sentinels();
        self.set(
            ACTIVATION_REGISTRY_ADDRESS,
            act_slot(SLOT_ADMIN),
            U256::from_be_bytes(admin.into_word().0),
        );
        for f in [FEATURE_ASSET, FEATURE_STABLECOIN, FEATURE_POLICY_REGISTRY] {
            self.set(
                ACTIVATION_REGISTRY_ADDRESS,
                mapping_slot(act_slot(SLOT_FEATURES), f),
                U256::from(1),
            );
        }
    }

    pub(crate) fn get(&self, at: Address, slot: U256) -> U256 {
        self.now.storage.get(&(at, slot)).copied().unwrap_or_default()
    }

    pub(crate) fn set(&mut self, at: Address, slot: U256, value: U256) {
        self.now.storage.insert((at, slot), value);
    }

    pub(crate) fn code_hash_of(&self, at: Address) -> Option<B256> {
        self.now.code_hash.get(&at).copied()
    }

    /// Every account the host knows code for.
    pub(crate) fn coded_accounts(&self) -> impl Iterator<Item = Address> + '_ {
        self.now.code_hash.keys().copied()
    }

    /// Every non-zero slot, by account.
    pub(crate) fn storage(&self) -> impl Iterator<Item = (Address, U256, U256)> + '_ {
        self.now.storage.iter().filter(|(_, v)| !v.is_zero()).map(|(&(a, k), &v)| (a, k, v))
    }

    /// A transaction boundary: what was written is now committed, and nothing is warm.
    pub(crate) fn finalize(&mut self) {
        self.original = self.now.storage.clone();
        self.now.warm_slots.clear();
        self.now.warm_addrs.clear();
    }

    pub(crate) fn checkpoint(&mut self) {
        self.checkpoints.push(self.now.clone());
    }

    pub(crate) fn commit(&mut self) {
        self.checkpoints.pop().expect("a checkpoint to commit");
    }

    /// Undoes everything since the checkpoint: storage, code, logs and warmth.
    pub(crate) fn revert(&mut self) {
        self.now = self.checkpoints.pop().expect("a checkpoint to revert to");
    }
}

impl Cas20State for MockHost {
    // Touching a slot warms its account too, as the journal and go-bsc's access
    // list both do.
    fn sload(&mut self, address: Address, key: U256) -> Result<StateLoad<U256>, String> {
        self.now.warm_addrs.insert(address);
        let is_cold = self.now.warm_slots.insert((address, key));
        Ok(StateLoad::new(self.get(address, key), is_cold))
    }

    fn sstore(
        &mut self,
        address: Address,
        key: U256,
        value: U256,
    ) -> Result<StateLoad<SStoreResult>, String> {
        self.now.warm_addrs.insert(address);
        let is_cold = self.now.warm_slots.insert((address, key));
        let original_value = self.original.get(&(address, key)).copied().unwrap_or_default();
        let present_value = self.get(address, key);
        self.set(address, key, value);
        Ok(StateLoad::new(
            SStoreResult { original_value, present_value, new_value: value },
            is_cold,
        ))
    }

    fn code_hash(&mut self, address: Address) -> Result<StateLoad<B256>, String> {
        let is_cold = self.now.warm_addrs.insert(address);
        Ok(StateLoad::new(self.code_hash_of(address).unwrap_or(KECCAK256_EMPTY), is_cold))
    }

    fn set_code(&mut self, address: Address, code: Bytecode) -> Result<(), String> {
        self.now.warm_addrs.insert(address);
        self.now.code_hash.insert(address, code.hash_slow());
        Ok(())
    }

    fn log(&mut self, log: Log) {
        self.now.logs.push(log);
    }

    fn block_timestamp(&self) -> u64 {
        self.time
    }

    fn chain_id(&self) -> u64 {
        self.chain_id
    }
}

/// The shape of a CALL, STATICCALL or DELEGATECALL into the family.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CallSpec {
    pub caller: Address,
    pub to: Address,
    pub gas: u64,
    pub is_static: bool,
    pub direct: bool,
    pub value: U256,
}

/// What a call handed back, closed the way the entry point closes it.
#[derive(Clone, Debug)]
pub(crate) struct CallResult {
    pub outcome: Outcome,
    pub used: u64,
    pub refund: i64,
    pub logs: Vec<Log>,
    pub stats: CallStats,
}

/// Runs one call against the host, as a frame would: a failed call leaves no
/// trace, a successful one keeps its writes, logs and warmth. The callee is not
/// warmed on entry — that is the CALL opcode's or the transaction's doing, and
/// go-bsc's `evm.Call` harness, which the golden trace was recorded with, does
/// not do it either.
pub(crate) fn run_call(host: &mut MockHost, spec: CallSpec, input: &[u8]) -> CallResult {
    let kind = resolve(spec.to).expect("a routed address");
    host.checkpoint();
    let log_start = host.now.logs.len();
    let (outcome, used, refund, stats) = {
        let mut frame = Frame::new(host, spec.gas);
        let ctx = Ctx {
            frame: &mut frame,
            self_addr: spec.to,
            caller: spec.caller,
            read_only: spec.is_static,
            direct_call: spec.direct,
            value: spec.value,
            admin_renounced: false,
        };
        let result = execute(kind, ctx, input);
        let (outcome, used, refund) = complete(&mut frame, result);
        (outcome, used, refund, frame.stats)
    };
    let logs = if matches!(outcome, Outcome::Return(_)) {
        let logs = host.now.logs[log_start..].to_vec();
        host.commit();
        logs
    } else {
        host.revert();
        Vec::new()
    };
    CallResult { outcome, used, refund, logs, stats }
}
