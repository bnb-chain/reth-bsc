use reth_network_api::{PeerId, Direction};
use reth_network::protocol::{ConnectionHandler, OnNotSupported, ProtocolHandler};
use reth_eth_wire::{capability::SharedCapabilities, multiplex::ProtocolConnection, protocol::Protocol};
use std::net::SocketAddr;
use tokio::sync::mpsc;

use super::proto::{BscProtoMessage};
use crate::node::network::bsc_protocol::stream::{BscProtocolConnection};

#[derive(Clone, Debug, Default)]
pub struct BscProtocolHandler;

#[derive(Clone, Debug)]
pub struct BscConnectionHandler;

impl ProtocolHandler for BscProtocolHandler {
    type ConnectionHandler = BscConnectionHandler;

    fn on_incoming(&self, _socket_addr: SocketAddr) -> Option<Self::ConnectionHandler> {
        Some(BscConnectionHandler)
    }

    fn on_outgoing(&self, _socket_addr: SocketAddr, _peer_id: PeerId) -> Option<Self::ConnectionHandler> {
        Some(BscConnectionHandler)
    }
}

impl ConnectionHandler for BscConnectionHandler {
    type Connection = BscProtocolConnection;

    fn protocol(&self) -> Protocol { BscProtoMessage::protocol() }

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
        let (_tx, rx) = mpsc::unbounded_channel();
        BscProtocolConnection::new(conn, rx, direction.is_outgoing())
    }
}


