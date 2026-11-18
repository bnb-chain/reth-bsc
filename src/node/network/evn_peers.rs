use alloy_primitives::keccak256;
use once_cell::sync::Lazy;
use reth_network_api::PeerId;
use std::{
    collections::{HashMap, HashSet},
    sync::RwLock,
};

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
static ONCHAIN_NODEIDS: Lazy<RwLock<HashSet<String>>> = Lazy::new(|| RwLock::new(HashSet::new()));

/// Latest advisory pre-execution membership, used only for proactive transaction filtering.
/// Unlike the legacy union above, this snapshot removes departed validators. It is not a
/// canonical/reorg-consistent validator registry and must not drive votes or trust decisions.
static TX_ONCHAIN_NODEIDS: Lazy<RwLock<HashSet<[u8; 32]>>> =
    Lazy::new(|| RwLock::new(HashSet::new()));

pub fn peer_id_to_node_id(peer: PeerId) -> String {
    alloy_primitives::hex::encode(keccak256(peer.as_slice()))
}

/// Attempt to mark a peer as EVN by whitelist entries in the global EVN config.
pub fn mark_evn_if_whitelisted(peer: PeerId) {
    if let Some(cfg) = crate::node::network::evn::get_global_evn_config() {
        if !cfg.enabled {
            return;
        }
        if cfg.whitelist_nodeids.is_empty() {
            return;
        }

        // Compare peer's ID string with whitelist entries
        let node_id = peer_id_to_node_id(peer);
        let is_whitelisted = cfg.whitelist_nodeids.contains(&node_id);
        if is_whitelisted {
            if let Ok(mut map) = EVN_PEERS.write() {
                map.entry(peer)
                    .and_modify(|e| {
                        e.is_evn = true;
                        e.reason = Some(EvnMarkReason::Whitelist);
                    })
                    .or_insert(EvnPeerInfo {
                        is_evn: true,
                        reason: Some(EvnMarkReason::Whitelist),
                    });
            }
        }
    }
}

/// Mark a peer as EVN due to onchain validator mapping.
pub fn mark_evn_onchain(peer: PeerId) {
    if let Ok(mut map) = EVN_PEERS.write() {
        map.entry(peer)
            .and_modify(|e| {
                e.is_evn = true;
                e.reason = Some(EvnMarkReason::OnchainValidator);
            })
            .or_insert(EvnPeerInfo { is_evn: true, reason: Some(EvnMarkReason::OnchainValidator) });
    }
}

/// Query whether a peer is currently marked EVN.
pub fn is_evn_peer(peer: PeerId) -> bool {
    if let Ok(map) = EVN_PEERS.read() {
        map.get(&peer).map(|i| i.is_evn).unwrap_or(false)
    } else {
        false
    }
}

/// Current EVN peer snapshot
pub fn snapshot() -> Vec<(PeerId, EvnPeerInfo)> {
    if let Ok(map) = EVN_PEERS.read() {
        map.iter().map(|(k, v)| (*k, v.clone())).collect()
    } else {
        Vec::new()
    }
}

/// Update the on-chain NodeIDs cache with the provided list
pub fn update_onchain_nodeids(ids: Vec<[u8; 32]>) {
    if let Ok(mut set) = ONCHAIN_NODEIDS.write() {
        for id in &ids {
            let hex = alloy_primitives::hex::encode(id);
            set.insert(hex);
        }
    }
    replace_tx_nodeids(&TX_ONCHAIN_NODEIDS, &ids);
}

/// Get a snapshot of on-chain NodeIDs as normalized hex strings
pub fn get_onchain_nodeids_set() -> HashSet<String> {
    if let Ok(set) = ONCHAIN_NODEIDS.read() {
        set.clone()
    } else {
        HashSet::new()
    }
}

fn replace_tx_nodeids(cache: &RwLock<HashSet<[u8; 32]>>, ids: &[[u8; 32]]) {
    let snapshot = ids.iter().copied().collect();
    if let Ok(mut current) = cache.write() {
        *current = snapshot;
    }
}

/// Evaluates current configuration/membership, not the insert-only peer marks. This also works
/// before the BSC subprotocol has registered the session and after membership removal/reconnect.
pub(crate) fn is_evn_for_proactive_transactions(peer: PeerId) -> bool {
    let cfg = super::evn::get_global_evn_config();
    let whitelist = cfg.map_or(&[][..], |cfg| cfg.whitelist_nodeids.as_slice());
    is_evn_for_transactions(peer, super::evn::is_evn_enabled(), whitelist, &TX_ONCHAIN_NODEIDS)
}

fn is_evn_for_transactions(
    peer: PeerId,
    enabled: bool,
    whitelist: &[String],
    current: &RwLock<HashSet<[u8; 32]>>,
) -> bool {
    if !enabled {
        return false;
    }
    let node_id = keccak256(peer.as_slice()).0;
    if is_whitelisted_node_id(&node_id, whitelist) {
        return true;
    }
    current.read().is_ok_and(|current| current.contains(&node_id))
}

fn is_whitelisted_node_id(node_id: &[u8; 32], whitelist: &[String]) -> bool {
    !whitelist.is_empty() && whitelist.contains(&alloy_primitives::hex::encode(node_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transaction_membership_snapshot_removes_departed_validators() {
        let current = RwLock::new(HashSet::new());
        let old = keccak256(PeerId::repeat_byte(1)).0;
        let new = keccak256(PeerId::repeat_byte(2)).0;
        replace_tx_nodeids(&current, &[old, old]);
        assert_eq!(current.read().unwrap().len(), 1);
        replace_tx_nodeids(&current, &[new]);
        assert!(!current.read().unwrap().contains(&old));
        assert!(current.read().unwrap().contains(&new));
        replace_tx_nodeids(&current, &[]);
        assert!(current.read().unwrap().is_empty());
    }

    #[test]
    fn transaction_whitelist_uses_the_existing_node_id_encoding() {
        let peer = PeerId::repeat_byte(3);
        let node_id = keccak256(peer.as_slice()).0;
        let whitelist = vec![peer_id_to_node_id(peer)];
        assert!(is_whitelisted_node_id(&node_id, &whitelist));
        assert!(!is_whitelisted_node_id(&keccak256(PeerId::repeat_byte(4)).0, &whitelist));
        assert!(!is_whitelisted_node_id(&node_id, &[]));
    }

    #[test]
    fn proactive_membership_checks_config_and_current_snapshot_without_session_marks() {
        let peer = PeerId::repeat_byte(5);
        let ordinary = PeerId::repeat_byte(6);
        let current = RwLock::new(HashSet::new());
        let whitelist = vec![peer_id_to_node_id(peer)];
        assert!(!is_evn_for_transactions(peer, false, &whitelist, &current));
        assert!(
            is_evn_for_transactions(peer, true, &whitelist, &current),
            "whitelist applies before subprotocol session registration"
        );
        assert!(!is_evn_for_transactions(ordinary, true, &whitelist, &current));
        replace_tx_nodeids(&current, &[keccak256(peer.as_slice()).0]);
        assert!(is_evn_for_transactions(peer, true, &[], &current));
        assert!(!is_evn_for_transactions(peer, false, &[], &current));
        replace_tx_nodeids(&current, &[keccak256(ordinary.as_slice()).0]);
        assert!(
            !is_evn_for_transactions(peer, true, &[], &current),
            "departed peer is eligible immediately and after reconnect"
        );
        assert!(is_evn_for_transactions(ordinary, true, &[], &current));
        assert!(
            is_evn_for_transactions(peer, true, &whitelist, &current),
            "static whitelist remains independent of dynamic membership"
        );
    }
}
