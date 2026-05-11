use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::RwLock;

use once_cell::sync::Lazy;
use tokio::sync::broadcast;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use reth_network_api::PeerId;

use super::stream::BscCommand;
use crate::node::network::blocks_by_range::{
    BlocksByRangePacket, GetBlocksByRangePacket, MAX_REQUEST_RANGE_BLOCKS_COUNT,
};
use alloy_primitives::B256;
use reth_network::Peers;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::time::timeout;

/// Per-connection token for [`PeerRegistryEntry`]. Starts at 1; `0` means
/// "not registered" (e.g. poisoned lock during `register_peer`).
static PEER_CONN_TOKEN: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(1));

struct PeerRegistryEntry {
    tx: UnboundedSender<BscCommand>,
    /// Monotonic id for this BSC subprotocol session; used so `Drop` / failed
    /// sends only remove the matching entry after same-`PeerId` reconnect.
    conn_token: u64,
}

/// Global registry of active BSC protocol senders per peer.
static REGISTRY: Lazy<RwLock<HashMap<PeerId, PeerRegistryEntry>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));

/// Optional background task handle for EVN post-sync peer refresh.
static EVN_REFRESH_TASK: Lazy<RwLock<Option<JoinHandle<()>>>> = Lazy::new(|| RwLock::new(None));

/// Global map of proxyed peer IDs for BSC protocol.
/// This mirrors the same functionality in the main peer manager.
static PROXYED_PEER_IDS_MAP: Lazy<RwLock<HashSet<PeerId>>> =
    Lazy::new(|| RwLock::new(HashSet::new()));

/// Register a new peer's sender channel. Returns a per-connection token for
/// [`unregister_peer_if_current`]; `0` if the registry lock was poisoned.
pub fn register_peer(peer: PeerId, tx: UnboundedSender<BscCommand>) -> u64 {
    let tx_for_sync = tx.clone();
    let mut g = match REGISTRY.write() {
        Ok(g) => g,
        Err(e) => {
            tracing::error!(
                target: "bsc::registry",
                error=%e,
                "Registry lock poisoned (register)"
            );
            return 0;
        }
    };
    let conn_token = PEER_CONN_TOKEN.fetch_add(1, Ordering::Relaxed);
    g.insert(
        peer,
        PeerRegistryEntry {
            tx,
            conn_token,
        },
    );
    sync_pending_votes_to_peer(peer, tx_for_sync);
    conn_token
}

/// Remove `peer` from the registry only if the stored session matches
/// `conn_token`. Safe across same-`PeerId` reconnects and replaced entries.
pub fn unregister_peer_if_current(peer: PeerId, conn_token: u64) {
    if conn_token == 0 {
        return;
    }
    let Ok(mut g) = REGISTRY.write() else {
        return;
    };
    let remove = g
        .get(&peer)
        .is_some_and(|entry| entry.conn_token == conn_token);
    if remove {
        g.remove(&peer);
        tracing::debug!(
            target: "bsc::registry",
            %peer,
            conn_token,
            "Unregistered BSC protocol peer (session ended)"
        );
    }
}

fn sync_pending_votes_to_peer(peer: PeerId, tx: UnboundedSender<BscCommand>) {
    // Keep parity with go-bsc's syncVotes: send currently pending votes to a new peer.
    let votes = crate::consensus::parlia::vote_pool::get_votes();
    if votes.is_empty() {
        return;
    }
    let votes_arc = Arc::new(votes);
    if tx.send(BscCommand::Votes(votes_arc)).is_err() {
        tracing::trace!(
            target: "bsc::registry",
            peer = %peer,
            "failed to sync pending votes to newly registered peer"
        );
    }
}

/// Snapshot the currently registered BSC protocol peers
pub fn list_registered_peers() -> Vec<PeerId> {
    match REGISTRY.read() {
        Ok(guard) => guard.keys().copied().collect(),
        Err(_) => Vec::new(),
    }
}

/// Returns true if the given peer is registered with the BSC subprotocol
pub fn has_registered_peer(peer: PeerId) -> bool {
    match REGISTRY.read() {
        Ok(guard) => guard.contains_key(&peer),
        Err(_) => false,
    }
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

    let (tx, conn_token) = {
        let guard = REGISTRY.read();
        match guard {
            Ok(g) => g.get(&peer).map(|e| (e.tx.clone(), e.conn_token)),
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
    if tx
        .send(BscCommand::GetBlocksByRange(packet, resp_tx))
        .is_err()
    {
        unregister_peer_if_current(peer, conn_token);
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
        let reg_snapshot: Vec<(PeerId, UnboundedSender<BscCommand>, u64)> = match REGISTRY.read() {
            Ok(guard) => guard
                .iter()
                .map(|(p, e)| (*p, e.tx.clone(), e.conn_token))
                .collect(),
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

        let mut to_remove: Vec<(PeerId, u64)> = Vec::new();
        for (peer, tx, conn_token) in reg_snapshot {
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
                to_remove.push((peer, conn_token));
            }
        }

        for (peer, conn_token) in to_remove {
            unregister_peer_if_current(peer, conn_token);
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

                        // Start periodic refresh every 60s to apply on-chain NodeIDs to existing peers
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

/// Compute a failover peer ordering: `preferred` first, then up to
/// `max_attempts - 1` other peers from `registered`, preserving order and
/// deduplicating. Returns at most `max_attempts` entries.
pub(crate) fn plan_failover_peers(
    preferred: PeerId,
    registered: Vec<PeerId>,
    max_attempts: usize,
) -> Vec<PeerId> {
    if max_attempts == 0 {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(max_attempts);
    out.push(preferred);
    for p in registered {
        if out.len() >= max_attempts {
            break;
        }
        if !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

/// Like [`request_blocks_by_range`], but rotates through other registered BSC
/// peers on `Err` or empty response. Returns the first non-empty success,
/// otherwise the last seen result (preserving the original error for
/// diagnostics).
pub async fn request_blocks_by_range_with_failover(
    preferred: PeerId,
    start_height: u64,
    start_hash: B256,
    count: u64,
    timeout_dur: Duration,
    max_attempts: usize,
) -> Result<BlocksByRangePacket, String> {
    let peers = plan_failover_peers(preferred, list_registered_peers(), max_attempts);
    if peers.is_empty() {
        return Err("no BSC peers available for range request".to_string());
    }

    let mut last: Result<BlocksByRangePacket, String> =
        Err("uninitialised failover".to_string());
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
mod failover_tests {
    use super::*;
    use alloy_primitives::B512;

    fn pid(byte: u8) -> PeerId {
        B512::repeat_byte(byte)
    }

    #[test]
    fn plan_puts_preferred_first_and_dedups() {
        let plan = plan_failover_peers(pid(1), vec![pid(2), pid(1), pid(3)], 3);
        assert_eq!(plan, vec![pid(1), pid(2), pid(3)]);
    }

    #[test]
    fn plan_respects_max_attempts() {
        let plan = plan_failover_peers(pid(1), vec![pid(2), pid(3), pid(4)], 2);
        assert_eq!(plan, vec![pid(1), pid(2)]);
    }

    #[test]
    fn plan_handles_zero_attempts() {
        let plan = plan_failover_peers(pid(1), vec![pid(2)], 0);
        assert!(plan.is_empty());
    }

    #[test]
    fn plan_handles_empty_registered() {
        let plan = plan_failover_peers(pid(1), vec![], 5);
        assert_eq!(plan, vec![pid(1)]);
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
        assert!(should_allow_vote_broadcast(
            false,
            Some(10_000u128),
            None,
            1000
        ));
        assert!(should_allow_vote_broadcast(
            false,
            None,
            Some(alloy_primitives::U256::from(10_000u64)),
            1000
        ));
    }
}
