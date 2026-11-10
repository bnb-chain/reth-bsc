//! Geth-aligned proactive transaction exclusion for locally known EVN peers.

use super::{evn_peers::is_evn_for_proactive_transactions, BscNetworkPrimitives};
use reth_network::transactions::{
    config::TransactionPropagationKind, PeerMetadata, TransactionPropagationPolicy,
};
use reth_network_api::PeerId;

/// Keeps the user's base policy and adds only the unsolicited EVN broadcast exclusion.
#[derive(Debug)]
pub(crate) struct BscTransactionPropagationPolicy {
    base: TransactionPropagationKind,
}

impl BscTransactionPropagationPolicy {
    pub(crate) fn new(base: TransactionPropagationKind) -> Self {
        metrics::counter!("network.tx_evn_filtered_peer_attempts_total").increment(0);
        Self { base }
    }
}

impl TransactionPropagationPolicy<BscNetworkPrimitives> for BscTransactionPropagationPolicy {
    fn can_propagate(&self, peer: &mut PeerMetadata<BscNetworkPrimitives>) -> bool {
        self.base.can_propagate(peer)
    }

    fn can_propagate_proactively(
        &self,
        peer_id: &PeerId,
        _peer: &mut PeerMetadata<BscNetworkPrimitives>,
    ) -> bool {
        if is_evn_for_proactive_transactions(*peer_id) {
            // One filtered peer/batch eligibility attempt, not a transaction or queued message.
            metrics::counter!("network.tx_evn_filtered_peer_attempts_total").increment(1);
            return false;
        }
        true
    }

    fn on_session_established(&mut self, peer: &mut PeerMetadata<BscNetworkPrimitives>) {
        self.base.on_session_established(peer);
    }

    fn on_session_closed(&mut self, peer: &mut PeerMetadata<BscNetworkPrimitives>) {
        self.base.on_session_closed(peer);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{B256, U256};
    use reth::args::DefaultNetworkArgs;
    use reth_eth_wire::EthVersion;
    use reth_network_api::{PeerKind, PeerRequestSender};
    use std::sync::Arc;
    use tokio::sync::mpsc;

    #[test]
    fn base_policy_preserves_explicit_request_permissions() {
        let peer_id = PeerId::repeat_byte(9);
        for (kind, policy, expected) in [
            (PeerKind::Basic, TransactionPropagationKind::All, true),
            (PeerKind::Basic, TransactionPropagationKind::None, false),
            (PeerKind::Basic, TransactionPropagationKind::Trusted, false),
            (PeerKind::Trusted, TransactionPropagationKind::Trusted, true),
        ] {
            let (tx, _rx) = mpsc::channel(1);
            let mut peer = PeerMetadata::new(
                PeerRequestSender::new(peer_id, tx),
                EthVersion::Eth68,
                Arc::from(""),
                16,
                kind,
            );
            let mut policy = BscTransactionPropagationPolicy::new(policy);
            policy.on_session_established(&mut peer);
            assert_eq!(policy.can_propagate(&mut peer), expected);
            policy.on_session_closed(&mut peer);
        }
    }

    #[test]
    fn bsc_history_prevents_source_echo_after_receive_to_pending_delay() {
        // Replay the real per-peer cache operations used by TransactionsManager: receive
        // records each hash, then pending propagation inserts each hash to test peer knowledge.
        // This counts propagation candidates, not bytes admitted to the transport.
        let old_history = DefaultNetworkArgs::default().max_seen_tx_history;
        let bsc_history = crate::node::network::bsc_network_args().max_seen_tx_history;
        assert_eq!(old_history, 320);
        assert_eq!(bsc_history, 32768);

        for (history, batch, expected_echoes) in [
            (old_history, 320, 0),
            // The upstream LRU reserves one extra entry internally.
            (old_history, 321, 0),
            (old_history, 322, 322),
            (old_history, 1024, 1024),
            (bsc_history, 322, 0),
            (bsc_history, 1024, 0),
        ] {
            let peer_id = PeerId::repeat_byte(9);
            let (tx, _rx) = mpsc::channel(1);
            let mut peer = PeerMetadata::<BscNetworkPrimitives>::new(
                PeerRequestSender::new(peer_id, tx),
                EthVersion::Eth68,
                Arc::from(""),
                history,
                PeerKind::Basic,
            );
            let hashes: Vec<_> = (0..batch).map(|index| B256::from(U256::from(index))).collect();
            peer.seen_transactions_mut().extend(hashes.iter().copied());
            let echoes =
                hashes.iter().filter(|hash| peer.seen_transactions_mut().insert(**hash)).count();
            assert_eq!(echoes, expected_echoes, "history={history}, batch={batch}");
        }
    }
}
