//! Diagnostic shim that counts what the sparse-trie state-root task actually receives.
//!
//! The producer-side probe in [`crate::node::evm::builder`] can tell us that the
//! sparse-trie root disagrees with a serial recomputation, but not *why*. Local runs
//! showed the sparse `TrieUpdates` missing the storage trie for the BSC ValidatorSet
//! contract on blocks whose only state change is the post-execution system call. Static
//! tracing of the path (hook installed on the DB before pre-execution, system txs
//! committed through `evm.db_mut().commit`, revm's `State::commit` invoking the hook)
//! found no defect, so the remaining question is empirical: did those updates ever reach
//! the task?
//!
//! Wrapping the real hook with [`CountingStateHook`] answers it. Two candidate
//! mechanisms are distinguished by the counts:
//!
//! * the ValidatorSet account never arrives -> the updates are lost on the way, e.g. the
//!   silent `let _ = self.sender.send(..)` in reth's `SparseTrieStateRootSink`
//! * it arrives but the returned updates still lack its storage trie -> the fault is
//!   inside the task's ingestion
//!
//! Diagnostic only, active solely when `BSC_VERIFY_SEALED_STATE_ROOT=1` installs it.

use alloy_primitives::{address, keccak256, Address, B256};
use reth_engine_tree::tree::evm_state_to_hashed_post_state;
use reth_evm::OnStateHook;
use reth_trie_common::HashedPostState;
use revm::state::EvmState;
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
};

/// BSC ValidatorSet, written by the post-execution system call in every block. This is the
/// account whose storage trie went missing in the local reproduction.
pub const VALIDATOR_SET: Address = address!("0000000000000000000000000000000000001000");

/// Tallies of what the state hook forwarded, shared with the builder so the mismatch log
/// can report them next to the sparse-vs-serial roots.
#[derive(Debug, Default)]
pub struct StateHookCounts {
    /// Number of `on_state` invocations.
    pub calls: AtomicU64,
    /// Total accounts across all invocations (with repeats).
    pub accounts: AtomicU64,
    /// Total storage slots across all invocations (with repeats).
    pub storage_slots: AtomicU64,
    /// Invocations that carried [`VALIDATOR_SET`].
    pub validator_set_hits: AtomicU64,
    /// Total storage slots seen for [`VALIDATOR_SET`].
    pub validator_set_slots: AtomicU64,
    /// Of those, the slots reth's `evm_state_to_hashed_post_state` would keep -- it filters
    /// on `EvmStorageSlot::is_changed()`, so slots that are merely read are discarded.
    pub validator_set_changed_slots: AtomicU64,
    /// Invocations where [`VALIDATOR_SET`] arrived without its touched flag. That filter runs
    /// before the per-slot one and drops the whole account.
    pub validator_set_untouched: AtomicU64,
    /// `(slot, original_value, present_value)` as streamed for [`VALIDATOR_SET`], capped.
    /// Comparing these against the bundle's entry for the same account shows why the two
    /// paths disagree about whether the slot changed.
    pub validator_set_samples: Mutex<Vec<(String, String, String)>>,
    /// The `HashedPostState` the sparse-trie task accumulates, rebuilt here with reth's own
    /// `evm_state_to_hashed_post_state` so the semantics match exactly. Diffing this against
    /// `state.hashed_post_state(&db.bundle_state)` at mismatch time is the comparison that
    /// actually matters -- an earlier probe compared `TrieUpdates` instead and produced a
    /// misleading answer.
    pub streamed: Mutex<HashedPostState>,
    /// Per-account record of what the hook saw, keyed by hashed address, so a bundle entry
    /// missing from the stream can be classified: never delivered at all, or delivered and
    /// then discarded by one of the filters in `evm_state_to_hashed_post_state`.
    pub seen: Mutex<HashMap<B256, SeenAccount>>,
}

/// What the hook observed for one account, and how the conversion's filters would treat it.
#[derive(Debug, Default, Clone)]
pub struct SeenAccount {
    /// Number of invocations carrying this account.
    pub hits: u64,
    /// Whether the last sighting had the touched flag. Untouched accounts are dropped whole.
    pub touched: bool,
    /// Whether `info` equalled `AccountInfo::default()`.
    pub info_default: bool,
    /// Whether `original_info` was unset. When it is, `original_info()` yields the default,
    /// so the `info != original_info()` guard degenerates into "info is non-empty".
    pub original_none: bool,
    /// Whether the guard `info != original_info()` would have rejected the account.
    pub filtered_by_info_guard: bool,
}

impl StateHookCounts {
    /// Diffs the accumulated streamed state against the authoritative bundle state,
    /// reporting entries missing from the stream and entries whose values disagree.
    pub fn diff_against(&self, bundle: &HashedPostState) -> String {
        let Ok(streamed) = self.streamed.lock() else { return "poisoned".to_string() };
        let mut acct_missing = 0usize;
        let mut acct_differs = 0usize;
        let mut examples: Vec<String> = Vec::new();
        for (addr, want) in &bundle.accounts {
            match streamed.accounts.get(addr) {
                None => {
                    acct_missing += 1;
                    if examples.len() < 6 {
                        let why = match self.seen.lock().ok().and_then(|m| m.get(addr).cloned()) {
                            None => "never-delivered".to_string(),
                            Some(r) => format!(
                                "hits={} touched={} info_default={} orig_none={} info_guard_rejected={}",
                                r.hits, r.touched, r.info_default, r.original_none,
                                r.filtered_by_info_guard
                            ),
                        };
                        examples.push(format!("acct-missing:{addr:#x}({why})"));
                    }
                }
                Some(got) if got != want => {
                    acct_differs += 1;
                    if examples.len() < 6 {
                        examples.push(format!("acct-differs:{addr:#x}"));
                    }
                }
                Some(_) => {}
            }
        }
        let mut stor_missing = 0usize;
        let mut slot_differs = 0usize;
        for (addr, want) in &bundle.storages {
            match streamed.storages.get(addr) {
                None => {
                    stor_missing += 1;
                    if examples.len() < 6 {
                        examples.push(format!("stor-missing:{addr:#x}"));
                    }
                }
                Some(got) => {
                    for (slot, v) in &want.storage {
                        if got.storage.get(slot) != Some(v) {
                            slot_differs += 1;
                            if examples.len() < 6 {
                                examples.push(format!("slot-differs:{addr:#x}/{slot:#x}"));
                            }
                        }
                    }
                }
            }
        }
        format!(
            "streamed_accounts={} streamed_storages={} bundle_accounts={} bundle_storages={} \
             acct_missing={acct_missing} acct_differs={acct_differs} \
             stor_missing={stor_missing} slot_differs={slot_differs} [{}]",
            streamed.accounts.len(),
            streamed.storages.len(),
            bundle.accounts.len(),
            bundle.storages.len(),
            examples.join(","),
        )
    }

    /// Renders the streamed `(slot, original, present)` samples for a log line.
    pub fn samples(&self) -> String {
        match self.validator_set_samples.lock() {
            Ok(s) => s
                .iter()
                .map(|(k, o, p)| format!("{k}:{o}->{p}"))
                .collect::<Vec<_>>()
                .join(","),
            Err(_) => "poisoned".to_string(),
        }
    }

    /// Renders the tallies for a log line.
    pub fn snapshot(&self) -> String {
        format!(
            "calls={} accounts={} slots={} vs_hits={} vs_slots={} vs_changed_slots={} \
             vs_untouched={}",
            self.calls.load(Ordering::Relaxed),
            self.accounts.load(Ordering::Relaxed),
            self.storage_slots.load(Ordering::Relaxed),
            self.validator_set_hits.load(Ordering::Relaxed),
            self.validator_set_slots.load(Ordering::Relaxed),
            self.validator_set_changed_slots.load(Ordering::Relaxed),
            self.validator_set_untouched.load(Ordering::Relaxed),
        )
    }
}

/// Wraps the sparse-trie state hook and counts everything passing through it.
///
/// Forwards every update unchanged, so installing it cannot alter the computed root --
/// it only observes. Drop order is preserved too: dropping this drops `inner`, which is
/// what signals end-of-updates to the task.
pub struct CountingStateHook {
    inner: Box<dyn OnStateHook>,
    counts: Arc<StateHookCounts>,
}

impl CountingStateHook {
    /// Wraps `inner`, reporting into `counts`.
    pub fn new(inner: Box<dyn OnStateHook>, counts: Arc<StateHookCounts>) -> Self {
        Self { inner, counts }
    }
}

impl std::fmt::Debug for CountingStateHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CountingStateHook").field("counts", &self.counts).finish_non_exhaustive()
    }
}

impl OnStateHook for CountingStateHook {
    fn on_state(&mut self, state: EvmState) {
        if let Ok(mut streamed) = self.counts.streamed.lock() {
            streamed.extend(evm_state_to_hashed_post_state(state.clone()));
        }
        if let Ok(mut seen) = self.counts.seen.lock() {
            for (address, account) in state.iter() {
                let e = seen.entry(keccak256(address)).or_default();
                e.hits += 1;
                e.touched = account.is_touched();
                // `is_default` and the `original_info` field are private, so compare against
                // a constructed default and infer "unset" from `original_info()` returning it.
                let default_info = revm::state::AccountInfo::default();
                e.info_default = account.info == default_info;
                e.original_none = account.original_info() == default_info;
                e.filtered_by_info_guard = account.info == account.original_info();
            }
        }
        let slots: usize = state.values().map(|a| a.storage.len()).sum();
        self.counts.calls.fetch_add(1, Ordering::Relaxed);
        self.counts.accounts.fetch_add(state.len() as u64, Ordering::Relaxed);
        self.counts.storage_slots.fetch_add(slots as u64, Ordering::Relaxed);
        if let Some(account) = state.get(&VALIDATOR_SET) {
            self.counts.validator_set_hits.fetch_add(1, Ordering::Relaxed);
            self.counts
                .validator_set_slots
                .fetch_add(account.storage.len() as u64, Ordering::Relaxed);
            if let Ok(mut samples) = self.counts.validator_set_samples.lock() {
                for (slot, value) in account.storage.iter().take(8) {
                    samples.push((
                        format!("{slot:#x}"),
                        format!("{:#x}", value.original_value()),
                        format!("{:#x}", value.present_value()),
                    ));
                }
            }
            let changed = account.storage.values().filter(|v| v.is_changed()).count();
            self.counts.validator_set_changed_slots.fetch_add(changed as u64, Ordering::Relaxed);
            if !account.is_touched() {
                self.counts.validator_set_untouched.fetch_add(1, Ordering::Relaxed);
            }
        }
        self.inner.on_state(state);
    }
}
