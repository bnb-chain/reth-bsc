use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, RwLock},
};

use once_cell::sync::Lazy;
use parking_lot::Mutex as ParkingMutex;
use tokio::{
    sync::{broadcast, mpsc::UnboundedSender, oneshot},
    task::JoinHandle,
};

use reth_network_api::PeerId;

use super::stream::BscCommand;
use crate::node::network::blocks_by_range::{
    BlocksByRangePacket, GetBlocksByRangePacket, MAX_REQUEST_RANGE_BLOCKS_COUNT,
};
use alloy_primitives::B256;
use reth_network::Peers;
use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};
use tokio::time::timeout;

/// Per-peer entry in [`REGISTRY`]. Tracks the negotiated `bsc/n` version so
/// callers can avoid sending v2-only messages (e.g. `GetBlocksByRange`) to a
/// peer that only speaks `bsc/1`.
struct PeerEntry {
    tx: UnboundedSender<BscCommand>,
    /// Negotiated bsc subprotocol version (1 or 2).
    version: u8,
}

/// Global registry of active BSC protocol peers.
static REGISTRY: Lazy<RwLock<HashMap<PeerId, PeerEntry>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));

/// Optional background task handle for EVN post-sync peer refresh.
static EVN_REFRESH_TASK: Lazy<RwLock<Option<JoinHandle<()>>>> = Lazy::new(|| RwLock::new(None));

/// Global map of proxyed peer IDs for BSC protocol.
/// This mirrors the same functionality in the main peer manager.
static PROXYED_PEER_IDS_MAP: Lazy<RwLock<HashSet<PeerId>>> =
    Lazy::new(|| RwLock::new(HashSet::new()));

/// Register a new peer's sender channel along with its negotiated bsc/n
/// version (1 or 2). Re-registering the same `peer` overwrites the previous
/// entry — used by reconnects that may negotiate a different version.
pub fn register_peer(peer: PeerId, tx: UnboundedSender<BscCommand>, version: u8) {
    let tx_for_sync = tx.clone();
    let mut inserted = false;
    let guard = REGISTRY.write();
    match guard {
        Ok(mut g) => {
            g.insert(peer, PeerEntry { tx, version });
            inserted = true;
        }
        Err(e) => {
            tracing::error!(target: "bsc::registry", error=%e, "Registry lock poisoned (register)");
        }
    }
    if inserted {
        sync_pending_votes_to_peer(peer, tx_for_sync);
    }
}

/// Defensive ceiling on votes sent in a single sync packet. At ~250 B per
/// envelope this caps the encoded `VotesPacket` near 8 MiB, well under RLPx's
/// 16 MiB frame limit. The pool is kept bounded by lazy prune in
/// [`crate::consensus::parlia::vote_pool::put_vote`], so this cap should not
/// trigger in steady state; it exists so a regression in pruning cannot again
/// produce frames that the local p2p stack refuses to send.
const MAX_SYNC_VOTES: usize = 32 * 1024;

fn sync_pending_votes_to_peer(peer: PeerId, tx: UnboundedSender<BscCommand>) {
    // Mirrors geth-bsc's syncVotes: dump currently pending votes to a new peer.
    let mut votes = crate::consensus::parlia::vote_pool::get_votes();
    if votes.is_empty() {
        return;
    }
    if votes.len() > MAX_SYNC_VOTES {
        tracing::warn!(
            target: "bsc::registry",
            peer = %peer,
            total = votes.len(),
            kept = MAX_SYNC_VOTES,
            "vote_pool oversized; truncating sync packet"
        );
        votes.truncate(MAX_SYNC_VOTES);
    }
    if tx.send(BscCommand::Votes(Arc::new(votes))).is_err() {
        tracing::trace!(
            target: "bsc::registry",
            peer = %peer,
            "failed to sync pending votes to newly registered peer"
        );
    }
}

/// Returns true if `peer` negotiated bsc/2 — required for `GetBlocksByRange`.
pub fn is_v2_peer(peer: PeerId) -> bool {
    match REGISTRY.read() {
        Ok(guard) => guard.get(&peer).is_some_and(|e| e.version >= 2),
        Err(_) => false,
    }
}

/// Snapshot of peers that negotiated bsc/2 (i.e. support `GetBlocksByRange`).
pub fn list_v2_peers() -> Vec<PeerId> {
    match REGISTRY.read() {
        Ok(guard) => guard.iter().filter_map(|(p, e)| (e.version >= 2).then_some(*p)).collect(),
        Err(_) => Vec::new(),
    }
}

/// Distinct hashes tracked for announcers. ~6 minutes of mainnet announce
/// traffic at typical rates.
const ANNOUNCER_LRU_CAP: u32 = 256;

/// Per-hash announcer cap. Defensive ceiling; once full, further announcers
/// are dropped FIFO (earlier announcers more likely to have already
/// committed the block).
const MAX_ANNOUNCERS_PER_HASH: usize = 16;

/// Per-hash announcers, used by `request_blocks_by_range_with_failover` to
/// prefer peers known to have the block. On miss/empty the caller falls
/// through to the v2 peer list.
static ANNOUNCERS: Lazy<ParkingMutex<schnellru::LruMap<B256, Vec<PeerId>, schnellru::ByLength>>> =
    Lazy::new(|| {
        ParkingMutex::new(schnellru::LruMap::new(schnellru::ByLength::new(ANNOUNCER_LRU_CAP)))
    });

/// Record `peer` as having announced `hash`. Idempotent on `peer`; bounded
/// by [`MAX_ANNOUNCERS_PER_HASH`] (further announcers dropped FIFO).
pub fn record_announcer(hash: B256, peer: PeerId) {
    let mut g = ANNOUNCERS.lock();
    let list = g.get_or_insert(hash, Vec::new).expect("ByLength limiter never rejects");
    if list.contains(&peer) || list.len() >= MAX_ANNOUNCERS_PER_HASH {
        return;
    }
    list.push(peer);
}

/// Snapshot of peers that announced `hash` and currently negotiate bsc/2.
pub fn list_announcers_for(hash: B256) -> Vec<PeerId> {
    let raw: Vec<PeerId> = {
        let mut g = ANNOUNCERS.lock();
        match g.get(&hash) {
            Some(list) => list.clone(),
            None => return Vec::new(),
        }
    };
    let v2: Vec<PeerId> = raw.into_iter().filter(|p| is_v2_peer(*p)).collect();
    if v2.is_empty() {
        tracing::trace!(
            target: "bsc::registry",
            %hash,
            "announcer LRU hit but all entries are non-v2 or disconnected; falling back to v2 list"
        );
    }
    v2
}

/// Initialize the proxyed peer IDs map from a list of peer IDs.
/// This should be called during network initialization with the same list from config.
pub fn initialize_proxyed_peers(peer_ids: Vec<PeerId>) {
    match PROXYED_PEER_IDS_MAP.write() {
        Ok(mut guard) => {
            guard.clear();
            for peer_id in peer_ids {
                guard.insert(peer_id);
            }
            tracing::info!(
                target: "bsc::registry",
                count = guard.len(),
                "Initialized BSC protocol proxyed peer IDs map"
            );
        }
        Err(e) => {
            tracing::error!(
                target: "bsc::registry",
                error=%e,
                "Failed to initialize proxyed peer IDs map (lock poisoned)"
            );
        }
    }
}

/// Check if a peer is in the proxyed peers list.
/// Returns true if the peer is a proxyed peer.
pub fn is_proxyed_peer(peer_id: &PeerId) -> bool {
    match PROXYED_PEER_IDS_MAP.read() {
        Ok(guard) => guard.contains(peer_id),
        Err(_) => false,
    }
}

/// Simple request id generator for GetBlocksByRange
static REQ_COUNTER: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(1));

/// Request blocks by range from a specific peer. Returns response or timeout error.
///
/// Defensive: rejects peers that did not negotiate bsc/2. `GetBlocksByRange`
/// (msg 0x02) is unknown to bsc/1 peers — sending it gets us kicked with
/// `SubprotocolSpecific`. Callers should normally pick from [`list_v2_peers`];
/// this guard catches any bypass.
pub async fn request_blocks_by_range(
    peer: PeerId,
    start_height: u64,
    start_hash: B256,
    count: u64,
    timeout_dur: Duration,
) -> Result<BlocksByRangePacket, String> {
    if count == 0 || count > MAX_REQUEST_RANGE_BLOCKS_COUNT {
        return Err(format!("invalid count {}", count));
    }

    let tx = {
        let guard = REGISTRY.read();
        match guard {
            Ok(g) => match g.get(&peer) {
                Some(entry) if entry.version >= 2 => Some(entry.tx.clone()),
                Some(_) => return Err("peer does not support bsc/2 GetBlocksByRange".to_string()),
                None => None,
            },
            Err(_) => None,
        }
    }
    .ok_or_else(|| "peer not registered for bsc protocol".to_string())?;

    let request_id = REQ_COUNTER.fetch_add(1, Ordering::Relaxed);
    let (resp_tx, resp_rx) = oneshot::channel();
    let packet = GetBlocksByRangePacket {
        request_id,
        start_block_height: start_height,
        start_block_hash: start_hash,
        count,
    };
    if tx.send(BscCommand::GetBlocksByRange(packet, resp_tx)).is_err() {
        // Send fails iff the bsc/n stream's receiver has been dropped (handshake
        // timeout, sub-protocol stream closed, version mismatch). The eth/68
        // session may still be alive, so the peer-manager has no reason to
        // recycle this connection on its own — without an explicit kick the
        // stale entry would linger and every future GetBlocksByRange to this
        // peer would fail instantly. Evict the dead entry (guarded by
        // `same_channel` so we don't clobber a fresh reconnect) and force an
        // RLPx disconnect; the resulting reconnect re-runs `into_connection`
        // and re-registers a live tx via `register_peer`.
        let evicted = match REGISTRY.write() {
            Ok(mut g) => match g.get(&peer) {
                Some(entry) if entry.tx.same_channel(&tx) => {
                    g.remove(&peer);
                    true
                }
                _ => false,
            },
            Err(e) => {
                tracing::error!(
                    target: "bsc::registry",
                    error = %e,
                    "Registry lock poisoned (range-request cleanup)"
                );
                false
            }
        };
        if evicted {
            tracing::warn!(
                target: "bsc::registry",
                %peer,
                "Evicted stale bsc/2 registry entry after send failure; disconnecting peer to force reconnect"
            );
            if let Some(net) = crate::shared::get_network_handle() {
                net.disconnect_peer(peer);
            }
        }
        return Err("failed to send GetBlocksByRange command".to_string());
    }

    match timeout(timeout_dur, resp_rx).await {
        Ok(Ok(Ok(res))) => Ok(res),
        Ok(Ok(Err(e))) => Err(e),
        Ok(Err(_canceled)) => Err("request canceled".to_string()),
        Err(_elapsed) => Err("request timed out".to_string()),
    }
}

/// Broadcast votes to all connected peers.
pub fn broadcast_votes(votes: Vec<crate::consensus::parlia::vote::VoteEnvelope>) {
    // Spawn async task to evaluate TD policy like geth's logic
    tokio::spawn(async move {
        let votes_arc = Arc::new(votes);
        // Snapshot registry to avoid holding lock during await
        let reg_snapshot: Vec<(PeerId, UnboundedSender<BscCommand>)> = match REGISTRY.read() {
            Ok(guard) => guard.iter().map(|(p, e)| (*p, e.tx.clone())).collect(),
            Err(e) => {
                tracing::error!(target: "bsc::registry", error=%e, "Registry lock poisoned (broadcast snapshot)");
                return;
            }
        };

        // EVN peers always included
        let is_evn = |peer: &PeerId| crate::node::network::evn_peers::is_evn_peer(*peer);

        // Determine local head TD (u128 approx)
        let local_best_td = crate::shared::get_best_canonical_td();
        // Matches go-bsc eth/handler.go: deltaTdThreshold = 1000
        let delta_td_threshold: u128 = 1000;

        // Build a map of PeerId -> PeerInfo for connected peers
        let peer_info_map = if let Some(net) = crate::shared::get_network_handle() {
            match net.get_all_peers().await {
                Ok(list) => list
                    .into_iter()
                    .map(|pi| (pi.remote_id, pi))
                    .collect::<std::collections::HashMap<_, _>>(),
                Err(e) => {
                    tracing::warn!(target: "bsc::registry", error=%e, "Failed to get_all_peers; broadcasting votes to all");
                    std::collections::HashMap::new()
                }
            }
        } else {
            std::collections::HashMap::new()
        };

        let mut to_remove: Vec<PeerId> = Vec::new();
        for (peer, tx) in reg_snapshot {
            let peer_best_td = peer_info_map.get(&peer).and_then(|info| info.best_td);
            let allow = should_allow_vote_broadcast(
                is_evn(&peer) || is_proxyed_peer(&peer),
                local_best_td,
                peer_best_td,
                delta_td_threshold,
            );

            if let Some(info) = peer_info_map.get(&peer) {
                tracing::debug!(
                    target: "bsc::vote",
                    peer=%peer,
                    latest_block=info.best_number,
                    local_best_td=local_best_td,
                    peer_best_td=u256_to_u128(info.best_td.unwrap_or_default()),
                    allow=allow,
                    "peer info when checking allow broadcast votes"
                );
            }

            tracing::trace!(target: "bsc::vote", peer=%peer, allow=allow, is_proxyed=is_proxyed_peer(&peer), "broadcast votes to peer");
            if allow && tx.send(BscCommand::Votes(Arc::clone(&votes_arc))).is_err() {
                tracing::trace!(target: "bsc::vote", peer=%peer, "failed to send votes to peer, remove from registry");
                to_remove.push(peer);
            }
        }

        if !to_remove.is_empty() {
            match REGISTRY.write() {
                Ok(mut guard) => {
                    for peer in to_remove {
                        guard.remove(&peer);
                    }
                }
                Err(e) => {
                    tracing::error!(target: "bsc::registry", error=%e, "Registry lock poisoned (cleanup)");
                }
            }
        }
    });
}

fn should_allow_vote_broadcast(
    is_evn_or_proxyed: bool,
    local_best_td: Option<u128>,
    peer_best_td: Option<alloy_primitives::U256>,
    delta_td_threshold: u128,
) -> bool {
    if is_evn_or_proxyed {
        return true;
    }
    let Some(local_td) = local_best_td else {
        // Keep previous permissive behavior when local TD is temporarily unavailable.
        return true;
    };
    let Some(peer_td) = peer_best_td.and_then(u256_to_u128) else {
        // Keep previous permissive behavior when peer metadata is temporarily unavailable.
        return true;
    };
    local_td.abs_diff(peer_td) <= delta_td_threshold
}

fn u256_to_u128(v: alloy_primitives::U256) -> Option<u128> {
    // Convert big-endian 32-byte array to u128 if it fits
    let be: [u8; 32] = v.to_be_bytes::<32>();
    let high = u128::from_be_bytes(be[0..16].try_into().unwrap());
    let low = u128::from_be_bytes(be[16..32].try_into().unwrap());
    if high == 0 {
        Some(low)
    } else {
        None
    }
}

// Snapshot current connected peers (BSC protocol) by PeerId.
// Note: currently used only as part of internal EVN refresh; can be reinstated if needed.

/// Subscribe to EVN-armed notification and log-refresh current peers.
/// This helps post-sync peers reflect EVN policy locally. Remote peers
/// will pick up EVN on subsequent handshakes; this is a best-effort local refresh.
pub fn spawn_evn_refresh_listener() {
    // One-shot install only
    if let Ok(mut guard) = EVN_REFRESH_TASK.write() {
        if guard.is_some() {
            return;
        }

        // Subscribe to EVN armed broadcast channel
        let rx = crate::node::network::evn::subscribe_evn_armed();
        let handle = tokio::spawn(async move {
            let mut rx = rx;
            loop {
                match rx.recv().await {
                    Ok(()) => {
                        // On EVN arm, log the currently registered peers
                        let peers: Vec<PeerId> = match REGISTRY.read() {
                            Ok(g) => g.keys().copied().collect(),
                            Err(_) => Vec::new(),
                        };
                        tracing::info!(
                            target: "bsc::evn",
                            peer_count = peers.len(),
                            "EVN armed: refreshing EVN state for existing peers"
                        );
                        // Apply on-chain NodeIDs to current peers if available
                        let nodeids = crate::node::network::evn_peers::get_onchain_nodeids_set();
                        tracing::debug!(target: "bsc::evn", nodeids = ?nodeids, "NodeIDs set");
                        let mut marked = 0usize;
                        for p in peers {
                            let node_id = crate::node::network::evn_peers::peer_id_to_node_id(p);
                            tracing::debug!(target: "bsc::evn", peer_id = ?p, node_id = ?node_id, "Checking if peer is EVN: {}", nodeids.contains(&node_id));
                            if nodeids.contains(&node_id) {
                                crate::node::network::evn_peers::mark_evn_onchain(p);
                                if let Some(net) = crate::shared::get_network_handle() {
                                    net.add_trusted_peer_id(p);
                                }
                                marked += 1;
                            }
                        }
                        tracing::info!(target: "bsc::evn", marked = marked, nodeids = ?nodeids, "Applied on-chain EVN NodeIDs to peers");

                        // Start periodic refresh every 60s to apply on-chain NodeIDs to existing
                        // peers
                        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(60));
                        loop {
                            ticker.tick().await;
                            let peers: Vec<PeerId> = match REGISTRY.read() {
                                Ok(g) => g.keys().copied().collect(),
                                Err(_) => Vec::new(),
                            };
                            let nodeids =
                                crate::node::network::evn_peers::get_onchain_nodeids_set();
                            tracing::debug!(target: "bsc::evn", nodeids = ?nodeids, "NodeIDs set");
                            let mut marked = 0usize;
                            for p in peers {
                                let node_id =
                                    crate::node::network::evn_peers::peer_id_to_node_id(p);
                                tracing::debug!(target: "bsc::evn", peer_id = ?p, node_id = ?node_id, "Checking if peer is EVN: {}", nodeids.contains(&node_id));
                                if nodeids.contains(&node_id) {
                                    crate::node::network::evn_peers::mark_evn_onchain(p);
                                    if let Some(net) = crate::shared::get_network_handle() {
                                        net.add_trusted_peer_id(p);
                                    }
                                    marked += 1;
                                }
                            }
                            tracing::debug!(target: "bsc::evn", marked = marked, nodeids = ?nodeids, "Periodic EVN on-chain NodeIDs applied to peers");
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                }
            }
        });
        *guard = Some(handle);
    }
}

/// Failover plan for `GetBlocksByRange` — only bsc/2 peers eligible.
/// Order: `preferred` (if bsc/2), `announcers`, then remaining v2 peers.
/// Duplicates are removed and the result is truncated to `max_attempts`.
///
/// `announcers` should already be v2-filtered (see [`list_announcers_for`]);
/// empty `announcers` falls back to the v2 list alone.
pub(crate) fn plan_v2_failover_peers(
    preferred: PeerId,
    announcers: Vec<PeerId>,
    v2_peers: Vec<PeerId>,
    max_attempts: usize,
) -> Vec<PeerId> {
    if max_attempts == 0 {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(max_attempts);
    if v2_peers.contains(&preferred) {
        out.push(preferred);
    }
    for p in announcers.into_iter().chain(v2_peers) {
        if out.len() >= max_attempts {
            break;
        }
        if !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

/// Like [`request_blocks_by_range`], but rotates through other bsc/2 peers on
/// `Err` or empty response. Returns the first non-empty success, otherwise
/// the last seen result (preserving the original error for diagnostics).
///
/// Candidates are restricted to bsc/2 peers; see [`plan_v2_failover_peers`].
pub async fn request_blocks_by_range_with_failover(
    preferred: PeerId,
    start_height: u64,
    start_hash: B256,
    count: u64,
    timeout_dur: Duration,
    max_attempts: usize,
) -> Result<BlocksByRangePacket, String> {
    let announcers = list_announcers_for(start_hash);
    let peers = plan_v2_failover_peers(preferred, announcers, list_v2_peers(), max_attempts);
    if peers.is_empty() {
        return Err("no bsc/2 peers available for range request".to_string());
    }

    let mut last: Result<BlocksByRangePacket, String> = Err("uninitialised failover".to_string());
    for (idx, peer) in peers.iter().enumerate() {
        match request_blocks_by_range(*peer, start_height, start_hash, count, timeout_dur).await {
            Ok(resp) if !resp.blocks.is_empty() => return Ok(resp),
            Ok(empty_resp) => {
                tracing::debug!(
                    target: "bsc_protocol",
                    %peer,
                    attempt = idx + 1,
                    total = peers.len(),
                    start_height,
                    %start_hash,
                    "Empty BlocksByRange response, trying next peer"
                );
                last = Ok(empty_resp);
            }
            Err(err) => {
                tracing::debug!(
                    target: "bsc_protocol",
                    %peer,
                    attempt = idx + 1,
                    total = peers.len(),
                    start_height,
                    %start_hash,
                    %err,
                    "BlocksByRange request failed, trying next peer"
                );
                last = Err(err);
            }
        }
    }
    last
}

#[cfg(test)]
mod version_tests {
    //! Unit tests for the bsc/n version-aware registry. Each test uses a
    //! locally-unique `PeerId` byte to avoid cross-test interference in the
    //! shared global `REGISTRY`.

    use super::*;
    use alloy_primitives::B512;

    fn pid(byte: u8) -> PeerId {
        B512::repeat_byte(byte)
    }

    /// Drops the test peer from the global registry on test exit so that
    /// subsequent tests (or repeated runs) start clean.
    struct TestPeerGuard(PeerId);
    impl Drop for TestPeerGuard {
        fn drop(&mut self) {
            if let Ok(mut g) = REGISTRY.write() {
                g.remove(&self.0);
            }
        }
    }

    #[test]
    fn v1_peer_is_not_v2() {
        let p = pid(0xA1);
        let _g = TestPeerGuard(p);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        register_peer(p, tx, 1);

        assert!(!is_v2_peer(p));
        assert!(!list_v2_peers().contains(&p));
    }

    #[test]
    fn v2_peer_is_v2() {
        let p = pid(0xA2);
        let _g = TestPeerGuard(p);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        register_peer(p, tx, 2);

        assert!(is_v2_peer(p));
        assert!(list_v2_peers().contains(&p));
    }

    #[test]
    fn upgrade_v1_to_v2_takes_effect() {
        // Same peer reconnects negotiating a higher version → registry
        // overwrites the entry, version gets upgraded.
        let p = pid(0xA3);
        let _g = TestPeerGuard(p);
        let (tx1, _rx1) = tokio::sync::mpsc::unbounded_channel();
        register_peer(p, tx1, 1);
        assert!(!is_v2_peer(p));

        let (tx2, _rx2) = tokio::sync::mpsc::unbounded_channel();
        register_peer(p, tx2, 2);
        assert!(is_v2_peer(p));
    }

    #[test]
    fn downgrade_v2_to_v1_takes_effect() {
        let p = pid(0xA4);
        let _g = TestPeerGuard(p);
        let (tx2, _rx2) = tokio::sync::mpsc::unbounded_channel();
        register_peer(p, tx2, 2);
        assert!(is_v2_peer(p));

        let (tx1, _rx1) = tokio::sync::mpsc::unbounded_channel();
        register_peer(p, tx1, 1);
        assert!(!is_v2_peer(p));
        assert!(!list_v2_peers().contains(&p));
    }

    #[tokio::test]
    async fn request_blocks_by_range_rejects_v1_peer() {
        // The defensive guard inside `request_blocks_by_range`: even if a
        // caller bypasses `with_failover` and dials a v1 peer directly, the
        // function returns Err instead of emitting a 0x02 frame.
        let p = pid(0xA5);
        let _g = TestPeerGuard(p);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        register_peer(p, tx, 1);

        let err = request_blocks_by_range(p, 1, B256::ZERO, 1, Duration::from_millis(50))
            .await
            .unwrap_err();
        assert!(err.contains("bsc/2"), "expected bsc/2 rejection, got: {err}");
    }

    // ---- announcer LRU tests ----

    /// Drops a hash from the announcer LRU on test exit.
    struct TestHashGuard(B256);
    impl Drop for TestHashGuard {
        fn drop(&mut self) {
            ANNOUNCERS.lock().remove(&self.0);
        }
    }

    fn hash(byte: u8) -> B256 {
        B256::repeat_byte(byte)
    }

    #[test]
    fn record_announcer_preserves_insertion_order() {
        let h = hash(0xB1);
        let _g = TestHashGuard(h);
        let p1 = pid(0xB1);
        let p2 = pid(0xB2);
        let _gp1 = TestPeerGuard(p1);
        let _gp2 = TestPeerGuard(p2);
        let (tx1, _rx1) = tokio::sync::mpsc::unbounded_channel();
        let (tx2, _rx2) = tokio::sync::mpsc::unbounded_channel();
        register_peer(p1, tx1, 2);
        register_peer(p2, tx2, 2);

        record_announcer(h, p1);
        record_announcer(h, p2);

        // p1 announced first → must come first in the list.
        assert_eq!(list_announcers_for(h), vec![p1, p2]);
    }

    #[test]
    fn record_announcer_dedups_repeat_record() {
        let h = hash(0xB3);
        let _g = TestHashGuard(h);
        let p = pid(0xB3);
        let _gp = TestPeerGuard(p);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        register_peer(p, tx, 2);

        record_announcer(h, p);
        record_announcer(h, p);
        record_announcer(h, p);

        assert_eq!(list_announcers_for(h), vec![p]);
    }

    #[test]
    fn record_announcer_drops_after_cap() {
        let h = hash(0xB4);
        let _g = TestHashGuard(h);
        // Register MAX_ANNOUNCERS_PER_HASH + 2 peers (all v2).
        let mut peers = Vec::with_capacity(MAX_ANNOUNCERS_PER_HASH + 2);
        let mut guards: Vec<TestPeerGuard> = Vec::with_capacity(MAX_ANNOUNCERS_PER_HASH + 2);
        for i in 0..(MAX_ANNOUNCERS_PER_HASH + 2) {
            let p = pid(0xC0 + i as u8);
            guards.push(TestPeerGuard(p));
            let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
            register_peer(p, tx, 2);
            record_announcer(h, p);
            peers.push(p);
        }

        let listed = list_announcers_for(h);
        assert_eq!(listed.len(), MAX_ANNOUNCERS_PER_HASH);
        // FIFO: the first `MAX_ANNOUNCERS_PER_HASH` peers are kept; later ones dropped.
        assert_eq!(listed, peers[..MAX_ANNOUNCERS_PER_HASH]);
    }

    #[test]
    fn list_announcers_for_filters_v1_and_disconnected() {
        let h = hash(0xB5);
        let _g = TestHashGuard(h);
        let v2_peer = pid(0xB6);
        let v1_peer = pid(0xB7);
        let _gp_v2 = TestPeerGuard(v2_peer);
        let _gp_v1 = TestPeerGuard(v1_peer);
        let (tx_v2, _rx_v2) = tokio::sync::mpsc::unbounded_channel();
        let (tx_v1, _rx_v1) = tokio::sync::mpsc::unbounded_channel();
        register_peer(v2_peer, tx_v2, 2);
        register_peer(v1_peer, tx_v1, 1);

        record_announcer(h, v2_peer);
        record_announcer(h, v1_peer);

        // Disconnected peer simulated by a peer that was never registered.
        let ghost_peer = pid(0xB8);
        record_announcer(h, ghost_peer);

        // Only the v2-and-still-registered peer survives the filter.
        assert_eq!(list_announcers_for(h), vec![v2_peer]);
    }

    #[test]
    fn list_announcers_for_returns_empty_on_lru_miss() {
        // A hash never recorded: list returns empty (no panic, no error).
        let h = hash(0xB9);
        // No record_announcer call.
        assert!(list_announcers_for(h).is_empty());
    }
}

#[cfg(test)]
mod failover_tests {
    use super::*;
    use alloy_primitives::B512;

    fn pid(byte: u8) -> PeerId {
        B512::repeat_byte(byte)
    }

    #[test]
    fn plan_v2_zero_attempts_returns_empty() {
        let plan = plan_v2_failover_peers(pid(1), vec![], vec![pid(1), pid(2)], 0);
        assert!(plan.is_empty());
    }

    // ---- plan_v2_failover_peers: legacy (no announcer) tests ----

    #[test]
    fn plan_v2_keeps_v2_preferred_at_head() {
        let plan = plan_v2_failover_peers(pid(1), vec![], vec![pid(1), pid(2), pid(3)], 3);
        assert_eq!(plan, vec![pid(1), pid(2), pid(3)]);
    }

    #[test]
    fn plan_v2_drops_non_v2_preferred() {
        // preferred (v1) is NOT in the v2 list → must not appear in the plan.
        let plan = plan_v2_failover_peers(pid(9), vec![], vec![pid(2), pid(3), pid(4)], 3);
        assert_eq!(plan, vec![pid(2), pid(3), pid(4)]);
        assert!(!plan.contains(&pid(9)));
    }

    #[test]
    fn plan_v2_empty_v2_list_yields_empty_plan() {
        let plan = plan_v2_failover_peers(pid(1), vec![], vec![], 3);
        assert!(plan.is_empty());
    }

    #[test]
    fn plan_v2_respects_max_attempts_on_non_v2_path() {
        let plan = plan_v2_failover_peers(pid(9), vec![], vec![pid(2), pid(3), pid(4)], 2);
        assert_eq!(plan, vec![pid(2), pid(3)]);
    }

    // ---- plan_v2_failover_peers: announcer-aware tests ----

    #[test]
    fn plan_v2_announcers_come_before_other_v2_peers() {
        // Announcers (already v2-filtered by the caller) are tried before
        // the rest of the v2 list, regardless of their position in v2_peers.
        let plan = plan_v2_failover_peers(
            pid(1),
            vec![pid(3)],                         // announcer
            vec![pid(1), pid(2), pid(3), pid(4)], // full v2 list
            4,
        );
        // Order: preferred → announcer → remaining v2.
        assert_eq!(plan, vec![pid(1), pid(3), pid(2), pid(4)]);
    }

    #[test]
    fn plan_v2_preferred_takes_precedence_over_announcers() {
        // Even if `preferred` is not the first announcer, when it's in the
        // v2 list it leads.
        let plan =
            plan_v2_failover_peers(pid(1), vec![pid(2), pid(3)], vec![pid(1), pid(2), pid(3)], 3);
        assert_eq!(plan, vec![pid(1), pid(2), pid(3)]);
    }

    #[test]
    fn plan_v2_dedups_across_preferred_announcers_and_v2() {
        // A peer appearing in all three sources only shows up once.
        let plan =
            plan_v2_failover_peers(pid(1), vec![pid(1), pid(2)], vec![pid(1), pid(2), pid(3)], 3);
        assert_eq!(plan, vec![pid(1), pid(2), pid(3)]);
    }

    #[test]
    fn plan_v2_truncates_to_max_attempts() {
        let plan = plan_v2_failover_peers(
            pid(1),
            vec![pid(2), pid(3)],
            vec![pid(1), pid(2), pid(3), pid(4)],
            2,
        );
        assert_eq!(plan, vec![pid(1), pid(2)]);
    }

    #[test]
    fn plan_v2_announcers_only_when_preferred_not_v2() {
        // Non-v2 preferred is dropped; announcers (v2) lead the plan.
        let plan =
            plan_v2_failover_peers(pid(9), vec![pid(3), pid(4)], vec![pid(2), pid(3), pid(4)], 3);
        assert_eq!(plan, vec![pid(3), pid(4), pid(2)]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allow_vote_broadcast_for_evn_or_proxyed_peer() {
        assert!(should_allow_vote_broadcast(true, None, None, 1000));
    }

    #[test]
    fn allow_vote_broadcast_when_td_delta_within_threshold() {
        let local_td = Some(10_000u128);
        let peer_td = Some(alloy_primitives::U256::from(10_500u64));
        assert!(should_allow_vote_broadcast(false, local_td, peer_td, 1000));
    }

    #[test]
    fn reject_vote_broadcast_when_td_delta_exceeds_threshold() {
        let local_td = Some(10_000u128);
        let peer_td = Some(alloy_primitives::U256::from(11_500u64));
        assert!(!should_allow_vote_broadcast(false, local_td, peer_td, 1000));
    }

    #[test]
    fn allow_vote_broadcast_when_td_missing() {
        assert!(should_allow_vote_broadcast(false, Some(10_000u128), None, 1000));
        assert!(should_allow_vote_broadcast(
            false,
            None,
            Some(alloy_primitives::U256::from(10_000u64)),
            1000
        ));
    }
}
