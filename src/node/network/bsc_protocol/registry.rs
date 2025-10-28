use std::collections::HashMap;
use std::sync::RwLock;
use std::sync::Arc;

use once_cell::sync::Lazy;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;
use tokio::sync::broadcast;

use reth_network_api::PeerId;

use super::stream::BscCommand;
use reth_network::Peers;

/// Global registry of active BSC protocol senders per peer.
static REGISTRY: Lazy<RwLock<HashMap<PeerId, UnboundedSender<BscCommand>>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));

/// Optional background task handle for EVN post-sync peer refresh.
static EVN_REFRESH_TASK: Lazy<RwLock<Option<JoinHandle<()>>>> =
    Lazy::new(|| RwLock::new(None));

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
        let local_best_td = crate::shared::get_best_total_difficulty_u128();
        let local_best_number = crate::shared::get_best_block_number().unwrap_or_default();
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
            let mut allow = is_evn(&peer);
            if !allow {
                if let Some(info) = peer_info_map.get(&peer) {
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

            if allow && tx.send(BscCommand::Votes(Arc::clone(&votes_arc))).is_err() {
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
