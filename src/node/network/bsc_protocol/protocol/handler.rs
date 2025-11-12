use reth_network_api::{PeerId, Direction};
use reth_network::protocol::{ConnectionHandler, OnNotSupported, ProtocolHandler};
use reth_eth_wire::{capability::SharedCapabilities, multiplex::ProtocolConnection, protocol::Protocol};
use std::net::SocketAddr;
use tokio::sync::mpsc;

use super::proto::BscProtoMessage;
use crate::node::network::bsc_protocol::stream::{BscProtocolConnection};
use crate::node::network::bsc_protocol::registry;
use reth_network::Peers;

#[derive(Clone, Debug, Default)]
pub struct BscProtocolHandlerV2;

#[derive(Clone, Debug)]
pub struct BscConnectionHandlerV2;

impl ProtocolHandler for BscProtocolHandlerV2 {
    type ConnectionHandler = BscConnectionHandlerV2;

    fn on_incoming(&self, _socket_addr: SocketAddr) -> Option<Self::ConnectionHandler> { Some(BscConnectionHandlerV2) }

    fn on_outgoing(&self, _socket_addr: SocketAddr, _peer_id: PeerId) -> Option<Self::ConnectionHandler> { Some(BscConnectionHandlerV2) }
}

impl ConnectionHandler for BscConnectionHandlerV2 {
    type Connection = BscProtocolConnection;

    fn protocol(&self) -> Protocol { BscProtoMessage::protocol_for(2) }

    fn on_unsupported_by_peer(
        self,
        _supported: &SharedCapabilities,
        _direction: reth_network_api::Direction,
        _peer_id: PeerId,
    ) -> OnNotSupported {
        tracing::debug!(target: "bsc_protocol", "Unsupported by peer, direction: {}, peer_id: {}", _direction, _peer_id);
        OnNotSupported::KeepAlive
    }

    fn into_connection(
        self,
        direction: Direction,
        _peer_id: PeerId,
        conn: ProtocolConnection,
    ) -> Self::Connection {
        tracing::debug!(target: "bsc_protocol", "Into connection, direction: {}, peer_id: {}", direction, _peer_id);
        let (tx, rx) = mpsc::unbounded_channel();
        // Save sender so other components can broadcast BSC messages
        // Note: PeerId is not exposed directly here, so we rely on the local peer id for keying
        // when available. However, reth passes `_peer_id` which we can use.
        // Even if the connection drops, failed sends will lazily clean up entries.
        registry::register_peer(_peer_id, tx);
        // EVN: mark this peer if present in whitelist and mark as trusted at runtime
        crate::node::network::evn_peers::mark_evn_if_whitelisted(_peer_id);
        if crate::node::network::evn_peers::is_evn_peer(_peer_id) {
            if let Some(net) = crate::shared::get_network_handle() {
                net.add_trusted_peer_id(_peer_id);
            }
        }
        // Ensure EVN refresh listener is running to handle post-sync EVN updates
        // for existing peers.
        crate::node::network::bsc_protocol::registry::spawn_evn_refresh_listener();
        BscProtocolConnection::new(conn, rx, direction.is_outgoing(), 2, Some(_peer_id))
    }
}

#[derive(Clone, Debug, Default)]
pub struct BscProtocolHandlerV1;

#[derive(Clone, Debug)]
pub struct BscConnectionHandlerV1;

impl ProtocolHandler for BscProtocolHandlerV1 {
    type ConnectionHandler = BscConnectionHandlerV1;

    fn on_incoming(&self, _socket_addr: SocketAddr) -> Option<Self::ConnectionHandler> {
        Some(BscConnectionHandlerV1)
    }

    fn on_outgoing(&self, _socket_addr: SocketAddr, _peer_id: PeerId) -> Option<Self::ConnectionHandler> {
        Some(BscConnectionHandlerV1)
    }
}

impl ConnectionHandler for BscConnectionHandlerV1 {
    type Connection = BscProtocolConnection;

    fn protocol(&self) -> Protocol { BscProtoMessage::protocol_for(1) }

    fn on_unsupported_by_peer(
        self,
        _supported: &SharedCapabilities,
        _direction: reth_network_api::Direction,
        _peer_id: PeerId,
    ) -> OnNotSupported {
        OnNotSupported::KeepAlive
    }

    fn into_connection(
        self,
        direction: Direction,
        _peer_id: PeerId,
        conn: ProtocolConnection,
    ) -> Self::Connection {
        let (tx, rx) = mpsc::unbounded_channel();
        registry::register_peer(_peer_id, tx);
        crate::node::network::evn_peers::mark_evn_if_whitelisted(_peer_id);
        if crate::node::network::evn_peers::is_evn_peer(_peer_id) {
            if let Some(net) = crate::shared::get_network_handle() {
                net.add_trusted_peer_id(_peer_id);
            }
        }
        crate::node::network::bsc_protocol::registry::spawn_evn_refresh_listener();
        BscProtocolConnection::new(conn, rx, direction.is_outgoing(), 1, Some(_peer_id))
    }
}
