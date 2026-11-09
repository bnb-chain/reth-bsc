//! Bounded, volatile block data promised by this node's ETH announcements.
//!
//! TODO: Match Geth's persistent side-branch storage separately. Restart loses this cache, and
//! capacity pressure can evict an entry before its thirty-minute publication lifetime ends.

use super::BscNetworkPrimitives;
use crate::BscBlock;
use alloy_eips::eip4844::Blob;
use alloy_primitives::{B256, U256};
use parking_lot::Mutex;
use reth_network::eth_requests::EthBlockSource;
use reth_primitives_traits::InMemorySize;
use schnellru::{ByLength, LruMap};
use std::{
    fmt,
    mem::size_of,
    sync::Arc,
    time::{Duration, Instant},
};

const RETENTION: Duration = Duration::from_secs(30 * 60);
const MAX_ENTRIES: u32 = 8192;
const MAX_BYTES: u64 = 4 * 1024 * 1024 * 1024;
/// Includes hash-table spare capacity, links, the Arc allocation and allocator bookkeeping.
const ENTRY_OVERHEAD: usize = 512;
/// Cache-only cleanup has no database work; large allocations are released after unlocking.
const CLEANUP_BUDGET: usize = 256;

pub(crate) struct AnnouncedBlocks {
    inner: Mutex<RetainedBlocks>,
    ttl: Duration,
    max_entries: u32,
    max_bytes: u64,
}

struct RetainedBlocks {
    blocks: LruMap<B256, RetainedBlock, ByLength>,
    bytes: u64,
    sidechains: usize,
    unclassified: usize,
    /// Changes on canonical notifications, preventing stale lock-free observations from applying.
    view_version: u64,
    next_identity: u64,
    classification_cursor: usize,
}

struct RetainedBlock {
    block: Arc<BscBlock>,
    td: U256,
    expires: Instant,
    bytes: u64,
    classification: Classification,
    identity: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Classification {
    Canonical,
    Sidechain,
    Unknown,
}

/// A bounded observation token; an eviction/reinsertion or reorg invalidates its result.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ClassificationProbe {
    pub(crate) hash: B256,
    pub(crate) number: u64,
    view_version: u64,
    identity: u64,
}

impl Default for AnnouncedBlocks {
    fn default() -> Self {
        Self::new(RETENTION, MAX_ENTRIES, MAX_BYTES)
    }
}

impl fmt::Debug for AnnouncedBlocks {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = formatter.debug_struct("AnnouncedBlocks");
        debug
            .field("ttl", &self.ttl)
            .field("max_entries", &self.max_entries)
            .field("max_bytes", &self.max_bytes);
        if let Some(inner) = self.inner.try_lock() {
            debug.field("entries", &inner.blocks.len()).field("accounted_bytes", &inner.bytes);
        }
        debug.finish_non_exhaustive()
    }
}

impl AnnouncedBlocks {
    pub(super) fn new(ttl: Duration, max_entries: u32, max_bytes: u64) -> Self {
        let mut blocks = LruMap::new(ByLength::new(max_entries));
        blocks.reserve_or_panic(max_entries as usize);
        let cache = Self {
            inner: Mutex::new(RetainedBlocks {
                blocks,
                bytes: 0,
                sidechains: 0,
                unclassified: 0,
                view_version: 0,
                next_identity: 0,
                classification_cursor: 0,
            }),
            ttl,
            max_entries,
            max_bytes,
        };
        cache.inner.lock().observe();
        cache
    }

    /// Capture before provider IO, then pass to `retain`; a racing reorg leaves classification
    /// unknown for the bounded maintenance pass instead of installing a stale canonical flag.
    pub(crate) fn view_version(&self) -> u64 {
        self.inner.lock().view_version
    }

    /// Retains an already validated, exact-hash block before publication. No execution outcome,
    /// receipts or trie allocations are kept. A failed admission must suppress that announcement.
    pub(crate) fn retain(
        &self,
        block: Arc<BscBlock>,
        td: U256,
        classification: Classification,
        observed_version: u64,
    ) -> bool {
        self.retain_at(block, td, classification, observed_version, None)
    }

    fn retain_at(
        &self,
        block: Arc<BscBlock>,
        td: U256,
        classification: Classification,
        observed_version: u64,
        now: Option<Instant>,
    ) -> bool {
        // Hashing and dynamic-buffer accounting must not hold the cache lock.
        let hash = block.header.hash_slow();
        let bytes = accounted_size(&block);
        if bytes > self.max_bytes || self.max_entries == 0 {
            metrics::counter!("bsc.network.retention.admission_rejected_total", "reason" => "oversize").increment(1);
            return false;
        }
        let mut removed = Vec::new();
        let mut inner = self.inner.lock();
        // Production timestamps follow admission order even when callers compute sizes in
        // parallel. Tests inject a monotonic clock at this same boundary.
        let now = now.unwrap_or_else(Instant::now);
        inner.expire(now, usize::MAX, &mut removed);
        if let Some(previous) = inner.blocks.remove(&hash) {
            inner.remove_accounting(&previous);
            removed.push(previous);
        }
        while inner.blocks.len() >= self.max_entries as usize ||
            inner.bytes.saturating_add(bytes) > self.max_bytes
        {
            let Some((_, entry)) = inner.blocks.pop_oldest() else { break };
            inner.remove_accounting(&entry);
            removed.push(entry);
            metrics::counter!("bsc.network.retention.evictions_total", "reason" => "capacity")
                .increment(1);
        }
        let classification = if observed_version == inner.view_version {
            classification
        } else {
            Classification::Unknown
        };
        inner.next_identity = inner.next_identity.wrapping_add(1);
        let identity = inner.next_identity;
        // Schnellru's fallible insertion may silently evict on allocation failure. Reserve
        // explicitly so only the accounted removals above can release entries. Deletions can
        // reduce guaranteed capacity (tombstones), even after the initial fixed-size reserve.
        inner.blocks.reserve_or_panic(1);
        // Keep this caller's Arc alive until after unlocking, including allocation failure.
        let admitted = inner.blocks.insert(
            hash,
            RetainedBlock {
                block: Arc::clone(&block),
                td,
                expires: now + self.ttl,
                bytes,
                classification,
                identity,
            },
        );
        if admitted {
            inner.bytes += bytes;
            inner.add_classification(classification);
        } else {
            metrics::counter!("bsc.network.retention.admission_rejected_total", "reason" => "allocation").increment(1);
        }
        inner.observe();
        drop(inner);
        drop(removed);
        admitted
    }

    /// Exact-hash lookup never renews the deadline or promotes eviction order.
    pub(crate) fn get(&self, hash: B256) -> Option<(Arc<BscBlock>, U256)> {
        self.get_at(hash, Instant::now())
    }

    /// Renew only at an outbound publication boundary, never for inbound ETH reads.
    pub(crate) fn republish(&self, hash: B256) -> Option<(Arc<BscBlock>, U256)> {
        self.republish_at(hash, None)
    }

    fn republish_at(&self, hash: B256, now: Option<Instant>) -> Option<(Arc<BscBlock>, U256)> {
        let mut inner = self.inner.lock();
        let now = now.unwrap_or_else(Instant::now);
        if inner.blocks.peek(&hash)?.expires <= now {
            return None;
        }
        let entry = inner.blocks.get(&hash)?;
        entry.expires = now + self.ttl;
        Some((Arc::clone(&entry.block), entry.td))
    }

    fn get_at(&self, hash: B256, now: Instant) -> Option<(Arc<BscBlock>, U256)> {
        let mut inner = self.inner.lock();
        let entry = inner.blocks.peek(&hash)?;
        if entry.expires > now {
            return Some((Arc::clone(&entry.block), entry.td));
        }
        let removed = inner.blocks.remove(&hash);
        if let Some(entry) = &removed {
            inner.remove_accounting(entry);
            metrics::counter!("bsc.network.retention.evictions_total", "reason" => "ttl")
                .increment(1);
        }
        inner.observe();
        drop(inner);
        drop(removed);
        None
    }

    /// Runs from the import service's existing periodic tick, including when the cache is idle.
    pub(crate) fn maintain(&self) {
        self.maintain_at(Instant::now());
    }

    fn maintain_at(&self, now: Instant) {
        let mut removed = Vec::new();
        let mut inner = self.inner.lock();
        inner.expire(now, CLEANUP_BUDGET, &mut removed);
        inner.observe();
        drop(inner);
        drop(removed);
    }

    /// Updates only retained hashes; canonical blocks are not counted as sidechain blocks.
    pub(crate) fn canonical_change(
        &self,
        old: impl IntoIterator<Item = B256>,
        new: impl IntoIterator<Item = B256>,
    ) {
        let mut inner = self.inner.lock();
        inner.view_version = inner.view_version.wrapping_add(1);
        for hash in old {
            inner.classify(hash, Classification::Sidechain);
        }
        for hash in new {
            inner.classify(hash, Classification::Canonical);
        }
        inner.observe();
    }

    /// A dropped notification invalidates classification, not immutable block serviceability.
    pub(crate) fn notifications_lagged(&self, missed: u64) {
        let mut inner = self.inner.lock();
        inner.view_version = inner.view_version.wrapping_add(1);
        for (_, entry) in inner.blocks.iter_mut() {
            entry.classification = Classification::Unknown;
        }
        inner.sidechains = 0;
        inner.unclassified = inner.blocks.len();
        inner.observe();
        metrics::counter!("bsc.network.retention.canonical_notifications_lagged_total")
            .increment(missed);
    }

    /// Takes a small metadata batch. The caller performs all canonical provider IO after return.
    pub(crate) fn classification_batch(&self, limit: usize) -> Vec<ClassificationProbe> {
        let mut inner = self.inner.lock();
        let count = inner.blocks.len();
        if inner.unclassified == 0 || limit == 0 {
            return Vec::new();
        }
        let start = inner.classification_cursor % count;
        let mut probes = Vec::new();
        let mut examined = 0;
        // Rotate metadata only; reads and failed reconciliation never change eviction order.
        for (hash, entry) in inner.blocks.iter().skip(start).chain(inner.blocks.iter().take(start))
        {
            examined += 1;
            if entry.classification == Classification::Unknown {
                probes.push(ClassificationProbe {
                    hash: *hash,
                    number: entry.block.header.number,
                    view_version: inner.view_version,
                    identity: entry.identity,
                });
                if probes.len() == limit {
                    break;
                }
            }
        }
        inner.classification_cursor = (start + examined) % count;
        probes
    }

    /// Missing/error provider observations remain unknown and are retried on a later tick.
    pub(crate) fn apply_classification(
        &self,
        probe: ClassificationProbe,
        canonical_hash: Option<B256>,
    ) {
        let Some(canonical_hash) = canonical_hash else { return };
        let mut inner = self.inner.lock();
        if inner.view_version != probe.view_version ||
            inner.blocks.peek(&probe.hash).is_none_or(|entry| entry.identity != probe.identity)
        {
            return;
        }
        inner.classify(
            probe.hash,
            if canonical_hash == probe.hash {
                Classification::Canonical
            } else {
                Classification::Sidechain
            },
        );
        inner.observe();
    }
}

impl EthBlockSource<BscNetworkPrimitives> for AnnouncedBlocks {
    fn block_by_hash(&self, hash: B256) -> Option<Arc<BscBlock>> {
        self.get(hash).map(|(block, _)| block)
    }
}

impl RetainedBlocks {
    fn add_classification(&mut self, classification: Classification) {
        match classification {
            Classification::Canonical => {}
            Classification::Sidechain => self.sidechains += 1,
            Classification::Unknown => self.unclassified += 1,
        }
    }

    fn remove_classification(&mut self, classification: Classification) {
        match classification {
            Classification::Canonical => {}
            Classification::Sidechain => self.sidechains -= 1,
            Classification::Unknown => self.unclassified -= 1,
        }
    }

    fn remove_accounting(&mut self, entry: &RetainedBlock) {
        self.bytes -= entry.bytes;
        self.remove_classification(entry.classification);
    }

    fn classify(&mut self, hash: B256, classification: Classification) {
        let Some(entry) = self.blocks.peek_mut(&hash) else { return };
        let previous = std::mem::replace(&mut entry.classification, classification);
        self.remove_classification(previous);
        self.add_classification(classification);
    }

    fn expire(&mut self, now: Instant, limit: usize, removed: &mut Vec<RetainedBlock>) {
        for _ in 0..limit {
            if self.blocks.peek_oldest().is_none_or(|(_, entry)| entry.expires > now) {
                break;
            }
            if let Some((_, entry)) = self.blocks.pop_oldest() {
                self.remove_accounting(&entry);
                removed.push(entry);
                metrics::counter!("bsc.network.retention.evictions_total", "reason" => "ttl")
                    .increment(1);
            }
        }
    }

    fn observe(&self) {
        metrics::gauge!("bsc.network.retention.blocks").set(self.blocks.len() as f64);
        metrics::gauge!("bsc.network.retention.sidechain_blocks").set(self.sidechains as f64);
        metrics::gauge!("bsc.network.retention.classification_pending_blocks")
            .set(self.unclassified as f64);
        metrics::gauge!("bsc.network.retention.accounted_bytes").set(self.bytes as f64);
    }
}

/// Decoded-data accounting, deliberately conservative; this is not a process RSS limit.
fn accounted_size(block: &BscBlock) -> u64 {
    let mut bytes =
        block.size().saturating_add(size_of::<RetainedBlock>()).saturating_add(ENTRY_OVERHEAD);
    // Alloy's body heuristic sums populated transaction variants and headers, not the Vec
    // allocations. Charge their full reserved storage as well: this also conservatively covers
    // the difference between a small active transaction variant and its enum's fixed slot size.
    bytes = bytes
        .saturating_add(
            block
                .body
                .inner
                .transactions
                .capacity()
                .saturating_mul(size_of::<reth_ethereum_primitives::TransactionSigned>()),
        )
        .saturating_add(
            block.body.inner.ommers.capacity().saturating_mul(size_of::<alloy_consensus::Header>()),
        );
    if let Some(withdrawals) = &block.body.inner.withdrawals {
        bytes = bytes.saturating_add(
            withdrawals.capacity().saturating_mul(size_of::<alloy_eips::eip4895::Withdrawal>()),
        );
    }
    if let Some(sidecars) = &block.body.sidecars {
        for sidecar in sidecars {
            // BscBlockBody::size counts the outer sidecar Vec but not these dynamic buffers.
            bytes = bytes
                .saturating_add(sidecar.inner.blobs.capacity().saturating_mul(size_of::<Blob>()))
                .saturating_add(sidecar.inner.commitments.capacity().saturating_mul(48))
                .saturating_add(sidecar.inner.proofs.capacity().saturating_mul(48));
        }
    }
    bytes as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::primitives::BscBlobTransactionSidecar;
    use alloy_consensus::{BlobTransactionSidecar, Header};

    fn block(number: u64) -> Arc<BscBlock> {
        Arc::new(BscBlock { header: Header { number, ..Default::default() }, ..Default::default() })
    }

    fn stats(cache: &AnnouncedBlocks) -> (usize, usize, usize, u64) {
        let inner = cache.inner.lock();
        (inner.blocks.len(), inner.sidechains, inner.unclassified, inner.bytes)
    }

    #[test]
    fn reads_do_not_renew_and_republication_renews_from_admission() {
        let cache = AnnouncedBlocks::new(RETENTION, 4, MAX_BYTES);
        let now = Instant::now();
        let first = block(1);
        let hash = first.header.hash_slow();
        assert!(cache.retain_at(
            first.clone(),
            U256::from(7),
            Classification::Canonical,
            0,
            Some(now)
        ));
        assert_eq!(
            cache.get_at(hash, now + RETENTION - Duration::from_secs(1)).unwrap().1,
            U256::from(7)
        );
        cache.maintain_at(now + RETENTION);
        assert_eq!(stats(&cache), (0, 0, 0, 0));
        assert!(cache.get_at(hash, now + RETENTION).is_none());

        assert!(cache.retain_at(
            first.clone(),
            U256::from(7),
            Classification::Canonical,
            0,
            Some(now)
        ));
        assert!(cache.retain_at(
            block(2),
            U256::from(7),
            Classification::Canonical,
            0,
            Some(now + Duration::from_secs(5))
        ));
        assert!(cache.republish_at(hash, Some(now + Duration::from_secs(10))).is_some());
        cache.maintain_at(now + RETENTION);
        assert_eq!(stats(&cache).0, 2);
        cache.maintain_at(now + RETENTION + Duration::from_secs(5));
        assert_eq!(
            stats(&cache).0,
            1,
            "republish moves the extended deadline after its older neighbor"
        );
        assert!(cache.get_at(hash, now + RETENTION + Duration::from_secs(5)).is_some());
        cache.maintain_at(now + RETENTION + Duration::from_secs(10));
        assert_eq!(stats(&cache), (0, 0, 0, 0));
    }

    #[test]
    fn entry_and_byte_caps_evict_old_publications_and_reject_oversize() {
        let now = Instant::now();
        let first = block(1);
        let second = block(2);
        let third = block(3);
        let bytes = accounted_size(&first);
        for cache in [
            AnnouncedBlocks::new(RETENTION, 2, MAX_BYTES),
            AnnouncedBlocks::new(RETENTION, 10, bytes * 2),
        ] {
            for entry in [&first, &second] {
                assert!(cache.retain_at(
                    entry.clone(),
                    U256::from(7),
                    Classification::Sidechain,
                    0,
                    Some(now)
                ));
            }
            // Reading the oldest entry must not move it behind a newer publication.
            let held = cache.get_at(first.header.hash_slow(), now).unwrap().0;
            assert!(cache.retain_at(
                third.clone(),
                U256::from(9),
                Classification::Canonical,
                0,
                Some(now)
            ));
            assert!(cache.get_at(first.header.hash_slow(), now).is_none());
            assert_eq!(*held, *first);
            assert_eq!(stats(&cache), (2, 1, 0, bytes * 2));
        }
        let cache = AnnouncedBlocks::new(RETENTION, 10, bytes - 1);
        assert!(!cache.retain_at(first, U256::from(7), Classification::Canonical, 0, Some(now)));
        assert_eq!(stats(&cache), (0, 0, 0, 0));
    }

    #[test]
    fn sidechain_metrics_follow_reorg_resurrection_expiry_and_capacity() {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            let cache = AnnouncedBlocks::new(RETENTION, 2, MAX_BYTES);
            let initial = handle.render();
            for gauge in
                ["blocks", "sidechain_blocks", "classification_pending_blocks", "accounted_bytes"]
            {
                assert!(initial
                    .lines()
                    .any(|line| line == format!("bsc_network_retention_{gauge} 0")));
            }
            let now = Instant::now();
            let first = block(1);
            let second = block(2);
            let hash = first.header.hash_slow();
            let next = second.header.hash_slow();
            assert!(cache.retain_at(first, U256::from(7), Classification::Canonical, 0, Some(now)));
            assert!(cache.retain_at(
                second,
                U256::from(8),
                Classification::Sidechain,
                0,
                Some(now)
            ));
            cache.canonical_change([hash], [next]);
            assert_eq!((stats(&cache).0, stats(&cache).1), (2, 1));
            cache.canonical_change([next], [hash]);
            assert_eq!((stats(&cache).0, stats(&cache).1), (2, 1));
            cache.canonical_change([hash], []);
            assert_eq!(stats(&cache).1, 2);
            assert!(cache.retain_at(
                block(3),
                U256::from(9),
                Classification::Canonical,
                cache.view_version(),
                Some(now)
            ));
            assert_eq!(stats(&cache).1, 1);
            cache.maintain_at(now + RETENTION);
            assert_eq!(stats(&cache), (0, 0, 0, 0));
        });
        let exported = handle.render();
        assert!(exported.lines().any(|line| line == "bsc_network_retention_blocks 0"));
        assert!(exported.lines().any(|line| line == "bsc_network_retention_sidechain_blocks 0"));
        assert!(exported.lines().any(|line| line == "bsc_network_retention_accounted_bytes 0"));
        assert!(exported
            .lines()
            .any(|line| line == "bsc_network_retention_evictions_total{reason=\"capacity\"} 1"));
        assert!(exported
            .lines()
            .any(|line| line == "bsc_network_retention_evictions_total{reason=\"ttl\"} 2"));
    }

    #[test]
    fn delayed_classification_cannot_overwrite_reorg_or_reinserted_entries() {
        let cache = AnnouncedBlocks::new(RETENTION, 1, MAX_BYTES);
        let now = Instant::now();
        let first = block(1);
        let hash = first.header.hash_slow();
        assert!(cache.retain_at(
            first.clone(),
            U256::from(7),
            Classification::Canonical,
            0,
            Some(now)
        ));
        cache.notifications_lagged(1);
        assert_eq!((stats(&cache).1, stats(&cache).2), (0, 1));
        let probe = cache.classification_batch(1)[0];
        cache.canonical_change([hash], []);
        cache.apply_classification(probe, Some(hash));
        assert_eq!((stats(&cache).1, stats(&cache).2), (1, 0));

        cache.notifications_lagged(1);
        let probe = cache.classification_batch(1)[0];
        let version = cache.view_version();
        assert!(cache.retain_at(
            block(2),
            U256::from(8),
            Classification::Canonical,
            version,
            Some(now)
        ));
        assert!(cache.retain_at(
            first,
            U256::from(7),
            Classification::Sidechain,
            version,
            Some(now)
        ));
        cache.apply_classification(probe, Some(hash));
        assert_eq!((stats(&cache).1, stats(&cache).2), (1, 0));

        cache.notifications_lagged(1);
        let probe = cache.classification_batch(1)[0];
        cache.apply_classification(probe, None);
        assert_eq!(stats(&cache).2, 1);
        cache.apply_classification(probe, Some(hash));
        assert_eq!((stats(&cache).1, stats(&cache).2), (0, 0));
    }

    #[test]
    fn stale_capture_classification_and_batches_remain_bounded() {
        let cache = AnnouncedBlocks::new(RETENTION, 4, MAX_BYTES);
        let before = cache.view_version();
        cache.canonical_change([], []);
        for number in 1..=4 {
            assert!(cache.retain(
                block(number),
                U256::from(number),
                Classification::Canonical,
                before
            ));
        }
        assert_eq!(cache.classification_batch(2).len(), 2);
        assert_eq!(stats(&cache).2, 4);
        let debug = format!("{cache:?}");
        assert_eq!(debug, format!("AnnouncedBlocks {{ ttl: 1800s, max_entries: 4, max_bytes: {MAX_BYTES}, entries: 4, accounted_bytes: {}, .. }}", stats(&cache).3));
    }

    #[test]
    fn accounted_bytes_include_reserved_sidecar_buffers() {
        let mut value = BscBlock::default();
        value.body.sidecars = Some(vec![BscBlobTransactionSidecar {
            inner: BlobTransactionSidecar {
                blobs: Vec::with_capacity(2),
                commitments: Vec::with_capacity(3),
                proofs: Vec::with_capacity(4),
            },
            block_number: 0,
            block_hash: B256::ZERO,
            tx_index: 0,
            tx_hash: B256::ZERO,
            version: 0,
        }]);
        let sidecar = &value.body.sidecars.as_ref().unwrap()[0];
        let dynamic = sidecar.inner.blobs.capacity() * size_of::<Blob>() +
            sidecar.inner.commitments.capacity() * 48 +
            sidecar.inner.proofs.capacity() * 48;
        assert_eq!(
            accounted_size(&value),
            (value.size() + size_of::<RetainedBlock>() + ENTRY_OVERHEAD + dynamic) as u64
        );
        let previous = accounted_size(&value);
        value.body.inner.transactions.reserve(17);
        value.body.inner.ommers.reserve(3);
        assert!(
            accounted_size(&value) >=
                previous +
                    (17 * size_of::<reth_ethereum_primitives::TransactionSigned>() +
                        3 * size_of::<alloy_consensus::Header>()) as u64
        );
    }

    #[test]
    fn classification_round_robin_passes_persistent_unknown_entries() {
        let cache = AnnouncedBlocks::new(RETENTION, 80, MAX_BYTES);
        for number in 0..70 {
            assert!(cache.retain(block(number), U256::from(number), Classification::Unknown, 0));
        }
        let first = cache.classification_batch(32);
        for probe in &first {
            cache.apply_classification(*probe, None);
        }
        let second = cache.classification_batch(32);
        assert!(second.iter().all(|probe| first.iter().all(|old| old.hash != probe.hash)));
        for probe in second {
            cache.apply_classification(probe, Some(probe.hash));
        }
        assert_eq!(stats(&cache).2, 38);
    }
}
