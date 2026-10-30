//! Mining-time speculative execution over a shared, parent-scoped read cache.
//!
//! Execution journals stay private to each EVM. Only successful reads of the original parent
//! state enter this cache, so a miner can consume reads produced after its attempt started.

use alloy_consensus::Transaction;
use alloy_primitives::{
    map::{AddressMap, B256Map, HashMap},
    Address, B256, U256,
};
use parking_lot::{Mutex, RwLock};
use reth_ethereum_primitives::TransactionSigned;
use reth_evm::{ConfigureEvm, Evm, EvmEnvFor};
use reth_provider::{EvmStateProviderAdapter, StateProviderFactory};
use reth_revm::{database::StateProviderDatabase, db::State};
use reth_tasks::ManualCancel;
use reth_transaction_pool::{
    BestTransactions, BestTransactionsAttributes, PoolTransaction, TransactionPool,
};
use revm::{bytecode::Bytecode, state::AccountInfo, Database, DatabaseCommit, DatabaseRef};
use std::{
    cell::Cell,
    collections::hash_map::Entry,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc, OnceLock,
    },
    time::{Duration, Instant},
};

const SHARDS: usize = 16;
// Entry count is not a byte limit. Code has a separate byte admission budget; map/allocator
// overhead is not included. Reaching either cap only stops admissions, never state reads.
const MAX_ENTRIES: usize = 131_072;
const MAX_CODE_BYTES: usize = 32 * 1024 * 1024;
static ENABLED: AtomicBool = AtomicBool::new(false);

/// One immutable parent identity, with concurrently populated original-state reads.
#[derive(Debug)]
pub(crate) struct SharedStateCache {
    parent: B256,
    shards: [RwLock<Shard>; SHARDS],
    entries: AtomicUsize,
    code_bytes: AtomicUsize,
    selected: RwLock<AddressMap<u64>>,
    phase: AtomicUsize,
}

#[derive(Debug, Default)]
struct Shard {
    accounts: AddressMap<Option<AccountInfo>>,
    storage: HashMap<(Address, U256), U256>,
    code: B256Map<Bytecode>,
    // Only sampled keys have metadata. Bounded separately from the value admission budget.
    samples: HashMap<SampleKey, WarmSample>,
    admissions: [[[u64; 3]; 3]; 2], // producer(prewarm/miner), resource, admitted/duplicate/limit
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum SampleKey {
    Account(Address),
    Storage(Address, U256),
    Code(B256),
}
impl SampleKey {
    fn sampled(self) -> bool {
        match self {
            Self::Account(address) => address[19] == 0,
            Self::Storage(address, slot) => address[19] ^ slot.as_limbs()[0] as u8 == 0,
            Self::Code(hash) => hash[31] == 0,
        }
    }
    const fn resource_index(self) -> usize {
        match self {
            Self::Account(_) => 0,
            Self::Storage(..) => 1,
            Self::Code(_) => 2,
        }
    }
}
#[derive(Debug)]
struct WarmSample {
    admitted: Instant,
    first_use: OnceLock<(Duration, &'static str)>,
}
impl Shard {
    fn admitted(&mut self, producer: usize, resource: usize, key: SampleKey) {
        self.admissions[producer][resource][0] += 1;
        if producer == 0 && key.sampled() && self.samples.len() < 32 {
            self.samples
                .insert(key, WarmSample { admitted: Instant::now(), first_use: OnceLock::new() });
        }
    }
    fn used(&self, key: SampleKey, phase: &'static str) {
        if key.sampled() {
            if let Some(sample) = self.samples.get(&key) {
                sample.first_use.get_or_init(|| (sample.admitted.elapsed(), phase));
            }
        }
    }
}

impl SharedStateCache {
    pub(crate) fn new(parent: B256) -> Arc<Self> {
        Arc::new(Self {
            parent,
            shards: Default::default(),
            entries: AtomicUsize::new(0),
            code_bytes: AtomicUsize::new(0),
            selected: Default::default(),
            phase: AtomicUsize::new(0),
        })
    }

    pub(crate) const fn parent(&self) -> B256 {
        self.parent
    }

    pub(crate) fn execution_started(&self) {
        self.phase.store(1, Ordering::Relaxed);
    }
    pub(crate) fn execution_finished(&self) {
        self.phase.store(2, Ordering::Relaxed);
    }
    fn phase(&self) -> &'static str {
        ["prepare", "execution", "finish"][self.phase.load(Ordering::Relaxed)]
    }

    pub(crate) fn miner_account(&self, address: Address) -> Option<Option<AccountInfo>> {
        let shard = self.shards[address[0] as usize % SHARDS].read();
        let value = shard.accounts.get(&address).cloned();
        if value.is_some() {
            shard.used(SampleKey::Account(address), self.phase());
        }
        value
    }
    pub(crate) fn miner_storage(&self, address: Address, slot: U256) -> Option<U256> {
        let shard = self.shards[address[0] as usize % SHARDS].read();
        let value = shard.storage.get(&(address, slot)).copied();
        if value.is_some() {
            shard.used(SampleKey::Storage(address, slot), self.phase());
        }
        value
    }
    pub(crate) fn miner_code(&self, hash: B256) -> Option<Bytecode> {
        let shard = self.shards[hash[0] as usize % SHARDS].read();
        let value = shard.code.get(&hash).cloned();
        if value.is_some() {
            shard.used(SampleKey::Code(hash), self.phase());
        }
        value
    }

    fn mark_selected(&self, sender: Address, nonce: u64) {
        let mut selected = self.selected.write();
        if let Some(previous) = selected.get_mut(&sender) {
            *previous = (*previous).max(nonce);
        } else if selected.len() < MAX_ENTRIES {
            selected.insert(sender, nonce);
        }
    }

    fn already_selected(&self, sender: Address, nonce: u64) -> bool {
        self.selected.read().get(&sender).is_some_and(|last| nonce <= *last)
    }

    pub(crate) fn account(&self, address: Address) -> Option<Option<AccountInfo>> {
        self.shards[address[0] as usize % SHARDS].read().accounts.get(&address).cloned()
    }

    pub(crate) fn storage(&self, address: Address, slot: U256) -> Option<U256> {
        self.shards[address[0] as usize % SHARDS].read().storage.get(&(address, slot)).copied()
    }

    pub(crate) fn code(&self, hash: B256) -> Option<Bytecode> {
        self.shards[hash[0] as usize % SHARDS].read().code.get(&hash).cloned()
    }

    fn reserve_entry(&self) -> bool {
        reserve(&self.entries, 1, MAX_ENTRIES)
    }

    pub(crate) fn insert_account(&self, address: Address, value: Option<AccountInfo>) {
        self.insert_account_from(address, value, 1);
    }
    fn insert_account_from(&self, address: Address, value: Option<AccountInfo>, producer: usize) {
        let mut shard = self.shards[address[0] as usize % SHARDS].write();
        let outcome = match shard.accounts.entry(address) {
            Entry::Occupied(_) => 1,
            Entry::Vacant(entry) => {
                if self.reserve_entry() {
                    entry.insert(value);
                    0
                } else {
                    2
                }
            }
        };
        if outcome == 0 {
            shard.admitted(producer, 0, SampleKey::Account(address));
        } else {
            shard.admissions[producer][0][outcome] += 1;
        }
    }
    pub(crate) fn insert_storage(&self, address: Address, slot: U256, value: U256) {
        self.insert_storage_from(address, slot, value, 1);
    }
    fn insert_storage_from(&self, address: Address, slot: U256, value: U256, producer: usize) {
        let mut shard = self.shards[address[0] as usize % SHARDS].write();
        let outcome = match shard.storage.entry((address, slot)) {
            Entry::Occupied(_) => 1,
            Entry::Vacant(entry) => {
                if self.reserve_entry() {
                    entry.insert(value);
                    0
                } else {
                    2
                }
            }
        };
        if outcome == 0 {
            shard.admitted(producer, 1, SampleKey::Storage(address, slot));
        } else {
            shard.admissions[producer][1][outcome] += 1;
        }
    }
    pub(crate) fn insert_code(&self, hash: B256, value: Bytecode) {
        self.insert_code_from(hash, value, 1);
    }
    fn insert_code_from(&self, hash: B256, value: Bytecode, producer: usize) {
        let mut shard = self.shards[hash[0] as usize % SHARDS].write();
        if shard.code.contains_key(&hash) {
            shard.admissions[producer][2][1] += 1;
            return
        }
        let bytes = value.original_bytes().len();
        if !reserve(&self.code_bytes, bytes, MAX_CODE_BYTES) {
            shard.admissions[producer][2][2] += 1;
            return
        }
        if self.reserve_entry() {
            shard.code.insert(hash, value);
            shard.admitted(producer, 2, SampleKey::Code(hash));
        } else {
            self.code_bytes.fetch_sub(bytes, Ordering::Relaxed);
            shard.admissions[producer][2][2] += 1;
        }
    }
}

impl Drop for SharedStateCache {
    fn drop(&mut self) {
        // Arc has no remaining users: export in batches without any per-read metric update
        // or additional cache lock. Cache lifetime can extend beyond the miner's finish.
        let mut admissions = [[[0u64; 3]; 3]; 2];
        let mut samples = [[0u64; 2]; 3];
        let mut ages = [[(0u64, 0u64, 0.0f64); 3]; 3];
        for shard in &mut self.shards {
            let shard = shard.get_mut();
            for (p, resources) in shard.admissions.iter().enumerate() {
                for (r, outcomes) in resources.iter().enumerate() {
                    for (o, n) in outcomes.iter().enumerate() {
                        admissions[p][r][o] += n;
                    }
                }
            }
            for (key, sample) in &shard.samples {
                let resource = key.resource_index();
                if let Some((age, phase)) = sample.first_use.get() {
                    samples[resource][0] += 1;
                    let phase = match *phase {
                        "prepare" => 0,
                        "execution" => 1,
                        _ => 2,
                    };
                    let stats = &mut ages[resource][phase];
                    stats.0 += 1;
                    stats.1 =
                        stats.1.saturating_add(u64::try_from(age.as_nanos()).unwrap_or(u64::MAX));
                    stats.2 = stats.2.max(age.as_secs_f64());
                } else {
                    samples[resource][1] += 1;
                }
            }
        }
        for (r, outcomes) in samples.iter().enumerate() {
            let resource = ["account", "storage", "code"][r];
            for (o, count) in outcomes.iter().enumerate() {
                if *count > 0 {
                    metrics::counter!("bsc.miner_prewarm.handoff_samples_total", "resource" => resource, "result" => ["used", "unobserved"][o]).increment(*count);
                }
            }
            for (p, (count, sum, max)) in ages[r].iter().enumerate() {
                if *count > 0 {
                    let phase = ["prepare", "execution", "finish"][p];
                    metrics::counter!("bsc.miner_prewarm.handoff_uses_total", "resource" => resource, "phase" => phase).increment(*count);
                    metrics::counter!("bsc.miner_prewarm.handoff_age_nanoseconds_total", "resource" => resource, "phase" => phase).increment(*sum);
                    metrics::histogram!("bsc.miner_prewarm.handoff_max_age_seconds", "resource" => resource, "phase" => phase).record(*max);
                }
            }
        }

        for (p, resources) in admissions.iter().enumerate() {
            for (r, outcomes) in resources.iter().enumerate() {
                for (o, n) in outcomes.iter().enumerate() {
                    if *n > 0 {
                        metrics::counter!("bsc.miner_prewarm.cache_admissions_total", "producer" => ["prewarm", "miner"][p], "resource" => ["account", "storage", "code"][r], "result" => ["admitted", "duplicate", "limit"][o]).increment(*n);
                    }
                }
            }
        }
        metrics::histogram!("bsc.miner_prewarm.cache_entries")
            .record(*self.entries.get_mut() as f64);
        metrics::histogram!("bsc.miner_prewarm.cache_code_bytes")
            .record(*self.code_bytes.get_mut() as f64);
    }
}

#[allow(deprecated, reason = "AtomicUsize::try_update is newer than the Reth v2.8 MSRV")]
fn reserve(counter: &AtomicUsize, amount: usize, limit: usize) -> bool {
    counter
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            current.checked_add(amount).filter(|next| *next <= limit)
        })
        .is_ok()
}

/// Read-through adapter used by speculative workers. It never receives execution writes.
#[derive(Debug)]
pub(crate) struct SharedStateDb<DB> {
    pub(crate) inner: DB,
    pub(crate) cache: Arc<SharedStateCache>,
}

impl<DB: DatabaseRef> DatabaseRef for SharedStateDb<DB> {
    type Error = DB::Error;
    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        if let Some(value) = self.cache.account(address) {
            return Ok(value)
        }
        let value = self.inner.basic_ref(address)?;
        self.cache.insert_account_from(address, value.clone(), 0);
        Ok(value)
    }
    fn storage_ref(&self, address: Address, slot: U256) -> Result<U256, Self::Error> {
        if let Some(value) = self.cache.storage(address, slot) {
            return Ok(value)
        }
        let value = self.inner.storage_ref(address, slot)?;
        self.cache.insert_storage_from(address, slot, value, 0);
        Ok(value)
    }
    fn code_by_hash_ref(&self, hash: B256) -> Result<Bytecode, Self::Error> {
        if let Some(value) = self.cache.code(hash) {
            return Ok(value)
        }
        let value = self.inner.code_by_hash_ref(hash)?;
        self.cache.insert_code_from(hash, value.clone(), 0);
        Ok(value)
    }
    fn block_hash_ref(&self, number: u64) -> Result<B256, Self::Error> {
        self.inner.block_hash_ref(number)
    }
}

pub(crate) fn configure(enabled: bool) {
    let _ = configured_lookahead();
    // Allocate OS threads during node setup, never on the first miner execution path.
    ENABLED.store(
        enabled && (configured_worker_count() == 0 || workers().is_some()),
        Ordering::Relaxed,
    );
}
pub(crate) fn cache_for_attempt(parent: B256) -> Option<Arc<SharedStateCache>> {
    ENABLED.load(Ordering::Relaxed).then(|| SharedStateCache::new(parent))
}

/// Dropping an attempt stops future speculative transactions without joining a slow EVM call.
pub(crate) struct PrewarmGuard {
    stopped: Arc<AtomicBool>,
    cache: Arc<SharedStateCache>,
    stop_requested: Arc<OnceLock<Instant>>,
    admission: Arc<Admission>,
}
impl PrewarmGuard {
    pub(crate) fn selected(&self, sender: Address, nonce: u64) {
        if self.admission.lookahead != 0 {
            self.admission.miner_scanned.fetch_add(1, Ordering::Relaxed);
        }
        self.cache.mark_selected(sender, nonce);
    }

    pub(crate) fn remaining_gas(&self, gas: u64) {
        if self.admission.lookahead != 0 {
            self.admission.remaining_gas.store(gas, Ordering::Relaxed);
        }
    }
}

/// Admission hints only: the miner never waits or changes transaction validity.
/// Cursor distances are approximate because the two pool iterators can diverge.
struct Admission {
    lookahead: usize,
    miner_scanned: AtomicUsize,
    remaining_gas: AtomicU64,
}
impl Admission {
    fn new(lookahead: usize, gas: u64) -> Self {
        Self { lookahead, miner_scanned: AtomicUsize::new(0), remaining_gas: AtomicU64::new(gas) }
    }
    fn ahead(&self, scanned: usize) -> bool {
        self.lookahead != 0 &&
            scanned.saturating_sub(self.miner_scanned.load(Ordering::Relaxed)) >= self.lookahead
    }
    fn fits(&self, gas: u64) -> bool {
        self.lookahead == 0 || gas <= self.remaining_gas.load(Ordering::Relaxed)
    }
}

fn configured_lookahead() -> usize {
    static LIMIT: OnceLock<usize> = OnceLock::new();
    *LIMIT.get_or_init(|| {
        // Preserve existing admission unless the operator explicitly enables a bound.
        let value = std::env::var("BSC_MINING_PREWARM_LOOKAHEAD").ok();
        let limit = match parse_lookahead(value.as_deref()) {
            Ok(limit) => limit,
            Err(reason) => {
                tracing::warn!(target: "bsc::miner", ?value, reason, "Invalid prewarm lookahead; leaving admission unbounded");
                0
            }
        };
        metrics::gauge!("bsc.miner_prewarm.configured_lookahead").set(limit as f64);
        limit
    })
}

fn parse_lookahead(value: Option<&str>) -> Result<usize, &'static str> {
    let limit = value
        .map_or(Ok(0), |value| value.trim().parse::<usize>())
        .map_err(|_| "expected a nonnegative integer")?;
    if limit > 1_000_000 {
        return Err("lookahead exceeds 1000000");
    }
    Ok(limit)
}

impl Drop for PrewarmGuard {
    fn drop(&mut self) {
        self.cache.execution_finished();
        self.stop_requested.get_or_init(Instant::now);
        self.stopped.store(true, Ordering::Relaxed);
    }
}

struct JobObservation {
    started: Instant,
    stop_requested: Arc<OnceLock<Instant>>,
    stage: &'static str,
}
impl JobObservation {
    fn start(queued: Instant, stop_requested: Arc<OnceLock<Instant>>, stage: &'static str) -> Self {
        metrics::histogram!("bsc.miner_prewarm.job_queue_seconds", "stage" => stage)
            .record(queued.elapsed().as_secs_f64());
        Self { started: Instant::now(), stop_requested, stage }
    }
}
impl Drop for JobObservation {
    fn drop(&mut self) {
        metrics::histogram!("bsc.miner_prewarm.job_wall_seconds", "stage" => self.stage)
            .record(self.started.elapsed().as_secs_f64());
        if let Some(stopped) = self.stop_requested.get() {
            metrics::histogram!("bsc.miner_prewarm.guard_stop_to_exit_seconds", "stage" => self.stage).record(stopped.elapsed().as_secs_f64());
        }
    }
}

#[derive(Default)]
struct CandidateObservation {
    selected: Cell<u64>,
    parent_consumed: Cell<u64>,
    miner_consumed: Cell<u64>,
    tip_filtered: Cell<u64>,
    iterator_wait: Cell<Duration>,
    ahead_wait: Cell<Duration>,
    gas_filtered: Cell<u64>,
    max_lead: Cell<usize>,
}
impl Drop for CandidateObservation {
    fn drop(&mut self) {
        for (result, count) in [
            ("selected", self.selected.get()),
            ("parent_consumed", self.parent_consumed.get()),
            ("miner_consumed", self.miner_consumed.get()),
            ("tip_filtered", self.tip_filtered.get()),
            ("gas_filtered", self.gas_filtered.get()),
        ] {
            if count > 0 {
                metrics::counter!("bsc.miner_prewarm.candidates_total", "result" => result)
                    .increment(count);
            }
        }
        metrics::histogram!("bsc.miner_prewarm.max_cursor_lead").record(self.max_lead.get() as f64);
        metrics::histogram!("bsc.miner_prewarm.lookahead_wait_seconds")
            .record(self.ahead_wait.get().as_secs_f64());
        metrics::histogram!("bsc.miner_prewarm.iterator_lock_wait_seconds")
            .record(self.iterator_wait.get().as_secs_f64());
    }
}

struct Workers {
    pool: rayon::ThreadPool,
    active: AtomicUsize,
}
struct WorkerPermit(&'static Workers);
impl Drop for WorkerPermit {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::Relaxed);
        metrics::gauge!("bsc.miner_prewarm.active_workers").decrement(1.0);
    }
}

fn worker_count(mev: bool, cpus: usize) -> usize {
    if mev {
        3
    } else {
        3.max(cpus.saturating_mul(3) / 5)
    }
}

fn configured_worker_count() -> usize {
    static COUNT: OnceLock<usize> = OnceLock::new();
    *COUNT.get_or_init(|| {
        let mev = super::config::get_global_mining_config()
            .is_some_and(|cfg| cfg.get_bid_block_enabled());
        super::perf_tuning::prewarm_threads(worker_count(
            mev,
            std::thread::available_parallelism().map_or(1, usize::from),
        ))
    })
}

fn workers() -> Option<&'static Workers> {
    static WORKERS: OnceLock<Option<Workers>> = OnceLock::new();
    WORKERS.get_or_init(|| {
        let count = configured_worker_count();
        if count == 0 { return None }
        match rayon::ThreadPoolBuilder::new().num_threads(count)
            .thread_name(|i| format!("miner-prewarm-{i}")).build() {
            Ok(pool) => Some(Workers { pool, active: AtomicUsize::new(0) }),
            Err(error) => {
                tracing::warn!(target: "bsc::miner", %error, "Unable to start mining prewarm workers");
                None
            }
        }
    }).as_ref()
}

/// Open on the prewarm worker itself: MDBX read transactions must not cross threads.
/// Private parents use the same appended state as formal execution, without importing it.
#[derive(Clone)]
pub(crate) struct ParentState {
    open:
        Arc<dyn Fn() -> reth_errors::ProviderResult<reth_provider::StateProviderBox> + Send + Sync>,
    consumed: Option<Arc<AddressMap<u64>>>,
}
impl ParentState {
    pub(crate) fn canonical(
        client: impl StateProviderFactory + Clone + 'static,
        hash: B256,
    ) -> Self {
        let factory = Arc::new(once_cell::sync::OnceCell::new());
        Self {
            open: Arc::new(move || {
                if super::pipeline::enabled() {
                    // Capture the parent view once per attempt, shared by all prewarm
                    // readers. Each invocation still opens its own read transaction.
                    let factory =
                        factory.get_or_try_init(|| crate::shared::miner_state_factory(hash))?;
                    factory()
                } else {
                    client.state_by_block_hash(hash)
                }
            }),
            consumed: None,
        }
    }

    pub(crate) fn private(
        parent: crate::shared::PrivateMinerChain,
    ) -> reth_errors::ProviderResult<Self> {
        let open = crate::shared::private_miner_state_factory(parent.clone())?;
        Ok(Self::with_private(parent, open))
    }
    fn with_private(
        parent: crate::shared::PrivateMinerChain,
        open: crate::shared::PrivateMinerStateFactory,
    ) -> Self {
        let mut consumed = AddressMap::<u64>::default();
        for block in parent.blocks() {
            for (sender, tx) in block.recovered_block().transactions_with_sender() {
                consumed
                    .entry(*sender)
                    .and_modify(|n| *n = (*n).max(tx.nonce()))
                    .or_insert(tx.nonce());
            }
        }
        Self { open, consumed: Some(Arc::new(consumed)) }
    }
    pub(crate) fn open(&self) -> reth_errors::ProviderResult<reth_provider::StateProviderBox> {
        (self.open)()
    }

    /// The private parent's transactions may still be pending until canonical import.
    /// Skip only its consumed nonces locally; never invalidate the pool's descendants.
    pub(crate) fn consumed(&self, sender: Address, nonce: u64) -> bool {
        self.consumed
            .as_ref()
            .is_some_and(|nonces| nonces.get(&sender).is_some_and(|n| nonce <= *n))
    }
}

/// Starts the go-bsc-style mining prefetch alongside the actual transaction loop.
///
/// A shared iterator assigns each candidate once. Unlike a dispatcher with a buffered queue,
/// at most one candidate per worker is outstanding. Every worker owns its provider and EVM.
/// Process-wide permits prevent old, slow attempts from accumulating queued worker jobs.
#[allow(clippy::too_many_arguments)]
pub(crate) fn start<Pool, EvmConfig>(
    cache: Option<Arc<SharedStateCache>>,
    pool: Pool,
    parent_state: ParentState,
    evm_config: EvmConfig,
    mut env: EvmEnvFor<EvmConfig>,
    base_fee: u64,
    blob_fee: Option<u64>,
    min_tip: u128,
    remaining_gas: u64,
    cancel: ManualCancel,
    control: Option<super::build_control::BuildControl>,
) -> Option<PrewarmGuard>
where
    EvmConfig: ConfigureEvm + 'static,
    EvmConfig::Primitives: reth_primitives_traits::NodePrimitives<SignedTx = TransactionSigned>,
    Pool: TransactionPool<Transaction: PoolTransaction<Consensus = TransactionSigned>> + 'static,
{
    let cache = cache?;
    let workers = workers()?;
    // Reserve the setup job as well: cancelled attempts cannot build up an unbounded queue.
    if !reserve(&workers.active, 1, workers.pool.current_num_threads()) {
        metrics::counter!("bsc.miner_prewarm.workers_total", "result" => "busy").increment(1);
        return None
    }
    let setup_permit = WorkerPermit(workers);
    metrics::gauge!("bsc.miner_prewarm.active_workers").increment(1.0);
    let stopped = Arc::new(AtomicBool::new(false));
    let stop_requested = Arc::new(OnceLock::new());
    let admission = Arc::new(Admission::new(configured_lookahead(), remaining_gas));
    let guard = PrewarmGuard {
        stopped: stopped.clone(),
        cache: cache.clone(),
        stop_requested: stop_requested.clone(),
        admission: admission.clone(),
    };
    let queued = Instant::now();
    workers.pool.spawn(move || {
        let setup_permit = setup_permit;
        let _observation = JobObservation::start(queued, stop_requested.clone(), "setup");
        let should_stop = || stopped.load(Ordering::Relaxed) || cancel.is_cancelled() ||
            control.as_ref().is_some_and(|c| c.stop_transactions(std::time::Instant::now()));
        if should_stop() { return }
        // Pool iteration can wait for maintenance. Keep both creation and initial Peek off
        // the miner thread, and recheck cancellation before opening any state provider.
        let iterator_started = Instant::now();
        let mut best = pool.best_transactions_with_attributes(
            BestTransactionsAttributes::new(base_fee, blob_fee));
        best.skip_blobs();
        let first = best.next();
        metrics::histogram!("bsc.miner_prewarm.iterator_setup_seconds").record(iterator_started.elapsed().as_secs_f64());
        let Some(first) = first else { return };
        if should_stop() { return }
        let candidates = Arc::new(Mutex::new((std::iter::once(first).chain(best), 0usize)));
        // Transfer capacity from setup to execution jobs in the same independent pool.
        drop(setup_permit);
        env.cfg_env.disable_nonce_check = true;
        for _ in 0..workers.pool.current_num_threads() {
            if !reserve(&workers.active, 1, workers.pool.current_num_threads()) {
                metrics::counter!("bsc.miner_prewarm.workers_total", "result" => "busy").increment(1);
                break
            }
            let permit = WorkerPermit(workers);
            metrics::gauge!("bsc.miner_prewarm.active_workers").increment(1.0);
            let (cache, parent_state, config, env, candidates, stopped, cancel, control) = (
                cache.clone(),
                parent_state.clone(),
                evm_config.clone(),
                env.clone(),
                candidates.clone(),
                stopped.clone(),
                cancel.clone(),
                control.clone(),
            );
            let admission = admission.clone();
            let queued = Instant::now();
            let stop_requested = stop_requested.clone();
            workers.pool.spawn(move || {
                let _observation = JobObservation::start(queued, stop_requested, "execution");
                let _permit = permit;
                let should_stop = || stopped.load(Ordering::Relaxed) || cancel.is_cancelled() ||
                    control.as_ref().is_some_and(|c| c.stop_transactions(std::time::Instant::now()));
                if should_stop() { return }
                let provider_started = Instant::now();
                let provider = match (parent_state.open)() {
                    Ok(provider) => provider,
                    Err(_) => {
                        metrics::counter!("bsc.miner_prewarm.workers_total", "result" => "provider_error").increment(1);
                        return
                    }
                };
                metrics::histogram!("bsc.miner_prewarm.provider_open_seconds").record(provider_started.elapsed().as_secs_f64());
                metrics::counter!("bsc.miner_prewarm.workers_total", "result" => "started").increment(1);
                let db = SharedStateDb { inner: StateProviderDatabase::new(EvmStateProviderAdapter(provider)), cache };
                let progress = db.cache.clone();
                let candidates_observed = CandidateObservation::default();
                let next = || loop {
                    if should_stop() { return None }
                    let waiting = Instant::now();
                    let mut iterator = candidates.lock();
                    candidates_observed.iterator_wait.set(candidates_observed.iterator_wait.get() + waiting.elapsed());
                    let lead = iterator.1.saturating_sub(admission.miner_scanned.load(Ordering::Relaxed));
                    candidates_observed.max_lead.set(candidates_observed.max_lead.get().max(lead));
                    if admission.ahead(iterator.1) {
                        drop(iterator);
                        // Only background workers pause, outside the iterator lock. Recheck
                        // cancellation/deadline every millisecond; never join from the miner.
                        let paused = Instant::now();
                        std::thread::sleep(Duration::from_millis(1));
                        candidates_observed.ahead_wait.set(candidates_observed.ahead_wait.get() + paused.elapsed());
                        continue
                    }
                    let tx = iterator.0.next();
                    if tx.is_some() { iterator.1 += 1; }
                    drop(iterator);
                    let tx = tx?;
                    // go-bsc periodically forwards its prefetch iterator to the miner's cursor.
                    // Skip consumed sender nonces without changing the actual pool or its ordering.
                    if parent_state.consumed(tx.sender(), tx.nonce()) {
                        candidates_observed.parent_consumed.set(candidates_observed.parent_consumed.get() + 1);
                        continue
                    }
                    if progress.already_selected(tx.sender(), tx.nonce()) {
                        candidates_observed.miner_consumed.set(candidates_observed.miner_consumed.get() + 1);
                        continue
                    }
                    if !admission.fits(tx.gas_limit()) {
                        candidates_observed.gas_filtered.set(candidates_observed.gas_filtered.get() + 1);
                        continue
                    }
                    if super::super::pool::tip::meets_tip_floor(tx.effective_tip_per_gas(base_fee), min_tip) {
                        candidates_observed.selected.set(candidates_observed.selected.get() + 1);
                        return Some(tx.transaction.clone_into_consensus());
                    }
                    candidates_observed.tip_filtered.set(candidates_observed.tip_filtered.get() + 1);
                };
                let (completed, failed) = warm_transactions(db, config, env, next, should_stop);
                metrics::counter!("bsc.miner_prewarm.transactions_total", "result" => "completed").increment(completed);
                metrics::counter!("bsc.miner_prewarm.transactions_total", "result" => "error").increment(failed);
            });
        }
    });
    Some(guard)
}

struct TransactionObservation {
    cache: Arc<SharedStateCache>,
    outcomes: [[u64; 2]; 3],
}
impl Drop for TransactionObservation {
    fn drop(&mut self) {
        for (phase, outcomes) in self.outcomes.iter().enumerate() {
            for (outcome, count) in outcomes.iter().enumerate() {
                if *count > 0 {
                    metrics::counter!("bsc.miner_prewarm.transactions_by_completion_phase_total", "phase" => ["prepare", "execution", "finish"][phase], "result" => ["completed", "error"][outcome]).increment(*count);
                }
            }
        }
    }
}

/// Executes against private journals while caching only the backing parent's reads.
fn warm_transactions<DB, Config>(
    db: SharedStateDb<DB>,
    config: Config,
    env: EvmEnvFor<Config>,
    mut next: impl FnMut() -> Option<reth_primitives_traits::Recovered<TransactionSigned>>,
    should_stop: impl Fn() -> bool,
) -> (u64, u64)
where
    DB: DatabaseRef<Error: std::error::Error + Send + Sync + 'static> + std::fmt::Debug,
    Config: ConfigureEvm,
    Config::Primitives: reth_primitives_traits::NodePrimitives<SignedTx = TransactionSigned>,
{
    let mut observation = TransactionObservation { cache: db.cache.clone(), outcomes: [[0; 2]; 3] };
    let mut state = State::builder().with_database_ref(db).build();
    let mut evm = config.evm_with_env(&mut state, env);
    let (mut completed, mut failed) = (0, 0);
    while !should_stop() {
        let Some(tx) = next() else { break };
        // Read explicit touches even when nonce/fee validation prevents execution.
        let _ = evm.db_mut().basic(tx.signer());
        if let Some(to) = tx.to() {
            if let Ok(Some(account)) = evm.db_mut().basic(to) {
                let _ = evm.db_mut().code_by_hash(account.code_hash);
            }
        }
        if let Some(list) = tx.access_list() {
            for item in &list.0 {
                if should_stop() {
                    return (completed, failed)
                }
                let _ = evm.db_mut().basic(item.address);
                for slot in &item.storage_keys {
                    if should_stop() {
                        return (completed, failed)
                    }
                    let _ = evm.db_mut().storage(item.address, U256::from_be_bytes(slot.0));
                }
            }
        }
        if should_stop() {
            break
        }
        let result = evm.transact(tx);
        let phase = observation.cache.phase.load(Ordering::Relaxed);
        observation.outcomes[phase][usize::from(result.is_err())] += 1;
        match result {
            Ok(result) => {
                // Like go-bsc's worker-local StateDB, preserve speculative transitions only in
                // this worker's private State. Its backing read cache still contains parent
                // values; no write reaches that cache, the provider or the actual miner.
                evm.db_mut().commit(result.state);
                completed += 1;
            }
            Err(_) => failed += 1,
        }
    }
    (completed, failed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::{Signed, TxLegacy};
    use alloy_primitives::{Bytes, Signature, TxKind};
    use reth_primitives_traits::Recovered;
    use std::convert::Infallible;

    const SENDER: Address = Address::repeat_byte(0x11);
    const CONTRACT: Address = Address::repeat_byte(0x22);

    #[test]
    fn admission_is_bounded_and_disabled_setting_restores_legacy() {
        let admission = Admission::new(2, 100_000);
        assert!(!admission.ahead(1));
        assert!(admission.ahead(2));
        admission.miner_scanned.store(1, Ordering::Relaxed);
        assert!(!admission.ahead(2));
        assert!(admission.fits(100_000));
        assert!(!admission.fits(100_001));
        admission.remaining_gas.store(30_000, Ordering::Relaxed);
        assert!(!admission.fits(40_000));
        let disabled = Admission::new(0, 0);
        assert!(!disabled.ahead(usize::MAX));
        assert!(disabled.fits(u64::MAX));
        assert_eq!(parse_lookahead(None), Ok(0));
        assert_eq!(parse_lookahead(Some("0")), Ok(0));
        assert_eq!(parse_lookahead(Some("-1")), Err("expected a nonnegative integer"));
        assert_eq!(parse_lookahead(Some("1000001")), Err("lookahead exceeds 1000000"));
    }

    #[test]
    fn sampled_handoff_only_counts_formal_use_of_worker_produced_entries() {
        let cache = SharedStateCache::new(B256::ZERO);
        let address = Address::ZERO;
        cache.insert_account_from(address, None, 0);
        assert_eq!(cache.account(address), Some(None)); // a worker hit is not a miner use
        {
            let shard = cache.shards[0].read();
            assert!(shard.samples[&SampleKey::Account(address)].first_use.get().is_none());
        }
        cache.execution_started();
        assert_eq!(cache.miner_account(address), Some(None));
        cache.execution_finished();
        assert_eq!(cache.miner_account(address), Some(None));
        let shard = cache.shards[0].read();
        assert_eq!(
            shard.samples[&SampleKey::Account(address)].first_use.get().unwrap().1,
            "execution"
        );
        drop(shard);
        // A miner-produced entry and a later speculative duplicate must not become a warm hit.
        cache.insert_storage(address, U256::ZERO, U256::from(7));
        cache.insert_storage_from(address, U256::ZERO, U256::from(9), 0);
        assert_eq!(cache.miner_storage(address, U256::ZERO), Some(U256::from(7)));
        let shard = cache.shards[0].read();
        assert!(!shard.samples.contains_key(&SampleKey::Storage(address, U256::ZERO)));
        assert_eq!(shard.admissions[0][1][1], 1);
    }

    #[test]
    fn sample_metadata_is_bounded_and_finish_use_is_distinct() {
        let cache = SharedStateCache::new(B256::ZERO);
        for slot in 0..100u64 {
            cache.insert_storage_from(Address::ZERO, U256::from(slot * 256), U256::from(slot), 0);
        }
        cache.execution_finished();
        assert_eq!(cache.miner_storage(Address::ZERO, U256::ZERO), Some(U256::ZERO));
        let shard = cache.shards[0].read();
        assert_eq!(shard.samples.len(), 32);
        assert_eq!(shard.storage.len(), 100); // diagnostic cap cannot reject state values
        assert_eq!(
            shard.samples[&SampleKey::Storage(Address::ZERO, U256::ZERO)]
                .first_use
                .get()
                .unwrap()
                .1,
            "finish"
        );
    }

    #[derive(Debug, Clone)]
    struct ParentDb {
        reads: Arc<AtomicU64>,
        code: Bytecode,
    }
    impl Default for ParentDb {
        fn default() -> Self {
            // Write slot zero, read it, and return it. The speculative write must never be
            // cached as the parent's slot value or alter the subsequent real execution.
            Self {
                reads: Default::default(),
                code: Bytecode::new_raw(Bytes::from_static(&[
                    0x60, 0x07, 0x60, 0x00, 0x55, 0x60, 0x00, 0x54, 0x60, 0x00, 0x52, 0x60, 0x20,
                    0x60, 0x00, 0xf3,
                ])),
            }
        }
    }
    impl DatabaseRef for ParentDb {
        type Error = Infallible;
        fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Infallible> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            Ok(if address == SENDER {
                Some(AccountInfo { balance: U256::from(1_000_000_000u64), ..Default::default() })
            } else if address == CONTRACT {
                Some(AccountInfo::default().with_code(self.code.clone()))
            } else {
                None
            })
        }
        fn storage_ref(&self, _: Address, _: U256) -> Result<U256, Infallible> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            Ok(U256::from(3))
        }
        fn code_by_hash_ref(&self, _: B256) -> Result<Bytecode, Infallible> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            Ok(self.code.clone())
        }
        fn block_hash_ref(&self, _: u64) -> Result<B256, Infallible> {
            Ok(B256::ZERO)
        }
    }

    fn config_and_env(
    ) -> (crate::node::evm::BscEvmConfig, EvmEnvFor<crate::node::evm::BscEvmConfig>) {
        let cfg = crate::node::evm::BscEvmConfig::bsc(Arc::new(
            crate::chainspec::BscChainSpec::from(crate::chainspec::bsc::bsc_mainnet()),
        ));
        let env = cfg
            .evm_env(&alloy_consensus::Header {
                number: 50_000_000,
                timestamp: 1_780_000_000,
                gas_limit: 200_000_000,
                excess_blob_gas: Some(0),
                base_fee_per_gas: Some(0),
                ..Default::default()
            })
            .unwrap();
        (cfg, env)
    }

    fn transaction(nonce: u64) -> Recovered<TransactionSigned> {
        let tx = TxLegacy {
            chain_id: Some(56),
            nonce,
            gas_limit: 100_000,
            gas_price: 1,
            to: TxKind::Call(CONTRACT),
            ..Default::default()
        };
        Recovered::new_unchecked(
            TransactionSigned::Legacy(Signed::new_unchecked(
                tx,
                Signature::test_signature(),
                B256::repeat_byte(nonce as u8),
            )),
            SENDER,
        )
    }

    fn execute_block_transaction(
        db: impl DatabaseRef<Error = Infallible> + std::fmt::Debug,
    ) -> (u64, String, Vec<reth_ethereum_primitives::Receipt>, Option<AccountInfo>, U256) {
        use reth_evm::block::BlockExecutor;
        use reth_primitives_traits::SealedBlock;
        let (cfg, _) = config_and_env();
        let block = SealedBlock::seal_slow(crate::BscBlock {
            header: alloy_consensus::Header {
                number: 50_000_000,
                timestamp: 1_780_000_000,
                gas_limit: 200_000_000,
                excess_blob_gas: Some(0),
                base_fee_per_gas: Some(0),
                ..Default::default()
            },
            body: Default::default(),
        });
        let mut state = State::builder().with_database_ref(db).build();
        let mut executor = cfg.executor_for_block(&mut state, &block).unwrap();
        let output = executor.execute_transaction_without_commit(transaction(0)).unwrap();
        let gas = executor.commit_transaction(output).tx_gas_used();
        let error = executor
            .execute_transaction_without_commit(transaction(9))
            .err()
            .expect("nonce mismatch must fail")
            .to_string();
        let receipts = executor.receipts().to_vec();
        drop(executor);
        (
            gas,
            error,
            receipts,
            state.basic(SENDER).unwrap(),
            state.storage(CONTRACT, U256::ZERO).unwrap(),
        )
    }

    #[test]
    fn speculative_sstore_does_not_change_parent_or_real_execution_result() {
        let parent = ParentDb::default();
        let cache = SharedStateCache::new(B256::repeat_byte(1));
        let (cfg, env) = config_and_env();
        let mut speculative_env = env.clone();
        speculative_env.cfg_env.disable_nonce_check = true;
        let mut txs = vec![transaction(7)].into_iter();
        assert_eq!(
            warm_transactions(
                SharedStateDb { inner: parent.clone(), cache: cache.clone() },
                cfg.clone(),
                speculative_env,
                || txs.next(),
                || false
            ),
            (1, 0)
        );
        assert_eq!(cache.storage(CONTRACT, U256::ZERO), Some(U256::from(3)));
        assert_eq!(cache.account(SENDER).unwrap().unwrap().nonce, 0);

        let mut cold = State::builder().with_database_ref(parent.clone()).build();
        let cold_result =
            cfg.evm_with_env(&mut cold, env.clone()).transact(transaction(0)).unwrap();
        let mut local = reth_revm::cached::CachedReads::default();
        let (observed, _) = super::super::state_read_diagnostics::ObservedStateDb::new(
            &mut local,
            parent.clone(),
            super::super::build_diagnostics::AttemptKind::Regular,
            true,
        );
        let observed = observed.with_shared_prewarm(cache.parent(), Some(cache.clone()));
        let mut warm = State::builder().with_database(observed).build();
        let warm_result =
            cfg.evm_with_env(&mut warm, env.clone()).transact(transaction(0)).unwrap();
        assert_eq!(cold_result.result, warm_result.result, "status, gas, output and logs");
        assert_eq!(cold_result.state, warm_result.state, "account/storage writes");
        let cold_error =
            cfg.evm_with_env(&mut cold, env.clone()).transact(transaction(9)).unwrap_err();
        let warm_error = cfg.evm_with_env(&mut warm, env).transact(transaction(9)).unwrap_err();
        assert_eq!(cold_error.to_string(), warm_error.to_string());
        assert_eq!(cache.storage(CONTRACT, U256::ZERO), Some(U256::from(3)));
        assert_eq!(parent.storage_ref(CONTRACT, U256::ZERO).unwrap(), U256::from(3));
        let baseline = execute_block_transaction(parent.clone());
        let prewarmed = execute_block_transaction(SharedStateDb { inner: parent, cache });
        assert_eq!(
            baseline, prewarmed,
            "executor gas, receipt, validation error and committed state"
        );
        assert_eq!(prewarmed.3.unwrap().nonce, 1);
        assert_eq!(prewarmed.4, U256::from(7));
    }

    #[test]
    fn private_parent_workers_read_appended_state_and_keep_journals_private() {
        use reth_execution_types::BlockExecutionOutput;
        use reth_primitives_traits::RecoveredBlock;
        use reth_provider::{AccountReader, DatabaseProviderROFactory};
        use reth_storage_overlay::{OverlayManager, OverlayStateProviderFactory};
        use reth_trie_common::SortedTrieData;
        use revm::database::BundleState;

        let factory = reth_provider::test_utils::create_test_provider_factory_with_chain_spec(
            Arc::new(reth_chainspec::ChainSpec::default()),
        );
        let genesis = reth_db_common::init::init_genesis(&factory).unwrap();
        let code = ParentDb::default().code;
        let state = BundleState::builder(1..=1)
            .state_present_account_info(
                SENDER,
                AccountInfo {
                    balance: U256::from(1_000_000_000u64),
                    nonce: 1,
                    ..Default::default()
                },
            )
            .state_present_account_info(CONTRACT, AccountInfo::default().with_code(code.clone()))
            .contract(code.hash_slow(), code.clone())
            .state_storage(
                CONTRACT,
                [(U256::ZERO, (U256::ZERO, U256::from(77)))].into_iter().collect(),
            )
            .build();
        let parent = reth_chain_state::ExecutedBlock::<crate::BscPrimitives>::new(
            Arc::new(RecoveredBlock::new_unhashed(
                crate::BscBlock {
                    header: alloy_consensus::Header {
                        number: 1,
                        parent_hash: genesis,
                        ..Default::default()
                    },
                    body: crate::node::primitives::BscBlockBody {
                        inner: alloy_consensus::BlockBody {
                            transactions: vec![transaction(0).into_inner()],
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                },
                vec![SENDER],
            )),
            Arc::new(BlockExecutionOutput { state, result: Default::default() }),
            SortedTrieData::default(),
        );
        let child_state = BundleState::builder(2..=2)
            .state_present_account_info(
                SENDER,
                AccountInfo {
                    balance: U256::from(1_000_000_000u64),
                    nonce: 2,
                    ..Default::default()
                },
            )
            .state_present_account_info(CONTRACT, AccountInfo::default().with_code(code.clone()))
            .state_storage(
                CONTRACT,
                [(U256::ZERO, (U256::from(77), U256::from(99)))].into_iter().collect(),
            )
            .build();
        let child = reth_chain_state::ExecutedBlock::<crate::BscPrimitives>::new(
            Arc::new(RecoveredBlock::new_unhashed(
                crate::BscBlock {
                    header: alloy_consensus::Header {
                        number: 2,
                        parent_hash: parent.recovered_block().hash(),
                        ..Default::default()
                    },
                    body: crate::node::primitives::BscBlockBody {
                        inner: alloy_consensus::BlockBody {
                            transactions: vec![transaction(1).into_inner()],
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                },
                vec![SENDER],
            )),
            Arc::new(BlockExecutionOutput { state: child_state, result: Default::default() }),
            SortedTrieData::default(),
        );
        let parent_hash = child.recovered_block().hash();
        let first = crate::shared::PrivateMinerChain::new(parent);
        let ancestry = first.appended(child.clone()).unwrap();
        assert_eq!(first.blocks().len(), 1);
        assert_eq!(ancestry.blocks().len(), 2);
        assert!(first.appended(first.parent().clone()).is_none(), "reject non-child");
        assert!(ancestry.appended(child).is_none(), "bound retained state at two private parents");
        let mut source = ParentState::with_private(
            ancestry.clone(),
            Arc::new(|| Err(reth_errors::ProviderError::UnsupportedProvider)),
        );
        assert!(source.consumed(SENDER, 0));
        assert!(source.consumed(SENDER, 1));
        assert!(!source.consumed(SENDER, 2), "next nonce must remain eligible");
        assert!(!source.consumed(CONTRACT, 0));
        // Same appended overlay as the production private-state callback, backed by a
        // test DB. Opening inside the worker is essential; the DB transaction is not sent.
        let opens = Arc::new(AtomicU64::new(0));
        let main_thread = std::thread::current().id();
        let private_factory = factory.clone();
        let opened = opens.clone();
        source.open = Arc::new(move || {
            assert_ne!(std::thread::current().id(), main_thread);
            opened.fetch_add(1, Ordering::Relaxed);
            let manager = OverlayManager::default();
            let mut overlay = manager.overlay_builder(genesis);
            for block in ancestry.blocks() {
                overlay = overlay.with_appended_block(block.clone());
            }
            Ok(Box::new(
                OverlayStateProviderFactory::new(private_factory.clone(), overlay)
                    .database_provider_ro()?,
            ))
        });
        let cache = SharedStateCache::new(parent_hash);
        let (cfg, mut env) = config_and_env();
        env.cfg_env.disable_nonce_check = true;
        std::thread::scope(|scope| {
            for _ in 0..3 {
                let (source, cache, cfg, env) =
                    (source.clone(), cache.clone(), cfg.clone(), env.clone());
                scope.spawn(move || {
                    let db = SharedStateDb {
                        inner: StateProviderDatabase::new(EvmStateProviderAdapter(
                            (source.open)().unwrap(),
                        )),
                        cache,
                    };
                    assert_eq!(db.basic_ref(SENDER).unwrap().unwrap().nonce, 2);
                    assert_eq!(db.storage_ref(CONTRACT, U256::ZERO).unwrap(), U256::from(99));
                    let mut txs = vec![transaction(0), transaction(1), transaction(2)].into_iter();
                    assert_eq!(
                        warm_transactions(
                            db,
                            cfg,
                            env,
                            || txs.find(|tx| !source.consumed(tx.signer(), tx.nonce())),
                            || false
                        ),
                        (1, 0)
                    );
                });
            }
        });
        assert_eq!(opens.load(Ordering::Relaxed), 3);
        assert_eq!(cache.account(SENDER).unwrap().unwrap().nonce, 2);
        assert_eq!(
            cache.storage(CONTRACT, U256::ZERO),
            Some(U256::from(99)),
            "speculative SSTORE must not replace parent value"
        );
        let canonical =
            reth_provider::providers::LatestStateProvider::new(factory.provider().unwrap());
        assert!(
            canonical.basic_account(&SENDER).unwrap().is_none(),
            "private parent was never imported"
        );
    }

    #[test]
    fn concurrent_workers_share_reads_without_changing_parent_identity() {
        let cache = SharedStateCache::new(B256::repeat_byte(1));
        let next_parent = SharedStateCache::new(B256::repeat_byte(2));
        let parent = ParentDb::default();
        let (cfg, mut env) = config_and_env();
        env.cfg_env.disable_nonce_check = true;
        let barrier = std::sync::Barrier::new(3);
        std::thread::scope(|scope| {
            for i in 0..3 {
                let (cache, parent) = (cache.clone(), parent.clone());
                let (cfg, env, barrier) = (cfg.clone(), env.clone(), &barrier);
                scope.spawn(move || {
                    let db = SharedStateDb { inner: parent, cache };
                    barrier.wait();
                    let mut transactions = vec![transaction(i as u64)].into_iter();
                    assert_eq!(
                        warm_transactions(
                            SharedStateDb { inner: db.inner.clone(), cache: db.cache.clone() },
                            cfg,
                            env,
                            || transactions.next(),
                            || false
                        ),
                        (1, 0)
                    );
                    for j in 0..100 {
                        let address = Address::repeat_byte((j + i) as u8);
                        assert_eq!(db.basic_ref(address).unwrap(), db.basic_ref(address).unwrap());
                        assert_eq!(db.storage_ref(address, U256::ZERO).unwrap(), U256::from(3));
                    }
                });
            }
        });
        assert_eq!(cache.parent(), B256::repeat_byte(1));
        assert_eq!(cache.account(Address::ZERO), Some(None));
        assert_eq!(next_parent.account(Address::ZERO), None);
        assert_eq!(next_parent.storage(Address::ZERO, U256::ZERO), None);
    }

    #[test]
    fn speculative_revert_and_halt_preserve_formal_receipts_and_errors() {
        for code in [Bytes::from_static(&[0x60, 0, 0x60, 0, 0xfd]), Bytes::from_static(&[0xfe])] {
            let parent = ParentDb { code: Bytecode::new_raw(code), ..Default::default() };
            let cache = SharedStateCache::new(B256::ZERO);
            let (cfg, env) = config_and_env();
            let mut transactions = vec![transaction(0)].into_iter();
            assert_eq!(
                warm_transactions(
                    SharedStateDb { inner: parent.clone(), cache: cache.clone() },
                    cfg,
                    env,
                    || transactions.next(),
                    || false
                ),
                (1, 0)
            );
            let cold = execute_block_transaction(parent.clone());
            let warm = execute_block_transaction(SharedStateDb { inner: parent, cache });
            assert_eq!(cold, warm);
        }
    }

    #[test]
    fn admission_caps_fall_back_to_reads_without_fabricating_values() {
        let cache = SharedStateCache::new(B256::ZERO);
        cache.entries.store(MAX_ENTRIES, Ordering::Relaxed);
        let db = SharedStateDb { inner: ParentDb::default(), cache: cache.clone() };
        assert!(db.basic_ref(SENDER).unwrap().is_some());
        assert_eq!(db.storage_ref(CONTRACT, U256::ZERO).unwrap(), U256::from(3));
        assert_eq!(cache.account(SENDER), None);
        assert_eq!(cache.storage(CONTRACT, U256::ZERO), None);
        cache.entries.store(0, Ordering::Relaxed);
        cache.code_bytes.store(MAX_CODE_BYTES, Ordering::Relaxed);
        assert!(db.code_by_hash_ref(B256::ZERO).is_ok());
        assert_eq!(cache.code(B256::ZERO), None);
        assert_eq!(cache.entries.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn dropping_guard_stops_before_selecting_another_transaction() {
        let stop = Arc::new(AtomicBool::new(false));
        let guard = PrewarmGuard {
            stopped: stop.clone(),
            cache: SharedStateCache::new(B256::ZERO),
            stop_requested: Arc::new(OnceLock::new()),
            admission: Arc::new(Admission::new(256, 200_000_000)),
        };
        drop(guard);
        let (cfg, env) = config_and_env();
        let result = warm_transactions(
            SharedStateDb { inner: ParentDb::default(), cache: SharedStateCache::new(B256::ZERO) },
            cfg,
            env,
            || panic!("cancelled worker selected a transaction"),
            || stop.load(Ordering::Relaxed),
        );
        assert_eq!(result, (0, 0));
    }

    #[test]
    fn worker_count_matches_go_bsc_mev_policy() {
        assert_eq!(worker_count(true, 64), 3);
        assert_eq!(worker_count(false, 64), 38);
        assert_eq!(worker_count(false, 1), 3);
    }

    #[test]
    fn mining_cursor_skips_consumed_nonces_without_skipping_future_transactions() {
        let cache = SharedStateCache::new(B256::ZERO);
        cache.mark_selected(SENDER, 3);
        cache.mark_selected(SENDER, 1);
        assert!(cache.already_selected(SENDER, 0));
        assert!(cache.already_selected(SENDER, 3));
        assert!(!cache.already_selected(SENDER, 4));
        assert!(!cache.already_selected(CONTRACT, 0));
        assert_eq!(cache.account(SENDER), None, "cursor is not execution state");
    }

    #[tokio::test]
    async fn mining_pool_workers_warm_live_cache_and_release_their_permits() {
        use reth_provider::test_utils::{ExtendedAccount, MockEthProvider};
        use reth_transaction_pool::{
            blobstore::InMemoryBlobStore, noop::MockTransactionValidator, BlockInfo,
            CoinbaseTipOrdering, EthPooledTransaction, Pool, PoolConfig, TransactionOrigin,
            TransactionPoolExt,
        };
        use std::time::Duration;

        let parent_hash = B256::repeat_byte(1);
        let provider = MockEthProvider::default();
        provider.add_account(SENDER, ExtendedAccount::new(0, U256::from(1_000_000_000u64)));
        provider.add_account(
            CONTRACT,
            ExtendedAccount::new(0, U256::ZERO)
                .with_bytecode(ParentDb::default().code.original_bytes())
                .extend_storage([(B256::ZERO, U256::from(3))]),
        );
        let pool = Pool::new(
            MockTransactionValidator::<EthPooledTransaction>::default(),
            CoinbaseTipOrdering::default(),
            InMemoryBlobStore::default(),
            PoolConfig::default().with_disabled_protocol_base_fee(),
        );
        let (cfg, env) = config_and_env();
        let empty_cache = SharedStateCache::new(parent_hash);
        let empty_guard = start(
            Some(empty_cache.clone()),
            pool.clone(),
            ParentState::canonical(provider.clone(), parent_hash),
            cfg.clone(),
            env.clone(),
            0,
            None,
            0,
            200_000_000,
            ManualCancel::default(),
            None,
        )
        .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while workers().unwrap().active.load(Ordering::Relaxed) != 0 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("empty setup job leaked a permit");
        assert_eq!(empty_cache.account(SENDER), None, "empty pool must not read state");
        drop(empty_guard);
        pool.set_block_info(BlockInfo {
            last_seen_block_hash: parent_hash,
            block_gas_limit: 200_000_000,
            pending_basefee: 0,
            ..Default::default()
        });
        pool.add_transaction(
            TransactionOrigin::External,
            EthPooledTransaction::new(transaction(0), 200),
        )
        .await
        .unwrap();
        let cache = SharedStateCache::new(parent_hash);
        let guard = start(
            Some(cache.clone()),
            pool,
            ParentState::canonical(provider.clone(), parent_hash),
            cfg,
            env,
            0,
            None,
            0,
            200_000_000,
            ManualCancel::default(),
            None,
        )
        .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while cache.storage(CONTRACT, U256::ZERO).is_none() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("actual worker did not warm the contract storage");
        assert_eq!(cache.storage(CONTRACT, U256::ZERO), Some(U256::from(3)));
        drop(guard);
        tokio::time::timeout(Duration::from_secs(5), async {
            while workers().unwrap().active.load(Ordering::Relaxed) != 0 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("worker permits leaked");
        use reth_provider::AccountReader;
        assert_eq!(provider.basic_account(&SENDER).unwrap().unwrap().nonce, 0);
    }
}
