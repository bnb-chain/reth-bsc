use super::*;
use alloy_chains::Chain;
use alloy_primitives::{hex, B256, U256};
use bytes::{Bytes, BytesMut};
use futures::{Sink, Stream};
use reth_eth_wire::{
    errors::P2PStreamError, CanDisconnect, EthMessage, EthNetworkPrimitives, ProtocolMessage,
};
use reth_ethereum_forks::{ForkHash, ForkId, Head};
use std::{
    collections::VecDeque,
    task::{Context, Poll},
};

#[derive(Default)]
struct PeerStream {
    incoming: VecDeque<BytesMut>,
    outgoing: Vec<Bytes>,
    disconnected: Option<DisconnectReason>,
    pending: bool,
    flushes: usize,
}

impl PeerStream {
    fn with_message(message: impl AsRef<[u8]>) -> Self {
        Self { incoming: VecDeque::from([BytesMut::from(message.as_ref())]), ..Default::default() }
    }
}

impl Stream for PeerStream {
    type Item = Result<BytesMut, P2PStreamError>;

    fn poll_next(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.pending {
            Poll::Pending
        } else {
            Poll::Ready(self.incoming.pop_front().map(Ok))
        }
    }
}

impl Sink<Bytes> for PeerStream {
    type Error = P2PStreamError;

    fn poll_ready(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn start_send(mut self: Pin<&mut Self>, item: Bytes) -> Result<(), Self::Error> {
        self.outgoing.push(item);
        Ok(())
    }

    fn poll_flush(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.flushes += 1;
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
}

impl CanDisconnect<Bytes> for PeerStream {
    fn disconnect(
        &mut self,
        reason: DisconnectReason,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + '_>> {
        self.disconnected = Some(reason);
        Box::pin(async { Ok(()) })
    }
}

fn local_status(version: EthVersion) -> (UnifiedStatus, ForkFilter) {
    let genesis = B256::repeat_byte(0x11);
    let filter = ForkFilter::new(Head::default(), genesis, 0, Vec::new());
    let status = UnifiedStatus {
        version,
        chain: Chain::from_id(56),
        genesis,
        forkid: filter.current(),
        blockhash: B256::repeat_byte(0x22),
        total_difficulty: Some(U256::from(42)),
        earliest_block: Some(1),
        latest_block: Some(2),
    };
    (status, filter)
}

#[test]
fn status_matches_go_bsc_field_order() {
    // StatusPacket{70, 56, 42, 0x11..11, {0xdeadbeef, 0}, 1, 2, 0x22..22}.
    // The first byte is the raw eth message ID; the packet itself is an RLP list.
    let encoded = hex!(
        "00f84e46382aa01111111111111111111111111111111111111111111111111111111111111111
         c684deadbeef800102a02222222222222222222222222222222222222222222222222222222222222222"
    );
    let (mut status, _) = local_status(EthVersion::Eth70);
    status.forkid = ForkId { hash: ForkHash([0xde, 0xad, 0xbe, 0xef]), next: 0 };
    assert_eq!(BscStatus::from(status).encode_message().as_ref(), encoded);
    assert_eq!(UnifiedStatus::from(BscStatus::decode_message(&encoded).unwrap()), status);
}

#[tokio::test]
async fn eth70_and_eth71_preserve_td_and_do_not_exchange_upgrade_status() {
    for version in [EthVersion::Eth70, EthVersion::Eth71] {
        let (local, filter) = local_status(version);
        let remote = UnifiedStatus {
            total_difficulty: Some(U256::from(84)),
            latest_block: Some(3),
            blockhash: B256::repeat_byte(0x33),
            ..local
        };
        let mut peer = PeerStream::with_message(BscStatus::from(remote).encode_message());
        let next_message = BytesMut::from(&[0x01, 0xc0][..]);
        peer.incoming.push_back(next_message.clone());

        let negotiated =
            BscHandshake.handshake(&mut peer, local, filter, Duration::from_secs(1)).await.unwrap();

        assert_eq!(negotiated, remote);
        assert_eq!(peer.outgoing, [BscStatus::from(local).encode_message()]);
        assert_eq!(peer.flushes, 1);
        assert_eq!(peer.incoming, [next_message]);
        assert_eq!(peer.disconnected, None);
    }
}

#[tokio::test]
async fn rejects_invalid_bsc_status_fields() {
    for version in [EthVersion::Eth70, EthVersion::Eth71] {
        for case in 0..7 {
            let (local, filter) = local_status(version);
            let mut remote = local;
            match case {
                0 => remote.genesis = B256::ZERO,
                1 => remote.version = EthVersion::Eth68,
                2 => remote.chain = Chain::from_id(97),
                3 => remote.total_difficulty = Some(U256::from(1) << 100),
                4 => remote.forkid.hash = ForkHash([0xff; 4]),
                5 => remote.earliest_block = Some(3),
                6 => remote.blockhash = B256::ZERO,
                _ => unreachable!(),
            }
            let mut peer = PeerStream::with_message(BscStatus::from(remote).encode_message());
            let error = BscHandshake
                .handshake(&mut peer, local, filter, Duration::from_secs(1))
                .await
                .unwrap_err();
            let EthStreamError::EthHandshakeError(error) = error else {
                panic!("expected handshake error for case {case}: {error:?}");
            };
            match case {
                0 => assert!(matches!(error, EthHandshakeError::MismatchedGenesis(_))),
                1 => assert!(matches!(error, EthHandshakeError::MismatchedProtocolVersion(_))),
                2 => assert!(matches!(error, EthHandshakeError::MismatchedChain(_))),
                3 => assert!(matches!(
                    error,
                    EthHandshakeError::TotalDifficultyBitLenTooLarge { got: 101, maximum: 100 }
                )),
                4 => assert!(matches!(error, EthHandshakeError::InvalidFork(_))),
                5 => assert!(matches!(
                    error,
                    EthHandshakeError::EarliestBlockGreaterThanLatestBlock { got: 3, latest: 2 }
                )),
                6 => assert!(matches!(error, EthHandshakeError::BlockhashZero)),
                _ => unreachable!(),
            }
            assert_eq!(peer.disconnected, Some(DisconnectReason::ProtocolBreach));
        }
    }
}

#[tokio::test]
async fn rejects_non_status_malformed_and_ethereum_status_packets() {
    let (local, _) = local_status(EthVersion::Eth70);
    let ethereum = alloy_rlp::encode(ProtocolMessage::<EthNetworkPrimitives>::from(
        EthMessage::Status(local.into_message()),
    ));
    let mut trailing = BscStatus::from(local).encode_message().to_vec();
    trailing.push(0);
    for invalid in [vec![], vec![0x01, 0xc0], vec![0x00, 0xc0], ethereum, trailing] {
        let (_, filter) = local_status(EthVersion::Eth70);
        let mut peer = PeerStream::with_message(invalid);
        let error = BscHandshake
            .handshake(&mut peer, local, filter, Duration::from_secs(1))
            .await
            .unwrap_err();
        assert!(matches!(error, EthStreamError::InvalidMessage(_)));
        assert_eq!(peer.disconnected, Some(DisconnectReason::ProtocolBreach));
    }
}

#[tokio::test]
async fn rejects_oversized_status_before_decoding() {
    let (local, filter) = local_status(EthVersion::Eth70);
    let mut peer = PeerStream::with_message(vec![0; MAX_MESSAGE_SIZE + 1]);
    let error =
        BscHandshake.handshake(&mut peer, local, filter, Duration::from_secs(1)).await.unwrap_err();
    assert!(matches!(error, EthStreamError::MessageTooBig(size) if size == MAX_MESSAGE_SIZE + 1));
    assert_eq!(peer.disconnected, Some(DisconnectReason::ProtocolBreach));
}

#[tokio::test]
async fn handles_closed_and_unresponsive_peers() {
    let (local, filter) = local_status(EthVersion::Eth71);
    let mut peer = PeerStream::default();
    let error =
        BscHandshake.handshake(&mut peer, local, filter, Duration::from_secs(1)).await.unwrap_err();
    assert!(matches!(error, EthStreamError::EthHandshakeError(EthHandshakeError::NoResponse)));
    assert_eq!(peer.disconnected, Some(DisconnectReason::DisconnectRequested));

    let (_, filter) = local_status(EthVersion::Eth71);
    let mut peer = PeerStream { pending: true, ..Default::default() };
    let error = BscHandshake
        .handshake(&mut peer, local, filter, Duration::from_millis(10))
        .await
        .unwrap_err();
    assert!(matches!(error, EthStreamError::StreamTimeout));
}

#[tokio::test]
async fn legacy_eth68_still_exchanges_upgrade_status() {
    let (local, filter) = local_status(EthVersion::Eth68);
    let status = alloy_rlp::encode(ProtocolMessage::<EthNetworkPrimitives>::from(
        EthMessage::Status(local.into_message()),
    ));
    let mut peer = PeerStream::with_message(status.clone());
    peer.incoming.push_back(BytesMut::from(&[0x0b, 0xc2, 0xc1, 0x80][..]));
    let negotiated =
        BscHandshake.handshake(&mut peer, local, filter, Duration::from_secs(1)).await.unwrap();
    assert_eq!(negotiated.total_difficulty, local.total_difficulty);
    assert_eq!(peer.outgoing.len(), 2);
    assert_eq!(peer.outgoing[0].as_ref(), status);
    assert_eq!(peer.outgoing[1][0], 0x0b);
}
