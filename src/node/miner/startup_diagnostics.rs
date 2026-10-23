//! Mining startup accounting, separate from attempt/permit/execution diagnostics.
//! Only fixed labels are exported. Hashes are used in a bounded, expiring local cache and logs.

use alloy_primitives::B256;
use lru::LruCache;
use parking_lot::Mutex;
use std::{
    num::NonZeroUsize,
    sync::LazyLock,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const CAPACITY: usize = 2048;
const MAX_AGE: Duration = Duration::from_secs(120);
static PARENTS: LazyLock<Mutex<LruCache<B256, ParentTiming>>> =
    LazyLock::new(|| Mutex::new(LruCache::new(NonZeroUsize::new(CAPACITY).unwrap())));
static LAST_LOG: Mutex<Option<Instant>> = Mutex::new(None);

#[derive(Clone, Debug)]
struct ParentTiming {
    source: &'static str,
    arrived: Instant,
    arrival_age_ms: i128,
    import_started: Option<Instant>,
    new_payload: Option<Duration>,
}

// Same BSC millisecond encoding as calculate_millisecond_timestamp, for generic canonical headers.
pub(crate) fn header_ms(header: &impl alloy_consensus::BlockHeader) -> u64 {
    let mix = header.mix_hash().unwrap_or_default();
    let remainder = u64::from_be_bytes(mix.as_slice()[24..32].try_into().unwrap());
    header.timestamp().wrapping_mul(1000).wrapping_add(remainder)
}

pub(crate) fn wall_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis()
}

/// First full-block ingress at ImportHandle (or service fallback), or local submit entry.
/// This is NOT wire latency: timestamp age includes producer delay and clock skew.
/// Duplicate announcements never reset the first observation. An expired entry is replaced.
pub(crate) fn ingress(hash: B256, timestamp_ms: u64, source: &'static str) {
    let now = Instant::now();
    let mut parents = PARENTS.lock();
    if parents.peek(&hash).is_some_and(|p| now.duration_since(p.arrived) <= MAX_AGE) {
        return;
    }
    parents.put(
        hash,
        ParentTiming {
            source,
            arrived: now,
            arrival_age_ms: wall_ms() as i128 - timestamp_ms as i128,
            import_started: None,
            new_payload: None,
        },
    );
}

/// Network: first poll of the import future. Local executed block: service handling entry.
/// Local insertion reuses an executed block; it does not imply an EVM re-execution.
pub(crate) fn import_started(hash: B256) {
    if let Some(p) = PARENTS.lock().get_mut(&hash) {
        p.import_started.get_or_insert_with(Instant::now);
    }
}

/// Inclusive engine.new_payload await (engine queue + execution/validation). Not added to
/// import-to-notification: it is a subset. Local executed-block insertion has no sample.
pub(crate) fn new_payload_finished(hash: B256, elapsed: Duration) {
    if let Some(p) = PARENTS.lock().get_mut(&hash) {
        p.new_payload.get_or_insert(elapsed);
    }
}

#[derive(Clone, Debug)]
pub(crate) struct StartupTiming {
    trigger: &'static str,
    observed: Instant,
    parent_age_ms: i128,
    parent: Option<ParentTiming>,
    cache: Duration,
    queued: Option<Instant>,
    dequeued: Option<Instant>,
    scheduled: Option<Instant>,
}

impl StartupTiming {
    /// Notification RECEIPT, not engine notification publication. Startup/retry have a
    /// separate trigger and must not be interpreted as canonical notification latency.
    pub(crate) fn new(hash: B256, timestamp_ms: u64, trigger: &'static str) -> Self {
        let now = Instant::now();
        let parent =
            PARENTS.lock().get(&hash).filter(|p| now.duration_since(p.arrived) <= MAX_AGE).cloned();
        Self {
            trigger,
            observed: now,
            parent_age_ms: wall_ms() as i128 - timestamp_ms as i128,
            parent,
            cache: Duration::ZERO,
            queued: None,
            dequeued: None,
            scheduled: None,
        }
    }

    pub(crate) fn cache_finished(&mut self, elapsed: Duration) {
        self.cache = elapsed;
    }
    pub(crate) fn queued(&mut self) {
        self.queued = Some(Instant::now());
    }
    pub(crate) fn dequeued(&mut self) {
        self.dequeued = Some(Instant::now());
    }
    pub(crate) fn scheduled(&mut self) {
        self.scheduled = Some(Instant::now());
    }

    /// Mutually exclusive miner startup stages for ONE job. prepare_context includes
    /// notification handling/eligibility/cache cloning outside cache_for_next itself.
    fn stages(&self, started: Instant) -> Option<[(&'static str, Duration); 5]> {
        let (queued, dequeued, scheduled) = (self.queued?, self.dequeued?, self.scheduled?);
        Some([
            ("cache_for_next", self.cache),
            ("prepare_context", queued.duration_since(self.observed).saturating_sub(self.cache)),
            ("mining_queue", dequeued.duration_since(queued)),
            ("prepare_job", scheduled.duration_since(dequeued)),
            ("job_schedule", started.duration_since(scheduled)),
        ])
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn report(
        &self,
        started: Instant,
        parent_hash: B256,
        block_number: u64,
        trace_id: u64,
        turn: &'static str,
        last_in_turn: bool,
        raw_slot_ms: i128,
        margin: Duration,
        reserve: Duration,
        total_budget: Duration,
        transaction_window: Duration,
    ) {
        let source = self.parent.as_ref().map_or("unknown", |p| p.source);
        let position = if last_in_turn { "last" } else { "other" };
        let Some(stages) = self.stages(started) else { return };
        // Startup phase histogram, seconds. Same job population as budget below; not attempts.
        let stage = |name: &'static str, value: Duration| {
            metrics::histogram!("bsc.mining_startup.stage_seconds", "stage" => name,
                "source" => source, "turn" => turn, "position" => position, "trigger" => self.trigger)
                .record(value.as_secs_f64());
        };
        for (name, value) in stages {
            stage(name, value);
        }
        // Clock-derived offsets are split into ahead/behind magnitudes, never silently clamped
        // to zero or added to monotonic durations. No cross-host timestamp subtraction here.
        let age = |name: &'static str, ms: i128| {
            metrics::histogram!("bsc.mining_startup.timestamp_offset_seconds", "point" => name,
                "direction" => if ms < 0 { "ahead" } else { "behind" },
                "source" => source, "turn" => turn, "position" => position, "trigger" => self.trigger)
                .record(ms.unsigned_abs() as f64 / 1000.0);
        };
        age("parent_at_notification", self.parent_age_ms);
        age("target_at_job_start", -raw_slot_ms);
        if let Some(p) = &self.parent {
            age("parent_at_ingress", p.arrival_age_ms);
            stage("ingress_to_notification", self.observed.duration_since(p.arrived));
            if let Some(import) = p.import_started.filter(|t| *t <= self.observed) {
                stage("ingress_to_import", import.duration_since(p.arrived));
                stage("import_to_notification", self.observed.duration_since(import));
            }
            if let Some(elapsed) = p.new_payload {
                stage("new_payload", elapsed);
            }
        }
        let slot = Duration::from_millis(raw_slot_ms.max(0).min(u64::MAX as i128) as u64);
        let usable = slot.saturating_sub(margin);
        let nominal = usable.min(total_budget).saturating_sub(reserve);
        let limiter =
            budget_limiter(usable, total_budget, last_in_turn, nominal, transaction_window);
        // Seconds, per job. Components describe caps/reservations, NOT additive observed work.
        for (component, value) in [
            ("raw_slot_remaining", slot),
            ("fixed_margin", margin),
            ("root_reserve", reserve),
            ("total_budget_cap", total_budget),
            ("transaction_window", transaction_window),
        ] {
            metrics::histogram!("bsc.mining_startup.budget_seconds", "component" => component,
                "source" => source, "turn" => turn, "position" => position, "trigger" => self.trigger)
                .record(value.as_secs_f64());
        }
        metrics::counter!("bsc.mining_startup.jobs_total", "limiter" => limiter,
            "source" => source, "turn" => turn, "position" => position, "trigger" => self.trigger)
        .increment(1);
        // Only short in-turn jobs compete for the process-wide 10s log allowance.
        if turn == "inturn" && transaction_window < Duration::from_millis(300) && allow_log(started)
        {
            tracing::info!(target: "bsc::miner", block_number, trace_id, %parent_hash, source,
                trigger = self.trigger, position, limiter, parent_age_ms = self.parent_age_ms,
                parent_ingress_age_ms = ?self.parent.as_ref().map(|p| p.arrival_age_ms),
                ingress_to_notification_ms = ?self.parent.as_ref().map(|p| self.observed.duration_since(p.arrived).as_secs_f64() * 1000.0),
                ingress_to_import_ms = ?self.parent.as_ref().and_then(|p| p.import_started.map(|t| t.duration_since(p.arrived).as_secs_f64() * 1000.0)),
                import_to_notification_ms = ?self.parent.as_ref().and_then(|p| p.import_started.map(|t| self.observed.saturating_duration_since(t).as_secs_f64() * 1000.0)),
                new_payload_ms = ?self.parent.as_ref().and_then(|p| p.new_payload.map(|d| d.as_secs_f64() * 1000.0)),
                cache_ms = stages[0].1.as_secs_f64() * 1000.0,
                context_ms = stages[1].1.as_secs_f64() * 1000.0,
                queue_ms = stages[2].1.as_secs_f64() * 1000.0,
                prepare_job_ms = stages[3].1.as_secs_f64() * 1000.0,
                schedule_ms = stages[4].1.as_secs_f64() * 1000.0,
                raw_slot_ms, margin_ms = margin.as_millis(), reserve_ms = reserve.as_millis(),
                total_budget_ms = total_budget.as_millis(), transaction_window_ms = transaction_window.as_millis(),
                "Sampled mining startup budget");
        }
    }
}

fn budget_limiter(
    slot: Duration,
    cap: Duration,
    last: bool,
    nominal: Duration,
    actual: Duration,
) -> &'static str {
    if actual > nominal {
        "recovery_floor"
    } else if slot <= cap {
        "slot_remaining"
    } else if last {
        "last_in_turn_cap"
    } else {
        "period_cap"
    }
}

fn allow_log(now: Instant) -> bool {
    let mut last = LAST_LOG.lock();
    if last.is_some_and(|t| now.saturating_duration_since(t) < Duration::from_secs(10)) {
        return false;
    }
    *last = Some(now);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn header_clock_preserves_bsc_millisecond_remainder() {
        let h = alloy_consensus::Header {
            timestamp: 123,
            mix_hash: B256::with_last_byte(90),
            ..Default::default()
        };
        assert_eq!(
            header_ms(&h),
            crate::consensus::parlia::util::calculate_millisecond_timestamp(&h)
        );
        assert_eq!(header_ms(&h), 123090);
    }
    #[test]
    fn report_exports_unknown_and_nonoverlapping_phases_without_ids() {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let base = Instant::now();
        let timing = StartupTiming {
            trigger: "canonical",
            observed: base,
            parent_age_ms: -20,
            parent: None,
            cache: Duration::ZERO,
            queued: Some(base),
            dequeued: Some(base),
            scheduled: Some(base),
        };
        metrics::with_local_recorder(&recorder, || {
            timing.report(
                base,
                B256::with_last_byte(213),
                12,
                99,
                "outofturn",
                false,
                1000,
                Duration::from_millis(30),
                Duration::from_millis(30),
                Duration::from_millis(450),
                Duration::from_millis(420),
            )
        });
        let output = handle.render();
        assert!(output.contains("source=\"unknown\""));
        assert!(output.contains("direction=\"ahead\""));
        assert!(output.contains("limiter=\"period_cap\""));
        assert!(!output.contains("block_number"));
        assert!(!output.contains("parent_hash"));
        assert!(!output.contains("stage=\"new_payload\""));
    }
    #[test]
    fn concurrent_short_jobs_share_log_allowance() {
        *LAST_LOG.lock() = None;
        let now = Instant::now();
        let winners = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..8).map(|_| scope.spawn(move || allow_log(now))).collect();
            workers.into_iter().map(|w| usize::from(w.join().unwrap())).sum::<usize>()
        });
        assert_eq!(winners, 1);
        assert!(!allow_log(now + Duration::from_secs(9)));
        assert!(allow_log(now + Duration::from_secs(10)));
    }
    #[test]
    fn phases_partition_notification_to_job_start() {
        let base = Instant::now();
        let timing = StartupTiming {
            trigger: "canonical",
            observed: base,
            parent_age_ms: 100,
            parent: None,
            cache: Duration::from_millis(7),
            queued: Some(base + Duration::from_millis(10)),
            dequeued: Some(base + Duration::from_millis(13)),
            scheduled: Some(base + Duration::from_millis(18)),
        };
        let stages = timing.stages(base + Duration::from_millis(20)).unwrap();
        assert_eq!(stages.map(|(_, d)| d.as_millis()), [7, 3, 3, 5, 2]);
        assert_eq!(stages.iter().map(|(_, d)| *d).sum::<Duration>(), Duration::from_millis(20));
    }
    #[test]
    fn duplicate_ingress_preserves_source_and_first_time() {
        let hash = B256::with_last_byte(211);
        ingress(hash, wall_ms() as u64 + 1000, "local");
        let first = PARENTS.lock().peek(&hash).unwrap().arrived;
        ingress(hash, 0, "network");
        import_started(hash);
        let timing = StartupTiming::new(hash, 0, "canonical");
        let p = timing.parent.unwrap();
        assert_eq!(p.source, "local");
        assert_eq!(p.arrived, first);
        assert!(p.arrival_age_ms < 0);
    }
    #[test]
    fn missing_and_expired_parents_stay_unknown() {
        let hash = B256::with_last_byte(212);
        assert!(StartupTiming::new(hash, 0, "retry").parent.is_none());
        ingress(hash, 0, "network");
        PARENTS.lock().get_mut(&hash).unwrap().arrived =
            Instant::now() - MAX_AGE - Duration::from_secs(1);
        assert!(StartupTiming::new(hash, 0, "canonical").parent.is_none());
        ingress(hash, 0, "local");
        assert_eq!(StartupTiming::new(hash, 0, "canonical").parent.unwrap().source, "local");
    }
    #[test]
    fn limiters_cover_slot_turn_and_recovery_without_changing_budget() {
        let d = Duration::from_millis;
        assert_eq!(budget_limiter(d(200), d(450), false, d(170), d(170)), "slot_remaining");
        assert_eq!(budget_limiter(d(300), d(180), true, d(150), d(150)), "last_in_turn_cap");
        assert_eq!(budget_limiter(d(2000), d(450), false, d(420), d(420)), "period_cap");
        assert_eq!(budget_limiter(d(0), d(50), false, d(0), d(50)), "recovery_floor");
    }
}
