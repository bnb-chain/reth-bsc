//! Bounded, parent-scoped proof hints. No speculative values enter the state-root stream.

use alloy_primitives::{
    keccak256,
    map::{B256Set, HashSet},
    B256,
};
use parking_lot::Mutex;
use reth_engine_tree::tree::state_root_strategy::{StateAccessHint, StateRootHintStream};
use revm::state::EvmState;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

const BATCH_TARGETS: usize = 64;
const MAX_TARGETS: usize = 512;
const MAX_BATCHES: usize = 8;
// Bound both scanning and hashing work for an individual speculative transaction.
const MAX_TRANSACTION_TARGETS: usize = 64;

pub(crate) struct ProofHints {
    parent: B256,
    closed: AtomicBool,
    stream: Option<StateRootHintStream>,
    state: Mutex<HintState>,
}

struct HintState {
    send: Option<Box<dyn Fn(StateAccessHint) + Send + Sync>>,
    accounts: B256Set,
    slots: HashSet<(B256, B256)>,
    pending: StateAccessHint,
    pending_targets: usize,
    batches: usize,
}

impl ProofHints {
    pub(crate) fn new(parent: B256, stream: StateRootHintStream) -> Arc<Self> {
        let sender = stream.clone();
        Self::build(parent, move |hint| sender.on_access_hint(hint), Some(stream))
    }

    #[cfg(test)]
    fn with_sender(
        parent: B256,
        send: impl Fn(StateAccessHint) + Send + Sync + 'static,
    ) -> Arc<Self> {
        Self::build(parent, send, None)
    }

    fn build(
        parent: B256,
        send: impl Fn(StateAccessHint) + Send + Sync + 'static,
        stream: Option<StateRootHintStream>,
    ) -> Arc<Self> {
        Arc::new(Self {
            parent,
            closed: AtomicBool::new(false),
            stream,
            state: Mutex::new(HintState {
                send: Some(Box::new(send)),
                accounts: Default::default(),
                slots: Default::default(),
                pending: Default::default(),
                pending_targets: 0,
                batches: 0,
            }),
        })
    }

    pub(crate) fn record(&self, parent: B256, state: &EvmState) {
        if self.closed.load(Ordering::Acquire) {
            return
        }
        if parent != self.parent {
            metrics::counter!("bsc.miner_prewarm.proof_hint_rejections_total", "reason" => "parent_mismatch").increment(1);
            return
        }
        {
            let state = self.state.lock();
            if state.send.is_none() || state.full() {
                return
            }
        }
        // Match Reth's prewarm target semantics: touched, non-selfdestructed accounts and
        // changed slots. Like geth's trie prefetcher, skip unchanged storage. Hash outside
        // the emission gate so finishing the miner never waits for Keccak or a provider.
        let mut hint = StateAccessHint::default();
        let mut targets = 0;
        for (address, account) in state.iter().take(MAX_TRANSACTION_TARGETS) {
            if targets == MAX_TRANSACTION_TARGETS {
                break
            }
            if !account.is_touched() || account.is_selfdestructed() {
                continue
            }
            let slots: Vec<_> = account
                .storage
                .iter()
                .take(MAX_TRANSACTION_TARGETS)
                .filter(|(_, value)| value.is_changed())
                // Reserve an account target alongside this account's slot targets.
                .take(MAX_TRANSACTION_TARGETS - targets - 1)
                .map(|(key, _)| keccak256(key.to_be_bytes::<32>()))
                .collect();
            if !account.is_changed() && slots.is_empty() {
                continue
            }
            let address = keccak256(address);
            hint.accounts.push(address);
            targets += 1 + slots.len();
            if !slots.is_empty() {
                hint.storages.insert(address, slots);
            }
        }
        if targets == 0 {
            return
        }
        self.admit(hint);
    }

    fn admit(&self, hint: StateAccessHint) {
        let mut state = self.state.lock();
        if self.closed.load(Ordering::Acquire) || state.send.is_none() {
            return
        }
        let mut duplicates = 0;
        let mut limited = 0;
        for account in hint.accounts {
            if state.accounts.contains(&account) {
                duplicates += 1;
                continue
            }
            if state.full() {
                limited += 1;
                continue
            }
            state.accounts.insert(account);
            state.pending.accounts.push(account);
            state.pending_targets += 1;
            state.flush_full_batch();
        }
        for (account, slots) in hint.storages {
            for slot in slots {
                if state.slots.contains(&(account, slot)) {
                    duplicates += 1;
                    continue
                }
                if state.full() {
                    limited += 1;
                    continue
                }
                state.slots.insert((account, slot));
                state.pending.storages.entry(account).or_default().push(slot);
                state.pending_targets += 1;
                state.flush_full_batch();
            }
        }
        drop(state);
        metrics::counter!("bsc.miner_prewarm.proof_hint_rejections_total", "reason" => "duplicate")
            .increment(duplicates);
        metrics::counter!("bsc.miner_prewarm.proof_hint_rejections_total", "reason" => "limit")
            .increment(limited);
    }

    /// Flush a partial batch when a worker exhausts the candidate iterator, not at a deadline.
    pub(crate) fn flush(&self) {
        if self.closed.load(Ordering::Acquire) {
            return
        }
        let mut state = self.state.lock();
        if !self.closed.load(Ordering::Acquire) {
            state.flush();
        }
    }

    /// Stops the independent lane without waiting for a speculative sender's mutex.
    /// The Reth close gate also rejects an already-in-flight submission and drops late results.
    pub(crate) fn close(&self) {
        self.closed.store(true, Ordering::Release);
        if let Some(stream) = &self.stream {
            stream.close();
        }
        if let Some(mut state) = self.state.try_lock() {
            state.send = None;
            state.pending = StateAccessHint::default();
            state.pending_targets = 0;
        }
    }

    #[cfg(test)]
    pub(super) fn captured(parent: B256) -> (Arc<Self>, Arc<Mutex<Vec<StateAccessHint>>>) {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let output = sent.clone();
        (Self::with_sender(parent, move |hint| output.lock().push(hint)), sent)
    }
}

impl HintState {
    fn full(&self) -> bool {
        self.accounts.len() + self.slots.len() == MAX_TARGETS || self.batches == MAX_BATCHES
    }

    fn flush_full_batch(&mut self) {
        if self.pending_targets == BATCH_TARGETS {
            self.flush();
        }
    }

    fn flush(&mut self) {
        if self.pending_targets == 0 || self.batches == MAX_BATCHES {
            return
        }
        let Some(send) = &self.send else { return };
        let hint = std::mem::take(&mut self.pending);
        self.pending_targets = 0;
        self.batches += 1;
        if self.full() {
            metrics::counter!("bsc.miner_prewarm.proof_hint_budget_exhausted_total",
                "reason" => if self.accounts.len() + self.slots.len() == MAX_TARGETS { "targets" } else { "batches" })
                .increment(1);
        }
        metrics::counter!("bsc.miner_prewarm.proof_hint_batches_total").increment(1);
        metrics::counter!("bsc.miner_prewarm.proof_hint_targets_total", "resource" => "account")
            .increment(hint.accounts.len() as u64);
        metrics::counter!("bsc.miner_prewarm.proof_hint_targets_total", "resource" => "storage")
            .increment(hint.storages.values().map(Vec::len).sum::<usize>() as u64);
        // Best effort: the Reth hint capability does not acknowledge delivery or proof use.
        send(hint);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, U256};
    use revm::state::{Account, AccountInfo, EvmStorageSlot, TransactionId};

    fn changed_state() -> EvmState {
        let mut account = Account::from(AccountInfo::default());
        account.mark_touch();
        account.storage.insert(
            U256::ZERO,
            EvmStorageSlot::new_changed(U256::from(3), U256::from(7), TransactionId::ZERO),
        );
        account
            .storage
            .insert(U256::from(1), EvmStorageSlot::new(U256::from(9), TransactionId::ZERO));
        [(Address::repeat_byte(1), account)].into_iter().collect()
    }

    #[test]
    fn emits_only_hashed_changed_paths_not_speculative_values_or_read_only_slots() {
        let parent = B256::repeat_byte(1);
        let (hints, sent) = ProofHints::captured(parent);
        let mut state = changed_state();
        // A merely touched but unchanged account has no update path to prefetch.
        let mut read_only = Account::from(AccountInfo::default());
        read_only.mark_touch();
        state.insert(Address::repeat_byte(2), read_only);
        let mut destroyed = Account::from(AccountInfo { nonce: 1, ..Default::default() });
        destroyed.mark_touch();
        destroyed.mark_selfdestruct();
        state.insert(Address::repeat_byte(3), destroyed);
        hints.record(parent, &state);
        hints.flush();
        let output = sent.lock();
        assert_eq!(output.len(), 1);
        let address = keccak256(Address::repeat_byte(1));
        assert_eq!(output[0].accounts, vec![address]);
        assert_eq!(output[0].storages.get(&address), Some(&vec![keccak256(B256::ZERO)]));
        assert_eq!(output[0].storages.len(), 1);
        assert_eq!(
            state[&Address::repeat_byte(1)].storage[&U256::ZERO].present_value,
            U256::from(7)
        );
    }

    #[test]
    fn deduplicates_targets_across_parallel_workers() {
        let parent = B256::repeat_byte(1);
        let (hints, sent) = ProofHints::captured(parent);
        std::thread::scope(|scope| {
            for _ in 0..3 {
                let hints = hints.clone();
                scope.spawn(move || {
                    for _ in 0..100 {
                        hints.record(parent, &changed_state());
                    }
                    hints.flush();
                });
            }
        });
        let output = sent.lock();
        assert_eq!(output.iter().map(|hint| hint.accounts.len()).sum::<usize>(), 1);
        assert_eq!(
            output.iter().flat_map(|hint| hint.storages.values()).map(Vec::len).sum::<usize>(),
            1
        );
    }

    #[test]
    fn wrong_parent_and_closed_attempts_cannot_emit_pending_or_late_hints() {
        let parent = B256::repeat_byte(1);
        let (hints, sent) = ProofHints::captured(parent);
        hints.record(B256::repeat_byte(2), &changed_state());
        hints.flush();
        assert!(sent.lock().is_empty());
        hints.record(parent, &changed_state());
        hints.close();
        hints.flush();
        hints.record(parent, &changed_state());
        hints.flush();
        assert!(sent.lock().is_empty());
    }

    #[test]
    fn target_and_message_budgets_bound_background_work() {
        let parent = B256::ZERO;
        let (hints, sent) = ProofHints::captured(parent);
        hints.admit(StateAccessHint {
            accounts: (0..MAX_TARGETS + 100)
                .map(|i| B256::from(U256::from(i).to_be_bytes::<32>()))
                .collect(),
            ..Default::default()
        });
        hints.flush();
        let output = sent.lock();
        assert_eq!(output.len(), MAX_BATCHES);
        assert!(output.iter().all(|hint| hint.accounts.len() == BATCH_TARGETS));
        assert_eq!(output.iter().map(|hint| hint.accounts.len()).sum::<usize>(), MAX_TARGETS);
        drop(output);
        let (hints, sent) = ProofHints::captured(parent);
        for i in 0..MAX_BATCHES + 100 {
            hints.admit(StateAccessHint {
                accounts: vec![B256::from(U256::from(i).to_be_bytes::<32>())],
                ..Default::default()
            });
            hints.flush();
        }
        assert_eq!(sent.lock().len(), MAX_BATCHES);
    }

    #[test]
    fn very_large_transactions_have_bounded_hint_extraction() {
        let parent = B256::ZERO;
        let (hints, sent) = ProofHints::captured(parent);
        let mut state = changed_state();
        let account = state.get_mut(&Address::repeat_byte(1)).unwrap();
        for slot in 2..1000 {
            account.storage.insert(
                U256::from(slot),
                EvmStorageSlot::new_changed(U256::ZERO, U256::from(1), TransactionId::ZERO),
            );
        }
        hints.record(parent, &state);
        hints.flush();
        let output = sent.lock();
        let targets = output
            .iter()
            .map(|hint| hint.accounts.len() + hint.storages.values().map(Vec::len).sum::<usize>())
            .sum::<usize>();
        assert!(targets > 1 && targets <= MAX_TRANSACTION_TARGETS);
    }

    #[test]
    fn close_does_not_wait_for_an_inflight_send_and_stops_future_sends() {
        use std::{sync::mpsc, time::Duration};
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let release_rx = Mutex::new(release_rx);
        let hints = ProofHints::with_sender(B256::ZERO, move |_| {
            started_tx.send(()).unwrap();
            release_rx.lock().recv_timeout(Duration::from_secs(5)).unwrap();
        });
        let sender = hints.clone();
        let send = std::thread::spawn(move || {
            sender.record(B256::ZERO, &changed_state());
            sender.flush();
        });
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let closer = hints.clone();
        let (closing_tx, closing_rx) = mpsc::channel();
        let close = std::thread::spawn(move || {
            closer.close();
            closing_tx.send(()).unwrap();
        });
        // A blocked sender still holds the mutex. Closing must return before we release it.
        closing_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        release_tx.send(()).unwrap();
        send.join().unwrap();
        close.join().unwrap();
        drop(release_tx);
        // A second emission would notify started_rx and fail on the disconnected release_rx.
        hints.record(B256::ZERO, &changed_state());
        hints.flush();
        assert!(started_rx.try_recv().is_err());
    }
}
