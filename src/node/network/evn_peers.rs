use alloy_primitives::keccak256;
use once_cell::sync::Lazy;
use reth_network_api::PeerId;
use std::collections::{HashMap, HashSet};
use std::sync::RwLock;

/// Why a peer is considered an EVN peer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EvnMarkReason {
    Whitelist,
    OnchainValidator,
}

#[derive(Clone, Debug, Default)]
pub struct EvnPeerInfo {
    pub is_evn: bool,
    pub reason: Option<EvnMarkReason>,
}

static EVN_PEERS: Lazy<RwLock<HashMap<PeerId, EvnPeerInfo>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));

/// Global on-chain NodeIDs set (normalized hex strings without 0x)
static ONCHAIN_NODEIDS: Lazy<RwLock<HashSet<String>>> =
    Lazy::new(|| RwLock::new(HashSet::new()));

pub fn peer_id_to_node_id(peer: PeerId) -> String {
    alloy_primitives::hex::encode(keccak256(peer.as_slice()))
}

/// Attempt to mark a peer as EVN by whitelist entries in the global EVN config.
pub fn mark_evn_if_whitelisted(peer: PeerId) {
    if let Some(cfg) = crate::node::network::evn::get_global_evn_config() {
        if !cfg.enabled { return; }
        if cfg.whitelist_nodeids.is_empty() { return; }

        // Compare peer's ID string with whitelist entries
        let node_id = peer_id_to_node_id(peer);
        let is_whitelisted = cfg.whitelist_nodeids.contains(&node_id);
        if is_whitelisted {
            if let Ok(mut map) = EVN_PEERS.write() {
                map.entry(peer).and_modify(|e| { e.is_evn = true; e.reason = Some(EvnMarkReason::Whitelist); })
                    .or_insert(EvnPeerInfo { is_evn: true, reason: Some(EvnMarkReason::Whitelist) });
            }
        }
    }
}

/// Mark a peer as EVN due to onchain validator mapping.
pub fn mark_evn_onchain(peer: PeerId) {
    if let Ok(mut map) = EVN_PEERS.write() {
        map.entry(peer).and_modify(|e| { e.is_evn = true; e.reason = Some(EvnMarkReason::OnchainValidator); })
            .or_insert(EvnPeerInfo { is_evn: true, reason: Some(EvnMarkReason::OnchainValidator) });
    }
}

/// Query whether a peer is currently marked EVN.
pub fn is_evn_peer(peer: PeerId) -> bool {
    if let Ok(map) = EVN_PEERS.read() { map.get(&peer).map(|i| i.is_evn).unwrap_or(false) } else { false }
}

/// Current EVN peer snapshot
pub fn snapshot() -> Vec<(PeerId, EvnPeerInfo)> {
    if let Ok(map) = EVN_PEERS.read() { map.iter().map(|(k,v)| (*k, v.clone())).collect() } else { Vec::new() }
}

/// Update the on-chain NodeIDs cache with the provided list
pub fn update_onchain_nodeids(ids: Vec<[u8; 32]>) {
    if let Ok(mut set) = ONCHAIN_NODEIDS.write() {
        for id in ids {
            let hex = alloy_primitives::hex::encode(id);
            set.insert(hex);
        }
    }
}

/// Get a snapshot of on-chain NodeIDs as normalized hex strings
pub fn get_onchain_nodeids_set() -> HashSet<String> {
    if let Ok(set) = ONCHAIN_NODEIDS.read() { set.clone() } else { HashSet::new() }
}
