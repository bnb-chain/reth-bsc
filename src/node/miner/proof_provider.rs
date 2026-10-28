//! Miner-only immutable parent overlays. No candidate writes, DB transactions or cursors are
//! cached.
use alloy_primitives::B256;
use parking_lot::Mutex;
use reth_provider::{
    BlockNumReader, ChangeSetReader, DatabaseProviderFactory, DatabaseProviderROFactory,
    ProviderResult, PruneCheckpointReader, StageCheckpointReader, StorageChangeSetReader,
    StorageSettingsCache,
};
use reth_storage_overlay::{
    database_state_frontiers, OverlayBuilder, OverlayStateProvider, OwnedProvider, StateTrieOverlay,
};
use reth_tasks::Runtime;
use std::{
    collections::VecDeque,
    sync::{Arc, OnceLock},
    time::Instant,
};
use tokio::sync::Semaphore;

// Bound ready overlays independently of persistence progress. In-flight readers retain their Arc.
const MAX_OVERLAYS: usize = 16;
// Includes queued and active destruction; overflow is released on the requesting worker.
const MAX_PENDING_RETIREMENTS: usize = 2;
type Key = (B256, B256, B256);

#[derive(Default)]
struct Entry {
    ready: OnceLock<StateTrieOverlay>,
    computing: Mutex<()>,
}

pub(crate) struct MinerProofOverlayCache {
    entries: Mutex<VecDeque<(Key, Arc<Entry>)>>,
    runtime: Runtime,
    retirement_slots: Arc<Semaphore>,
}

impl MinerProofOverlayCache {
    pub(crate) fn new(runtime: Runtime) -> Self {
        Self {
            entries: Mutex::new(VecDeque::new()),
            runtime,
            retirement_slots: Arc::new(Semaphore::new(MAX_PENDING_RETIREMENTS)),
        }
    }

    fn entry(&self, key: Key, fallback: bool) -> Arc<Entry> {
        let start = Instant::now();
        let mut entries = self.entries.lock();
        let acquired = Instant::now();
        let (entry, retired) = if let Some(index) = entries.iter().position(|(k, _)| *k == key) {
            let item = entries.remove(index).expect("index exists");
            let entry = item.1.clone();
            entries.push_back(item);
            (entry, None)
        } else {
            let entry = Arc::new(Entry::default());
            let retired = if entries.len() == MAX_OVERLAYS { entries.pop_front() } else { None };
            entries.push_back((key, entry.clone()));
            (entry, retired)
        };
        drop(entries);
        let released = Instant::now();
        // Large overlay data must never be released under the cache index lock.
        let evicted = retired.is_some();
        if let Some((_, retired)) = retired {
            self.retire(retired, fallback);
        }
        let retired_at = Instant::now();
        // Record caller latency after unlocking. Background destruction has a separate timer.
        duration("cache_index_wait", acquired.duration_since(start), fallback);
        duration("cache_index_work", released.duration_since(acquired), fallback);
        if evicted {
            duration("cache_retire", retired_at.duration_since(released), fallback);
            metrics::counter!("bsc_miner_proof_overlay_evictions_total").increment(1);
        }
        entry
    }

    fn retire(&self, retired: Arc<Entry>, fallback: bool) {
        let permit = self.retirement_slots.clone().try_acquire_owned();
        let mode = if permit.is_ok() { "background" } else { "inline" };
        metrics::counter!("bsc_miner_proof_overlay_retirements_total", "mode" => mode).increment(1);
        let elapsed = metrics::histogram!("bsc_miner_proof_overlay_retire_seconds",
            "mode" => mode, "kind" => if fallback { "empty" } else { "normal" });
        if let Ok(permit) = permit {
            let span = tracing::Span::current();
            // The permit bounds this cache's contribution to the shared drop worker's queue.
            self.runtime.spawn_blocking_named("drop", move || {
                let _entered = span.enter();
                let start = Instant::now();
                drop(retired);
                elapsed.record(start.elapsed().as_secs_f64());
                drop(permit);
            });
        } else {
            let start = Instant::now();
            drop(retired);
            elapsed.record(start.elapsed().as_secs_f64());
        }
    }

    fn get_or_build(
        &self,
        key: Key,
        fallback: bool,
        build: impl FnOnce() -> ProviderResult<StateTrieOverlay>,
    ) -> ProviderResult<StateTrieOverlay> {
        let start = Instant::now();
        let entry = self.entry(key, fallback);
        stage("cache_lookup", start, fallback);
        if let Some(overlay) = entry.ready.get() {
            metrics::counter!("bsc_miner_proof_overlay_cache_total", "result" => "hit")
                .increment(1);
            return Ok(overlay.clone());
        }
        // Empty fallback shares only completed values: never queue it behind a normal build.
        let start = Instant::now();
        let _guard = if fallback { None } else { Some(entry.computing.lock()) };
        stage("cache_wait", start, fallback);
        if let Some(overlay) = entry.ready.get() {
            metrics::counter!("bsc_miner_proof_overlay_cache_total", "result" => "shared")
                .increment(1);
            return Ok(overlay.clone());
        }
        let start = Instant::now();
        let result = build();
        stage("build", start, fallback);
        // Errors/cancellation never poison the cache. A fallback may race a normal builder;
        // both values describe the exact same parent and durable frontiers.
        let overlay = result?;
        // O(1) top-level counts, sampled once per successful build, not once per cache hit.
        // These are neither storage-slot counts nor allocated-byte estimates.
        for (component, count) in [
            ("accounts", overlay.input().state.accounts.len()),
            ("storage_accounts", overlay.input().state.storages.len()),
            ("account_trie_nodes", overlay.input().nodes.account_nodes_ref().len()),
            ("storage_tries", overlay.input().nodes.storage_tries_ref().len()),
        ] {
            metrics::histogram!("bsc_miner_proof_overlay_build_items",
                "kind" => if fallback { "empty" } else { "normal" }, "component" => component)
            .record(count as f64);
        }
        let _ = entry.ready.set(overlay.clone());
        metrics::counter!("bsc_miner_proof_overlay_cache_total", "result" => if fallback { "fallback_fill" } else { "fill" }).increment(1);
        Ok(overlay)
    }
}

fn stage(phase: &'static str, start: Instant, fallback: bool) {
    duration(phase, start.elapsed(), fallback);
}

fn duration(phase: &'static str, elapsed: std::time::Duration, fallback: bool) {
    // Wall time, not CPU time; concurrent workers overlap. No hashes or task IDs in labels.
    metrics::histogram!("bsc_miner_proof_overlay_prepare_seconds", "phase" => phase, "kind" => if fallback { "empty" } else { "normal" })
        .record(elapsed.as_secs_f64());
}

#[derive(Clone)]
pub(crate) struct MinerProofProviderFactory<F> {
    factory: F,
    builder: OverlayBuilder<crate::BscPrimitives>,
    parent: B256,
    cache: Arc<MinerProofOverlayCache>,
    fallback: bool,
}

impl<F> MinerProofProviderFactory<F> {
    pub(crate) fn new(
        factory: F,
        builder: OverlayBuilder<crate::BscPrimitives>,
        parent: B256,
        cache: Arc<MinerProofOverlayCache>,
        fallback: bool,
    ) -> Self {
        Self { factory, builder, parent, cache, fallback }
    }
}

impl<F> DatabaseProviderROFactory for MinerProofProviderFactory<F>
where
    F: DatabaseProviderFactory,
    F::Provider: StageCheckpointReader
        + BlockNumReader
        + PruneCheckpointReader
        + ChangeSetReader
        + StorageChangeSetReader
        + StorageSettingsCache,
{
    type Provider = OverlayStateProvider<OwnedProvider<F::Provider>, crate::BscPrimitives>;

    fn database_provider_ro(&self) -> ProviderResult<Self::Provider> {
        let start = Instant::now();
        let provider = self.factory.database_provider_ro()?;
        stage("database_provider", start, self.fallback);
        let start = Instant::now();
        let (trie, finish) = database_state_frontiers(&provider)?;
        stage("frontiers", start, self.fallback);
        let overlay = self.cache.get_or_build(
            (self.parent, trie.hash, finish.hash),
            self.fallback,
            || self.builder.build_state_trie_overlay_at_frontiers(&provider, trie, finish, true),
        )?;
        Ok(OverlayStateProvider::new(provider, self.builder.clone())
            .with_state_trie_overlay(overlay))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reth_trie_common::{updates::TrieUpdatesSorted, HashedPostState};
    fn value() -> StateTrieOverlay {
        StateTrieOverlay::new(reth_trie_common::TrieInputSorted::new(
            Arc::new(TrieUpdatesSorted::default()),
            Arc::new(
                HashedPostState::default()
                    .with_accounts([(
                        B256::with_last_byte(9),
                        Some(reth_primitives_traits::Account { nonce: 7, ..Default::default() }),
                    )])
                    .into_sorted(),
            ),
            Default::default(),
        ))
    }
    fn root(overlay: &StateTrieOverlay) -> B256 {
        use reth_provider::{
            providers::LatestStateProvider, test_utils::create_test_provider_factory,
            StateRootProvider,
        };
        let factory = create_test_provider_factory();
        LatestStateProvider::new(factory.provider().unwrap())
            .state_root(HashedPostState {
                accounts: overlay.input().state.accounts.iter().copied().collect(),
                storages: Default::default(),
            })
            .unwrap()
    }

    fn key(n: u8) -> Key {
        (B256::with_last_byte(n), B256::ZERO, B256::ZERO)
    }
    #[test]
    fn telemetry_preserves_failure_retry_and_records_retirement_only_on_eviction() {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            let cache = MinerProofOverlayCache::new(Runtime::test());
            assert!(cache
                .get_or_build(key(1), false, || {
                    Err(reth_provider::ProviderError::other(std::io::Error::other("failure")))
                })
                .is_err());
            cache.get_or_build(key(1), false, || Ok(value())).unwrap();
            cache.get_or_build(key(1), false, || panic!("must hit")).unwrap();
            for n in 2..=MAX_OVERLAYS as u8 + 1 {
                cache.get_or_build(key(n), false, || Ok(value())).unwrap();
            }
        });
        let text = handle.render();
        for phase in [
            "cache_index_wait",
            "cache_index_work",
            "cache_lookup",
            "cache_wait",
            "build",
            "cache_retire",
        ] {
            assert!(text.contains(&format!("phase=\"{phase}\"")), "{text}");
        }
        assert!(text.contains("bsc_miner_proof_overlay_evictions_total 1"), "{text}");
        assert!(text.contains("component=\"accounts\""), "{text}");
        // Failed initialization has no item sample; a ready cache hit adds none either.
        assert!(text.contains("bsc_miner_proof_overlay_build_items_count{kind=\"normal\",component=\"accounts\"} 17"), "{text}");
    }

    #[test]
    fn ready_values_reuse_but_parent_and_both_frontiers_isolate() {
        let cache = MinerProofOverlayCache::new(Runtime::test());
        let first = cache.get_or_build(key(1), false, || Ok(value())).unwrap();
        let retry =
            cache.get_or_build(key(1), true, || panic!("ready value must be reused")).unwrap();
        assert!(Arc::ptr_eq(&first.input().state, &retry.input().state));
        assert_eq!(root(&first), root(&retry));
        for k in [
            key(2),
            (key(1).0, B256::with_last_byte(1), B256::ZERO),
            (key(1).0, B256::ZERO, B256::with_last_byte(1)),
        ] {
            let other = cache.get_or_build(k, false, || Ok(value())).unwrap();
            assert_eq!(root(&first), root(&other));
            assert!(!Arc::ptr_eq(&first.input().state, &other.input().state));
        }
    }
    #[test]
    fn fallback_does_not_wait_for_normal_initialization() {
        let cache = Arc::new(MinerProofOverlayCache::new(Runtime::test()));
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let normal_cache = cache.clone();
        let normal = std::thread::spawn(move || {
            normal_cache.get_or_build(key(1), false, || {
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Ok(value())
            })
        });
        started_rx.recv().unwrap();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let fallback = std::thread::spawn(move || {
            let result = cache.get_or_build(key(1), true, || Ok(value()));
            done_tx.send(result.is_ok()).unwrap();
        });
        let done = done_rx.recv_timeout(std::time::Duration::from_secs(2));
        release_tx.send(()).unwrap();
        normal.join().unwrap().unwrap();
        fallback.join().unwrap();
        assert!(done.unwrap());
    }

    #[test]
    fn concurrent_normal_workers_build_once() {
        let cache = Arc::new(MinerProofOverlayCache::new(Runtime::test()));
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let barrier = std::sync::Barrier::new(4);
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..4)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        cache
                            .get_or_build(key(1), false, || {
                                calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                Ok(value())
                            })
                            .unwrap()
                    })
                })
                .collect();
            let overlays: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
            for overlay in &overlays[1..] {
                assert!(Arc::ptr_eq(&overlays[0].input().state, &overlay.input().state));
            }
        });
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    #[test]
    fn failed_build_can_be_retried() {
        let cache = MinerProofOverlayCache::new(Runtime::test());
        let error = cache.get_or_build(key(1), false, || {
            Err(reth_provider::ProviderError::other(std::io::Error::other("injected failure")))
        });
        assert!(error.is_err());
        let result = cache.get_or_build(key(1), false, || Ok(value())).unwrap();
        let cached =
            cache.get_or_build(key(1), false, || panic!("retry must cache success")).unwrap();
        assert!(Arc::ptr_eq(&result.input().state, &cached.input().state));
    }

    #[test]
    fn eviction_keeps_inflight_values_alive() {
        let cache = MinerProofOverlayCache::new(Runtime::test());
        let original = cache.get_or_build(key(1), false, || Ok(value())).unwrap();
        for n in 2..=MAX_OVERLAYS as u8 + 1 {
            cache.get_or_build(key(n), false, || Ok(value())).unwrap();
        }
        assert_eq!(cache.entries.lock().len(), MAX_OVERLAYS);
        let rebuilt = cache.get_or_build(key(1), false, || Ok(value())).unwrap();
        assert!(!Arc::ptr_eq(&original.input().state, &rebuilt.input().state));
        assert_eq!(*original.input().state, *rebuilt.input().state);
    }

    #[test]
    fn background_retirement_is_bounded_and_overflow_drops_inline() {
        let runtime = Runtime::test();
        let cache = MinerProofOverlayCache::new(runtime.clone());
        let (release, wait) = std::sync::mpsc::channel();
        runtime.spawn_blocking_named("drop", move || {
            wait.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        });
        let mut queued = Vec::new();
        for _ in 0..MAX_PENDING_RETIREMENTS {
            let entry = Arc::new(Entry::default());
            queued.push(Arc::downgrade(&entry));
            cache.retire(entry, false);
        }
        assert_eq!(cache.retirement_slots.available_permits(), 0);
        assert!(queued.iter().all(|entry| entry.upgrade().is_some()));
        let overflow = Arc::new(Entry::default());
        let weak = Arc::downgrade(&overflow);
        cache.retire(overflow, true);
        assert!(weak.upgrade().is_none());
        assert_eq!(cache.retirement_slots.available_permits(), 0);
        release.send(()).unwrap();
        let (done, drained) = std::sync::mpsc::channel();
        runtime.spawn_blocking_named("drop", move || done.send(()).unwrap());
        drained.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert!(queued.iter().all(|entry| entry.upgrade().is_none()));
        assert_eq!(cache.retirement_slots.available_permits(), MAX_PENDING_RETIREMENTS);
    }

    #[test]
    fn closed_retirement_slots_drop_inline() {
        let cache = MinerProofOverlayCache::new(Runtime::test());
        cache.retirement_slots.close();
        let entry = Arc::new(Entry::default());
        let weak = Arc::downgrade(&entry);
        cache.retire(entry, false);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn dropping_cache_drains_background_retirements_and_preserves_readers() {
        let cache = MinerProofOverlayCache::new(Runtime::test());
        let original = cache.get_or_build(key(1), false, || Ok(value())).unwrap();
        let entry = cache.entry(key(1), false);
        let weak = Arc::downgrade(&entry);
        drop(entry);
        for n in 2..=MAX_OVERLAYS as u8 + 1 {
            cache.get_or_build(key(n), false, || Ok(value())).unwrap();
        }
        drop(cache);
        assert!(weak.upgrade().is_none());
        assert_eq!(original.input().state.accounts[0].1.unwrap().nonce, 7);
    }
}
