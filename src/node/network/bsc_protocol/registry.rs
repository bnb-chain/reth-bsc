use std::collections::HashMap;
use std::sync::RwLock;
use std::sync::Arc;

use alloy_primitives::U128;
use once_cell::sync::Lazy;
use reth_eth_wire::NewBlock;
use reth_network::message::NewBlockMessage;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::sync::broadcast;

use reth_network_api::PeerId;

use super::stream::BscCommand;
use reth_network::Peers;
use crate::node::network::BscNewBlock;
use crate::node::network::blocks_by_range::{GetBlocksByRangePacket, BlocksByRangePacket, MAX_REQUEST_RANGE_BLOCKS_COUNT};
use alloy_primitives::B256;
use std::time::Duration;
use tokio::time::timeout;
use std::sync::atomic::{AtomicU64, Ordering};

/// Global registry of active BSC protocol senders per peer.
static REGISTRY: Lazy<RwLock<HashMap<PeerId, UnboundedSender<BscCommand>>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));

/// Optional background task handle for EVN post-sync peer refresh.
static EVN_REFRESH_TASK: Lazy<RwLock<Option<JoinHandle<()>>>> =
    Lazy::new(|| RwLock::new(None));

static ALL_PEERS_ALLOW_BROADCAST: bool = true;

/// Register a new peer's sender channel.
pub fn register_peer(peer: PeerId, tx: UnboundedSender<BscCommand>) {
    let guard = REGISTRY.write();
    match guard {
        Ok(mut g) => {
            g.insert(peer, tx);
        }
        Err(e) => {
            tracing::error!(target: "bsc::registry", error=%e, "Registry lock poisoned (register)");
        }
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

    let tx = {
        let guard = REGISTRY.read();
        match guard {
            Ok(g) => g.get(&peer).cloned(),
            Err(_) => None,
        }
    }.ok_or_else(|| "peer not registered for bsc protocol".to_string())?;

    let request_id = REQ_COUNTER.fetch_add(1, Ordering::Relaxed);
    let (resp_tx, resp_rx) = oneshot::channel();
    let packet = GetBlocksByRangePacket { request_id, start_block_height: start_height, start_block_hash: start_hash, count };
    tx.send(BscCommand::GetBlocksByRange(packet, resp_tx))
        .map_err(|_| "failed to send GetBlocksByRange command".to_string())?;

    match timeout(timeout_dur, resp_rx).await {
        Ok(Ok(Ok(res))) => Ok(res),
        Ok(Ok(Err(e))) => Err(e),
        Ok(Err(_canceled)) => Err("request canceled".to_string()),
        Err(_elapsed) => Err("request timed out".to_string()),
    }
}

/// Batch request a range and wait for import of the returned blocks.
/// Returns the list of imported block hashes in ascending order (oldest -> newest).
pub async fn batch_request_range_and_await_import(
    peer: PeerId,
    start_height: u64,
    start_hash: B256,
    count: u64,
    request_timeout: Duration
) -> Result<(), String> {
    let resp = request_blocks_by_range(peer, start_height, start_hash, count, request_timeout).await?;
    tracing::debug!(
        target: "bsc::registry",
        peer = %peer,
        start_height = start_height,
        start_hash = %start_hash,
        count = count,
        blocks = resp.blocks.len(),
        "Batch request range and await importing blocks"
    );

    // Forward blocks to import path (iterate oldest -> newest)
    if let Some(sender) = crate::shared::get_block_import_sender() {
        for block in resp.blocks.iter().rev() {
            let nb = BscNewBlock(NewBlock {
                block: block.clone(),
                td: U128::from(0u64),
            });
            let hash = block.header.hash_slow();
            let msg = NewBlockMessage { hash, block: Arc::new(nb) };
            if let Err(e) = sender.send((msg, peer)) {
                tracing::error!(target: "bsc::registry", error=%e, "Failed to send block to import path");
            }
        }
    } else {
        tracing::debug!(target: "bsc_protocol", "Block import sender not available; dropping range import forward");
    }

    Ok(())
}

/// Broadcast votes to all connected peers.
pub fn broadcast_votes(votes: Vec<crate::consensus::parlia::vote::VoteEnvelope>) {
    // Spawn async task to evaluate TD policy like geth's logic
    tokio::spawn(async move {
        let votes_arc = Arc::new(votes);
        // Snapshot registry to avoid holding lock during await
        let reg_snapshot: Vec<(PeerId, UnboundedSender<BscCommand>)> = match REGISTRY.read() {
            Ok(guard) => guard.iter().map(|(p, tx)| (*p, tx.clone())).collect(),
            Err(e) => {
                tracing::error!(target: "bsc::registry", error=%e, "Registry lock poisoned (broadcast snapshot)");
                return;
            }
        };

        // EVN peers always included
        let is_evn = |peer: &PeerId| crate::node::network::evn_peers::is_evn_peer(*peer);

        // Determine local head TD (u128 approx) and latest block
        let local_best_td = crate::shared::get_best_canonical_td();
        let local_best_number = crate::shared::get_best_canonical_block_number().unwrap_or_default();
        let delta_td_threshold: u128 = 20;

        // Build a map of PeerId -> PeerInfo for connected peers
        let peer_info_map = if let Some(net) = crate::shared::get_network_handle() {
            match net.get_all_peers().await {
                Ok(list) => list.into_iter().map(|pi| (pi.remote_id, pi)).collect::<std::collections::HashMap<_, _>>() ,
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
            // Always include EVN peers
            // TODO: fix the allow broadcast logic, it should be based on the peer's TD status, it seems not working.
            let mut allow = is_evn(&peer);
            if !allow {
                if let Some(info) = peer_info_map.get(&peer) {
                    tracing::trace!(target: "bsc::vote", peer=%peer, latest_block=info.status.latest_block, 
                        total_difficulty=u256_to_u128(info.status.total_difficulty.unwrap_or_default()), 
                        "peer info when checking allow broadcast votes");
                    // Prefer Eth69 latest block distance; else use total_difficulty delta if both are known
                    if let Some(peer_latest) = info.status.latest_block {
                        let delta = (local_best_number as u128).abs_diff(peer_latest as u128);
                        if delta <= delta_td_threshold { allow = true; }
                    } else if let (Some(local_td), Some(peer_td)) = (local_best_td, info.status.total_difficulty) {
                        // Convert peer td (U256 alloy) to u128
                        let peer_td_u128 = u256_to_u128(peer_td);
                        if let Some(peer_td_u128) = peer_td_u128 {
                            let delta = local_td.abs_diff(peer_td_u128);
                            if delta <= delta_td_threshold { allow = true; }
                        }
                    } else {
                        // If no info, fallback to include
                        allow = true;
                    }
                } else {
                    // No info, fallback include
                    allow = true;
                }
            }
            if ALL_PEERS_ALLOW_BROADCAST {
                allow = true;
            }

            tracing::trace!(target: "bsc::vote", peer=%peer, allow=allow, "broadcast votes to peer");
            if allow && tx.send(BscCommand::Votes(Arc::clone(&votes_arc))).is_err() {
                tracing::trace!(target: "bsc::vote", peer=%peer, "failed to send votes to peer, remove from registry");
                to_remove.push(peer);
            }
        }

        if !to_remove.is_empty() {
            match REGISTRY.write() {
                Ok(mut guard) => {
                    for peer in to_remove { guard.remove(&peer); }
                }
                Err(e) => {
                    tracing::error!(target: "bsc::registry", error=%e, "Registry lock poisoned (cleanup)");
                }
            }
        }
    });
}

fn u256_to_u128(v: alloy_primitives::U256) -> Option<u128> {
    // Convert big-endian 32-byte array to u128 if it fits
    let be: [u8; 32] = v.to_be_bytes::<32>();
    let high = u128::from_be_bytes(be[0..16].try_into().unwrap());
    let low = u128::from_be_bytes(be[16..32].try_into().unwrap());
    if high == 0 { Some(low) } else { None }
}

// Snapshot current connected peers (BSC protocol) by PeerId.
// Note: currently used only as part of internal EVN refresh; can be reinstated if needed.

/// Subscribe to EVN-armed notification and log-refresh current peers.
/// This helps post-sync peers reflect EVN policy locally. Remote peers
/// will pick up EVN on subsequent handshakes; this is a best-effort local refresh.
pub fn spawn_evn_refresh_listener() {
    // One-shot install only
    if let Ok(mut guard) = EVN_REFRESH_TASK.write() {
        if guard.is_some() { return; }

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
                        let mut marked = 0usize;
                        for p in peers {
                            let pid = p.to_string();
                            let pid_norm = crate::node::network::evn_peers::normalize_node_id_str(&pid);
                            if nodeids.contains(&pid_norm) {
                                crate::node::network::evn_peers::mark_evn_onchain(p);
                                if let Some(net) = crate::shared::get_network_handle() {
                                    net.add_trusted_peer_id(p);
                                }
                                marked += 1;
                            }
                        }
                        tracing::info!(target: "bsc::evn", marked, "Applied on-chain EVN NodeIDs to peers");

                        // Start periodic refresh every 60s to apply on-chain NodeIDs to existing peers
                        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(60));
                        loop {
                            ticker.tick().await;
                            let peers: Vec<PeerId> = match REGISTRY.read() {
                                Ok(g) => g.keys().copied().collect(),
                                Err(_) => Vec::new(),
                            };
                            let nodeids = crate::node::network::evn_peers::get_onchain_nodeids_set();
                            let mut marked = 0usize;
                            for p in peers {
                                let pid = p.to_string();
                                let pid_norm = crate::node::network::evn_peers::normalize_node_id_str(&pid);
                                if nodeids.contains(&pid_norm) {
                                    crate::node::network::evn_peers::mark_evn_onchain(p);
                                    if let Some(net) = crate::shared::get_network_handle() {
                                        net.add_trusted_peer_id(p);
                                    }
                                    marked += 1;
                                }
                            }
                            tracing::debug!(target: "bsc::evn", marked, "Periodic EVN on-chain NodeIDs applied to peers");
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
