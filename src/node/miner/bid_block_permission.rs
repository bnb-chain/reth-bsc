//! BidBlock builder permission management (BEP-675).
//!
//! Tracks per-builder `SendBidBlock` revokes. A builder is revoked for a lockout window
//! (e.g. after submitting an invalid BidBlock, or by an operator), and the revoke expires lazily
//! once the window passes. Ported from bnb-chain/bsc `miner/bid_block_permission.go` +
//! `core/types/bid_block_permission.go`.
//!
//! # Persistence
//!
//! Revokes survive restarts through a JSON journal — a single standalone file under the node's
//! datadir (`bidblockrevokes.json`), NOT the chain database: the lockouts are validator-local
//! MEV policy, not chain state, and a plain file is inspectable and trivially resettable by
//! deleting it. JSON over a binary codec for the same operability reason; the write pattern is a
//! whole-map snapshot atomically replacing the file (temp file + rename), so no incremental
//! format is needed and a crash mid-write leaves the previous journal intact.
//!
//! Persistence is asynchronous and best-effort: mutations snapshot the map and hand it to a
//! background writer, so the mining path never waits on disk. Load happens once at startup and
//! drops revokes whose window elapsed while the node was down. An empty journal path keeps the
//! manager purely in-memory (the pre-persistence behavior).
//!
//! The journal is node-local and never exchanged between clients, so the value encoding is
//! Rust-native (unix seconds for `revokedAt`, seconds for `duration`) while the JSON key names
//! are pinned to go-bsc's tags — a key or meaning change needs a version bump, and an unknown
//! version is refused on load (starting empty only costs the remaining lockout windows, which
//! self-expire; misreading a future format could revoke or release the wrong builders).

use alloy_primitives::{Address, B256};
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{SystemTime, UNIX_EPOCH},
};
use tracing::{info, warn};

/// `Reason` value used when an operator manually revokes a builder via [`set_allowed`].
/// Automatic revokes carry the underlying error or policy message as the reason directly.
///
/// [`set_allowed`]: BidBlockPermissionManager::set_allowed
pub const REVOKE_REASON_MANUAL: &str = "manual";

/// Default lockout window for invalid BidBlocks (24h), in seconds.
pub const BID_BLOCK_REVOKE_DURATION_SECS: u64 = 24 * 60 * 60;
/// Lockout window for gas-price policy revokes (one epoch, 450s), in seconds.
pub const BID_BLOCK_GAS_PRICE_LOW_REVOKE_DURATION_SECS: u64 = 450;

/// Snapshot of a builder's current BidBlock permission, exposed by the permission RPC.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BidBlockPermissionStatus {
    /// Whether the builder may currently use `SendBidBlock`.
    pub allowed: bool,
    /// Error detail for auto revokes, or [`REVOKE_REASON_MANUAL`] for admin revokes.
    pub reason: String,
    /// Block hash that triggered an auto revoke (zero for manual / none).
    pub block_hash: B256,
    /// Block number that triggered an auto revoke.
    pub block_num: u64,
    /// Unix seconds when the revoke was recorded (0 when allowed).
    pub revoked_at: u64,
    /// Unix seconds when the revoke expires (0 when allowed).
    pub reset_at: u64,
}

/// Version of the on-disk revoke journal. Bump on any change to the JSON keys or their meaning;
/// an older build refuses a newer journal (and starts empty) rather than misreading it.
const BID_BLOCK_REVOKE_JOURNAL_VERSION: u32 = 1;

/// One active revoke event.
///
/// The serde renames pin the journal's JSON keys (go-bsc's tags). They MUST stay stable across
/// releases so restarts across upgrades keep working — change them only together with a
/// [`BID_BLOCK_REVOKE_JOURNAL_VERSION`] bump.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct BidBlockRevokeRecord {
    #[serde(rename = "revokedAt")]
    revoked_at: u64,
    #[serde(rename = "duration")]
    duration_secs: u64,
    reason: String,
    #[serde(rename = "blockHash")]
    block_hash: B256,
    #[serde(rename = "blockNum")]
    block_num: u64,
}

/// On-disk envelope: a version stamp plus the whole revoke map. The journal is always written as
/// one complete snapshot, never appended to.
#[derive(Debug, Serialize, Deserialize)]
struct BidBlockRevokeJournal {
    version: u32,
    revokes: HashMap<Address, BidBlockRevokeRecord>,
}

impl BidBlockRevokeRecord {
    /// Whether `now` is still within the lockout window.
    fn is_active(&self, now: u64) -> bool {
        now < self.revoked_at.saturating_add(self.duration_secs)
    }
}

/// Shared state of the background journal writer. Lives in an [`Arc`] so each fire-and-forget
/// writer thread can own a handle without borrowing the manager.
struct JournalState {
    /// Journal file location (`<datadir>/bidblockrevokes.json`).
    path: PathBuf,
    /// Sequence stamped on each snapshot; bumped under the manager's write lock, so snapshot
    /// content and sequence order always agree.
    persist_seq: AtomicU64,
    /// Highest sequence that entered the writer. The mutex serializes writers; the value drops
    /// stale snapshots (see [`JournalState::persist`]).
    persisted_seq: Mutex<u64>,
}

impl JournalState {
    /// Writes `snapshot` to the journal file unless a newer snapshot has already been handled.
    /// Errors are logged and swallowed: persistence is best-effort and must not affect the caller.
    ///
    /// The write is atomic (temp file + rename), so a crash mid-write leaves the previous journal
    /// intact rather than a truncated one.
    ///
    /// `persisted_seq` is advanced BEFORE the write and regardless of its outcome. This makes the
    /// sequence that actually reaches the disk strictly monotonic: once a newer snapshot has
    /// entered here, every older one is dropped even if the newer write failed. Otherwise a newer
    /// write failing could let an older snapshot win the disk afterwards and resurrect stale
    /// state (e.g. a revoke overwriting a later manual clear). Losing the newest write on failure
    /// is acceptable (best-effort); an older snapshot overwriting a newer one is not.
    fn persist(&self, seq: u64, snapshot: HashMap<Address, BidBlockRevokeRecord>) {
        let mut persisted = self.persisted_seq.lock();
        if seq <= *persisted {
            return; // an equal-or-newer snapshot has already been handled; this one is stale
        }
        *persisted = seq;
        let blob = match serde_json::to_vec(&BidBlockRevokeJournal {
            version: BID_BLOCK_REVOKE_JOURNAL_VERSION,
            revokes: snapshot,
        }) {
            Ok(blob) => blob,
            Err(err) => {
                warn!(target: "miner::bid_block", %err, "Failed to encode BidBlock revokes for persistence");
                return;
            }
        };
        let mut tmp = self.path.clone().into_os_string();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        if let Err(err) = write_owner_only(&tmp, &blob) {
            warn!(target: "miner::bid_block", path = %tmp.display(), %err, "Failed to write BidBlock revoke journal");
            return;
        }
        if let Err(err) = std::fs::rename(&tmp, &self.path) {
            warn!(target: "miner::bid_block", path = %self.path.display(), %err, "Failed to replace BidBlock revoke journal");
        }
    }
}

/// Writes `blob` to `path`, creating/truncating it with owner-only permissions (0600) on unix —
/// the journal names misbehaving builders, which is operator-private policy data.
fn write_owner_only(path: &PathBuf, blob: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(blob)
}

/// Tracks per-builder `SendBidBlock` revokes. Revokes expire lazily after their lockout window
/// and are mirrored to the journal file (when one is configured) so they survive restarts.
pub struct BidBlockPermissionManager {
    revoked: RwLock<HashMap<Address, BidBlockRevokeRecord>>,
    /// Clock returning the current time in unix seconds; injectable for tests.
    clock: Box<dyn Fn() -> u64 + Send + Sync>,
    /// Journal writer state; `None` keeps the manager purely in-memory (go-bsc's empty path).
    journal: Option<Arc<JournalState>>,
}

impl Default for BidBlockPermissionManager {
    fn default() -> Self {
        Self::new()
    }
}

impl BidBlockPermissionManager {
    /// A fresh manager with no builders revoked, using the system clock. Purely in-memory: no
    /// journal is read or written.
    pub fn new() -> Self {
        Self::with_clock(Self::system_clock())
    }

    /// A manager persisting revokes to the journal file at `path`, restoring any still-active
    /// revokes it holds. The load is synchronous, once, off the hot path; any load error leaves
    /// the manager empty — same as a fresh node.
    pub fn with_journal(path: PathBuf) -> Self {
        let mut mgr = Self::with_clock(Self::system_clock());
        mgr.journal = Some(Arc::new(JournalState {
            path,
            persist_seq: AtomicU64::new(0),
            persisted_seq: Mutex::new(0),
        }));
        mgr.load();
        mgr
    }

    /// A manager with a custom clock (used in tests to control expiry).
    pub fn with_clock(clock: Box<dyn Fn() -> u64 + Send + Sync>) -> Self {
        Self { revoked: RwLock::new(HashMap::new()), clock, journal: None }
    }

    fn system_clock() -> Box<dyn Fn() -> u64 + Send + Sync> {
        Box::new(|| {
            SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
        })
    }

    /// Restores persisted revokes at startup, dropping any whose window has already elapsed
    /// (including time spent offline).
    fn load(&self) {
        let Some(journal) = &self.journal else { return };
        let blob = match std::fs::read(&journal.path) {
            Ok(blob) if !blob.is_empty() => blob,
            _ => return, // no journal yet (or unreadable): start empty
        };
        let decoded: BidBlockRevokeJournal = match serde_json::from_slice(&blob) {
            Ok(decoded) => decoded,
            Err(err) => {
                warn!(
                    target: "miner::bid_block",
                    path = %journal.path.display(), %err,
                    "Failed to decode persisted BidBlock revokes, starting empty"
                );
                return;
            }
        };
        if decoded.version != BID_BLOCK_REVOKE_JOURNAL_VERSION {
            warn!(
                target: "miner::bid_block",
                path = %journal.path.display(),
                version = decoded.version,
                supported = BID_BLOCK_REVOKE_JOURNAL_VERSION,
                "Unsupported BidBlock revoke journal version, starting empty"
            );
            return;
        }
        let now = (self.clock)();
        let mut revoked = self.revoked.write();
        for (builder, rec) in decoded.revokes {
            if rec.is_active(now) {
                revoked.insert(builder, rec);
            }
        }
        if !revoked.is_empty() {
            info!(
                target: "miner::bid_block",
                path = %journal.path.display(),
                count = revoked.len(),
                "Restored BidBlock revokes from journal"
            );
        }
    }

    /// Snapshots the current map and kicks off an asynchronous write. MUST be called with the
    /// `revoked` write lock held (the caller passes the guarded map) so the snapshot and its
    /// sequence agree; the caller returns immediately without waiting for the disk write.
    ///
    /// Fire-and-forget: mutations are rare (only on bad bids or operator action), so spawning a
    /// thread per change is cheap. [`JournalState::persist`] serializes writers and drops stale
    /// snapshots via the sequence guard.
    fn mark_dirty_locked(&self, revoked: &HashMap<Address, BidBlockRevokeRecord>) {
        let Some(journal) = &self.journal else { return };
        let seq = journal.persist_seq.fetch_add(1, Ordering::SeqCst) + 1;
        let snapshot = revoked.clone();
        let journal = journal.clone();
        std::thread::spawn(move || journal.persist(seq, snapshot));
    }

    /// Whether `builder` may currently use `SendBidBlock`.
    pub fn is_allowed(&self, builder: Address) -> bool {
        self.active_record(builder, (self.clock)()).is_none()
    }

    /// Deny `builder` for the default lockout window, recording the reason for the permission RPC.
    pub fn revoke(&self, builder: Address, reason: impl Into<String>, block_hash: B256, block_num: u64) {
        self.revoke_for(builder, reason, block_hash, block_num, BID_BLOCK_REVOKE_DURATION_SECS);
    }

    /// Deny `builder` for `duration_secs` (defaulting when zero), recording the reason.
    pub fn revoke_for(
        &self,
        builder: Address,
        reason: impl Into<String>,
        block_hash: B256,
        block_num: u64,
        duration_secs: u64,
    ) {
        let duration_secs =
            if duration_secs == 0 { BID_BLOCK_REVOKE_DURATION_SECS } else { duration_secs };
        let mut revoked = self.revoked.write();
        revoked.insert(
            builder,
            BidBlockRevokeRecord {
                revoked_at: (self.clock)(),
                duration_secs,
                reason: reason.into(),
                block_hash,
                block_num,
            },
        );
        self.mark_dirty_locked(&revoked);
    }

    /// Current permission snapshot for `builder`.
    pub fn get_status(&self, builder: Address) -> BidBlockPermissionStatus {
        match self.active_record(builder, (self.clock)()) {
            None => BidBlockPermissionStatus { allowed: true, ..Default::default() },
            Some(rec) => BidBlockPermissionStatus {
                allowed: false,
                reason: rec.reason,
                block_hash: rec.block_hash,
                block_num: rec.block_num,
                revoked_at: rec.revoked_at,
                reset_at: rec.revoked_at.saturating_add(rec.duration_secs),
            },
        }
    }

    /// Number of builders currently revoked (expired revokes excluded).
    pub fn active_revoke_count(&self) -> usize {
        let now = (self.clock)();
        self.revoked.read().values().filter(|rec| rec.is_active(now)).count()
    }

    /// Operator override: `allowed = true` clears any revoke; `false` records a manual revoke.
    pub fn set_allowed(&self, builder: Address, allowed: bool) {
        let mut revoked = self.revoked.write();
        if allowed {
            revoked.remove(&builder);
            // A manual clear is mirrored to disk too, so it is not resurrected on restart.
            self.mark_dirty_locked(&revoked);
            return;
        }
        revoked.insert(
            builder,
            BidBlockRevokeRecord {
                revoked_at: (self.clock)(),
                duration_secs: BID_BLOCK_REVOKE_DURATION_SECS,
                reason: REVOKE_REASON_MANUAL.to_string(),
                block_hash: B256::ZERO,
                block_num: 0,
            },
        );
        self.mark_dirty_locked(&revoked);
    }

    /// Return the active revoke record for `builder`, if one exists and hasn't expired.
    fn active_record(&self, builder: Address, now: u64) -> Option<BidBlockRevokeRecord> {
        let revoked = self.revoked.read();
        revoked.get(&builder).filter(|rec| rec.is_active(now)).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;
    use std::sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    };

    /// Synthetic builder address. The value is arbitrary — these tests only round-trip it —
    /// so it is deliberately not a realistic-looking address.
    const BUILDER: Address = Address::repeat_byte(0xb1);
    const BUILDER_A: Address = address!("0x000000000000000000000000000000000000000a");
    const BUILDER_B: Address = address!("0x000000000000000000000000000000000000000b");
    const BUILDER_C: Address = address!("0x000000000000000000000000000000000000000c");
    const DAY: u64 = BID_BLOCK_REVOKE_DURATION_SECS;
    const INSERT_CHAIN_REASON: &str = "insert chain failed";

    /// Manager with a test-controllable clock; returns the manager and the shared "now" cell.
    fn manager_with_fake_clock() -> (BidBlockPermissionManager, Arc<AtomicU64>) {
        let now = Arc::new(AtomicU64::new(1_000));
        let now_for_clock = now.clone();
        let mgr = BidBlockPermissionManager::with_clock(Box::new(move || {
            now_for_clock.load(Ordering::Relaxed)
        }));
        (mgr, now)
    }

    #[test]
    fn fresh_manager_allows_all_builders() {
        let (mgr, _now) = manager_with_fake_clock();
        assert!(mgr.is_allowed(BUILDER));
        assert!(mgr.get_status(BUILDER).allowed);
        assert_eq!(mgr.active_revoke_count(), 0);
    }

    #[test]
    fn revoke_blocks_then_expires_lazily() {
        let (mgr, now) = manager_with_fake_clock();
        mgr.revoke(BUILDER, "bad block", B256::repeat_byte(0xab), 42);

        assert!(!mgr.is_allowed(BUILDER));
        let status = mgr.get_status(BUILDER);
        assert!(!status.allowed);
        assert_eq!(status.reason, "bad block");
        assert_eq!(status.block_num, 42);
        assert_eq!(status.revoked_at, 1_000);
        assert_eq!(status.reset_at, 1_000 + BID_BLOCK_REVOKE_DURATION_SECS);
        assert_eq!(mgr.active_revoke_count(), 1);

        // Just before expiry: still revoked.
        now.store(1_000 + BID_BLOCK_REVOKE_DURATION_SECS - 1, Ordering::Relaxed);
        assert!(!mgr.is_allowed(BUILDER));

        // At/after the reset time: allowed again (lazy expiry).
        now.store(1_000 + BID_BLOCK_REVOKE_DURATION_SECS, Ordering::Relaxed);
        assert!(mgr.is_allowed(BUILDER));
        assert!(mgr.get_status(BUILDER).allowed);
        assert_eq!(mgr.active_revoke_count(), 0);
    }

    #[test]
    fn revoke_for_uses_custom_duration_and_defaults_on_zero() {
        let (mgr, now) = manager_with_fake_clock();

        mgr.revoke_for(BUILDER, "gas price", B256::ZERO, 7, BID_BLOCK_GAS_PRICE_LOW_REVOKE_DURATION_SECS);
        assert_eq!(
            mgr.get_status(BUILDER).reset_at,
            1_000 + BID_BLOCK_GAS_PRICE_LOW_REVOKE_DURATION_SECS
        );

        // Zero duration falls back to the default window.
        now.store(2_000, Ordering::Relaxed);
        mgr.revoke_for(BUILDER, "x", B256::ZERO, 0, 0);
        assert_eq!(mgr.get_status(BUILDER).reset_at, 2_000 + BID_BLOCK_REVOKE_DURATION_SECS);
    }

    #[test]
    fn set_allowed_toggles_manual_revoke() {
        let (mgr, _now) = manager_with_fake_clock();

        mgr.set_allowed(BUILDER, false);
        let status = mgr.get_status(BUILDER);
        assert!(!status.allowed);
        assert_eq!(status.reason, REVOKE_REASON_MANUAL);

        mgr.set_allowed(BUILDER, true);
        assert!(mgr.is_allowed(BUILDER));
    }

    #[test]
    fn builders_are_independent() {
        let (mgr, _now) = manager_with_fake_clock();
        mgr.revoke(BUILDER_A, INSERT_CHAIN_REASON, B256::ZERO, 1);
        assert!(!mgr.is_allowed(BUILDER_A), "A should be revoked");
        assert!(mgr.is_allowed(BUILDER_B), "B should remain active");
    }

    #[test]
    fn revoke_overwrites_previous_record() {
        let (mgr, _now) = manager_with_fake_clock();
        mgr.revoke(BUILDER, INSERT_CHAIN_REASON, B256::repeat_byte(0x01), 1);
        mgr.revoke(BUILDER, REVOKE_REASON_MANUAL, B256::repeat_byte(0x02), 2);

        // Most recent revoke wins.
        let status = mgr.get_status(BUILDER);
        assert_eq!(status.reason, REVOKE_REASON_MANUAL);
        assert_eq!(status.block_num, 2);
        assert_eq!(status.block_hash, B256::repeat_byte(0x02));
    }

    #[test]
    fn revoke_expiry_tracks_elapsed_time_not_wall_day() {
        // Only elapsed time matters — a UTC day rollover must not reset the revoke.
        let (mgr, now) = manager_with_fake_clock();
        let t = 1_000_000;
        now.store(t, Ordering::Relaxed);
        mgr.revoke(BUILDER, INSERT_CHAIN_REASON, B256::ZERO, 1);

        // 2s later (even across a day boundary): still revoked.
        now.store(t + 2, Ordering::Relaxed);
        assert!(!mgr.is_allowed(BUILDER));
        // 1s before the 24h boundary: still revoked.
        now.store(t + DAY - 1, Ordering::Relaxed);
        assert!(!mgr.is_allowed(BUILDER));
        // exactly revoked_at + 24h: expired.
        now.store(t + DAY, Ordering::Relaxed);
        assert!(mgr.is_allowed(BUILDER));
    }

    #[test]
    fn builders_expire_independently() {
        let (mgr, now) = manager_with_fake_clock();
        let t0 = 100_000;
        let five_hours = 5 * 60 * 60;

        now.store(t0, Ordering::Relaxed);
        mgr.revoke(BUILDER_A, INSERT_CHAIN_REASON, B256::ZERO, 1);
        now.store(t0 + five_hours, Ordering::Relaxed);
        mgr.revoke(BUILDER_B, INSERT_CHAIN_REASON, B256::ZERO, 2);

        // Both revoked at t0 + 6h; reset times are independent of each other.
        now.store(t0 + 6 * 60 * 60, Ordering::Relaxed);
        assert!(!mgr.is_allowed(BUILDER_A));
        assert!(!mgr.is_allowed(BUILDER_B));
        assert_eq!(mgr.get_status(BUILDER_A).reset_at, t0 + DAY);
        assert_eq!(mgr.get_status(BUILDER_B).reset_at, t0 + five_hours + DAY);

        // At A's own revoked_at + 24h, A expires but B still has time left.
        now.store(t0 + DAY, Ordering::Relaxed);
        assert!(mgr.is_allowed(BUILDER_A));
        assert!(!mgr.is_allowed(BUILDER_B));

        // At B's own revoked_at + 24h, B also expires.
        now.store(t0 + five_hours + DAY, Ordering::Relaxed);
        assert!(mgr.is_allowed(BUILDER_B));
    }

    #[test]
    fn active_revoke_count_excludes_expired() {
        let (mgr, now) = manager_with_fake_clock();
        assert_eq!(mgr.active_revoke_count(), 0);

        let t = 500_000;
        now.store(t, Ordering::Relaxed);
        mgr.revoke(BUILDER_A, INSERT_CHAIN_REASON, B256::ZERO, 1);
        mgr.revoke(BUILDER_B, REVOKE_REASON_MANUAL, B256::ZERO, 2);
        assert_eq!(mgr.active_revoke_count(), 2);

        // After the window the entries are stale, not active.
        now.store(t + DAY, Ordering::Relaxed);
        assert_eq!(mgr.active_revoke_count(), 0);
    }

    #[test]
    fn get_status_reports_full_detail() {
        let (mgr, now) = manager_with_fake_clock();
        let t = 700_000;
        now.store(t, Ordering::Relaxed);

        // Allowed builders carry no reset time.
        let status = mgr.get_status(BUILDER);
        assert!(status.allowed);
        assert_eq!(status.reset_at, 0);

        let hash = B256::repeat_byte(0xab);
        mgr.revoke(BUILDER, INSERT_CHAIN_REASON, hash, 100);
        let status = mgr.get_status(BUILDER);
        assert_eq!(
            status,
            BidBlockPermissionStatus {
                allowed: false,
                reason: INSERT_CHAIN_REASON.to_string(),
                block_hash: hash,
                block_num: 100,
                revoked_at: t,
                reset_at: t + DAY,
            }
        );
    }

    // --- Journal persistence (go-bsc PR #3796 parity) ---------------------------------------

    /// Fresh journal path in a per-test temp directory (the go `testJournalPath` counterpart).
    fn test_journal_path(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("bid-block-revokes-{}-{name}", std::process::id()));
        // A stale directory from an aborted earlier run must not leak state into this one.
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create test journal dir");
        dir.join("bidblockrevokes.json")
    }

    /// Polls until the journal decodes and satisfies `pred` (the go `waitForBidBlockPersist`
    /// counterpart — persistence is async and best-effort, so tests wait for the write).
    fn wait_for_journal(
        path: &std::path::Path,
        pred: impl Fn(&BidBlockRevokeJournal) -> bool,
    ) -> BidBlockRevokeJournal {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            if let Ok(blob) = std::fs::read(path) {
                if let Ok(journal) = serde_json::from_slice::<BidBlockRevokeJournal>(&blob) {
                    if pred(&journal) {
                        return journal;
                    }
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        panic!("journal at {} did not reach the expected state in time", path.display());
    }

    fn unix_now() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_secs()
    }

    fn seed_journal(path: &std::path::Path, journal: &BidBlockRevokeJournal) {
        std::fs::write(path, serde_json::to_vec(journal).expect("marshal seed"))
            .expect("seed journal");
    }

    /// Mirrors go `TestBidBlockPermission_SurvivesRestart`: a revoke persisted by one manager is
    /// restored — with an identical status snapshot — by a brand-new manager over the same file.
    #[test]
    fn revoke_survives_restart() {
        let path = test_journal_path("survives-restart");
        let hash = B256::repeat_byte(0xab);

        let m1 = BidBlockPermissionManager::with_journal(path.clone());
        m1.revoke(BUILDER, INSERT_CHAIN_REASON, hash, 100);
        wait_for_journal(&path, |j| j.revokes.contains_key(&BUILDER));
        let want = m1.get_status(BUILDER);

        // Simulate a restart: a brand-new manager over the same journal.
        let m2 = BidBlockPermissionManager::with_journal(path);
        assert!(!m2.is_allowed(BUILDER), "revoke must survive a restart within its window");
        let got = m2.get_status(BUILDER);
        assert_eq!(got.reset_at, want.reset_at, "resetAt changed across restart");
        assert_eq!(got, want, "restored record differs from the persisted one");
    }

    /// Mirrors go `TestBidBlockPermission_ExpiredNotRestored`: a revoke whose window elapsed
    /// while the process was down is dropped on load, while a still-active one is restored. The
    /// journal is seeded directly so the elapsed time is deterministic.
    #[test]
    fn expired_revoke_not_restored() {
        let path = test_journal_path("expired-not-restored");
        let active = BUILDER_A;
        let expired = BUILDER_B;

        let now = unix_now();
        let record = |revoked_at: u64, reason: &str| BidBlockRevokeRecord {
            revoked_at,
            duration_secs: BID_BLOCK_REVOKE_DURATION_SECS,
            reason: reason.to_string(),
            block_hash: B256::ZERO,
            block_num: 0,
        };
        seed_journal(
            &path,
            &BidBlockRevokeJournal {
                version: BID_BLOCK_REVOKE_JOURNAL_VERSION,
                revokes: HashMap::from([
                    (active, record(now - 60 * 60, "active")),
                    (expired, record(now - 25 * 60 * 60, "expired")),
                ]),
            },
        );

        let mgr = BidBlockPermissionManager::with_journal(path);
        assert!(!mgr.is_allowed(active), "still-active revoke must be restored");
        assert!(mgr.is_allowed(expired), "revoke expired during downtime must not be restored");
    }

    /// Mirrors go `TestBidBlockPermission_LoadToleratesBadState`: a missing or corrupt journal
    /// leaves the manager empty instead of panicking.
    #[test]
    fn load_tolerates_bad_state() {
        // Corrupt journal: start empty, no panic.
        let path = test_journal_path("load-tolerates-bad-state");
        std::fs::write(&path, b"not json").expect("seed journal");
        let mgr = BidBlockPermissionManager::with_journal(path);
        assert!(mgr.is_allowed(BUILDER), "a corrupt journal must yield an empty manager");

        // Missing file (fresh datadir): start empty, no panic.
        let mgr = BidBlockPermissionManager::with_journal(test_journal_path("missing-journal"));
        assert!(mgr.is_allowed(BUILDER), "a missing journal must yield an empty manager");
    }

    /// Mirrors go `TestBidBlockPermission_RejectsUnknownVersion`: a journal written by a future
    /// build must be refused outright rather than decoded with this build's interpretation of the
    /// fields. Starting empty only costs the remaining lockout windows, which self-expire;
    /// misreading a changed field could revoke or release the wrong builders.
    #[test]
    fn rejects_unknown_journal_version() {
        let path = test_journal_path("rejects-unknown-version");
        seed_journal(
            &path,
            &BidBlockRevokeJournal {
                version: BID_BLOCK_REVOKE_JOURNAL_VERSION + 1,
                revokes: HashMap::from([(
                    BUILDER,
                    BidBlockRevokeRecord {
                        revoked_at: unix_now(),
                        duration_secs: BID_BLOCK_REVOKE_DURATION_SECS,
                        reason: INSERT_CHAIN_REASON.to_string(),
                        block_hash: B256::ZERO,
                        block_num: 1,
                    },
                )]),
            },
        );

        let mgr = BidBlockPermissionManager::with_journal(path);
        assert!(mgr.is_allowed(BUILDER), "an unknown journal version must be refused wholesale");
    }

    /// Mirrors go `TestBidBlockPermission_JournalKeysAreStable`: the on-disk JSON keys are a
    /// compatibility surface — a key change needs a version bump.
    #[test]
    fn journal_keys_are_stable() {
        let path = test_journal_path("journal-keys-stable");
        let mgr = BidBlockPermissionManager::with_journal(path.clone());
        mgr.revoke(BUILDER, INSERT_CHAIN_REASON, B256::repeat_byte(0xab), 100);
        wait_for_journal(&path, |j| j.revokes.contains_key(&BUILDER));

        let blob = String::from_utf8(std::fs::read(&path).expect("read journal")).unwrap();
        for key in
            ["\"version\"", "\"revokes\"", "\"revokedAt\"", "\"duration\"", "\"reason\"", "\"blockHash\"", "\"blockNum\""]
        {
            assert!(
                blob.contains(key),
                "journal is missing the stable key {key}; a tag change needs a version bump\ngot: {blob}"
            );
        }
    }

    /// Mirrors go `TestBidBlockPermission_FailedNewerBlocksOlder`: once a newer snapshot has
    /// entered the writer — even if its write fails — an older snapshot must never overwrite it
    /// and resurrect stale state. `persist` is called directly to control ordering
    /// deterministically; the write failure is injected by occupying the journal path with a
    /// directory, which makes the atomic rename fail.
    #[test]
    fn failed_newer_write_blocks_older_snapshot() {
        let path = test_journal_path("failed-newer-blocks-older");
        let mgr = BidBlockPermissionManager::with_journal(path.clone());
        let journal = mgr.journal.as_ref().expect("journal-backed manager").clone();

        let revoked = HashMap::from([(
            BUILDER,
            BidBlockRevokeRecord {
                revoked_at: unix_now(),
                duration_secs: BID_BLOCK_REVOKE_DURATION_SECS,
                reason: "revoke".to_string(),
                block_hash: B256::ZERO,
                block_num: 0,
            },
        )]);
        let cleared = HashMap::new(); // the set_allowed(true) result

        // The newer snapshot (seq 2, cleared) is attempted first but its write fails: the
        // journal path is occupied by a directory.
        std::fs::create_dir(&path).expect("occupy journal path");
        journal.persist(2, cleared);
        assert_eq!(
            *journal.persisted_seq.lock(),
            2,
            "persisted_seq must advance even when the write fails"
        );

        // The older snapshot (seq 1, revoked) is now handled with writes working again — it
        // must be dropped, not overwrite the newer intent.
        std::fs::remove_dir(&path).expect("free journal path");
        journal.persist(1, revoked);
        assert!(
            !path.exists(),
            "older snapshot must not overwrite a newer (failed) one; journal should be absent"
        );
    }

    /// Mirrors go `TestBidBlockPermission_SetAllowedPersists`: a manual clear is also mirrored
    /// to disk, so it is not resurrected on restart.
    #[test]
    fn set_allowed_clear_persists() {
        let path = test_journal_path("set-allowed-persists");

        let m1 = BidBlockPermissionManager::with_journal(path.clone());
        m1.revoke(BUILDER, INSERT_CHAIN_REASON, B256::repeat_byte(0xab), 100);
        wait_for_journal(&path, |j| j.revokes.contains_key(&BUILDER));
        m1.set_allowed(BUILDER, true); // manual clear must persist too
        wait_for_journal(&path, |j| !j.revokes.contains_key(&BUILDER));

        let m2 = BidBlockPermissionManager::with_journal(path);
        assert!(
            m2.is_allowed(BUILDER),
            "a manually cleared builder must not be resurrected on restart"
        );
    }

    #[test]
    fn concurrent_access_is_safe() {
        use std::thread;

        let mgr = Arc::new(BidBlockPermissionManager::new());
        let builders = [BUILDER_A, BUILDER_B, BUILDER_C];

        let mut handles = Vec::new();
        for i in 0..50usize {
            let builder = builders[i % builders.len()];
            for _ in 0..3 {
                let mgr = mgr.clone();
                handles.push(thread::spawn(move || {
                    mgr.is_allowed(builder);
                    mgr.revoke(builder, INSERT_CHAIN_REASON, B256::ZERO, 1);
                    let _ = mgr.get_status(builder);
                    mgr.active_revoke_count();
                }));
            }
        }
        for handle in handles {
            handle.join().unwrap();
        }
    }
}
