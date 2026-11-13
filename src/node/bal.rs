//! Optional block-level read hints. This is a separate `bsc-bal/1` capability: neither BSC
//! block encoding nor Parlia validity changes. Hints only select local database reads.

use alloy_eips::eip7928::{bal::DecodedBal, AccountChanges, BlockAccessList};
use alloy_primitives::{bytes::BytesMut, Bytes, B256};
use alloy_rlp::{Decodable, RlpDecodable, RlpEncodable};
use futures::{Stream, StreamExt};
use parking_lot::Mutex;
use reth_eth_wire::{
    capability::SharedCapabilities,
    multiplex::ProtocolConnection,
    protocol::{Protocol, ProtocolIngressLimits},
    Capability,
};
use reth_network::protocol::{ConnectionHandler, OnNotSupported, ProtocolHandler};
use reth_network_api::{Direction, PeerId};
use std::{
    collections::VecDeque,
    net::SocketAddr,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, OnceLock,
    },
    task::{Context, Poll},
    time::{Duration, Instant},
};
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;

const MAX_HINT_BYTES: usize = 1024 * 1024;
const MAX_CACHE_BYTES: usize = 16 * MAX_HINT_BYTES;
const MAX_CACHE_ENTRIES: usize = 32;
const TTL: Duration = Duration::from_secs(10);
static ENABLED: AtomicBool = AtomicBool::new(false);

/// Configure before node components are started. Disabled unless explicitly requested.
pub fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::Relaxed);
}

/// Whether the process opted in to exchanging and consuming advisory read hints.
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Drop every claimed value and transaction index before sending a locally collected BAL.
pub(crate) fn encode_read_hint(bal: BlockAccessList) -> Option<DecodedBal> {
    let hints: BlockAccessList = bal
        .into_iter()
        .map(|account| {
            let mut storage_reads = account.storage_slots().collect::<Vec<_>>();
            storage_reads.sort_unstable();
            storage_reads.dedup();
            AccountChanges { address: account.address, storage_reads, ..Default::default() }
        })
        .collect();
    let encoded = alloy_rlp::encode(&hints);
    (encoded.len() <= MAX_HINT_BYTES).then(|| DecodedBal::new(hints.into(), encoded.into()))
}

#[derive(Debug, Clone, RlpEncodable, RlpDecodable)]
struct HintPacket {
    hash: B256,
    parent: B256,
    number: u64,
    bal: Bytes,
}

#[derive(Default)]
struct HintCache {
    entries: VecDeque<(Instant, HintPacket)>,
    bytes: usize,
}

impl HintCache {
    fn prune(&mut self, now: Instant) {
        while let Some((_, packet)) =
            self.entries.pop_front_if(|(at, _)| now.duration_since(*at) >= TTL)
        {
            self.bytes -= packet.bal.len();
        }
    }

    fn insert(&mut self, packet: HintPacket, now: Instant) {
        self.prune(now);
        if packet.bal.len() > MAX_HINT_BYTES ||
            self.entries.iter().any(|(_, p)| p.hash == packet.hash)
        {
            return;
        }
        while self.entries.len() >= MAX_CACHE_ENTRIES ||
            self.bytes + packet.bal.len() > MAX_CACHE_BYTES
        {
            if let Some((_, packet)) = self.entries.pop_front() {
                self.bytes -= packet.bal.len();
            }
        }
        self.bytes += packet.bal.len();
        self.entries.push_back((now, packet));
    }

    fn take(&mut self, hash: B256, parent: B256, number: u64, now: Instant) -> Option<Bytes> {
        self.prune(now);
        let index = self.entries.iter().position(|(_, p)| p.hash == hash)?;
        let (_, packet) = self.entries.remove(index)?;
        self.bytes -= packet.bal.len();
        (packet.parent == parent && packet.number == number).then_some(packet.bal)
    }
}

fn cache() -> &'static Mutex<HintCache> {
    static CACHE: OnceLock<Mutex<HintCache>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

fn outgoing() -> &'static broadcast::Sender<Arc<HintPacket>> {
    static OUTGOING: OnceLock<broadcast::Sender<Arc<HintPacket>>> = OnceLock::new();
    OUTGOING.get_or_init(|| broadcast::channel(8).0)
}

/// No waiting for peers: a late or absent hint simply leaves normal prewarming in place.
pub(crate) fn take(hash: B256, parent: B256, number: u64) -> Option<Bytes> {
    if !enabled() {
        return None;
    }
    let result = cache().lock().take(hash, parent, number, Instant::now());
    metrics::counter!("bsc_bal_hint_lookup_total", "result" => if result.is_some() { "hit" } else { "miss" }).increment(1);
    result
}

/// Called only once the candidate has its final sealed block hash, before block diffusion.
pub(crate) fn publish(hash: B256, parent: B256, number: u64, bal: Option<Bytes>) {
    if !enabled() {
        return;
    }
    let Some(bal) = bal.filter(|bal| bal.len() <= MAX_HINT_BYTES) else {
        return;
    };
    let _ = outgoing().send(Arc::new(HintPacket { hash, parent, number, bal }));
}

fn decode_frame(frame: &[u8]) -> Option<HintPacket> {
    if frame.first() != Some(&0) || frame.len() > MAX_HINT_BYTES + 128 {
        return None;
    }
    let mut input = &frame[1..];
    let packet = HintPacket::decode(&mut input).ok()?;
    (input.is_empty() && packet.bal.len() <= MAX_HINT_BYTES).then_some(packet)
}

/// Optional capability; peers lacking it remain connected using normal BSC protocols.
#[derive(Clone, Debug)]
pub(crate) struct BalProtocolHandler;

impl ProtocolHandler for BalProtocolHandler {
    type ConnectionHandler = Self;
    fn on_incoming(&self, _: SocketAddr) -> Option<Self> {
        Some(self.clone())
    }
    fn on_outgoing(&self, _: SocketAddr, _: PeerId) -> Option<Self> {
        Some(self.clone())
    }
}

impl ConnectionHandler for BalProtocolHandler {
    type Connection = BalConnection;
    fn protocol(&self) -> Protocol {
        Protocol::new(Capability::new_static("bsc-bal", 1), 1)
    }
    fn inbound_limits(&self) -> ProtocolIngressLimits {
        ProtocolIngressLimits::new(MAX_HINT_BYTES + 128)
            .with_max_buffered_bytes(2 * (MAX_HINT_BYTES + 128))
            .with_max_buffered_messages(8)
    }
    fn on_unsupported_by_peer(
        self,
        _: &SharedCapabilities,
        _: Direction,
        _: PeerId,
    ) -> OnNotSupported {
        OnNotSupported::KeepAlive
    }
    fn into_connection(self, _: Direction, _: PeerId, conn: ProtocolConnection) -> BalConnection {
        BalConnection {
            conn,
            outgoing: BroadcastStream::new(outgoing().subscribe()),
            period: Instant::now(),
            received: 0,
        }
    }
}

pub(crate) struct BalConnection {
    conn: ProtocolConnection,
    outgoing: BroadcastStream<Arc<HintPacket>>,
    period: Instant,
    received: u32,
}

impl Stream for BalConnection {
    type Item = BytesMut;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        // A bounded amount of work per poll keeps hint traffic from starving block traffic.
        for _ in 0..8 {
            match this.outgoing.poll_next_unpin(cx) {
                Poll::Ready(Some(Ok(packet))) => {
                    let mut frame = BytesMut::from(&[0u8][..]);
                    alloy_rlp::Encodable::encode(packet.as_ref(), &mut frame);
                    metrics::counter!("bsc_bal_hints_sent_total").increment(1);
                    return Poll::Ready(Some(frame));
                }
                Poll::Ready(Some(Err(_))) => {
                    // Re-poll after lag to register a waker or drain the remaining hints.
                    metrics::counter!("bsc_bal_hints_lagged_total").increment(1);
                    continue;
                }
                _ => {}
            }
            match this.conn.poll_next_unpin(cx) {
                Poll::Ready(Some(frame)) => {
                    if this.period.elapsed() >= Duration::from_secs(1) {
                        this.period = Instant::now();
                        this.received = 0;
                    }
                    this.received = this.received.saturating_add(1);
                    if this.received > 8 {
                        continue;
                    }
                    if let Some(packet) = decode_frame(&frame) {
                        cache().lock().insert(packet, Instant::now());
                        metrics::counter!("bsc_bal_hints_received_total").increment(1);
                    }
                }
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        }
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        evm::transaction::BscTxEnv, hardforks::bsc::BscHardfork, node::evm::BscEvmFactory,
    };
    use alloy_eips::eip7928::{BalanceChange, BlockAccessIndex, SlotChanges, StorageChange};
    use alloy_primitives::{Address, TxKind, U256};
    use reth_evm::{Evm, EvmEnv, EvmFactory};
    use revm::{
        bytecode::Bytecode,
        context::{BlockEnv, CfgEnv, TxEnv},
        database::State,
        database_interface::{bal::BalState, DatabaseCommit},
        state::AccountInfo,
    };

    fn packet(id: u64, len: usize) -> HintPacket {
        HintPacket {
            hash: B256::from(U256::from(id)),
            parent: B256::ZERO,
            number: id,
            bal: vec![0; len].into(),
        }
    }

    #[test]
    fn cache_is_bounded_deduplicated_and_expires() {
        let mut cache = HintCache::default();
        let now = Instant::now();
        for i in 0..40 {
            cache.insert(packet(i, MAX_HINT_BYTES), now);
        }
        assert_eq!(cache.bytes, MAX_CACHE_BYTES);
        assert_eq!(cache.entries.len(), 16);
        cache.insert(packet(39, MAX_HINT_BYTES), now);
        assert_eq!(cache.bytes, MAX_CACHE_BYTES);
        cache.insert(packet(41, MAX_HINT_BYTES + 1), now);
        assert_eq!(cache.bytes, MAX_CACHE_BYTES);
        assert!(cache.take(packet(39, 0).hash, B256::ZERO, 39, now + TTL).is_none());
        assert_eq!(cache.bytes, 0);
        for i in 0..40 {
            cache.insert(packet(i, 0), now);
        }
        assert_eq!(cache.entries.len(), MAX_CACHE_ENTRIES);
    }

    #[test]
    fn cache_binds_hash_parent_and_number_and_consumes_once() {
        let mut cache = HintCache::default();
        let now = Instant::now();
        cache.insert(packet(1, 3), now);
        assert!(cache.take(packet(1, 0).hash, B256::repeat_byte(1), 1, now).is_none());
        cache.insert(packet(1, 3), now);
        assert!(cache.take(packet(1, 0).hash, B256::ZERO, 2, now).is_none());
        cache.insert(packet(1, 3), now);
        assert_eq!(cache.take(packet(1, 0).hash, B256::ZERO, 1, now).unwrap().len(), 3);
        assert!(cache.take(packet(1, 0).hash, B256::ZERO, 1, now).is_none());
        assert_eq!(cache.bytes, 0);
    }

    #[test]
    fn frame_rejects_oversize_unknown_and_trailing_data() {
        let mut frame = vec![0];
        frame.extend(alloy_rlp::encode(packet(1, 3)));
        assert_eq!(decode_frame(&frame).unwrap().number, 1);
        frame.push(0);
        assert!(decode_frame(&frame).is_none());
        assert!(decode_frame(&[1, 0xc0]).is_none());
        assert!(decode_frame(&vec![0; MAX_HINT_BYTES + 129]).is_none());
    }

    #[test]
    fn read_hint_keeps_slots_and_discards_claimed_values() {
        let account = AccountChanges {
            address: Address::repeat_byte(1),
            storage_reads: vec![U256::from(7)],
            storage_changes: vec![SlotChanges {
                slot: U256::from(3),
                changes: vec![StorageChange {
                    block_access_index: BlockAccessIndex::new(1),
                    new_value: U256::from(999),
                }],
            }],
            balance_changes: vec![BalanceChange {
                block_access_index: BlockAccessIndex::new(1),
                post_balance: U256::from(999),
            }],
            ..Default::default()
        };
        let hint = encode_read_hint(vec![account]).unwrap();
        let decoded = BlockAccessList::decode(&mut hint.as_raw().as_ref()).unwrap();
        assert_eq!(decoded[0].storage_reads, vec![U256::from(3), U256::from(7)]);
        assert!(decoded[0].storage_changes.is_empty());
        assert!(decoded[0].balance_changes.is_empty());
    }

    #[test]
    fn oversized_local_read_hint_is_not_published() {
        let account = AccountChanges {
            storage_reads: (0..35_000).map(|i| U256::MAX - U256::from(i)).collect(),
            ..Default::default()
        };
        assert!(encode_read_hint(vec![account]).is_none());
    }

    #[test]
    fn peers_without_hint_capability_remain_connected() {
        let limits = BalProtocolHandler.inbound_limits();
        assert_eq!(limits.max_frame_bytes(), Some(MAX_HINT_BYTES + 128));
        assert_eq!(limits.max_buffered_messages(), 8);
        assert_eq!(limits.max_buffered_bytes(), 2 * (MAX_HINT_BYTES + 128));
        let eth = Protocol::new(Capability::new_static("eth", 68), 17);
        let shared = SharedCapabilities::try_new(
            vec![eth, BalProtocolHandler.protocol()],
            vec![Capability::new_static("eth", 68)],
        )
        .unwrap();
        assert!(matches!(
            BalProtocolHandler.on_unsupported_by_peer(&shared, Direction::Incoming, PeerId::ZERO),
            OnNotSupported::KeepAlive
        ));
        assert_eq!(shared.iter_caps().count(), 1);
    }

    #[test]
    fn collecting_read_hints_preserves_bsc_execution_result_and_state() {
        let caller = Address::repeat_byte(1);
        let contract = Address::repeat_byte(2);
        let execute = |collect| {
            let mut state = State::builder().build();
            state.insert_account(
                caller,
                AccountInfo { balance: U256::from(1_000_000), ..Default::default() },
            );
            // Store 7 in slot 3.
            state.insert_account(
                contract,
                AccountInfo::default().with_code(Bytecode::new_raw(Bytes::from_static(&[
                    0x60, 0x07, 0x60, 0x03, 0x55, 0x00,
                ]))),
            );
            if collect {
                state.bal_state = BalState::new().with_bal_builder();
            }
            let env = EvmEnv::new(
                CfgEnv::new_with_spec(BscHardfork::Osaka).with_chain_id(56),
                BlockEnv { prevrandao: Some(B256::with_last_byte(1)), ..Default::default() }.into(),
            );
            let mut evm = BscEvmFactory.create_evm(&mut state, env);
            let tx = BscTxEnv::new(
                TxEnv::builder()
                    .caller(caller)
                    .chain_id(Some(56))
                    .gas_limit(100_000)
                    .gas_price(1)
                    .kind(TxKind::Call(contract))
                    .build()
                    .unwrap(),
            );
            let result = evm.transact(tx).unwrap();
            assert!(result.result.is_success());
            evm.db_mut().commit(result.state.clone());
            let hint = evm.db_mut().take_built_alloy_bal().and_then(encode_read_hint);
            (result, hint)
        };
        let (normal, absent) = execute(false);
        let (collected, hint) = execute(true);
        assert_eq!(normal.result, collected.result);
        assert_eq!(normal.state, collected.state);
        assert!(absent.is_none());
        let hint = hint.unwrap();
        let account = hint.as_bal().iter().find(|a| a.address == contract).unwrap();
        assert_eq!(account.storage_reads, vec![U256::from(3)]);
        assert!(account.storage_changes.is_empty());
    }
}
