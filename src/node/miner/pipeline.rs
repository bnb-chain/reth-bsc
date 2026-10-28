//! A bounded linear private pipeline. Never advances canonical state.
use crate::node::engine::BscBuiltPayload;
use alloy_primitives::{Address, B256};
use parking_lot::Mutex;
use reth_tasks::ManualCancel;
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, LazyLock,
    },
};
use tokio::sync::{oneshot, OwnedSemaphorePermit, Semaphore, SemaphorePermit};

type BuildResult = Result<BscBuiltPayload, String>;
type WorkerReservation = (u64, ManualCancel, oneshot::Sender<BuildResult>, OwnedSemaphorePermit);

/// Held by the actual computation, not the cancellable coordinator entry. A fork
/// can invalidate the result immediately, but cannot admit a second live stream.
pub(crate) struct BuildLease<'a> {
    _worker: OwnedSemaphorePermit,
    _local: SemaphorePermit<'a>,
}
type Reservation<'a> = (u64, ManualCancel, oneshot::Sender<BuildResult>, BuildLease<'a>);

fn reserve_on<'a>(
    coordinator: &Coordinator,
    gate: &'a Semaphore,
    key: Key,
    base: B256,
) -> Option<Reservation<'a>> {
    // Opportunistic only: never put an unpublished child ahead of queued formal work.
    let local = gate.try_acquire().ok()?;
    let (id, cancel, sender, worker) = coordinator.reserve(key, base)?;
    Some((id, cancel, sender, BuildLease { _worker: worker, _local: local }))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Key {
    pub parent: B256,
    pub timestamp_ms: u64,
    pub beneficiary: Address,
    pub gas_limit: u64,
    pub min_gas_tip: u128,
}
struct Entry {
    depth: usize,
    ready: Arc<std::sync::OnceLock<std::time::Instant>>,
    key: Key,
    receiver: oneshot::Receiver<BuildResult>,
}
struct Chain {
    id: u64,
    base: B256,
    parents: Vec<B256>,
    ready: Vec<Arc<std::sync::OnceLock<std::time::Instant>>>,
    cancel: ManualCancel,
    entries: VecDeque<Entry>,
    running: bool,
}
#[derive(Default)]
struct State {
    chain: Option<Chain>,
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
static LAST_WORKER_EXIT: Mutex<Option<std::time::Instant>> = Mutex::new(None);
pub(crate) fn launch(task: impl std::future::Future<Output = ()> + Send + 'static) -> bool {
    EXECUTOR.get().is_some_and(|executor| {
        executor
            .try_send(Box::pin(async move {
                let previous = LAST_WORKER_EXIT.lock().take();
                if let Some(previous) = previous {
                    metrics::histogram!("bsc.miner_pipeline_batch_gap_seconds")
                        .record(previous.elapsed().as_secs_f64());
                }
                task.await;
            }))
            .is_ok()
    })
}

pub(crate) fn enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("BSC_MINING_PIPELINED_BUILD").is_ok_and(|s| s == "1"))
}
impl Coordinator {
    /// The permit bounds running work, even after its chain was invalidated.
    fn reserve(&self, key: Key, base: B256) -> Option<WorkerReservation> {
        let permit = self.worker.clone().try_acquire_owned().ok()?;
        let mut state = self.state.lock();
        if state.chain.is_some() || state.head != Some(base) {
            return None
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let cancel = ManualCancel::default();
        let (sender, receiver) = oneshot::channel();
        let ready = Arc::new(std::sync::OnceLock::new());
        state.chain = Some(Chain {
            id,
            base,
            parents: vec![key.parent],
            ready: vec![ready.clone()],
            cancel: cancel.clone(),
            entries: VecDeque::from([Entry { key, receiver, depth: 1, ready }]),
            running: true,
        });
        Some((id, cancel, sender, permit))
    }
    fn extend(&self, id: u64, key: Key) -> Option<oneshot::Sender<BuildResult>> {
        let mut state = self.state.lock();
        let chain = state.chain.as_mut()?;
        if chain.id != id ||
            chain.cancel.is_cancelled() ||
            !chain.running ||
            chain.parents.len() >= 2
        {
            return None
        }
        let (sender, receiver) = oneshot::channel();
        chain.parents.push(key.parent);
        let ready = Arc::new(std::sync::OnceLock::new());
        chain.ready.push(ready.clone());
        chain.entries.push_back(Entry { key, receiver, depth: chain.parents.len(), ready });
        Some(sender)
    }
    fn finished(&self, id: u64) {
        let retired = {
            let mut state = self.state.lock();
            if let Some(chain) = state.chain.as_mut().filter(|c| c.id == id) {
                chain.running = false;
                if chain.entries.is_empty() {
                    state.chain.take()
                } else {
                    None
                }
            } else {
                None
            }
        };
        drop(retired);
    }
    fn canonical_head(&self, head: B256) {
        let retired = {
            let mut state = self.state.lock();
            state.head = Some(head);
            if state.chain.as_ref().is_some_and(|c| head != c.base && !c.parents.contains(&head)) {
                state.chain.take()
            } else {
                None
            }
        };
        if let Some(chain) = retired {
            chain.cancel.cancel();
            event("parent_changed");
        }
    }
    fn selected_parent(&self, parent: Option<B256>) {
        let retired = {
            let mut state = self.state.lock();
            if state.chain.as_ref().is_some_and(|c| !parent.is_some_and(|p| c.parents.contains(&p)))
            {
                state.chain.take()
            } else {
                None
            }
        };
        if let Some(chain) = retired {
            chain.cancel.cancel();
            event("candidate_changed");
        }
    }
    fn failed(&self, id: u64) {
        let retired = {
            let mut state = self.state.lock();
            if state.chain.as_ref().is_some_and(|c| c.id == id) {
                state.chain.take()
            } else {
                None
            }
        };
        if let Some(chain) = retired {
            chain.cancel.cancel();
        }
    }
    fn take(&self, key: &Key) -> Option<Adoption> {
        let (entry, cancel, retired) = {
            let mut state = self.state.lock();
            if state.head != Some(key.parent) {
                return None
            }
            let chain = state.chain.as_mut()?;
            let entry = chain.entries.pop_front()?;
            let cancel = chain.cancel.clone();
            let retired = if entry.key != *key || (!chain.running && chain.entries.is_empty()) {
                state.chain.take()
            } else {
                None
            };
            (entry, cancel, retired)
        };
        drop(retired);
        if entry.key == *key && !cancel.is_cancelled() {
            event("adopted");
            depth_event(entry.depth, "adopted");
            Some(Adoption {
                receiver: entry.receiver,
                depth: entry.depth,
                ready: entry.ready,
                cancel,
                accepted: false,
            })
        } else {
            cancel.cancel();
            event("parameters_changed");
            None
        }
    }
}
fn parse_depth(value: Option<&str>) -> usize {
    value.and_then(|v| v.parse::<usize>().ok()).unwrap_or(1).clamp(1, 2)
}
pub(crate) fn depth() -> usize {
    static DEPTH: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *DEPTH.get_or_init(|| parse_depth(std::env::var("BSC_MINING_PIPELINE_DEPTH").ok().as_deref()))
}
pub(crate) fn extend(id: u64, key: Key) -> Option<oneshot::Sender<BuildResult>> {
    COORDINATOR.extend(id, key)
}
pub(crate) struct WorkerCompletion(pub u64);
impl Drop for WorkerCompletion {
    fn drop(&mut self) {
        COORDINATOR.finished(self.0);
        *LAST_WORKER_EXIT.lock() = Some(std::time::Instant::now());
    }
}
pub(crate) fn can_start(base: B256) -> bool {
    if !enabled() {
        return false
    }
    if COORDINATOR.worker.available_permits() == 0 {
        depth_event(1, "start_check_worker_busy");
        return false
    }
    let state = COORDINATOR.state.lock();
    if state.chain.is_some() {
        depth_event(1, "start_check_pending_chain");
        return false
    }
    if state.head != Some(base) {
        depth_event(1, "start_check_parent_not_head");
        return false
    }
    true
}
pub(crate) fn reserve(key: Key, base: B256) -> Option<Reservation<'static>> {
    if !enabled() {
        return None
    }
    reserve_on(&COORDINATOR, &super::build_control::LOCAL_BUILD_PERMIT, key, base)
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
    pub depth: usize,
    ready: Arc<std::sync::OnceLock<std::time::Instant>>,
    cancel: ManualCancel,
    accepted: bool,
}
impl Adoption {
    /// Accepting one result must not cancel descendants still using its private state.
    pub(crate) fn accept(&mut self) {
        self.accepted = true;
        depth_event(self.depth, "reused");
        if let Some(ready) = self.ready.get() {
            metrics::histogram!("bsc.miner_pipeline_ready_to_adoption_seconds", "depth" => super::perf_tuning::depth_label(self.depth)).record(ready.elapsed().as_secs_f64());
        }
    }
}
impl Drop for Adoption {
    fn drop(&mut self) {
        if !self.accepted {
            depth_event(self.depth, "adoption_abandoned");
            self.cancel.clone().cancel();
        }
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

/// Records only bounded private-depth labels, never block identities.
pub(crate) fn depth_event(depth: usize, outcome: &'static str) {
    metrics::counter!("bsc.miner_pipeline_depth_total", "depth" => super::perf_tuning::depth_label(depth), "outcome" => outcome).increment(1);
}
pub(crate) fn ready(id: u64, depth: usize) {
    let state = COORDINATOR.state.lock();
    if let Some(chain) = state.chain.as_ref().filter(|chain| chain.id == id) {
        if let Some(ready) = chain.ready.get(depth.saturating_sub(1)) {
            let _ = ready.set(std::time::Instant::now());
        }
    }
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
    fn reserve(c: &Coordinator) -> WorkerReservation {
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
    #[tokio::test]
    async fn adoption_keeps_the_running_build_without_reacquiring_the_local_lease() {
        let c = coordinator();
        let gate = Semaphore::new(1);
        let (_, cancel, sender, lease) =
            reserve_on(&c, &gate, key(), B256::with_last_byte(1)).unwrap();
        assert!(gate.try_acquire().is_err());
        c.canonical_head(key().parent);
        let mut adopted = c.take(&key()).unwrap();
        // Same producer, still holding the full-size runtime's build admission.
        assert!(sender.send(Err("original producer result".into())).is_ok());
        let result = tokio::time::timeout(std::time::Duration::from_secs(1), &mut adopted.receiver)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.unwrap_err(), "original producer result");
        assert!(!cancel.is_cancelled());
        assert!(gate.try_acquire().is_err());
        drop(lease);
        assert!(gate.try_acquire().is_ok());
    }
    #[test]
    fn busy_formal_build_skips_pipeline_without_reserving_or_queuing() {
        let c = coordinator();
        let gate = Semaphore::new(1);
        let formal = gate.try_acquire().unwrap();
        assert!(reserve_on(&c, &gate, key(), B256::with_last_byte(1)).is_none());
        assert!(c.state.lock().chain.is_none());
        assert_eq!(c.worker.available_permits(), 1);
        drop(formal);
        assert!(reserve_on(&c, &gate, key(), B256::with_last_byte(1)).is_some());
    }
    #[tokio::test]
    async fn cancellation_does_not_release_the_local_lease_before_the_worker_exits() {
        let c = coordinator();
        let gate = Semaphore::new(1);
        let (_, cancel, _, lease) = reserve_on(&c, &gate, key(), B256::with_last_byte(1)).unwrap();
        c.selected_parent(None);
        assert!(cancel.is_cancelled());
        let formal = gate.acquire();
        tokio::pin!(formal);
        assert!(tokio::time::timeout(std::time::Duration::from_millis(5), &mut formal)
            .await
            .is_err());
        assert!(reserve_on(&c, &gate, key(), B256::with_last_byte(1)).is_none());
        drop(lease); // producer finished unwinding, not merely coordinator invalidation
        let permit =
            tokio::time::timeout(std::time::Duration::from_secs(1), formal).await.unwrap().unwrap();
        assert_eq!(c.worker.available_permits(), 1);
        drop(permit);
    }
    #[test]
    fn depth_configuration_is_bounded_and_defaults_to_one() {
        assert_eq!(parse_depth(None), 1);
        assert_eq!(parse_depth(Some("bad")), 1);
        assert_eq!(parse_depth(Some("0")), 1);
        assert_eq!(parse_depth(Some("1")), 1);
        assert_eq!(parse_depth(Some("2")), 2);
        assert_eq!(parse_depth(Some("100")), 2);
    }
    #[tokio::test]
    async fn two_results_are_adopted_in_order_without_cancelling_the_descendant() {
        let c = coordinator();
        let (_, cancel, first_sender, _permit) = reserve(&c);
        let mut second_key = key();
        second_key.parent = B256::with_last_byte(3);
        second_key.timestamp_ms += 450;
        let second_sender = c.extend(1, second_key.clone()).unwrap();
        assert!(c.extend(1, second_key.clone()).is_none(), "bounded at two");
        assert!(c.take(&second_key).is_none());
        c.selected_parent(Some(key().parent));
        c.canonical_head(key().parent);
        let mut first = c.take(&key()).unwrap();
        assert_eq!(first.depth, 1);
        first_sender.send(Err("first original result".into())).unwrap();
        assert_eq!((&mut first.receiver).await.unwrap().unwrap_err(), "first original result");
        first.accept();
        drop(first);
        assert!(!cancel.is_cancelled(), "accepting parent must leave child running");
        assert!(c.take(&second_key).is_none(), "second parent is not canonical yet");
        second_sender.send(Err("second original result".into())).unwrap();
        c.finished(1);
        c.selected_parent(Some(second_key.parent));
        c.canonical_head(second_key.parent);
        let mut second = c.take(&second_key).unwrap();
        assert_eq!(second.depth, 2);
        assert_eq!((&mut second.receiver).await.unwrap().unwrap_err(), "second original result");
        second.accept();
        drop(second);
        assert!(!cancel.is_cancelled());
        assert!(c.state.lock().chain.is_none());
    }
    #[test]
    fn fork_or_parameter_change_discards_the_whole_chain() {
        for fork in [false, true] {
            let c = coordinator();
            let (_, cancel, first_sender, permit) = reserve(&c);
            let mut next_key = key();
            next_key.parent = B256::with_last_byte(3);
            let second_sender = c.extend(1, next_key).unwrap();
            if fork {
                c.canonical_head(B256::with_last_byte(99));
            } else {
                c.canonical_head(key().parent);
                let mut changed = key();
                changed.gas_limit += 1;
                assert!(c.take(&changed).is_none());
            }
            assert!(cancel.is_cancelled());
            assert!(first_sender.is_closed());
            assert!(second_sender.is_closed());
            assert!(c.state.lock().chain.is_none());
            assert_eq!(c.worker.available_permits(), 0, "cancellation alone does not free compute");
            drop(permit);
        }
    }
    /// Controlled scheduler experiment using the real coordinator/adoption channels.
    /// Mock durations replace EVM work; this is not a throughput benchmark.
    #[tokio::test(start_paused = true)]
    async fn controlled_timing_buffers_an_isolated_slow_block_without_early_publication() {
        use tokio::time::{sleep, sleep_until, Duration, Instant};
        let start = Instant::now();
        let mut ordinary = Vec::new();
        for (index, ms) in [200, 200, 600].into_iter().enumerate() {
            sleep(Duration::from_millis(ms)).await;
            sleep_until(start + Duration::from_millis(450 * (index as u64 + 1))).await;
            ordinary.push(start.elapsed().as_millis());
        }
        assert_eq!(ordinary, [450, 900, 1500]);

        let start = Instant::now();
        sleep(Duration::from_millis(200)).await; // initial parent candidate finished
        let c = coordinator();
        let gate = Semaphore::new(1);
        let (id, cancel, first, lease) =
            reserve_on(&c, &gate, key(), B256::with_last_byte(1)).unwrap();
        let mut next = key();
        next.parent = B256::with_last_byte(3);
        next.timestamp_ms += 450;
        let worker = async {
            let _lease = lease;
            sleep(Duration::from_millis(200)).await;
            let second = c.extend(id, next.clone()).unwrap();
            first.send(Err("ready".into())).unwrap();
            let first_ready = start.elapsed().as_millis();
            sleep(Duration::from_millis(600)).await;
            second.send(Err("ready".into())).unwrap();
            c.finished(id);
            [first_ready, start.elapsed().as_millis()]
        };
        let publish = async {
            sleep_until(start + Duration::from_millis(450)).await;
            let first_publication = start.elapsed().as_millis();
            c.canonical_head(key().parent);
            let mut adopted = c.take(&key()).unwrap();
            assert_eq!((&mut adopted.receiver).await.unwrap().unwrap_err(), "ready");
            adopted.accept();
            drop(adopted);
            assert!(!cancel.is_cancelled());
            sleep_until(start + Duration::from_millis(900)).await;
            let second_publication = start.elapsed().as_millis();
            c.selected_parent(Some(next.parent));
            c.canonical_head(next.parent);
            let mut adopted = c.take(&next).unwrap();
            assert_eq!((&mut adopted.receiver).await.unwrap().unwrap_err(), "ready");
            adopted.accept();
            drop(adopted);
            sleep_until(start + Duration::from_millis(1350)).await;
            [first_publication, second_publication, start.elapsed().as_millis()]
        };
        let (ready, pipelined) = tokio::join!(worker, publish);
        assert_eq!(ready, [400, 1000]);
        assert_eq!(pipelined, [450, 900, 1350]);
        assert!(gate.try_acquire().is_ok(), "same compute lease released after both builds");
        println!("controlled milliseconds: ordinary={ordinary:?}; private_ready={ready:?}; pipeline_publication={pipelined:?}; publication_gain=150ms");
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
            assert!(c.state.lock().chain.is_none());
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
        assert_eq!(c.state.lock().chain.as_ref().unwrap().id, new_id);
    }
}
