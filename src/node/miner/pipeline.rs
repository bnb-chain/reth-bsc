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
    plan: Option<Plan>,
) -> Option<Reservation<'a>> {
    // Opportunistic only: never put an unpublished child ahead of queued formal work.
    let local = gate.try_acquire().ok()?;
    let (id, cancel, sender, worker) = coordinator.reserve_planned(key, base, plan)?;
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
#[derive(Clone, Debug)]
pub(crate) struct Plan {
    pub context: super::bsc_miner::MiningContext,
    pub attributes: reth_node_ethereum::engine::EthPayloadAttributes,
}

impl Plan {
    pub(crate) fn valid_for(
        &self,
        key: &Key,
        canonical: &super::bsc_miner::MiningContext,
        parlia: &crate::consensus::parlia::Parlia<crate::chainspec::BscChainSpec>,
    ) -> bool {
        let Some(header) = self.context.header.as_ref() else { return false };
        let parent = &canonical.parent_header;
        canonical.is_inturn &&
            self.context.is_inturn &&
            key.parent == parent.hash() &&
            self.context.parent_header.hash() == parent.hash() &&
            header.parent_hash == parent.hash() &&
            header.number == parent.number + 1 &&
            header.beneficiary == key.beneficiary &&
            crate::consensus::parlia::util::calculate_millisecond_timestamp(header) ==
                key.timestamp_ms &&
            self.context.block_timestamp_ms == key.timestamp_ms &&
            self.attributes.timestamp == header.timestamp &&
            self.attributes.suggested_fee_recipient == key.beneficiary &&
            canonical.parent_snapshot.is_inturn(key.beneficiary) &&
            !canonical.parent_snapshot.sign_recently(key.beneficiary) &&
            parlia
                .block_time_verify_for_ramanujan_fork(&canonical.parent_snapshot, header, parent)
                .is_ok()
    }
}

struct Entry {
    plan: Option<Plan>,
    depth: usize,
    ready: Arc<std::sync::OnceLock<std::time::Instant>>,
    key: Key,
    receiver: oneshot::Receiver<BuildResult>,
}
struct Chain {
    discard_reason: Arc<std::sync::OnceLock<&'static str>>,
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
    limit: usize,
}
impl Default for Coordinator {
    fn default() -> Self {
        Self {
            state: Mutex::default(),
            worker: Arc::new(Semaphore::new(1)),
            next_id: AtomicU64::new(1),
            limit: depth(),
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
        return Ok(());
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
    #[cfg(test)]
    fn reserve(&self, key: Key, base: B256) -> Option<WorkerReservation> {
        self.reserve_planned(key, base, None)
    }
    fn reserve_planned(
        &self,
        key: Key,
        base: B256,
        plan: Option<Plan>,
    ) -> Option<WorkerReservation> {
        let permit = self.worker.clone().try_acquire_owned().ok()?;
        let mut state = self.state.lock();
        if state.chain.is_some() || state.head != Some(base) {
            return None;
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let cancel = ManualCancel::default();
        let (sender, receiver) = oneshot::channel();
        let ready = Arc::new(std::sync::OnceLock::new());
        state.chain = Some(Chain {
            discard_reason: Arc::new(std::sync::OnceLock::new()),
            id,
            base,
            parents: vec![key.parent],
            ready: vec![ready.clone()],
            cancel: cancel.clone(),
            entries: VecDeque::from([Entry { key, receiver, depth: 1, ready, plan }]),
            running: true,
        });
        Some((id, cancel, sender, permit))
    }
    #[cfg(test)]
    fn extend(&self, id: u64, key: Key) -> Option<oneshot::Sender<BuildResult>> {
        self.extend_planned(id, key, None)
    }
    fn extend_planned(
        &self,
        id: u64,
        key: Key,
        plan: Option<Plan>,
    ) -> Option<oneshot::Sender<BuildResult>> {
        let mut state = self.state.lock();
        let chain = state.chain.as_mut()?;
        if chain.id != id ||
            chain.cancel.is_cancelled() ||
            !chain.running ||
            chain.parents.len() > self.limit
        {
            return None;
        }
        let (sender, receiver) = oneshot::channel();
        chain.parents.push(key.parent);
        let ready = Arc::new(std::sync::OnceLock::new());
        chain.ready.push(ready.clone());
        chain.entries.push_back(Entry { key, receiver, depth: chain.parents.len(), ready, plan });
        Some(sender)
    }
    fn slot(&self, id: u64, parent: B256) -> Option<(usize, bool)> {
        let state = self.state.lock();
        let chain =
            state.chain.as_ref().filter(|chain| chain.id == id && !chain.cancel.is_cancelled())?;
        let index = chain.parents.iter().position(|hash| *hash == parent)?;
        Some((index + 1, index < self.limit))
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
            // Retain the accepted tip as the first parent, dropping only its ancestors.
            // Adoption alone does not advance the window: canonical acceptance does.
            if let Some(chain) = state.chain.as_mut() {
                if let Some(index) = chain.parents.iter().position(|parent| *parent == head) {
                    chain.parents.drain(..index);
                    chain.ready.drain(..index);
                    chain.base = head;
                }
            }
            if state.chain.as_ref().is_some_and(|c| head != c.base && !c.parents.contains(&head)) {
                state.chain.take()
            } else {
                None
            }
        };
        if let Some(chain) = retired {
            retire(&chain, "parent_changed");
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
            retire(&chain, "candidate_changed");
            chain.cancel.cancel();
            event("candidate_changed");
        }
    }
    #[cfg(test)]
    fn failed(&self, id: u64) {
        self.failed_reason(id, "producer_failed");
    }
    fn failed_reason(&self, id: u64, reason: &'static str) {
        let retired = {
            let mut state = self.state.lock();
            if state.chain.as_ref().is_some_and(|c| c.id == id) {
                state.chain.take()
            } else {
                None
            }
        };
        if let Some(chain) = retired {
            retire(&chain, reason);
            chain.cancel.cancel();
        }
    }
    fn claim(
        &self,
        parent: B256,
        validate: impl FnOnce(&Key, &Plan) -> bool,
    ) -> Option<(Plan, Adoption)> {
        // Validate outside the coordinator lock. Claim only the exact generation inspected.
        let (id, key, plan) = {
            let state = self.state.lock();
            if state.head != Some(parent) {
                return None;
            }
            let chain = state.chain.as_ref()?;
            if chain.cancel.is_cancelled() {
                return None;
            }
            let entry = chain.entries.front()?;
            (chain.id, entry.key.clone(), entry.plan.clone()?)
        };
        if key.parent != parent || !validate(&key, &plan) {
            self.failed_reason(id, "plan_rejected");
            event("plan_rejected");
            return None;
        }
        self.take_generation(&key, Some(id)).map(|adoption| (plan, adoption))
    }
    #[cfg(test)]
    fn take(&self, key: &Key) -> Option<Adoption> {
        self.take_generation(key, None)
    }
    fn take_generation(&self, key: &Key, generation: Option<u64>) -> Option<Adoption> {
        let (entry, cancel, discard_reason, retired) = {
            let mut state = self.state.lock();
            if state.head != Some(key.parent) {
                return None;
            }
            let chain = state.chain.as_mut()?;
            if generation.is_some_and(|id| id != chain.id) {
                return None;
            }
            let entry = chain.entries.pop_front()?;
            let cancel = chain.cancel.clone();
            let discard_reason = chain.discard_reason.clone();
            let retired = if entry.key != *key || (!chain.running && chain.entries.is_empty()) {
                state.chain.take()
            } else {
                None
            };
            if entry.key != *key {
                if let Some(chain) = &retired {
                    retire(chain, "parameters_changed");
                }
            }
            (entry, cancel, discard_reason, retired)
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
                discard_reason,
            })
        } else {
            cancel.cancel();
            event("parameters_changed");
            None
        }
    }
}
fn retire(chain: &Chain, reason: &'static str) {
    if chain.discard_reason.set(reason).is_ok() {
        metrics::counter!("bsc.miner_pipeline_invalidated_branches_total", "reason" => reason)
            .increment(1);
        metrics::counter!("bsc.miner_pipeline_invalidated_entries_total", "reason" => reason)
            .increment(chain.entries.len() as u64);
    }
}

pub(crate) fn track_candidate(id: u64, payload: &BscBuiltPayload) {
    if let Some(diagnostics) = &payload.diagnostics {
        let state = COORDINATOR.state.lock();
        if let Some(chain) = state.chain.as_ref().filter(|c| c.id == id) {
            let _ = diagnostics.pipeline_discard_reason.set(chain.discard_reason.clone());
        }
    }
}

// Bounds unpublished state and queued results, not concurrent execution threads.
pub(crate) const MAX_DEPTH: usize = 8;
fn parse_depth(value: Option<&str>) -> usize {
    value.and_then(|v| v.parse::<usize>().ok()).unwrap_or(1).clamp(1, MAX_DEPTH)
}
pub(crate) fn depth() -> usize {
    static DEPTH: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *DEPTH.get_or_init(|| parse_depth(std::env::var("BSC_MINING_PIPELINE_DEPTH").ok().as_deref()))
}
pub(crate) fn extend(id: u64, key: Key, plan: Plan) -> Option<oneshot::Sender<BuildResult>> {
    COORDINATOR.extend_planned(id, key, Some(plan))
}
/// A successor receiver is registered before its parent is delivered, so a fast
/// canonical import can adopt it even while the window is full. Only computation
/// waits for capacity; the extra receiver does not hold another computed block.
pub(crate) async fn wait_for_slot(id: u64, parent: B256, deadline_ms: u64) -> Option<usize> {
    let start = std::time::Instant::now();
    loop {
        let (depth, available) = COORDINATOR.slot(id, parent)?;
        if available {
            metrics::histogram!("bsc.miner_pipeline_window_wait_seconds")
                .record(start.elapsed().as_secs_f64());
            metrics::gauge!("bsc.miner_pipeline_window_used").set(depth as f64);
            return Some(depth);
        }
        if super::startup_diagnostics::wall_ms() >= u128::from(deadline_ms) {
            event("window_wait_expired");
            return None;
        }
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
}
pub(crate) fn accepted_parent(id: u64) -> Option<B256> {
    let state = COORDINATOR.state.lock();
    state.chain.as_ref().filter(|chain| chain.id == id && !chain.cancel.is_cancelled())?;
    state.head
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
        return false;
    }
    if COORDINATOR.worker.available_permits() == 0 {
        depth_event(1, "start_check_worker_busy");
        return false;
    }
    let state = COORDINATOR.state.lock();
    if state.chain.is_some() {
        depth_event(1, "start_check_pending_chain");
        return false;
    }
    if state.head != Some(base) {
        depth_event(1, "start_check_parent_not_head");
        return false;
    }
    true
}
pub(crate) fn reserve(key: Key, base: B256, plan: Plan) -> Option<Reservation<'static>> {
    if !enabled() {
        return None;
    }
    reserve_on(&COORDINATOR, &super::build_control::LOCAL_BUILD_PERMIT, key, base, Some(plan))
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
pub(crate) fn failed(id: u64, reason: &'static str) {
    COORDINATOR.failed_reason(id, reason);
}

/// Adopts the same running computation, before taking the ordinary builder's build permit.
pub(crate) struct Adoption {
    discard_reason: Arc<std::sync::OnceLock<&'static str>>,
    pub receiver: oneshot::Receiver<BuildResult>,
    pub depth: usize,
    ready: Arc<std::sync::OnceLock<std::time::Instant>>,
    cancel: ManualCancel,
    accepted: bool,
}
impl Adoption {
    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }
    pub(crate) async fn wait(
        &mut self,
        abort: &mut oneshot::Receiver<()>,
    ) -> Result<BuildResult, ()> {
        let cancellation = self.cancel.clone();
        let result = tokio::select! {
            biased;
            _ = abort => return Err(()),
            _ = async { while !cancellation.is_cancelled() { tokio::time::sleep(std::time::Duration::from_millis(10)).await; } } => {
                return Ok(Err("pipeline generation cancelled".into()))
            }
            result = &mut self.receiver => result,
        };
        if self.is_cancelled() {
            return Ok(Err("pipeline generation cancelled".into()));
        }
        Ok(result.unwrap_or_else(|_| Err("pipeline producer disconnected".into())))
    }

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
            if self.discard_reason.set("adoption_abandoned").is_ok() {
                metrics::counter!("bsc.miner_pipeline_invalidated_branches_total", "reason" => "adoption_abandoned").increment(1);
            }
            depth_event(self.depth, "adoption_abandoned");
            self.cancel.clone().cancel();
        }
    }
}
pub(crate) fn is_head(parent: B256) -> bool {
    COORDINATOR.state.lock().head == Some(parent)
}
pub(crate) fn claim(
    parent: B256,
    validate: impl FnOnce(&Key, &Plan) -> bool,
) -> Option<(Plan, Adoption)> {
    enabled().then(|| COORDINATOR.claim(parent, validate)).flatten()
}
pub(crate) fn event(outcome: &'static str) {
    metrics::counter!("bsc_miner_pipeline_total", "outcome" => outcome).increment(1);
}

/// Records only bounded private-depth labels, never block identities.
pub(crate) fn depth_event(depth: usize, outcome: &'static str) {
    metrics::counter!("bsc.miner_pipeline_depth_total", "depth" => super::perf_tuning::depth_label(depth), "outcome" => outcome).increment(1);
}
pub(crate) fn ready(id: u64, parent: B256) {
    let state = COORDINATOR.state.lock();
    if let Some(chain) = state.chain.as_ref().filter(|chain| chain.id == id) {
        if let Some(ready) = chain
            .parents
            .iter()
            .position(|hash| *hash == parent)
            .and_then(|index| chain.ready.get(index))
        {
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
        let c = Coordinator { limit: 2, ..Default::default() };
        c.canonical_head(B256::with_last_byte(1));
        c
    }
    fn reserve(c: &Coordinator) -> WorkerReservation {
        c.reserve(key(), B256::with_last_byte(1)).unwrap()
    }
    fn planned() -> (Plan, crate::consensus::parlia::Parlia<crate::chainspec::BscChainSpec>, Key) {
        use crate::consensus::parlia::{util::set_millisecond_part_of_timestamp, Parlia, Snapshot};
        use alloy_consensus::Header;
        use reth_primitives_traits::SealedHeader;
        let parlia = Parlia::new(
            Arc::new(crate::chainspec::BscChainSpec::from(crate::chainspec::bsc::bsc_mainnet())),
            200,
        );
        // Deliberately in the past: a late formal job would round this up to now.
        let parent_ms = parlia.present_millis_timestamp().saturating_sub(2_000) / 1000 * 1000;
        let mut parent =
            Header { number: 100_000_000, timestamp: parent_ms / 1000, ..Default::default() };
        set_millisecond_part_of_timestamp(parent_ms, &mut parent);
        let parent = SealedHeader::seal_slow(parent);
        let mut header = Header {
            number: parent.number + 1,
            parent_hash: parent.hash(),
            timestamp: parent.timestamp,
            beneficiary: Address::ZERO,
            ..Default::default()
        };
        set_millisecond_part_of_timestamp(parent_ms + 450, &mut header);
        let mut snapshot =
            Snapshot::new(vec![Address::ZERO], parent.number, parent.hash(), 200, None);
        snapshot.block_interval = 450;
        let context = super::super::bsc_miner::MiningContext {
            header: Some(header),
            parent_header: parent.clone(),
            parent_snapshot: Arc::new(snapshot),
            is_inturn: true,
            cached_reads: None,
            block_timestamp_ms: parent_ms + 450,
            end_mining_timestamp_ms: (parent_ms + 450) as u128,
            startup_timing: None,
        };
        let attributes = reth_node_ethereum::engine::EthPayloadAttributes {
            timestamp: parent.timestamp,
            suggested_fee_recipient: Address::ZERO,
            prev_randao: B256::ZERO,
            withdrawals: None,
            parent_beacon_block_root: None,
            slot_number: None,
            target_gas_limit: None,
        };
        let key = Key { parent: parent.hash(), timestamp_ms: parent_ms + 450, ..key() };
        (Plan { context, attributes }, parlia, key)
    }

    #[test]
    fn cancellation_records_first_reason_once_and_normal_head_does_not_invalidate() {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            let c = coordinator();
            let (id, _, _sender, _permit) = reserve(&c);
            let reason = c.state.lock().chain.as_ref().unwrap().discard_reason.clone();
            c.canonical_head(key().parent);
            assert!(reason.get().is_none());
            c.selected_parent(Some(B256::with_last_byte(99)));
            c.failed(id);
            assert_eq!(reason.get(), Some(&"candidate_changed"));
        });
        let text = handle.render();
        let branches: Vec<_> = text
            .lines()
            .filter(|l| l.starts_with("bsc_miner_pipeline_invalidated_branches_total{"))
            .collect();
        assert_eq!(branches.len(), 1);
        assert!(branches[0].contains("candidate_changed"));
        assert!(branches[0].ends_with(" 1"));
    }

    #[test]
    fn late_canonical_job_preserves_the_original_valid_timestamp() {
        let (plan, parlia, key) = planned();
        assert!(parlia.present_millis_timestamp() > key.timestamp_ms);
        assert!(plan.valid_for(&key, &plan.context, &parlia));
        let c = Coordinator::default();
        let base = B256::repeat_byte(8);
        c.canonical_head(base);
        let (_, cancel, _sender, _permit) =
            c.reserve_planned(key.clone(), base, Some(plan.clone())).unwrap();
        c.canonical_head(key.parent);
        let (restored, mut adoption) =
            c.claim(key.parent, |k, p| p.valid_for(k, &plan.context, &parlia)).unwrap();
        assert_eq!(restored.attributes.timestamp, plan.attributes.timestamp);
        assert_eq!(restored.context.block_timestamp_ms, key.timestamp_ms);
        assert_eq!(restored.context.end_mining_timestamp_ms, plan.context.end_mining_timestamp_ms);
        adoption.accept();
        assert!(!cancel.is_cancelled());
        // Ordinary fallback must derive a new timestamp, not reuse the expired plan.
        let mut fallback = plan.context.clone();
        let parent = fallback.parent_header.clone();
        let attrs = super::super::util::prepare_new_attributes(
            &mut fallback,
            Arc::new(parlia),
            &parent,
            key.beneficiary,
        );
        assert!(fallback.block_timestamp_ms > key.timestamp_ms);
        assert_eq!(attrs.timestamp, fallback.block_timestamp_ms / 1000);
    }

    #[test]
    fn plan_validation_rejects_invalid_time_and_parent() {
        let (mut plan, parlia, mut key) = planned();
        let canonical = plan.context.clone();
        key.timestamp_ms -= 1;
        plan.context.block_timestamp_ms = key.timestamp_ms;
        crate::consensus::parlia::util::set_millisecond_part_of_timestamp(
            key.timestamp_ms,
            plan.context.header.as_mut().unwrap(),
        );
        assert!(!plan.valid_for(&key, &canonical, &parlia), "parent interval remains enforced");
        let (plan, parlia, mut key) = planned();
        key.parent = B256::repeat_byte(99);
        assert!(!plan.valid_for(&key, &plan.context, &parlia));
    }

    #[test]
    fn claim_cannot_steal_a_replacement_generation_during_validation() {
        let (plan, _, key) = planned();
        let c = Coordinator::default();
        let base = B256::repeat_byte(8);
        c.canonical_head(base);
        let (id, _, _, permit) = c.reserve_planned(key.clone(), base, Some(plan.clone())).unwrap();
        c.canonical_head(key.parent);
        let mut replacement = None;
        assert!(c
            .claim(key.parent, |_, _| {
                c.failed(id);
                drop(permit);
                replacement = c.reserve_planned(key.clone(), key.parent, Some(plan.clone()));
                true
            })
            .is_none());
        assert!(replacement.is_some());
        assert_ne!(c.state.lock().chain.as_ref().unwrap().id, id);
    }

    #[tokio::test]
    async fn claimed_task_reports_producer_failure_and_abort_without_hanging() {
        let c = coordinator();
        let (_, _, sender, _permit) = reserve(&c);
        c.canonical_head(key().parent);
        let mut adoption = c.take(&key()).unwrap();
        let (_abort, mut abort_rx) = oneshot::channel();
        sender.send(Err("provider read failed".into())).unwrap();
        assert_eq!(
            adoption.wait(&mut abort_rx).await.unwrap().unwrap_err(),
            "provider read failed"
        );
        drop(adoption);
        let c = coordinator();
        let (_, cancel, _sender, _permit) = reserve(&c);
        c.canonical_head(key().parent);
        let mut adoption = c.take(&key()).unwrap();
        let (abort, mut abort_rx) = oneshot::channel();
        abort.send(()).unwrap();
        assert!(adoption.wait(&mut abort_rx).await.is_err());
        drop(adoption);
        assert!(cancel.is_cancelled());
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
            reserve_on(&c, &gate, key(), B256::with_last_byte(1), None).unwrap();
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
        assert!(reserve_on(&c, &gate, key(), B256::with_last_byte(1), None).is_none());
        assert!(c.state.lock().chain.is_none());
        assert_eq!(c.worker.available_permits(), 1);
        drop(formal);
        assert!(reserve_on(&c, &gate, key(), B256::with_last_byte(1), None).is_some());
    }
    #[tokio::test]
    async fn cancellation_does_not_release_the_local_lease_before_the_worker_exits() {
        let c = coordinator();
        let gate = Semaphore::new(1);
        let (_, cancel, _, lease) =
            reserve_on(&c, &gate, key(), B256::with_last_byte(1), None).unwrap();
        c.selected_parent(None);
        assert!(cancel.is_cancelled());
        let formal = gate.acquire();
        tokio::pin!(formal);
        assert!(tokio::time::timeout(std::time::Duration::from_millis(5), &mut formal)
            .await
            .is_err());
        assert!(reserve_on(&c, &gate, key(), B256::with_last_byte(1), None).is_none());
        drop(lease); // producer finished unwinding, not merely coordinator invalidation
        let permit =
            tokio::time::timeout(std::time::Duration::from_secs(1), formal).await.unwrap().unwrap();
        assert_eq!(c.worker.available_permits(), 1);
        drop(permit);
    }
    #[test]
    fn sliding_window_refills_after_acceptance_without_growing_history() {
        for limit in [1, 2, MAX_DEPTH] {
            let c = Coordinator { limit, ..Default::default() };
            c.canonical_head(B256::with_last_byte(1));
            let (id, cancel, first, _permit) = reserve(&c);
            let mut senders = VecDeque::from([first]);
            let mut keys = VecDeque::from([key()]);
            let mut next = key();
            for i in 0..limit {
                next.parent = B256::with_last_byte((3 + i) as u8);
                next.timestamp_ms += 450;
                senders.push_back(c.extend(id, next.clone()).unwrap());
                keys.push_back(next.clone());
            }
            assert_eq!(c.slot(id, next.parent), Some((limit + 1, false)));
            assert!(c.extend(id, next.clone()).is_none());
            for round in 0..30 {
                let current = keys.pop_front().unwrap();
                c.canonical_head(current.parent);
                let mut adopted = c.take(&current).unwrap();
                senders.pop_front().unwrap().send(Err("fixture".into())).unwrap();
                adopted.accept();
                // Merely adopting does not open the last receiver's compute slot.
                assert_eq!(c.slot(id, next.parent), Some((limit + 1, false)));
                c.canonical_head(keys.front().unwrap().parent);
                assert_eq!(c.slot(id, next.parent), Some((limit, true)));
                next.parent = B256::with_last_byte((30 + round) as u8);
                next.timestamp_ms += 450;
                senders.push_back(c.extend(id, next.clone()).unwrap());
                keys.push_back(next.clone());
                let state = c.state.lock();
                let chain = state.chain.as_ref().unwrap();
                assert_eq!(chain.parents.len(), limit + 1);
                assert_eq!(chain.ready.len(), limit + 1);
                assert_eq!(chain.entries.len(), limit + 1);
            }
            c.canonical_head(B256::repeat_byte(99));
            assert!(cancel.is_cancelled());
            assert!(c.slot(id, next.parent).is_none());
            assert!(senders.iter().all(oneshot::Sender::is_closed));
        }
    }

    #[test]
    fn depth_configuration_is_bounded_and_defaults_to_one() {
        assert_eq!(parse_depth(None), 1);
        assert_eq!(parse_depth(Some("bad")), 1);
        assert_eq!(parse_depth(Some("0")), 1);
        assert_eq!(parse_depth(Some("1")), 1);
        assert_eq!(parse_depth(Some("2")), 2);
        assert_eq!(parse_depth(Some("100")), MAX_DEPTH);
    }
    #[tokio::test]
    async fn two_results_are_adopted_in_order_without_cancelling_the_descendant() {
        let c = coordinator();
        let (_, cancel, first_sender, _permit) = reserve(&c);
        let mut second_key = key();
        second_key.parent = B256::with_last_byte(3);
        second_key.timestamp_ms += 450;
        let second_sender = c.extend(1, second_key.clone()).unwrap();
        // One successor receiver may be queued while the computation window is full.
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
            reserve_on(&c, &gate, key(), B256::with_last_byte(1), None).unwrap();
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
