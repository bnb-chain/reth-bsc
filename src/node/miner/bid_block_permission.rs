//! BidBlock builder permission management (BEP-675).
//!
//! Tracks per-builder `SendBidBlock` revokes in memory. A builder is revoked for a lockout window
//! (e.g. after submitting an invalid BidBlock, or by an operator), and the revoke expires lazily
//! once the window passes. Ported from bnb-chain/bsc `miner/bid_block_permission.go` +
//! `core/types/bid_block_permission.go`.

use alloy_primitives::{Address, B256};
use parking_lot::RwLock;
use std::{
    collections::HashMap,
    time::{SystemTime, UNIX_EPOCH},
};

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

/// One active revoke event.
#[derive(Debug, Clone)]
struct BidBlockRevokeRecord {
    revoked_at: u64,
    duration_secs: u64,
    reason: String,
    block_hash: B256,
    block_num: u64,
}

impl BidBlockRevokeRecord {
    /// Whether `now` is still within the lockout window.
    fn is_active(&self, now: u64) -> bool {
        now < self.revoked_at.saturating_add(self.duration_secs)
    }
}

/// Tracks per-builder `SendBidBlock` revokes. Revokes are kept in memory and expire lazily after
/// their lockout window.
pub struct BidBlockPermissionManager {
    revoked: RwLock<HashMap<Address, BidBlockRevokeRecord>>,
    /// Clock returning the current time in unix seconds; injectable for tests.
    clock: Box<dyn Fn() -> u64 + Send + Sync>,
}

impl Default for BidBlockPermissionManager {
    fn default() -> Self {
        Self::new()
    }
}

impl BidBlockPermissionManager {
    /// A fresh manager with no builders revoked, using the system clock.
    pub fn new() -> Self {
        Self::with_clock(Box::new(|| {
            SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
        }))
    }

    /// A manager with a custom clock (used in tests to control expiry).
    pub fn with_clock(clock: Box<dyn Fn() -> u64 + Send + Sync>) -> Self {
        Self { revoked: RwLock::new(HashMap::new()), clock }
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
        self.revoked.write().insert(
            builder,
            BidBlockRevokeRecord {
                revoked_at: (self.clock)(),
                duration_secs,
                reason: reason.into(),
                block_hash,
                block_num,
            },
        );
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

    const BUILDER: Address = address!("0xb32d0723583040f3a16d1380d1e6aa874cd1bdf7");

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
}
