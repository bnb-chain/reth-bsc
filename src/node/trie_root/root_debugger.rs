use super::trie_overlay::{init_trie_overlay_cache, trie_overlay_cache, TrieOverlayEntry};
use alloy_primitives::B256;
use reth_chain_state::{ExecutedTrieUpdates, NewCanonicalChain};
use reth_evm::execute::BlockExecutionError;
use reth_provider::{
    providers::ConsistentDbView, BlockNumReader, BlockReader, DatabaseProviderFactory, HeaderProvider,
    NewCanonicalChainSubscriptions, NodePrimitivesProvider,
};
use reth_trie::{
    HashedPostState, TrieInput,
};
use reth_trie::updates::TrieUpdates;
use reth_trie_parallel::root::ParallelStateRoot;
use parking_lot::{Condvar, Mutex};
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};
use tokio::sync::broadcast;

tokio::task_local! {
    /// Current payload build attempt id for this async task.
    ///
    /// This must be task-local (not thread-local) because payload building is async and may hop
    /// across runtime worker threads between `.await` points.
    static CURRENT_PAYLOAD_TRACE_ID: u64;
}

tokio::task_local! {
    /// Current payload build attempt counter for this async task.
    ///
    /// `trace_id` identifies a *payload job*, but the miner may run multiple build attempts (retries)
    /// under the same `trace_id`. This disambiguates those attempts.
    static CURRENT_PAYLOAD_ATTEMPT: u32;
}

/// Runs `fut` inside a task-local scope where [`current_payload_build_trace_id`] returns `trace_id`.
pub fn payload_build_trace_id_scope<Fut>(
    trace_id: u64,
    fut: Fut,
) -> impl std::future::Future<Output = Fut::Output>
where
    Fut: std::future::Future,
{
    CURRENT_PAYLOAD_TRACE_ID.scope(trace_id, fut)
}

/// Runs `fut` inside a task-local scope where [`current_payload_build_attempt`] returns `attempt`.
pub fn payload_build_attempt_scope<Fut>(
    attempt: u32,
    fut: Fut,
) -> impl std::future::Future<Output = Fut::Output>
where
    Fut: std::future::Future,
{
    CURRENT_PAYLOAD_ATTEMPT.scope(attempt, fut)
}

/// Returns the "current payload build trace_id" for this async task (if set).
pub fn current_payload_build_trace_id() -> Option<u64> {
    CURRENT_PAYLOAD_TRACE_ID.try_with(|c| *c).ok()
}

/// Returns the "current payload build attempt" for this async task (if set).
pub fn current_payload_build_attempt() -> Option<u32> {
    CURRENT_PAYLOAD_ATTEMPT.try_with(|c| *c).ok()
}

/// Trie root accelerator facade.
///
/// The goal is to keep all complexity localized here so callers remain simple and can always
/// downgrade to serial computation.
#[derive(Debug, Default, Clone, Copy)]
pub struct RootDebugger;

/// Key identifying a payload build (parent block).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PayloadProcessorKey {
    parent_number: u64,
    parent_hash: B256,
    /// Uniquely identifies a single payload build attempt under the same parent.
    ///
    /// Miner can retry payload building multiple times for the same `(parent_number, parent_hash)`
    /// (different tx set -> different `block_hash`). If we key only by parent, background results
    /// (payload_processor/parallel) will overwrite each other and cause false mismatches.
    trace_id: u64,
    /// Build attempt counter under the same `trace_id`.
    ///
    /// `BscPayloadJob` may spawn multiple `build_payload()` runs with the same `trace_id`.
    attempt: u32,
}

impl PayloadProcessorKey {
    pub const fn new(parent_number: u64, parent_hash: B256, trace_id: u64, attempt: u32) -> Self {
        Self { parent_number, parent_hash, trace_id, attempt }
    }

    pub const fn parent_number(&self) -> u64 {
        self.parent_number
    }

    pub const fn parent_hash(&self) -> B256 {
        self.parent_hash
    }

    pub const fn trace_id(&self) -> u64 {
        self.trace_id
    }

    pub const fn attempt(&self) -> u32 {
        self.attempt
    }
}

#[derive(Debug, Clone)]
pub struct PayloadProcessorStateRootResult {
    pub state_root: B256,
    pub trie_updates: TrieUpdates,
    pub completed_at: Instant,
    /// Total wall time from payload_processor start to completion.
    pub duration_ms: u128,
    /// Extra wall time after the hook sender is dropped (i.e. after tx execution completes).
    ///
    /// This approximates the "real sparse-tree tail latency" that does not overlap with
    /// foreground tx execution.
    pub post_exec_duration_ms: Option<u128>,
}

#[derive(Debug, Clone)]
struct PayloadProcessorStateRootEntry {
    value: Result<PayloadProcessorStateRootResult, String>,
    /// How many consumers still need to read this result.
    ///
    /// We use 2 consumers: block builder (authoritative selection) + comparer (logging).
    remaining_consumers: u8,
}

#[derive(Debug, Clone)]
struct ParallelStateRootResult {
    pub value: Result<B256, String>,
    pub duration_ms: u128,
}

static PAYLOAD_PROCESSOR_ROOTS: OnceLock<Mutex<HashMap<PayloadProcessorKey, PayloadProcessorStateRootEntry>>> =
    OnceLock::new();
static PAYLOAD_PROCESSOR_ROOTS_CV: OnceLock<Condvar> = OnceLock::new();
static PAYLOAD_PROCESSOR_STARTED: OnceLock<Mutex<HashSet<PayloadProcessorKey>>> = OnceLock::new();
static PARALLEL_STATE_ROOTS: OnceLock<Mutex<HashMap<PayloadProcessorKey, ParallelStateRootResult>>> =
    OnceLock::new();
static PAYLOAD_PROCESSOR_HOOK_DROPS: OnceLock<Mutex<HashMap<PayloadProcessorKey, Instant>>> =
    OnceLock::new();

fn payload_processor_roots() -> &'static Mutex<HashMap<PayloadProcessorKey, PayloadProcessorStateRootEntry>> {
    PAYLOAD_PROCESSOR_ROOTS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn payload_processor_roots_cv() -> &'static Condvar {
    PAYLOAD_PROCESSOR_ROOTS_CV.get_or_init(Condvar::new)
}

fn payload_processor_started() -> &'static Mutex<HashSet<PayloadProcessorKey>> {
    PAYLOAD_PROCESSOR_STARTED.get_or_init(|| Mutex::new(HashSet::new()))
}

fn parallel_state_roots() -> &'static Mutex<HashMap<PayloadProcessorKey, ParallelStateRootResult>> {
    PARALLEL_STATE_ROOTS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn payload_processor_hook_drops() -> &'static Mutex<HashMap<PayloadProcessorKey, Instant>> {
    PAYLOAD_PROCESSOR_HOOK_DROPS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Record when the payload_processor hook sender is dropped (after system tx execution).
pub fn insert_payload_processor_hook_drop(key: PayloadProcessorKey, at: Instant) {
    payload_processor_hook_drops().lock().insert(key, at);
}

/// Take the recorded hook-drop instant (if any).
pub fn take_payload_processor_hook_drop(key: PayloadProcessorKey) -> Option<Instant> {
    payload_processor_hook_drops().lock().remove(&key)
}

pub fn take_payload_processor_state_root(
    key: PayloadProcessorKey,
) -> Option<PayloadProcessorStateRootResult> {
    consume_payload_processor_state_root(key)
}

/// Marks that a payload_processor was started for this key.
pub fn mark_payload_processor_started(key: PayloadProcessorKey) {
    payload_processor_started().lock().insert(key);
}

/// Consumes and returns whether a payload_processor was started for this key.
///
/// This is designed to be used by the block builder to decide whether it should wait for the
/// payload_processor result (instead of falling back to serial). Using a take avoids races and
/// avoids unbounded growth of the started set.
pub fn take_payload_processor_started(key: PayloadProcessorKey) -> bool {
    payload_processor_started().lock().remove(&key)
}

/// Waits (up to `timeout`) for a payload_processor result, and removes it from the internal map.
pub fn wait_take_payload_processor_state_root(
    key: PayloadProcessorKey,
    timeout: Duration,
) -> Option<PayloadProcessorStateRootResult> {
    let deadline = Instant::now() + timeout;
    let mut guard = payload_processor_roots().lock();
    loop {
        if let Some(entry) = guard.get_mut(&key) {
            let res = entry.value.clone().ok();
            entry.remaining_consumers = entry.remaining_consumers.saturating_sub(1);
            if entry.remaining_consumers == 0 {
                guard.remove(&key);
            }
            return res;
        }

        let now = Instant::now();
        if now >= deadline {
            return None;
        }

        let remaining = deadline - now;
        let _ = payload_processor_roots_cv().wait_for(&mut guard, remaining);
    }
}

/// Waits indefinitely for a payload_processor result, and removes it from the internal map.
///
/// Callers should only use this if they have high confidence the payload_processor was started.
pub fn wait_take_payload_processor_state_root_blocking(
    key: PayloadProcessorKey,
) -> PayloadProcessorStateRootResult {
    let mut guard = payload_processor_roots().lock();
    loop {
        if let Some(entry) = guard.get_mut(&key) {
            let res = entry.value.clone().ok();
            entry.remaining_consumers = entry.remaining_consumers.saturating_sub(1);
            if entry.remaining_consumers == 0 {
                guard.remove(&key);
            }
            if let Some(res) = res {
                return res;
            }
            // If the payload_processor failed, keep waiting for a successful result.
            // NOTE: The builder should guard against this by not waiting indefinitely in the
            // authoritative path (it can fall back to serial).
        }
        payload_processor_roots_cv().wait(&mut guard);
    }
}

fn take_parallel_state_root(key: PayloadProcessorKey) -> Option<ParallelStateRootResult> {
    parallel_state_roots().lock().remove(&key)
}

fn consume_payload_processor_state_root(key: PayloadProcessorKey) -> Option<PayloadProcessorStateRootResult> {
    let mut guard = payload_processor_roots().lock();
    let entry = guard.get_mut(&key)?;
    let res = entry.value.clone().ok();
    entry.remaining_consumers = entry.remaining_consumers.saturating_sub(1);
    if entry.remaining_consumers == 0 {
        guard.remove(&key);
    }
    res
}

pub fn insert_payload_processor_state_root(
    key: PayloadProcessorKey,
    value: PayloadProcessorStateRootResult,
) {
    payload_processor_roots().lock().insert(
        key,
        PayloadProcessorStateRootEntry { value: Ok(value), remaining_consumers: 2 },
    );
    payload_processor_roots_cv().notify_all();
}

pub fn insert_payload_processor_state_root_error(key: PayloadProcessorKey, err: String) {
    payload_processor_roots().lock().insert(
        key,
        PayloadProcessorStateRootEntry { value: Err(err), remaining_consumers: 2 },
    );
    payload_processor_roots_cv().notify_all();
}

fn insert_parallel_state_root(key: PayloadProcessorKey, value: ParallelStateRootResult) {
    parallel_state_roots().lock().insert(key, value);
}

/// Shared state used to correlate serial/accelerated state-root computations.
#[derive(Debug, Default, Clone, Copy)]
pub struct StateRootCompareState {
    pub block_hash: Option<B256>,
    pub user_tx_len: Option<usize>,
    pub system_tx_len: Option<usize>,
    pub total_tx_len: Option<usize>,
    /// Approx wall time spent executing the block (txs + system txs), excluding state-root hashing.
    ///
    /// Measured in `builder.finish()` as time spent before starting `state_root_with_updates`.
    pub execution_duration_ms: Option<u128>,
    pub serial_root: Option<B256>,
    pub serial_duration_ms: Option<u128>,
}

#[derive(Debug, Default)]
struct StateRootCompareStats {
    samples: u64,
    serial_fastest: u64,
    parallel_fastest: u64,
    payload_processor_fastest: u64,
    tie: u64,

    sum_total_tx_len: u128,
    sum_execution_ms: u128,
    sum_serial_ms: u128,
    sum_parallel_ms: u128,
    sum_payload_processor_total_ms: u128,
    sum_payload_processor_post_exec_ms: u128,
}

static STATE_ROOT_COMPARE_STATS: OnceLock<Arc<Mutex<StateRootCompareStats>>> = OnceLock::new();
static STATE_ROOT_COMPARE_PRINTER_STARTED: OnceLock<()> = OnceLock::new();

impl RootDebugger {
    fn compare_stats() -> Arc<Mutex<StateRootCompareStats>> {
        STATE_ROOT_COMPARE_STATS
            .get_or_init(|| Arc::new(Mutex::new(StateRootCompareStats::default())))
            .clone()
    }

    fn start_compare_printer_thread_once() {
        STATE_ROOT_COMPARE_PRINTER_STARTED.get_or_init(|| {
            let stats = Self::compare_stats();
            std::thread::spawn(move || loop {
                std::thread::sleep(Duration::from_secs(30));
                let snapshot = {
                    let s = stats.lock();
                    (
                        s.samples,
                        s.serial_fastest,
                        s.parallel_fastest,
                        s.payload_processor_fastest,
                        s.tie,
                        s.sum_total_tx_len,
                        s.sum_execution_ms,
                        s.sum_serial_ms,
                        s.sum_parallel_ms,
                        s.sum_payload_processor_total_ms,
                        s.sum_payload_processor_post_exec_ms,
                    )
                };

                let (
                    samples,
                    serial_fastest,
                    parallel_fastest,
                    payload_processor_fastest,
                    tie,
                    sum_total_tx_len,
                    sum_execution_ms,
                    sum_serial_ms,
                    sum_parallel_ms,
                    sum_payload_processor_total_ms,
                    sum_payload_processor_post_exec_ms,
                ) =
                    snapshot;

                if samples == 0 {
                    continue;
                }

                let avg_tx = (sum_total_tx_len as f64) / (samples as f64);
                let avg_execution_ms = (sum_execution_ms as f64) / (samples as f64);
                let avg_serial_ms = (sum_serial_ms as f64) / (samples as f64);
                let avg_parallel_ms = (sum_parallel_ms as f64) / (samples as f64);
                let avg_payload_processor_total_ms =
                    (sum_payload_processor_total_ms as f64) / (samples as f64);
                let avg_payload_processor_post_exec_ms =
                    (sum_payload_processor_post_exec_ms as f64) / (samples as f64);

                tracing::info!(
                    target: "bsc::builder",
                    samples,
                    serial_fastest,
                    parallel_fastest,
                    payload_processor_fastest,
                    tie,
                    avg_total_tx_len = avg_tx,
                    avg_execution_ms = avg_execution_ms,
                    avg_serial_ms = avg_serial_ms,
                    avg_parallel_ms = avg_parallel_ms,
                    avg_payload_processor_total_ms = avg_payload_processor_total_ms,
                    avg_payload_processor_post_exec_ms = avg_payload_processor_post_exec_ms,
                    "State root compare summary (serial/parallel/payload_processor)"
                );
            });
        });
    }

    fn wait_for_compare_state(
        state: &Mutex<StateRootCompareState>,
    ) -> StateRootCompareState {
        let mut tries = 0usize;
        loop {
            let snapshot = *state.lock();
            if snapshot.block_hash.is_some()
                && snapshot.serial_root.is_some()
                && snapshot.serial_duration_ms.is_some()
            {
                return snapshot
            }
            tries += 1;
            if tries >= 500 {
                // ~5s with 10ms sleep
                return snapshot
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn record_compare_sample(
        snapshot: StateRootCompareState,
        parallel_ms: u128,
        payload_total_ms: u128,
        payload_post_exec_ms: u128,
    ) {
        let Some(serial_ms) = snapshot.serial_duration_ms else { return };
        let total_tx_len = snapshot.total_tx_len.unwrap_or(0) as u128;
        let execution_ms = snapshot.execution_duration_ms.unwrap_or(0);

        let stats_arc = Self::compare_stats();
        let mut stats = stats_arc.lock();
        stats.samples += 1;
        stats.sum_total_tx_len += total_tx_len;
        stats.sum_execution_ms += execution_ms;
        stats.sum_serial_ms += serial_ms;
        stats.sum_parallel_ms += parallel_ms;
        stats.sum_payload_processor_total_ms += payload_total_ms;
        stats.sum_payload_processor_post_exec_ms += payload_post_exec_ms;

        let min_ms = serial_ms.min(parallel_ms).min(payload_post_exec_ms);
        let winners = [
            (serial_ms == min_ms, 0usize),
            (parallel_ms == min_ms, 1usize),
            (payload_post_exec_ms == min_ms, 2usize),
        ]
        .into_iter()
        .filter(|(is, _)| *is)
        .count();

        if winners != 1 {
            stats.tie += 1;
        } else if serial_ms == min_ms {
            stats.serial_fastest += 1;
        } else if parallel_ms == min_ms {
            stats.parallel_fastest += 1;
        } else {
            stats.payload_processor_fastest += 1;
        }
    }

    /// Spawn a background `ParallelStateRoot` computation (comparison only).
    pub fn spawn_parallel_state_root_compare<Factory>(
        provider_factory: Factory,
        key: PayloadProcessorKey,
        hashed_state: HashedPostState,
    ) where
        Factory: DatabaseProviderFactory<Provider: BlockNumReader + HeaderProvider + BlockReader>
            + Clone
            + Send
            + Sync
            + 'static,
    {
        std::thread::spawn(move || {
            let start = std::time::Instant::now();
            let parent_number = key.parent_number();
            let parent_hash = key.parent_hash();
            let trace_id = key.trace_id();
            let attempt = key.attempt();
            let res = Self::compute_parallel_state_root(
                provider_factory,
                parent_number,
                parent_hash,
                &hashed_state,
            );
            match res {
                Ok(root) => {
                    let duration_ms = start.elapsed().as_millis();
                    insert_parallel_state_root(key, ParallelStateRootResult { value: Ok(root), duration_ms });
                    tracing::debug!(
                        target: "bsc::builder",
                        trace_id,
                        attempt,
                        parent_number,
                        parent_hash = ?parent_hash,
                        state_root = ?root,
                        duration_ms,
                        "ParallelStateRoot computed state root"
                    );
                }
                Err(err) => {
                    let duration_ms = start.elapsed().as_millis();
                    insert_parallel_state_root(key, ParallelStateRootResult { value: Err(err.to_string()), duration_ms });
                    tracing::debug!(
                        target: "bsc::builder",
                        trace_id,
                        attempt,
                        parent_number,
                        parent_hash = ?parent_hash,
                        %err,
                        duration_ms,
                        "ParallelStateRoot unavailable"
                    );
                }
            }
        });
    }

    /// Spawn a background comparer that logs Serial vs Parallel vs PayloadProcessor.
    ///
    /// - Serial is authoritative (foreground).
    /// - ParallelStateRoot is computed by [`spawn_parallel_state_root_compare`].
    /// - PayloadProcessor result is produced during payload building and inserted via
    ///   [`insert_payload_processor_state_root`].
    pub fn spawn_triple_compare(
        key: PayloadProcessorKey,
        hashed_state: HashedPostState,
        compare_state: Arc<Mutex<StateRootCompareState>>,
    ) {
        // Ensure the periodic summary thread is running.
        Self::start_compare_printer_thread_once();

        std::thread::spawn(move || {
            let snapshot = Self::wait_for_compare_state(&compare_state);
            let block_hash = snapshot.block_hash;
            let user_tx_len = snapshot.user_tx_len;
            let system_tx_len = snapshot.system_tx_len;
            let total_tx_len = snapshot.total_tx_len;
            let execution_duration_ms = snapshot.execution_duration_ms;
            let serial_root = snapshot.serial_root;
            let serial_ms = snapshot.serial_duration_ms;
            let parent_number = key.parent_number();
            let parent_hash = key.parent_hash();
            let trace_id = key.trace_id();
            let attempt = key.attempt();

            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            let mut parallel_res: Option<ParallelStateRootResult> = None;
            let mut payload_res: Option<PayloadProcessorStateRootResult> = None;
            while std::time::Instant::now() < deadline
                && (parallel_res.is_none() || payload_res.is_none())
            {
                if parallel_res.is_none() {
                    parallel_res = take_parallel_state_root(key);
                }
                if payload_res.is_none() {
                    payload_res = take_payload_processor_state_root(key);
                }
                if parallel_res.is_some() && payload_res.is_some() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }

            let approx_change_units: usize = hashed_state.accounts.len()
                + hashed_state
                    .storages
                    .values()
                    .map(|s| s.storage.len())
                    .sum::<usize>();

            // If we don't have all 3, still log what we have (for debugging).
            let parallel_root = parallel_res.as_ref().and_then(|r| r.value.as_ref().ok().copied());
            let parallel_ms = parallel_res.as_ref().map(|r| r.duration_ms);
            let parallel_err = parallel_res.as_ref().and_then(|r| r.value.as_ref().err()).map(|s| s.as_str());
            let payload_root = payload_res.as_ref().map(|r| r.state_root);
            let payload_total_ms = payload_res.as_ref().map(|r| r.duration_ms);
            let payload_post_exec_ms = payload_res
                .as_ref()
                .and_then(|r| r.post_exec_duration_ms)
                .or(payload_total_ms);

            let all_present = serial_root.is_some()
                && serial_ms.is_some()
                && parallel_root.is_some()
                && parallel_ms.is_some()
                && payload_root.is_some()
                && payload_total_ms.is_some()
                && payload_post_exec_ms.is_some();

            if all_present {
                let serial_root = serial_root.expect("checked");
                let serial_ms = serial_ms.expect("checked");
                let parallel_root = parallel_root.expect("checked");
                let parallel_ms = parallel_ms.expect("checked");
                let payload_root = payload_root.expect("checked");
                let payload_total_ms = payload_total_ms.expect("checked");
                let payload_post_exec_ms = payload_post_exec_ms.expect("checked");

                // Update stats.
                Self::record_compare_sample(snapshot, parallel_ms, payload_total_ms, payload_post_exec_ms);

                let serial_eq_parallel = serial_root == parallel_root;
                let serial_eq_payload = serial_root == payload_root;
                let parallel_eq_payload = parallel_root == payload_root;
                let all_equal = serial_eq_parallel && serial_eq_payload;

                let min_ms = serial_ms.min(parallel_ms).min(payload_post_exec_ms);
                let winners = [
                    (serial_ms == min_ms, "serial"),
                    (parallel_ms == min_ms, "parallel"),
                    (payload_post_exec_ms == min_ms, "payload_processor"),
                ]
                .into_iter()
                .filter(|(is, _)| *is)
                .map(|(_, name)| name)
                .collect::<Vec<_>>();
                let fastest_side = if winners.len() == 1 { Some(winners[0]) } else { Some("tie") };

                if !all_equal {
                    tracing::error!(
                        target: "bsc::builder",
                        trace_id,
                        attempt,
                        block_number = parent_number + 1,
                        block_hash = ?block_hash,
                        user_tx_len = ?user_tx_len,
                        system_tx_len = ?system_tx_len,
                        total_tx_len = ?total_tx_len,
                        block_execution_duration_ms = ?execution_duration_ms,
                        parent_hash = ?parent_hash,
                        approx_change_units,
                        serial_state_root = ?serial_root,
                        serial_state_root_duration_ms = serial_ms,
                        parallel_state_root = ?parallel_root,
                        parallel_state_root_duration_ms = parallel_ms,
                        payload_processor_state_root = ?payload_root,
                        payload_processor_state_root_duration_ms_total = payload_total_ms,
                        payload_processor_state_root_duration_ms_post_exec = payload_post_exec_ms,
                        serial_eq_parallel,
                        serial_eq_payload,
                        parallel_eq_payload,
                        fastest_side = ?fastest_side,
                        "State root MISMATCH (serial vs parallel vs payload_processor)"
                    );
                } else {
                    tracing::debug!(
                        target: "bsc::builder",
                        trace_id,
                        attempt,
                        block_number = parent_number + 1,
                        block_hash = ?block_hash,
                        user_tx_len = ?user_tx_len,
                        system_tx_len = ?system_tx_len,
                        total_tx_len = ?total_tx_len,
                        block_execution_duration_ms = ?execution_duration_ms,
                        parent_hash = ?parent_hash,
                        approx_change_units,
                        serial_state_root = ?serial_root,
                        serial_state_root_duration_ms = serial_ms,
                        parallel_state_root = ?parallel_root,
                        parallel_state_root_duration_ms = parallel_ms,
                        payload_processor_state_root = ?payload_root,
                        payload_processor_state_root_duration_ms_total = payload_total_ms,
                        payload_processor_state_root_duration_ms_post_exec = payload_post_exec_ms,
                        fastest_side = ?fastest_side,
                        "State root comparison (serial vs parallel vs payload_processor)"
                    );
                }
            } else {
                tracing::debug!(
                    target: "bsc::builder",
                    trace_id,
                    attempt,
                    block_number = parent_number + 1,
                    block_hash = ?block_hash,
                    user_tx_len = ?user_tx_len,
                    system_tx_len = ?system_tx_len,
                    total_tx_len = ?total_tx_len,
                    block_execution_duration_ms = ?execution_duration_ms,
                    parent_hash = ?parent_hash,
                    approx_change_units,
                    serial_state_root = ?serial_root,
                    serial_state_root_duration_ms = ?serial_ms,
                    parallel_state_root = ?parallel_root,
                    parallel_state_root_duration_ms = ?parallel_ms,
                    parallel_state_root_err = ?parallel_err,
                    payload_processor_state_root = ?payload_root,
                    payload_processor_state_root_duration_ms_total = ?payload_total_ms,
                    payload_processor_state_root_duration_ms_post_exec = ?payload_post_exec_ms,
                    missing_parallel = parallel_res.is_none(),
                    missing_payload_processor = payload_res.is_none(),
                    "State root comparison incomplete (missing parallel/payload_processor result)"
                );
            }
        });
    }

    fn compute_parallel_state_root<Factory>(
        provider_factory: Factory,
        parent_number: u64,
        parent_hash: B256,
        hashed_state: &HashedPostState,
    ) -> Result<B256, BlockExecutionError>
    where
        Factory: DatabaseProviderFactory<Provider: BlockNumReader + HeaderProvider + BlockReader>
            + Clone
            + Send
            + Sync
            + 'static,
    {
        let provider_ro = provider_factory
            .database_provider_ro()
            .map_err(BlockExecutionError::other)?;
        let db_last = provider_ro.best_block_number().map_err(BlockExecutionError::other)?;

        if db_last > parent_number {
            return Err(BlockExecutionError::other(std::io::Error::other(format!(
                "db tip ({db_last}) ahead of parent ({parent_number})"
            ))));
        }

        let db_tip = provider_ro
            .sealed_header(db_last)
            .map_err(BlockExecutionError::other)?
            .ok_or_else(|| BlockExecutionError::other(std::io::Error::other("db tip missing")))?;

        // If the DB tip is exactly at the parent height but points to a different hash, then the
        // parent block we're building on is not the DB's canonical tip. In that case we cannot
        // build a consistent view for `parent_hash` (ConsistentDbView requires the tip hash to
        // match `sealed_header(number)`), so any parallel computation would be meaningless.
        if db_last == parent_number && db_tip.hash() != parent_hash {
            return Err(BlockExecutionError::other(std::io::Error::other(format!(
                "db tip hash mismatch at parent height: db_tip_hash={:?} parent_hash={:?} number={}",
                db_tip.hash(),
                parent_hash,
                parent_number
            ))));
        }

        let consistent_view = ConsistentDbView::new(provider_factory.clone(), Some((db_tip.hash(), db_last)));

        let mut trie_input = TrieInput::default();
        if db_last < parent_number {
            let cache = trie_overlay_cache().ok_or_else(|| {
                BlockExecutionError::other(std::io::Error::other("trie overlay cache not initialized"))
            })?;
            let needed_range = (db_last + 1)..=parent_number;
            let overlays = cache.read().get_range(needed_range.clone());
            if overlays.len() != (parent_number - db_last) as usize {
                return Err(BlockExecutionError::other(std::io::Error::other(format!(
                    "missing trie overlay blocks for range {:?} (have {})",
                    needed_range,
                    overlays.len()
                ))));
            }
            for entry in overlays {
                if entry.number == parent_number && entry.hash != parent_hash {
                    return Err(BlockExecutionError::other(std::io::Error::other(format!(
                        "parent hash mismatch in overlay cache: expected={:?} got={:?}",
                        parent_hash, entry.hash
                    ))));
                }
                let Some(nodes) = entry.trie_updates.as_deref() else {
                    return Err(BlockExecutionError::other(std::io::Error::other(format!(
                        "missing trie_updates for overlay block {}",
                        entry.number
                    ))));
                };
                trie_input.append_cached_ref(nodes, &entry.hashed_state);
            }
        }

        trie_input.append(hashed_state.clone());
        let (root, _updates) = ParallelStateRoot::new(consistent_view, trie_input)
            .incremental_root_with_updates()
            .map_err(BlockExecutionError::other)?;
        Ok(root)
    }
}

/// A helper that owns the canonical-chain subscription and keeps the overlay cache up to date.
///
/// This is intended to be used by the miner: miner calls a single `drain()` method per loop.
pub struct RootDebuggerUpdater<P>
where
    P: NewCanonicalChainSubscriptions + NodePrimitivesProvider + Clone + Send + Sync + 'static,
{
    rx: broadcast::Receiver<NewCanonicalChain<<P as NodePrimitivesProvider>::Primitives>>,
}

impl<P> RootDebuggerUpdater<P>
where
    P: NewCanonicalChainSubscriptions + NodePrimitivesProvider + Clone + Send + Sync + 'static,
{
    /// Initializes overlay cache and subscribes to new canonical chain updates.
    pub fn new(provider: &P, overlay_capacity: usize) -> Self {
        let _ = init_trie_overlay_cache(overlay_capacity);
        let rx = provider.subscribe_to_new_canonical_chain();
        Self { rx }
    }

    /// Drain pending canonical chain updates and apply them to the overlay cache.
    pub fn drain(&mut self) {
        loop {
            match self.rx.try_recv() {
                Ok(update) => Self::apply_update(&update),
                Err(broadcast::error::TryRecvError::Empty) => break,
                Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
                Err(broadcast::error::TryRecvError::Closed) => break,
            }
        }
    }

    fn apply_update<N: reth_primitives_traits::NodePrimitives>(event: &NewCanonicalChain<N>) {
        let Some(cache) = trie_overlay_cache() else { return };
        let mut w = cache.write();
        match event {
            NewCanonicalChain::Commit { new } => {
                for exec in new {
                    let num = exec.block.block_number();
                    let hash = exec.block.recovered_block.hash();
                    let hashed_state = Arc::clone(&exec.block.hashed_state);
                    let trie_updates = match &exec.trie {
                        ExecutedTrieUpdates::Present(updates) => Some(Arc::clone(updates)),
                        ExecutedTrieUpdates::Missing => None,
                    };
                    w.insert(TrieOverlayEntry { number: num, hash, hashed_state, trie_updates });
                }
            }
            NewCanonicalChain::Reorg { new, old } => {
                for exec in old {
                    let num = exec.block_number();
                    w.remove_range(num..=num);
                }
                for exec in new {
                    let num = exec.block.block_number();
                    let hash = exec.block.recovered_block.hash();
                    let hashed_state = Arc::clone(&exec.block.hashed_state);
                    let trie_updates = match &exec.trie {
                        ExecutedTrieUpdates::Present(updates) => Some(Arc::clone(updates)),
                        ExecutedTrieUpdates::Missing => None,
                    };
                    w.insert(TrieOverlayEntry { number: num, hash, hashed_state, trie_updates });
                }
            }
        }
    }
}

