use alloy_primitives::B256;
use std::{
    fmt,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
        Mutex,
    },
    time::{Duration, Instant},
};

/// Per-payload-job performance context.
///
/// This is designed to be:
/// - **cheap** to clone (`Arc`)
/// - **thread-safe** (used across async build tasks + EVM builder)
/// - **structured** (stable fields for later log/metrics parsing)
///
/// The context is created once per payload job (block_number/parent_hash/trace_id identify it)
/// and then passed down into execution via `BscNextBlockEnvAttributes`.
#[derive(Clone)]
pub struct PerfContext {
    inner: Arc<PerfContextInner>,
}

impl PerfContext {
    pub fn new(block_number: u64, parent_hash: B256, trace_id: u64) -> Self {
        Self {
            inner: Arc::new(PerfContextInner {
                block_number,
                parent_hash,
                trace_id,
                created_at: Instant::now(),
                attempt_seq: AtomicU64::new(0),
                bg_wait_nanos: AtomicU64::new(0),
                empty_payload_build_nanos: AtomicU64::new(0),
                wait_outer_args_nanos: AtomicU64::new(0),
                wait_outer_join_nanos: AtomicU64::new(0),
                wait_inner_sleep_nanos: AtomicU64::new(0),
                wait_inner_tx_nanos: AtomicU64::new(0),
                wait_inner_abort_nanos: AtomicU64::new(0),
                attempts: Mutex::new(Vec::new()),
            }),
        }
    }

    /// Block number being built (parent + 1).
    pub fn block_number(&self) -> u64 {
        self.inner.block_number
    }

    /// Parent block hash.
    pub fn parent_hash(&self) -> B256 {
        self.inner.parent_hash
    }

    /// Trace id associated with this job.
    pub fn trace_id(&self) -> u64 {
        self.inner.trace_id
    }

    /// Returns duration since this context was created.
    pub fn age(&self) -> Duration {
        self.inner.created_at.elapsed()
    }

    /// Start measuring the "background wait" duration (RAII guard).
    ///
    /// This timer is used in places where we briefly wait for background payload build tasks
    /// (e.g. before picking the best payload).
    pub fn bg_wait_timer(&self) -> PerfTimer {
        PerfTimer::new(self.clone(), PerfTimerKind::BgWait)
    }

    /// Backwards-compatible alias; prefer `bg_wait_timer()`.
    pub fn empty_fallback_wait_timer(&self) -> PerfTimer {
        self.bg_wait_timer()
    }

    /// Record the duration spent building an empty payload (job-level measurement).
    ///
    /// This is recorded from the payload job's empty-fallback path (not from the EVM builder),
    /// and includes any synchronous/block_in_place overhead around the actual empty build.
    pub fn record_empty_payload_build_duration(&self, duration: Duration) {
        let nanos = duration.as_nanos().min(u64::MAX as u128) as u64;
        self.inner
            .empty_payload_build_nanos
            .store(nanos, Ordering::Relaxed);
    }

    pub fn add_wait_outer_args(&self, duration: Duration) {
        let nanos = duration.as_nanos().min(u64::MAX as u128) as u64;
        self.inner.wait_outer_args_nanos.fetch_add(nanos, Ordering::Relaxed);
    }

    pub fn add_wait_outer_join(&self, duration: Duration) {
        let nanos = duration.as_nanos().min(u64::MAX as u128) as u64;
        self.inner.wait_outer_join_nanos.fetch_add(nanos, Ordering::Relaxed);
    }

    pub fn add_wait_inner_sleep(&self, duration: Duration) {
        let nanos = duration.as_nanos().min(u64::MAX as u128) as u64;
        self.inner.wait_inner_sleep_nanos.fetch_add(nanos, Ordering::Relaxed);
    }

    pub fn add_wait_inner_tx(&self, duration: Duration) {
        let nanos = duration.as_nanos().min(u64::MAX as u128) as u64;
        self.inner.wait_inner_tx_nanos.fetch_add(nanos, Ordering::Relaxed);
    }

    pub fn add_wait_inner_abort(&self, duration: Duration) {
        let nanos = duration.as_nanos().min(u64::MAX as u128) as u64;
        self.inner.wait_inner_abort_nanos.fetch_add(nanos, Ordering::Relaxed);
    }

    /// Create a new build attempt record. The returned guard should be finalized (or dropped).
    pub fn start_attempt(&self) -> AttemptGuard {
        let id = self.inner.attempt_seq.fetch_add(1, Ordering::Relaxed) + 1;
        // Insert a placeholder record immediately so downstream components (EVM builder) can attach
        // their timings even before the attempt is finalized.
        {
            let mut attempts = self.inner.attempts.lock().expect("perf attempts poisoned");
            attempts.push(AttemptRecord {
                attempt_id: id,
                started_at_ms: self.age().as_millis() as u64,
                duration_ms: 0,
                tx_considered: 0,
                tx_executed: 0,
                tx_skipped_blacklist: 0,
                tx_skipped_min_tip: 0,
                tx_skipped_gas_or_blob_limit: 0,
                tx_invalid: 0,
                cumulative_gas_used: 0,
                total_fees_wei: alloy_primitives::U256::ZERO,
                finish_total_ms: 0,
                hashed_post_state_ms: 0,
                state_root_ms: 0,
                state_root_source: "pending",
                assemble_ms: 0,
            });
        }
        AttemptGuard { ctx: self.clone(), attempt_id: id, started_at: Instant::now(), finished: false }
    }

    /// Called by the EVM builder to attach finish/state-root timing information to a specific
    /// build attempt.
    pub fn record_finish_timings_for_attempt(
        &self,
        attempt_id: u64,
        finish_total: Duration,
        hashed_post_state: Duration,
        state_root_wait_or_compute: Duration,
        state_root_source: &'static str,
        assemble: Duration,
    ) {
        let mut attempts = self.inner.attempts.lock().expect("perf attempts poisoned");
        if let Some(rec) = attempts.iter_mut().find(|r| r.attempt_id == attempt_id) {
            rec.finish_total_ms = finish_total.as_millis() as u64;
            rec.hashed_post_state_ms = hashed_post_state.as_millis() as u64;
            rec.state_root_ms = state_root_wait_or_compute.as_millis() as u64;
            rec.state_root_source = state_root_source;
            rec.assemble_ms = assemble.as_millis() as u64;
        }
    }

    /// Snapshot the current perf data for logging.
    pub fn snapshot(&self) -> PerfSnapshot {
        let attempts = self.inner.attempts.lock().expect("perf attempts poisoned").clone();
        let age_ms = self.age().as_millis() as u64;
        PerfSnapshot {
            block_number: self.block_number(),
            parent_hash: self.parent_hash(),
            trace_id: self.trace_id(),
            age_ms,
            bg_wait_ms: nanos_to_ms(self.inner.bg_wait_nanos.load(Ordering::Relaxed)),
            empty_payload_build_ms: nanos_to_ms(
                self.inner.empty_payload_build_nanos.load(Ordering::Relaxed),
            ),
            wait_outer_args_ms: nanos_to_ms(self.inner.wait_outer_args_nanos.load(Ordering::Relaxed)),
            wait_outer_join_ms: nanos_to_ms(self.inner.wait_outer_join_nanos.load(Ordering::Relaxed)),
            wait_inner_sleep_ms: nanos_to_ms(self.inner.wait_inner_sleep_nanos.load(Ordering::Relaxed)),
            wait_inner_tx_ms: nanos_to_ms(self.inner.wait_inner_tx_nanos.load(Ordering::Relaxed)),
            wait_inner_abort_ms: nanos_to_ms(self.inner.wait_inner_abort_nanos.load(Ordering::Relaxed)),
            attempts,
        }
    }
}

impl fmt::Debug for PerfContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PerfContext")
            .field("block_number", &self.block_number())
            .field("parent_hash", &self.parent_hash())
            .field("trace_id", &self.trace_id())
            .field("age", &self.age())
            .finish()
    }
}

#[derive(Debug)]
struct PerfContextInner {
    block_number: u64,
    parent_hash: B256,
    trace_id: u64,
    created_at: Instant,

    attempt_seq: AtomicU64,

    bg_wait_nanos: AtomicU64,
    empty_payload_build_nanos: AtomicU64,
    wait_outer_args_nanos: AtomicU64,
    wait_outer_join_nanos: AtomicU64,
    wait_inner_sleep_nanos: AtomicU64,
    wait_inner_tx_nanos: AtomicU64,
    wait_inner_abort_nanos: AtomicU64,

    attempts: Mutex<Vec<AttemptRecord>>,
}

/// A small RAII timer that accumulates elapsed time into the owning `PerfContext`.
pub struct PerfTimer {
    ctx: PerfContext,
    kind: PerfTimerKind,
    started_at: Instant,
}

impl PerfTimer {
    fn new(ctx: PerfContext, kind: PerfTimerKind) -> Self {
        Self { ctx, kind, started_at: Instant::now() }
    }
}

impl Drop for PerfTimer {
    fn drop(&mut self) {
        let nanos = self.started_at.elapsed().as_nanos() as u64;
        match self.kind {
            PerfTimerKind::BgWait => {
                self.ctx
                    .inner
                    .bg_wait_nanos
                    .store(nanos, Ordering::Relaxed);
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum PerfTimerKind {
    BgWait,
}

/// Guard for a single build attempt.
pub struct AttemptGuard {
    ctx: PerfContext,
    attempt_id: u64,
    started_at: Instant,
    finished: bool,
}

impl AttemptGuard {
    pub fn attempt_id(&self) -> u64 {
        self.attempt_id
    }

    pub fn finish_with_tx_stats(
        mut self,
        tx_considered: u64,
        tx_executed: u64,
        tx_skipped_blacklist: u64,
        tx_skipped_min_tip: u64,
        tx_skipped_gas_or_blob_limit: u64,
        tx_invalid: u64,
        cumulative_gas_used: u64,
        total_fees: alloy_primitives::U256,
    ) {
        let elapsed = self.started_at.elapsed();
        let mut attempts = self.ctx.inner.attempts.lock().expect("perf attempts poisoned");
        if let Some(rec) = attempts.iter_mut().find(|r| r.attempt_id == self.attempt_id) {
            rec.duration_ms = elapsed.as_millis() as u64;
            rec.tx_considered = tx_considered;
            rec.tx_executed = tx_executed;
            rec.tx_skipped_blacklist = tx_skipped_blacklist;
            rec.tx_skipped_min_tip = tx_skipped_min_tip;
            rec.tx_skipped_gas_or_blob_limit = tx_skipped_gas_or_blob_limit;
            rec.tx_invalid = tx_invalid;
            rec.cumulative_gas_used = cumulative_gas_used;
            rec.total_fees_wei = total_fees;
            if rec.state_root_source == "pending" {
                rec.state_root_source = "unknown";
            }
        }
        self.finished = true;
    }
}

impl Drop for AttemptGuard {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        // Best-effort: mark an incomplete attempt so we still see retries/time spent even when
        // the attempt bails out early (cancel/error).
        let elapsed = self.started_at.elapsed();
        let mut attempts = self.ctx.inner.attempts.lock().expect("perf attempts poisoned");
        if let Some(rec) = attempts.iter_mut().find(|r| r.attempt_id == self.attempt_id) {
            rec.duration_ms = elapsed.as_millis() as u64;
            if rec.state_root_source == "pending" {
                rec.state_root_source = "incomplete";
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct AttemptRecord {
    pub attempt_id: u64,
    pub started_at_ms: u64,
    pub duration_ms: u64,

    pub tx_considered: u64,
    pub tx_executed: u64,
    pub tx_skipped_blacklist: u64,
    pub tx_skipped_min_tip: u64,
    pub tx_skipped_gas_or_blob_limit: u64,
    pub tx_invalid: u64,
    pub cumulative_gas_used: u64,
    pub total_fees_wei: alloy_primitives::U256,

    pub finish_total_ms: u64,
    pub hashed_post_state_ms: u64,
    pub state_root_ms: u64,
    pub state_root_source: &'static str,
    pub assemble_ms: u64,
}

#[derive(Debug, Clone)]
pub struct PerfSnapshot {
    pub block_number: u64,
    pub parent_hash: B256,
    pub trace_id: u64,

    pub age_ms: u64,
    pub bg_wait_ms: u64,
    pub empty_payload_build_ms: u64,
    pub wait_outer_args_ms: u64,
    pub wait_outer_join_ms: u64,
    pub wait_inner_sleep_ms: u64,
    pub wait_inner_tx_ms: u64,
    pub wait_inner_abort_ms: u64,

    pub attempts: Vec<AttemptRecord>,
}

fn nanos_to_ms(nanos: u64) -> u64 {
    nanos / 1_000_000
}


