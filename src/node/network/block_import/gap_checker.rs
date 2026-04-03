//! Periodic block-gap detection and recovery.
//!
//! # Background – go-bsc's `chainSyncer` / `forceSyncCycle`
//!
//! go-bsc (`eth/sync.go`) runs a dedicated `chainSyncer` goroutine alongside
//! the event-driven block fetcher.  Its core loop looks like:
//!
//! ```text
//! // eth/sync.go
//! forceSyncCycle = 10 * time.Second
//!
//! func (cs *chainSyncer) loop() {
//!     cs.force = time.NewTimer(forceSyncCycle)
//!     for {
//!         if op := cs.nextSyncOp(); op != nil {
//!             cs.startSync(op)          // → doSync → LegacySync
//!         }
//!         select {
//!         case <-cs.peerEventCh:        // peer connected / new head announced
//!         case <-cs.doneCh:             // previous sync finished
//!             cs.force.Reset(forceSyncCycle)
//!         case <-cs.force.C:            // 10-second safety-net tick
//!             cs.forced = true
//!         }
//!     }
//! }
//!
//! func (cs *chainSyncer) nextSyncOp() *chainSyncOp {
//!     peer := cs.handler.peers.peerWithHighestTD()   // best peer by TD
//!     if peer.td <= ourTD { return nil }             // already in sync
//!     return peerToSyncOp(mode, peer)
//! }
//! ```
//!
//! `startSync` ultimately calls `downloader.LegacySync` (GetBlockHeaders +
//! GetBlockBodies).  This 10-second timer is the **safety net**: if the fast,
//! event-driven path (`NewBlockHashes` → `asyncFetchRangeBlocks` →
//! `RequestBlocksByRange`) fails silently, the timer fires and forces the node
//! to catch up anyway.
//!
//! # reth-bsc equivalent – `GapChecker`
//!
//! reth-bsc uses `GetBlocksByRange` (BSC sub-protocol) instead of LegacySync.
//! It does not maintain a per-peer TD table, so rather than calling
//! `peerWithHighestTD()` we simply ask **every** connected BSC peer for the
//! blocks immediately above our local head.  The cost of an empty reply is
//! negligible, making a TD pre-check unnecessary.
//!
//! Trigger conditions (both must hold, matching go-bsc semantics):
//!
//! 1. No block has been successfully imported for ≥ [`STALE_SECS`] seconds –
//!    the node is "stuck" (mirrors the timer firing in `chainSyncer.loop`).
//! 2. There are no in-flight import futures – avoids redundant requests while
//!    the node is actively processing blocks.
//!
//! On trigger, [`GapChecker::fill_gap`] corresponds to go-bsc's `doSync`:
//! it requests blocks starting at `local_head + 1` from all peers.
//! `start_hash = B256::ZERO` instructs the peer to resolve by height
//! (the height-based fallback in `build_blocks_by_range_response`), because
//! in the stuck scenario we do not have the peer's latest block hash.

use crate::node::network::bsc_protocol::registry;
use alloy_primitives::B256;
use reth_provider::BlockNumReader;
use std::{
    task::{Context, Poll},
    time::{Duration, Instant},
};

/// Seconds of no imported blocks before triggering a gap-fill round.
/// Matches go-bsc's `forceSyncCycle = 10 * time.Second`.
const STALE_SECS: u64 = 10;

/// Blocks requested per peer per gap-fill round.
///
/// `GetBlocksByRange` walks **backward** from `start_height` via `parent_hash`,
/// so `count > 1` would return blocks we already have.  We only need the single
/// block immediately above our head; subsequent blocks arrive via the
/// event-driven path once this one is imported.
const REQUEST_COUNT: u64 = 1;

/// Per-peer request timeout.  Must be shorter than `STALE_SECS` so a slow
/// peer does not delay the next round.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(3);

/// Periodically detects and fills block gaps between this node and its BSC
/// peers.
///
/// See the module-level documentation for the full design rationale and the
/// mapping to go-bsc's `chainSyncer`.
pub struct GapChecker<Provider> {
    provider: Provider,
    /// Fires every [`STALE_SECS`].  Missed ticks are delayed (no burst).
    interval: tokio::time::Interval,
    /// When was the last block successfully imported?
    last_import_at: Instant,
}

impl<Provider> GapChecker<Provider>
where
    Provider: BlockNumReader,
{
    /// Create a new `GapChecker`.  `provider` is used only for synchronous
    /// [`BlockNumReader::best_block_number`] calls.
    pub fn new(provider: Provider) -> Self {
        let mut interval = tokio::time::interval(Duration::from_secs(STALE_SECS));
        // Delay: a missed tick is skipped and the next one fires a full
        // interval later.  This avoids a burst of gap-fill requests after a
        // period of inactivity (e.g. node paused in a debugger).
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        Self { provider, interval, last_import_at: Instant::now() }
    }

    /// Must be called every time the import service successfully imports a
    /// block.  Resets the staleness clock so the gap filler does not fire
    /// while the node is healthy.
    pub fn on_block_imported(&mut self) {
        self.last_import_at = Instant::now();
    }

    /// Drive the gap checker from the owning [`Future::poll`].
    ///
    /// `pending_imports_empty` should be `true` when the caller's
    /// `FuturesUnordered` import queue is empty.  This prevents sending
    /// redundant gap-fill requests while blocks are already being processed.
    pub fn poll(&mut self, cx: &mut Context<'_>, pending_imports_empty: bool) {
        if let Poll::Ready(_) = self.interval.poll_tick(cx) {
            let stale = self.last_import_at.elapsed() >= Duration::from_secs(STALE_SECS);
            if stale && pending_imports_empty {
                self.fill_gap();
            }
        }
    }

    /// Request the next few blocks above our head from every connected BSC
    /// peer.
    ///
    /// Corresponds to go-bsc's `doSync` → `LegacySync`, but uses
    /// `GetBlocksByRange` (BSC sub-protocol) instead of the standard
    /// GetBlockHeaders + GetBlockBodies sequence.
    fn fill_gap(&self) {
        let peers = registry::list_registered_peers();
        if peers.is_empty() {
            return;
        }
        let local_height = match self.provider.best_block_number() {
            Ok(h) => h,
            Err(_) => return,
        };
        let start_height = local_height.saturating_add(1);
        tracing::debug!(
            target: "bsc::gap_checker",
            local_height,
            peer_count = peers.len(),
            "Gap fill: requesting blocks above local head from BSC peers"
        );
        for peer in peers {
            tokio::spawn(async move {
                let _ = registry::batch_request_range_and_await_import(
                    peer,
                    start_height,
                    B256::ZERO,
                    REQUEST_COUNT,
                    REQUEST_TIMEOUT,
                )
                .await;
            });
        }
    }
}
