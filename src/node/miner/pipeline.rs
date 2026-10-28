//! One unpublished child of an exact, private parent candidate. Never advances canonical state.
use crate::node::engine::BscBuiltPayload;
use alloy_primitives::{Address, B256};
use parking_lot::Mutex;
use reth_tasks::ManualCancel;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, LazyLock,
};
use tokio::sync::{oneshot, OwnedSemaphorePermit, Semaphore};

type BuildResult = Result<BscBuiltPayload, String>;
type Reservation = (u64, ManualCancel, oneshot::Sender<BuildResult>, OwnedSemaphorePermit);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Key {
    pub parent: B256,
    pub timestamp_ms: u64,
    pub beneficiary: Address,
    pub gas_limit: u64,
    pub min_gas_tip: u128,
}
struct Entry {
    id: u64,
    base: B256,
    key: Key,
    cancel: ManualCancel,
    receiver: oneshot::Receiver<BuildResult>,
}
#[derive(Default)]
struct State {
    entry: Option<Entry>,
    head: Option<B256>,
}
struct Coordinator {
    state: Mutex<State>,
    worker: Arc<Semaphore>,
    next_id: AtomicU64,
}
impl Default for Coordinator {
    fn default() -> Self {
        Self {
            state: Mutex::default(),
            worker: Arc::new(Semaphore::new(1)),
            next_id: AtomicU64::new(1),
        }
    }
}
static COORDINATOR: LazyLock<Coordinator> = LazyLock::new(Coordinator::default);

type Task = std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>;
static EXECUTOR: std::sync::OnceLock<std::sync::mpsc::SyncSender<Task>> =
    std::sync::OnceLock::new();

/// Allocate a persistent execution thread at node setup, away from the canonical
/// Tokio worker queue. The runtime handle is used only to drive async timers/receivers.
fn executor(handle: tokio::runtime::Handle) -> std::io::Result<std::sync::mpsc::SyncSender<Task>> {
    let (sender, receiver) = std::sync::mpsc::sync_channel::<Task>(1);
    std::thread::Builder::new().name("miner-pipeline-execution".into()).spawn(move || {
        while let Ok(task) = receiver.recv() {
            handle.block_on(task);
        }
    })?;
    Ok(sender)
}
pub(crate) fn configure(handle: tokio::runtime::Handle) -> std::io::Result<()> {
    if !enabled() || EXECUTOR.get().is_some() {
        return Ok(())
    }
    let _ = EXECUTOR.set(executor(handle)?);
    Ok(())
}
pub(crate) fn launch(task: impl std::future::Future<Output = ()> + Send + 'static) -> bool {
    EXECUTOR.get().is_some_and(|executor| executor.try_send(Box::pin(task)).is_ok())
}

pub(crate) fn enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("BSC_MINING_PIPELINED_BUILD").is_ok_and(|s| s == "1"))
}
impl Coordinator {
    /// The permit bounds running work, even after its entry was invalidated.
    fn reserve(&self, key: Key, base: B256) -> Option<Reservation> {
        let permit = self.worker.clone().try_acquire_owned().ok()?;
        let mut state = self.state.lock();
        if state.entry.is_some() || state.head != Some(base) {
            return None
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let cancel = ManualCancel::default();
        let (sender, receiver) = oneshot::channel();
        state.entry = Some(Entry { id, base, key, cancel: cancel.clone(), receiver });
        Some((id, cancel, sender, permit))
    }
    fn canonical_head(&self, head: B256) {
        let retired = {
            let mut state = self.state.lock();
            state.head = Some(head);
            if state.entry.as_ref().is_some_and(|e| head != e.base && head != e.key.parent) {
                state.entry.take()
            } else {
                None
            }
        };
        if let Some(entry) = retired {
            entry.cancel.cancel();
            event("parent_changed");
        }
    }
    fn selected_parent(&self, parent: Option<B256>) {
        let retired = {
            let mut state = self.state.lock();
            if state.entry.as_ref().is_some_and(|e| Some(e.key.parent) != parent) {
                state.entry.take()
            } else {
                None
            }
        };
        if let Some(entry) = retired {
            entry.cancel.cancel();
            event("candidate_changed");
        }
    }
    fn failed(&self, id: u64) {
        let retired = {
            let mut state = self.state.lock();
            if state.entry.as_ref().is_some_and(|e| e.id == id) {
                state.entry.take()
            } else {
                None
            }
        };
        // A ready payload can be large. Drop it after releasing the coordination lock.
        drop(retired);
    }
    fn take(&self, key: &Key) -> Option<Adoption> {
        let retired = {
            let mut state = self.state.lock();
            // A private parent must first become canonical; a matching hash alone is insufficient.
            if state.head != Some(key.parent) {
                return None
            }
            state.entry.take()?
        };
        if retired.key == *key {
            event("adopted");
            Some(Adoption { receiver: retired.receiver, cancel: retired.cancel })
        } else {
            retired.cancel.cancel();
            event("parameters_changed");
            None
        }
    }
}
pub(crate) fn can_start(base: B256) -> bool {
    if !enabled() || COORDINATOR.worker.available_permits() == 0 {
        return false
    }
    let state = COORDINATOR.state.lock();
    state.entry.is_none() && state.head == Some(base)
}
pub(crate) fn reserve(key: Key, base: B256) -> Option<Reservation> {
    if !enabled() {
        return None
    }
    COORDINATOR.reserve(key, base)
}
pub(crate) fn canonical_head(head: B256) {
    if enabled() {
        COORDINATOR.canonical_head(head);
    }
}
/// Retain only the parent actually selected for publication. A late bid/reseal invalidates work.
pub(crate) fn selected_parent(parent: Option<B256>) {
    if enabled() {
        COORDINATOR.selected_parent(parent);
    }
}
pub(crate) fn failed(id: u64) {
    COORDINATOR.failed(id);
}

/// Adopts the same running computation, before taking the ordinary builder's build permit.
pub(crate) struct Adoption {
    pub receiver: oneshot::Receiver<BuildResult>,
    cancel: ManualCancel,
}
impl Drop for Adoption {
    fn drop(&mut self) {
        self.cancel.clone().cancel();
    }
}
pub(crate) fn take(key: &Key) -> Option<Adoption> {
    if !enabled() {
        return None
    }
    COORDINATOR.take(key)
}
pub(crate) fn event(outcome: &'static str) {
    metrics::counter!("bsc_miner_pipeline_total", "outcome" => outcome).increment(1);
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key() -> Key {
        Key {
            parent: B256::with_last_byte(2),
            timestamp_ms: 450,
            beneficiary: Address::ZERO,
            gas_limit: 200_000_000,
            min_gas_tip: 0,
        }
    }
    fn coordinator() -> Coordinator {
        let c = Coordinator::default();
        c.canonical_head(B256::with_last_byte(1));
        c
    }
    fn reserve(c: &Coordinator) -> Reservation {
        c.reserve(key(), B256::with_last_byte(1)).unwrap()
    }
    #[tokio::test]
    async fn execution_uses_its_own_thread_and_async_timers_remain_live() {
        let main = std::thread::current().id();
        let sender = executor(tokio::runtime::Handle::current()).unwrap();
        let (done, receiver) = oneshot::channel();
        assert!(sender
            .try_send(Box::pin(async move {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                let _ = done.send(std::thread::current().id());
            }))
            .is_ok());
        let worker = tokio::time::timeout(std::time::Duration::from_secs(2), receiver)
            .await
            .unwrap()
            .unwrap();
        assert_ne!(worker, main);
    }
    #[test]
    fn cannot_adopt_before_private_parent_is_canonical() {
        let c = coordinator();
        let (_, cancel, _, _permit) = reserve(&c);
        assert!(c.take(&key()).is_none());
        assert!(!cancel.is_cancelled());
        c.canonical_head(key().parent);
        let adopted = c.take(&key()).unwrap();
        assert!(!cancel.is_cancelled());
        drop(adopted);
        assert!(cancel.is_cancelled());
    }
    #[test]
    fn replaced_bid_and_same_height_fork_cancel_child() {
        for head_change in [true, false] {
            let c = coordinator();
            let (_, cancel, _, _permit) = reserve(&c);
            let replacement = B256::with_last_byte(3);
            if head_change {
                c.canonical_head(replacement);
            } else {
                c.selected_parent(Some(replacement));
            }
            assert!(cancel.is_cancelled());
            assert!(c.state.lock().entry.is_none());
        }
    }
    #[test]
    fn invalidation_does_not_allow_overlapping_background_work() {
        let c = coordinator();
        let (_, cancel, _, permit) = reserve(&c);
        assert!(c.reserve(key(), B256::with_last_byte(1)).is_none());
        c.selected_parent(None);
        assert!(cancel.is_cancelled());
        assert!(c.reserve(key(), B256::with_last_byte(1)).is_none());
        drop(permit);
        assert!(c.reserve(key(), B256::with_last_byte(1)).is_some());
    }
    #[test]
    fn changed_time_beneficiary_or_limits_fall_back_and_cancel() {
        for field in 0..4 {
            let c = coordinator();
            let (_, cancel, _, _permit) = reserve(&c);
            c.canonical_head(key().parent);
            let mut changed = key();
            match field {
                0 => changed.timestamp_ms += 1,
                1 => changed.gas_limit += 1,
                2 => changed.min_gas_tip += 1,
                _ => changed.beneficiary = Address::with_last_byte(1),
            }
            assert!(c.take(&changed).is_none());
            assert!(cancel.is_cancelled());
        }
    }
    #[tokio::test]
    async fn producer_failure_is_delivered_without_restarting_work() {
        let c = coordinator();
        let (_, cancel, sender, _permit) = reserve(&c);
        c.canonical_head(key().parent);
        let mut adopted = c.take(&key()).unwrap();
        assert!(sender.send(Err("failed".into())).is_ok());
        assert_eq!((&mut adopted.receiver).await.as_ref().unwrap().as_ref().unwrap_err(), "failed");
        assert!(!cancel.is_cancelled());
    }
    #[test]
    fn delayed_failure_cannot_remove_a_new_entry() {
        let c = coordinator();
        let (id, _, _, permit) = reserve(&c);
        c.failed(id);
        drop(permit);
        let (new_id, _, _, _permit) = reserve(&c);
        c.failed(id);
        assert_eq!(c.state.lock().entry.as_ref().unwrap().id, new_id);
    }
}
