//! Miner-only observation at the CachedReads boundary and its backing provider.
//! Counters stay local to the DB; only the current build phase is shared with the caller.

use super::build_diagnostics::AttemptKind;
use alloy_primitives::{Address, B256, U256};
use reth_revm::cached::{CachedReads, CachedReadsDbMut};
use revm::{bytecode::Bytecode, state::AccountInfo, Database, DatabaseRef};
use std::{
    cell::Cell,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

#[derive(Debug)]
pub(crate) struct ObservedStateDb<'a, DB> {
    inner: CachedReadsDbMut<'a, ObservedProvider<DB>>,
    reads: ReadObservation,
}

impl<'a, DB> ObservedStateDb<'a, DB> {
    pub(crate) fn new(
        cache: &'a mut CachedReads,
        db: DB,
        kind: AttemptKind,
        inturn: bool,
    ) -> (Self, ReadPhase) {
        let phase = ReadPhase::default();
        let reads = ReadObservation::new(kind, inturn, "cached", phase.clone());
        let provider = ObservedProvider {
            inner: db,
            reads: ReadObservation::new(kind, inturn, "provider", phase.clone()),
            prewarm_reads: ReadObservation::new(kind, inturn, "prewarm", phase.clone()),
            shared: None,
            calls: Cell::new(0),
        };
        (Self { inner: cache.as_db_mut(provider), reads }, phase)
    }

    pub(crate) fn with_shared_prewarm(
        mut self,
        parent: B256,
        cache: Option<Arc<super::prewarm::SharedStateCache>>,
    ) -> Self {
        let result = match cache.as_ref() {
            None => "disabled",
            Some(cache) if cache.parent() != parent => "parent_mismatch",
            Some(_) => "attached",
        };
        metrics::counter!("bsc.build_diagnostics.prewarm_cache_total",
            "kind" => self.reads.kind, "turn" => self.reads.turn, "result" => result)
        .increment(1);
        self.inner.db.shared = cache.filter(|cache| cache.parent() == parent);
        self
    }
}

impl<DB: DatabaseRef> ObservedStateDb<'_, DB> {
    fn read<T>(
        &mut self,
        resource: usize,
        f: impl FnOnce(&mut CachedReadsDbMut<'_, ObservedProvider<DB>>) -> Result<T, DB::Error>,
    ) -> Result<T, DB::Error> {
        let phase = self.reads.phase.index();
        let calls = self.inner.db.calls.get();
        let start = Instant::now();
        let result = f(&mut self.inner);
        let elapsed = start.elapsed();
        // Observe actual backing calls, without a second hash-table lookup. A storage miss
        // may load account info first, and a nonexistent account can suppress the slot read.
        let miss = self.inner.db.calls.get() != calls;
        self.reads.record(phase, resource, usize::from(miss), elapsed, result.is_err());
        result
    }
}

impl<DB: DatabaseRef> Database for ObservedStateDb<'_, DB> {
    type Error = DB::Error;
    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        self.read(0, |db| db.basic(address))
    }
    fn storage(&mut self, address: Address, index: U256) -> Result<U256, Self::Error> {
        self.read(1, |db| db.storage(address, index))
    }
    fn code_by_hash(&mut self, hash: B256) -> Result<Bytecode, Self::Error> {
        self.read(2, |db| db.code_by_hash(hash))
    }
    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        self.inner.block_hash(number)
    }
}

impl<DB, T> AsRef<T> for ObservedStateDb<'_, DB>
where
    DB: AsRef<T>,
{
    fn as_ref(&self) -> &T {
        self.inner.db.inner.as_ref()
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ReadPhase(Arc<AtomicUsize>);
impl ReadPhase {
    pub(crate) fn selection(&self) {
        self.0.store(1, Ordering::Relaxed);
    }
    pub(crate) fn finish(&self) {
        self.0.store(2, Ordering::Relaxed);
    }
    fn index(&self) -> usize {
        self.0.load(Ordering::Relaxed)
    }
}

#[derive(Debug)]
struct ObservedProvider<DB> {
    inner: DB,
    reads: ReadObservation,
    prewarm_reads: ReadObservation,
    shared: Option<Arc<super::prewarm::SharedStateCache>>,
    calls: Cell<u64>,
}
impl<DB: DatabaseRef> ObservedProvider<DB> {
    fn shared_prewarmed<T>(
        &self,
        resource: usize,
        lookup: impl FnOnce(&super::prewarm::SharedStateCache) -> Option<T>,
    ) -> Option<T> {
        let cache = self.shared.as_ref()?;
        let start = Instant::now();
        let value = lookup(cache);
        self.prewarm_reads.record(
            self.reads.phase.index(),
            resource,
            usize::from(value.is_none()),
            start.elapsed(),
            false,
        );
        if value.is_some() {
            self.calls.set(self.calls.get() + 1);
        }
        value
    }
    fn read<T>(
        &self,
        resource: usize,
        f: impl FnOnce(&DB) -> Result<T, DB::Error>,
    ) -> Result<T, DB::Error> {
        let phase = self.reads.phase.index();
        self.calls.set(self.calls.get() + 1);
        let start = Instant::now();
        let result = f(&self.inner);
        self.reads.record(phase, resource, 0, start.elapsed(), result.is_err());
        result
    }
}
impl<DB: DatabaseRef> DatabaseRef for ObservedProvider<DB> {
    type Error = DB::Error;
    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        if let Some(account) = self.shared_prewarmed(0, |s| s.account(address)) {
            return Ok(account);
        }
        let value = self.read(0, |db| db.basic_ref(address))?;
        if let Some(cache) = &self.shared {
            cache.insert_account(address, value.clone());
        }
        Ok(value)
    }
    fn storage_ref(&self, address: Address, index: U256) -> Result<U256, Self::Error> {
        if let Some(value) = self.shared_prewarmed(1, |s| s.storage(address, index)) {
            return Ok(value);
        }
        let value = self.read(1, |db| db.storage_ref(address, index))?;
        if let Some(cache) = &self.shared {
            cache.insert_storage(address, index, value);
        }
        Ok(value)
    }
    fn code_by_hash_ref(&self, hash: B256) -> Result<Bytecode, Self::Error> {
        if let Some(code) = self.shared_prewarmed(2, |s| s.code(hash)) {
            return Ok(code);
        }
        let value = self.read(2, |db| db.code_by_hash_ref(hash))?;
        if let Some(cache) = &self.shared {
            cache.insert_code(hash, value.clone());
        }
        Ok(value)
    }
    fn block_hash_ref(&self, number: u64) -> Result<B256, Self::Error> {
        self.inner.block_hash_ref(number)
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct ReadTotals {
    calls: u64,
    errors: u64,
    elapsed: Duration,
}
#[derive(Debug)]
struct ReadObservation {
    kind: &'static str,
    turn: &'static str,
    layer: &'static str,
    phase: ReadPhase,
    totals: [[[Cell<ReadTotals>; 2]; 3]; 3],
}
impl ReadObservation {
    fn new(kind: AttemptKind, inturn: bool, layer: &'static str, phase: ReadPhase) -> Self {
        Self {
            kind: kind.label(),
            turn: if inturn { "inturn" } else { "outofturn" },
            layer,
            phase,
            totals: Default::default(),
        }
    }
    fn record(
        &self,
        phase: usize,
        resource: usize,
        outcome: usize,
        elapsed: Duration,
        error: bool,
    ) {
        let cell = &self.totals[phase][resource][outcome];
        let mut total = cell.get();
        total.calls += 1;
        total.errors += u64::from(error);
        total.elapsed += elapsed;
        cell.set(total);
    }
}
impl Drop for ReadObservation {
    fn drop(&mut self) {
        for (p, stage) in ["prepare", "selection", "finish"].into_iter().enumerate() {
            for (r, resource) in ["account", "storage", "code"].into_iter().enumerate() {
                for (o, cached_outcome) in ["hit", "miss"].into_iter().enumerate() {
                    let total = self.totals[p][r][o].get();
                    // Untaken paths do not create misleading zero-duration samples.
                    if total.calls == 0 {
                        continue;
                    }
                    let outcome = if self.layer == "provider" { "read" } else { cached_outcome };
                    metrics::counter!("bsc.build_diagnostics.state_reads_total", "kind" => self.kind,
                        "turn" => self.turn, "stage" => stage, "layer" => self.layer,
                        "resource" => resource, "outcome" => outcome).increment(total.calls);
                    metrics::counter!("bsc.build_diagnostics.state_read_errors_total", "kind" => self.kind,
                        "turn" => self.turn, "stage" => stage, "layer" => self.layer,
                        "resource" => resource, "outcome" => outcome).increment(total.errors);
                    metrics::histogram!("bsc.build_diagnostics.state_read_seconds", "kind" => self.kind,
                        "turn" => self.turn, "stage" => stage, "layer" => self.layer,
                        "resource" => resource, "outcome" => outcome).record(total.elapsed.as_secs_f64());
                }
            }
        }
    }
}

/// User execution accounting, enabled only by explicit miner attempt labels.
/// Owns only local counters and flushes even if its executor returns an error.
pub(crate) struct ExecutionObservation {
    labels: Option<(&'static str, &'static str)>,
    elapsed: [Duration; 3],
    calls: [u64; 3],
}
impl ExecutionObservation {
    pub(crate) fn new(labels: Option<(&'static str, &'static str)>) -> Self {
        Self { labels, elapsed: [Duration::ZERO; 3], calls: [0; 3] }
    }
    pub(crate) fn start(&self) -> Option<Instant> {
        self.labels.map(|_| Instant::now())
    }
    pub(crate) fn record(&mut self, stage: usize, started: Option<Instant>) {
        if let Some(started) = started {
            self.elapsed[stage] += started.elapsed();
            self.calls[stage] += 1;
        }
    }
}
impl Drop for ExecutionObservation {
    fn drop(&mut self) {
        let Some((kind, turn)) = self.labels else { return };
        // without_commit includes transact; expose disjoint residual processing time.
        let elapsed =
            [self.elapsed[0].saturating_sub(self.elapsed[1]), self.elapsed[1], self.elapsed[2]];
        for (i, stage) in
            ["executor_prepare", "evm_transact", "receipt_commit"].into_iter().enumerate()
        {
            if self.calls[i] == 0 {
                continue;
            }
            metrics::histogram!("bsc.build_diagnostics.transaction_stage_seconds",
                "kind" => kind, "turn" => turn, "stage" => stage)
            .record(elapsed[i].as_secs_f64());
            metrics::counter!("bsc.build_diagnostics.transaction_stage_calls_total",
                "kind" => kind, "turn" => turn, "stage" => stage)
            .increment(self.calls[i]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use metrics_exporter_prometheus::PrometheusBuilder;

    #[derive(Debug)]
    struct TestError(&'static str);
    impl std::fmt::Display for TestError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(self.0)
        }
    }
    impl std::error::Error for TestError {}
    impl revm_database_interface::DBErrorMarker for TestError {}

    #[derive(Debug, Default)]
    struct TestDb {
        absent: bool,
        fail: bool,
    }
    impl DatabaseRef for TestDb {
        type Error = TestError;
        fn basic_ref(&self, _: Address) -> Result<Option<AccountInfo>, Self::Error> {
            if self.fail {
                return Err(TestError("account read failed"));
            }
            Ok((!self.absent).then(AccountInfo::default))
        }
        fn storage_ref(&self, _: Address, _: U256) -> Result<U256, Self::Error> {
            if self.fail {
                return Err(TestError("storage read failed"));
            }
            Ok(U256::from(7))
        }
        fn code_by_hash_ref(&self, _: B256) -> Result<Bytecode, Self::Error> {
            if self.fail {
                return Err(TestError("code read failed"));
            }
            Ok(Bytecode::default())
        }
        fn block_hash_ref(&self, _: u64) -> Result<B256, Self::Error> {
            Ok(B256::ZERO)
        }
    }

    #[test]
    fn cold_and_warm_reads_preserve_values_and_count_each_layer() {
        let mut cache = CachedReads::default();
        let (mut db, phase) =
            ObservedStateDb::new(&mut cache, TestDb::default(), AttemptKind::Regular, true);
        phase.selection();
        for _ in 0..2 {
            assert_eq!(db.basic(Address::ZERO).unwrap(), Some(AccountInfo::default()));
            assert_eq!(db.storage(Address::ZERO, U256::ZERO).unwrap(), U256::from(7));
            assert_eq!(db.code_by_hash(B256::ZERO).unwrap(), Bytecode::default());
        }
        assert_eq!(db.inner.db.calls.get(), 3);
        for resource in 0..3 {
            assert_eq!(db.reads.totals[1][resource][0].get().calls, 1);
            assert_eq!(db.reads.totals[1][resource][1].get().calls, 1);
            assert_eq!(db.inner.db.reads.totals[1][resource][0].get().calls, 1);
        }
        // BLOCKHASH remains delegated, outside account/storage/code accounting.
        assert_eq!(db.block_hash(1).unwrap(), B256::ZERO);
        assert_eq!(db.inner.db.calls.get(), 3);
        drop(db);
        // Reusing the cache must not carry observations into a new attempt.
        let (mut db, _) =
            ObservedStateDb::new(&mut cache, TestDb::default(), AttemptKind::Rebuild, true);
        db.basic(Address::ZERO).unwrap();
        assert_eq!(db.inner.db.calls.get(), 0);
        assert_eq!(db.reads.totals[0][0][0].get().calls, 1);
    }

    fn populated_cache(parent: B256) -> Arc<super::super::prewarm::SharedStateCache> {
        let cache = super::super::prewarm::SharedStateCache::new(parent);
        cache.insert_account(Address::ZERO, Some(AccountInfo::default()));
        cache.insert_storage(Address::ZERO, U256::ZERO, U256::from(7));
        cache.insert_storage(Address::ZERO, U256::from(1), U256::ZERO);
        cache.insert_account(Address::repeat_byte(1), None);
        cache.insert_code(
            B256::repeat_byte(2),
            Bytecode::new_raw(alloy_primitives::Bytes::from_static(&[0x00])),
        );
        cache
    }

    #[test]
    fn prewarm_reads_preserve_zero_absent_and_code_without_backing_reads() {
        let parent = B256::repeat_byte(3);
        let shared = populated_cache(parent);
        let mut cache = CachedReads::default();
        let (db, phase) = ObservedStateDb::new(
            &mut cache,
            TestDb { fail: true, absent: false },
            AttemptKind::Regular,
            true,
        );
        let mut db = db.with_shared_prewarm(parent, Some(shared.clone()));
        phase.selection();
        assert_eq!(db.basic(Address::ZERO).unwrap(), Some(AccountInfo::default()));
        assert_eq!(db.storage(Address::ZERO, U256::ZERO).unwrap(), U256::from(7));
        assert_eq!(db.storage(Address::ZERO, U256::from(1)).unwrap(), U256::ZERO);
        assert_eq!(db.basic(Address::repeat_byte(1)).unwrap(), None);
        assert_eq!(db.code_by_hash(B256::repeat_byte(2)).unwrap().bytes().as_ref(), &[0x00]);
        assert_eq!(db.inner.db.reads.totals[1][0][0].get().calls, 0);
        assert_eq!(db.inner.db.prewarm_reads.totals[1][0][0].get().calls, 2);
        assert_eq!(db.inner.db.prewarm_reads.totals[1][1][0].get().calls, 2);
        assert_eq!(db.inner.db.prewarm_reads.totals[1][2][0].get().calls, 1);
        // A local cache change must not mutate the shared original-state cache.
        db.inner
            .cached
            .accounts
            .get_mut(&Address::ZERO)
            .unwrap()
            .storage
            .insert(U256::ZERO, U256::from(99));
        assert_eq!(shared.storage(Address::ZERO, U256::ZERO), Some(U256::from(7)));
    }

    #[test]
    fn prewarm_parent_mismatch_is_ignored_and_missing_values_preserve_errors() {
        let parent = B256::repeat_byte(3);
        for requested in [parent, B256::ZERO] {
            let mut cache = CachedReads::default();
            let (db, _) = ObservedStateDb::new(
                &mut cache,
                TestDb { fail: true, absent: false },
                AttemptKind::Regular,
                true,
            );
            let mut db = db.with_shared_prewarm(requested, Some(populated_cache(parent)));
            assert_eq!(db.inner.db.shared.is_some(), requested == parent);
            let address = if requested == parent { Address::repeat_byte(9) } else { Address::ZERO };
            assert_eq!(db.basic(address).unwrap_err().to_string(), "account read failed");
            assert_eq!(
                db.code_by_hash(B256::repeat_byte(9)).unwrap_err().to_string(),
                "code read failed"
            );
            assert!(db.inner.cached.accounts.is_empty());
        }
    }

    #[test]
    fn prewarm_and_backing_values_match_for_the_same_parent() {
        let parent = B256::repeat_byte(3);
        let mut cold_cache = CachedReads::default();
        let mut warm_cache = CachedReads::default();
        let (mut cold, _) =
            ObservedStateDb::new(&mut cold_cache, TestDb::default(), AttemptKind::Regular, true);
        let (warm, _) =
            ObservedStateDb::new(&mut warm_cache, TestDb::default(), AttemptKind::Regular, true);
        let mut warm = warm.with_shared_prewarm(parent, Some(populated_cache(parent)));
        assert_eq!(cold.basic(Address::ZERO).unwrap(), warm.basic(Address::ZERO).unwrap());
        assert_eq!(
            cold.storage(Address::ZERO, U256::ZERO).unwrap(),
            warm.storage(Address::ZERO, U256::ZERO).unwrap()
        );
        assert_eq!(cold.block_hash(1).unwrap(), warm.block_hash(1).unwrap());
        assert_eq!(warm.inner.db.reads.totals[0][0][0].get().calls, 0);
        assert_eq!(cold.inner.db.reads.totals[0][0][0].get().calls, 1);
    }

    #[test]
    fn running_attempt_consumes_reads_produced_after_it_started() {
        let parent = B256::repeat_byte(3);
        let shared = super::super::prewarm::SharedStateCache::new(parent);
        let mut local = CachedReads::default();
        let (db, _) = ObservedStateDb::new(
            &mut local,
            TestDb { fail: true, absent: false },
            AttemptKind::Regular,
            true,
        );
        let mut db = db.with_shared_prewarm(parent, Some(shared.clone()));
        // Attach an empty cache, then establish that the backing provider cannot serve it.
        assert!(db.basic(Address::ZERO).is_err());
        std::thread::spawn(move || {
            shared.insert_account(Address::ZERO, Some(AccountInfo::default()));
            shared.insert_storage(Address::ZERO, U256::ZERO, U256::from(7));
        })
        .join()
        .unwrap();
        // No refresh, re-attachment or new attempt: this same DB sees the later worker reads.
        assert_eq!(db.basic(Address::ZERO).unwrap(), Some(AccountInfo::default()));
        assert_eq!(db.storage(Address::ZERO, U256::ZERO).unwrap(), U256::from(7));
        assert_eq!(db.inner.db.reads.totals[0][0][0].get().calls, 1);
        assert_eq!(db.inner.db.prewarm_reads.totals[0][0][0].get().calls, 1);
    }

    #[test]
    fn storage_miss_includes_implicit_account_load_and_negative_accounts_are_hits() {
        let mut cache = CachedReads::default();
        let (mut db, _) =
            ObservedStateDb::new(&mut cache, TestDb::default(), AttemptKind::Regular, true);
        assert_eq!(db.storage(Address::ZERO, U256::ZERO).unwrap(), U256::from(7));
        assert_eq!(db.reads.totals[0][1][1].get().calls, 1);
        assert_eq!(db.inner.db.reads.totals[0][0][0].get().calls, 1);
        assert_eq!(db.inner.db.reads.totals[0][1][0].get().calls, 1);
        assert_eq!(db.reads.totals[0][0][1].get().calls, 0);

        let mut cache = CachedReads::default();
        let (mut db, _) = ObservedStateDb::new(
            &mut cache,
            TestDb { absent: true, fail: false },
            AttemptKind::Empty,
            false,
        );
        assert_eq!(db.storage(Address::ZERO, U256::ZERO).unwrap(), U256::ZERO);
        assert_eq!(db.inner.db.calls.get(), 1);
        assert_eq!(db.inner.db.reads.totals[0][1][0].get().calls, 0);
        assert_eq!(db.basic(Address::ZERO).unwrap(), None);
        assert_eq!(db.reads.totals[0][0][0].get().calls, 1);
    }

    #[test]
    fn errors_are_counted_as_misses_and_not_cached() {
        let mut cache = CachedReads::default();
        cache.insert_account(Address::ZERO, AccountInfo::default(), Default::default());
        let (mut db, _) = ObservedStateDb::new(
            &mut cache,
            TestDb { fail: true, absent: false },
            AttemptKind::Regular,
            true,
        );
        let missing = Address::repeat_byte(1);
        for _ in 0..2 {
            assert_eq!(db.basic(missing).unwrap_err().to_string(), "account read failed");
            assert_eq!(
                db.storage(Address::ZERO, U256::ZERO).unwrap_err().to_string(),
                "storage read failed"
            );
            assert_eq!(db.code_by_hash(B256::ZERO).unwrap_err().to_string(), "code read failed");
        }
        for resource in 0..3 {
            let cached = db.reads.totals[0][resource][1].get();
            let provider = db.inner.db.reads.totals[0][resource][0].get();
            assert_eq!((cached.calls, cached.errors), (2, 2));
            assert_eq!((provider.calls, provider.errors), (2, 2));
        }
        assert!(!db.inner.cached.accounts.contains_key(&missing));
        assert!(db.inner.cached.contracts.is_empty());
    }

    #[test]
    fn drop_exports_partial_build_reads_with_phase_labels() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            let mut cache = CachedReads::default();
            let (mut db, phase) = ObservedStateDb::new(
                &mut cache,
                TestDb::default(),
                AttemptKind::Speculative,
                false,
            );
            db.basic(Address::ZERO).unwrap();
            phase.selection();
            db.basic(Address::ZERO).unwrap();
            phase.finish();
            db.inner.db.inner.fail = true;
            assert!(db.code_by_hash(B256::ZERO).is_err());
            // Error/drop path: no explicit finish/flush call.
        });
        let rendered = handle.render();
        for labels in [
            ["stage=\"prepare\"", "layer=\"cached\"", "outcome=\"miss\""],
            ["stage=\"selection\"", "layer=\"cached\"", "outcome=\"hit\""],
            ["stage=\"finish\"", "layer=\"provider\"", "outcome=\"read\""],
        ] {
            assert!(
                rendered.lines().any(
                    |l| l.starts_with("bsc_build_diagnostics_state_reads_total{") &&
                        labels.iter().all(|label| l.contains(label)) &&
                        l.ends_with(" 1")
                ),
                "{rendered}"
            );
        }
        assert!(
            rendered
                .lines()
                .any(|l| l.starts_with("bsc_build_diagnostics_state_read_errors_total{") &&
                    l.contains("stage=\"finish\"") &&
                    l.ends_with(" 1")),
            "{rendered}"
        );
        assert!(rendered.contains("bsc_build_diagnostics_state_read_seconds_sum"));
    }

    #[test]
    fn executor_phases_are_disjoint_and_disabled_outside_miner() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            let mut disabled = ExecutionObservation::new(None);
            assert!(disabled.start().is_none());
            disabled.record(0, disabled.start());
            assert_eq!(disabled.calls, [0; 3]);
            let mut observed = ExecutionObservation::new(Some(("regular", "inturn")));
            observed.elapsed =
                [Duration::from_millis(10), Duration::from_millis(7), Duration::from_millis(2)];
            observed.calls = [3, 2, 1];
        });
        let rendered = handle.render();
        for (stage, time) in
            [("executor_prepare", "0.003"), ("evm_transact", "0.007"), ("receipt_commit", "0.002")]
        {
            assert!(
                rendered.lines().any(|l| l
                    .starts_with("bsc_build_diagnostics_transaction_stage_seconds_sum") &&
                    l.contains(stage) &&
                    l.ends_with(time)),
                "{rendered}"
            );
        }
        assert_eq!(
            rendered
                .lines()
                .filter(|l| l.starts_with("bsc_build_diagnostics_transaction_stage_seconds_count"))
                .count(),
            3
        );
    }
}
