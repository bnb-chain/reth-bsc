//! Local wire-level reproductions against the pinned network dependency; no remote nodes.

use super::*;
use alloy_primitives::{map::B256Set, B256, U128};
use futures::future::poll_fn;
use reth_eth_wire::{EthNetworkPrimitives, EthVersion};
use reth_ethereum_primitives::Block;
use reth_metrics::common::mpsc::{memory_bounded_channel, MemoryBoundedReceiver};
use reth_network::{
    import::{BlockImport, BlockImportEvent, NewBlockEvent},
    message::{NewBlockMessage, PeerMessage},
    transactions::{NetworkTransactionEvent, TransactionsManager, TransactionsManagerConfig},
    NetworkConfigBuilder,
};
use reth_network_api::{PeerId, Peers};
use reth_network_p2p::sync::{NetworkSyncUpdater, SyncState};
use reth_primitives_traits::SignedTransaction;
use reth_provider::test_utils::MockEthProvider;
use reth_tasks::Runtime;
use reth_transaction_pool::{
    blobstore::InMemoryBlobStore,
    test_utils::{OkValidator, TransactionBuilder},
    CoinbaseTipOrdering, EthPooledTransaction, Pool, PoolTransaction, TransactionPool,
};
use secp256k1::SecretKey;
use std::{
    future::Future,
    pin::Pin,
    sync::Mutex,
    task::{Context, Poll},
};

type LocalPool = Pool<
    OkValidator<EthPooledTransaction>,
    CoinbaseTipOrdering<EthPooledTransaction>,
    InMemoryBlobStore,
>;

#[derive(Debug)]
struct BlockSpy(Arc<Mutex<Vec<NewBlockEvent>>>);

impl BlockImport for BlockSpy {
    fn on_new_block(&mut self, _: PeerId, event: NewBlockEvent) {
        self.0.lock().unwrap().push(event);
    }

    fn poll(&mut self, _: &mut Context<'_>) -> Poll<BlockImportEvent<NewBlock>> {
        Poll::Pending
    }
}

struct LocalWire {
    a: NetworkManager<EthNetworkPrimitives>,
    b: NetworkManager<EthNetworkPrimitives>,
    a_txs: MemoryBoundedReceiver<NetworkTransactionEvent>,
    b_txs: MemoryBoundedReceiver<NetworkTransactionEvent>,
    b_blocks: Arc<Mutex<Vec<NewBlockEvent>>>,
}

impl LocalWire {
    async fn new() -> Self {
        let mut nodes = Vec::new();
        let mut receivers = Vec::new();
        let b_blocks = Arc::new(Mutex::new(Vec::new()));
        for id in [1, 2] {
            let builder = NetworkConfigBuilder::<EthNetworkPrimitives>::new(
                SecretKey::from_slice(&[id; 32]).unwrap(),
                Runtime::test(),
            )
            .disable_discovery()
            .listener_addr("127.0.0.1:0".parse().unwrap())
            .peer_config(PeersConfig::test())
            .with_pow();
            let hello = HelloMessageWithProtocols::builder(builder.get_peer_id())
                .protocols([EthVersion::Eth68.into()])
                .build();
            let config = builder
                .hello_message(hello)
                .block_import(Box::new(BlockSpy(b_blocks.clone())))
                .build(MockEthProvider::default());
            let mut node = NetworkManager::new(config).await.unwrap();
            let (tx, rx) = memory_bounded_channel(16 * 1024 * 1024, "bsc_local_wire_test");
            node.set_transactions(tx);
            node.handle().update_sync_state(SyncState::Idle);
            nodes.push(node);
            receivers.push(rx);
        }
        let b = nodes.pop().unwrap();
        let a = nodes.pop().unwrap();
        let b_txs = receivers.pop().unwrap();
        let a_txs = receivers.pop().unwrap();
        Self { a, b, a_txs, b_txs, b_blocks }
    }

    async fn drive<T>(
        &mut self,
        mut condition: impl FnMut(&mut Self, &mut Context<'_>) -> Poll<T>,
    ) -> T {
        tokio::time::timeout(
            Duration::from_secs(15),
            poll_fn(|cx| {
                assert!(Pin::new(&mut self.a).poll(cx).is_pending());
                assert!(Pin::new(&mut self.b).poll(cx).is_pending());
                condition(self, cx)
            }),
        )
        .await
        .expect("local wire operation must finish")
    }

    async fn connect(&mut self) {
        self.a.handle().add_peer(*self.b.peer_id(), self.b.local_addr());
        self.drive(|wire, _| {
            if wire.a.handle().num_connected_peers() == 1 &&
                wire.b.handle().num_connected_peers() == 1
            {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;
    }
    fn full_count(&self, hash: B256) -> usize {
        self.b_blocks
            .lock()
            .unwrap()
            .iter()
            .filter(|event| matches!(event, NewBlockEvent::Block(block) if block.hash == hash))
            .count()
    }
}

fn block(number: u64) -> NewBlockMessage<NewBlock> {
    let mut block = Block::default();
    block.header.number = number;
    NewBlockMessage {
        hash: block.header.hash_slow(),
        block: Arc::new(NewBlock { block, td: U128::ZERO }),
    }
}

#[tokio::test]
async fn ordinary_and_evn_share_dedup_in_both_entry_orders() {
    for direct_first in [false, true] {
        let mut wire = LocalWire::new().await;
        wire.connect().await;
        let peer = *wire.b.peer_id();
        let block = block(42);
        if direct_first {
            assert_eq!(
                wire.a.handle().try_send_block_announcement_once(
                    peer,
                    PeerMessage::NewBlock(block.clone()),
                    None,
                ),
                Ok(())
            );
        } else {
            wire.a.handle().announce_block((*block.block).clone(), block.hash);
        }
        wire.drive(
            |wire, _| {
                if wire.full_count(block.hash) == 1 {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            },
        )
        .await;
        // Repeat both production routes, including EVN early/completion forwarding.
        for _ in 0..2 {
            assert_eq!(
                wire.a.handle().try_send_block_announcement_once(
                    peer,
                    PeerMessage::NewBlock(block.clone()),
                    None,
                ),
                Ok(())
            );
            wire.a.handle().announce_block((*block.block).clone(), block.hash);
        }
        let handle = wire.a.handle().clone();
        let mut barrier = Box::pin(handle.get_peer_by_id(peer));
        wire.drive(|_, cx| barrier.as_mut().poll(cx)).await.unwrap().unwrap();
        let marker = self::block(99);
        assert_eq!(
            wire.a.handle().try_send_block_announcement(
                peer,
                PeerMessage::NewBlock(marker.clone()),
                None,
            ),
            Ok(())
        );
        wire.drive(
            |wire, _| {
                if wire.full_count(marker.hash) == 1 {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            },
        )
        .await;
        assert_eq!(wire.full_count(block.hash), 1);
        println!("WIRE_BLOCK direct_first={direct_first} same_peer_same_hash_received=1");
    }
}

#[tokio::test]
async fn manager_acceptance_can_hide_session_rejection_but_ordinary_retry_works() {
    let mut wire = LocalWire::new().await;
    wire.connect().await;
    let peer = *wire.b.peer_id();
    let victim = block(100);
    // A single-thread runtime cannot advance session tasks until this test yields. Fill the
    // 16-item session quota, then dequeue a victim accepted by the 64-item manager ingress.
    for number in 1..=16 {
        assert_eq!(
            wire.a.handle().try_send_block_announcement(
                peer,
                PeerMessage::NewBlock(block(number)),
                None,
            ),
            Ok(())
        );
    }
    assert_eq!(
        wire.a.handle().try_send_block_announcement_once(
            peer,
            PeerMessage::NewBlock(victim.clone()),
            None,
        ),
        Ok(())
    );
    let mut cx = Context::from_waker(futures::task::noop_waker_ref());
    assert!(Pin::new(&mut wire.a).poll(&mut cx).is_pending());
    assert!(Pin::new(&mut wire.a).poll(&mut cx).is_pending());
    wire.drive(|wire, _| {
        if wire.b_blocks.lock().unwrap().len() == 16 {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
    .await;
    assert_eq!(wire.full_count(victim.hash), 0);
    wire.a.handle().announce_block((*victim.block).clone(), victim.hash);
    wire.drive(
        |wire, _| {
            if wire.full_count(victim.hash) == 1 {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        },
    )
    .await;
    println!("WIRE_ADMISSION manager_ok=true initial_delivery=0 ordinary_retry_delivery=1");
}

#[tokio::test]
async fn forced_head_hash_replay_reaches_wire_even_when_peer_already_knows_block() {
    let mut wire = LocalWire::new().await;
    wire.connect().await;
    let peer = *wire.b.peer_id();
    let block = block(55);
    wire.a.handle().announce_block((*block.block).clone(), block.hash);
    wire.drive(
        |wire, _| {
            if wire.full_count(block.hash) == 1 {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        },
    )
    .await;
    for expected in [1, 2] {
        let hashes = reth_eth_wire::NewBlockHashes(vec![reth_eth_wire::BlockHashNumber {
            hash: block.hash,
            number: 55,
        }]);
        assert_eq!(
            wire.a.handle().try_send_block_announcement(
                peer,
                PeerMessage::NewBlockHashes(hashes),
                None,
            ),
            Ok(())
        );
        wire.drive(|wire, _| {
            let count = wire
                .b_blocks
                .lock()
                .unwrap()
                .iter()
                .filter(|event| matches!(event, NewBlockEvent::Hashes(_)))
                .count();
            if count == expected {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;
    }
    println!("WIRE_HEAD known_full_block=1 repeated_same_hash_announcements=2");
}

fn pool(propagate: bool) -> LocalPool {
    Pool::new(
        OkValidator::default().set_propagate_transactions(propagate),
        CoinbaseTipOrdering::default(),
        InMemoryBlobStore::default(),
        Default::default(),
    )
}

#[tokio::test]
async fn targeted_hash_dedup_filters_known_items_but_forced_marker_is_delivered() {
    let mut wire = LocalWire::new().await;
    wire.connect().await;
    let peer = *wire.b.peer_id();
    let known = block(60);
    wire.a.handle().announce_block((*known.block).clone(), known.hash);
    wire.drive(
        |wire, _| {
            if wire.full_count(known.hash) == 1 {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        },
    )
    .await;
    let other = reth_eth_wire::BlockHashNumber { hash: B256::repeat_byte(61), number: 61 };
    let marker = reth_eth_wire::BlockHashNumber { hash: B256::repeat_byte(62), number: 62 };
    let batch = reth_eth_wire::NewBlockHashes(vec![
        reth_eth_wire::BlockHashNumber { hash: known.hash, number: 60 },
        other.clone(),
    ]);
    assert_eq!(
        wire.a.handle().try_send_block_announcement_once(
            peer,
            PeerMessage::NewBlockHashes(batch.clone()),
            None,
        ),
        Ok(())
    );
    wire.drive(|wire, _| {
        if wire
            .b_blocks
            .lock()
            .unwrap()
            .iter()
            .any(|event| matches!(event, NewBlockEvent::Hashes(_)))
        {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
    .await;
    // Now both items are known; repeating the batch must not produce a second frame.
    assert_eq!(
        wire.a.handle().try_send_block_announcement_once(
            peer,
            PeerMessage::NewBlockHashes(batch),
            None,
        ),
        Ok(())
    );
    assert_eq!(
        wire.a.handle().try_send_block_announcement(
            peer,
            PeerMessage::NewBlockHashes(reth_eth_wire::NewBlockHashes(vec![marker.clone()])),
            None,
        ),
        Ok(())
    );
    wire.drive(|wire, _| {
        if wire.b_blocks.lock().unwrap().iter().any(
            |event| matches!(event, NewBlockEvent::Hashes(hashes) if hashes.0 == [marker.clone()]),
        ) {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
    .await;
    let frames: Vec<_> = wire
        .b_blocks
        .lock()
        .unwrap()
        .iter()
        .filter_map(|event| match event {
            NewBlockEvent::Hashes(hashes) => Some(hashes.0.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(frames, [vec![other], vec![marker]]);
    println!("WIRE_HASH known_item_skipped=1 repeated_batch_skipped=1 forced_marker_delivered=1");
}

fn signed_tx(index: u64) -> TransactionSigned {
    TransactionBuilder::default()
        .signer(B256::from(U256::from(index + 1)))
        .gas_limit(21_000)
        .max_fee_per_gas(1_000_000_000)
        .max_priority_fee_per_gas(1_000_000_000)
        .to(Address::repeat_byte(7))
        .into_eip1559()
}

#[tokio::test]
async fn source_echo_reproduction_uses_real_pool_pending_and_wire() {
    for (history, batch, chunk_size, expected_echoes) in [
        (320, 320, 320, 0),
        (320, 322, 322, 322),
        (320, 1024, 128, 0),
        (320, 1024, 1024, 1024),
        (bsc_network_args().max_seen_tx_history, 1024, 1024, 0),
    ] {
        let mut wire = LocalWire::new().await;
        let source_pool = pool(false);
        let target_pool = pool(true);
        let signed: Vec<_> = (0..batch).map(signed_tx).collect();
        let original_hashes: B256Set = signed.iter().map(|tx| *tx.tx_hash()).collect();
        let pooled: Vec<_> = signed
            .iter()
            .cloned()
            .map(|tx| {
                EthPooledTransaction::try_from_consensus(tx.try_into_recovered().unwrap()).unwrap()
            })
            .collect();
        let inserted = source_pool.add_external_transactions(pooled).await;
        assert!(
            inserted.iter().all(Result::is_ok),
            "source inserts: {:?}",
            inserted.iter().filter_map(|result| result.as_ref().err()).take(3).collect::<Vec<_>>()
        );
        let mut source_pending = source_pool.pending_transactions_listener();
        let mut target_pending = target_pool.pending_transactions_listener();
        let (source_input, source_rx) =
            memory_bounded_channel(16 * 1024 * 1024, "bsc_source_echo_test");
        let (target_input, target_rx) =
            memory_bounded_channel(16 * 1024 * 1024, "bsc_target_echo_test");
        let mut source_manager = TransactionsManager::new(
            wire.a.handle().clone(),
            source_pool.clone(),
            source_rx,
            TransactionsManagerConfig::default(),
        );
        let mut target_manager = TransactionsManager::new(
            wire.b.handle().clone(),
            target_pool.clone(),
            target_rx,
            TransactionsManagerConfig {
                max_transactions_seen_by_peer_history: history,
                ..Default::default()
            },
        );
        wire.connect().await;
        let source_peer = *wire.a.peer_id();
        let target_peer = *wire.b.peer_id();
        let mut delivered = B256Set::default();
        let mut pending = B256Set::default();
        // Accumulate the real incoming batch before advancing the application task. No fake
        // seen-cache mutations, pending notifications or manual propagation commands are used.
        for (index, chunk) in signed.chunks(chunk_size).enumerate() {
            let processed = index * chunk_size + chunk.len();
            wire.a
                .handle()
                .send_transactions(target_peer, chunk.iter().cloned().map(Arc::new).collect());
            wire.drive(|wire, cx| {
                while let Poll::Ready(Some(event)) = wire.b_txs.poll_recv(cx) {
                    if let NetworkTransactionEvent::IncomingTransactions { msg, .. } = &event {
                        delivered.extend(msg.0.iter().map(|tx| *tx.tx_hash()));
                    }
                    target_input.try_send(event).unwrap();
                }
                if delivered.len() == processed {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            })
            .await;
            wire.drive(|_, cx| {
                assert!(Pin::new(&mut target_manager).poll(cx).is_pending());
                while let Poll::Ready(Some(hash)) = target_pending.poll_recv(cx) {
                    pending.insert(hash);
                }
                if pending.len() == processed {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            })
            .await;
            let tx_handle = target_manager.handle();
            let mut app_barrier = Box::pin(tx_handle.get_peer_transaction_hashes(source_peer));
            wire.drive(|_, cx| {
                assert!(Pin::new(&mut target_manager).poll(cx).is_pending());
                app_barrier.as_mut().poll(cx)
            })
            .await
            .unwrap();
        }
        let network_handle = wire.b.handle().clone();
        let mut network_barrier = Box::pin(network_handle.get_peer_by_id(source_peer));
        wire.drive(|_, cx| network_barrier.as_mut().poll(cx)).await.unwrap().unwrap();
        // A raw TX sentinel follows gossip on the same ordinary FIFO. Unlike a block sentinel,
        // it cannot overtake transaction messages through the priority lane.
        let sentinel = signed_tx(100_000);
        let sentinel_hash = *sentinel.tx_hash();
        wire.b.handle().send_transactions(source_peer, vec![Arc::new(sentinel)]);
        let mut echoed = Vec::new();
        let mut full_echoes = 0;
        let mut hash_echoes = 0;
        wire.drive(|wire, cx| {
            let mut reached_sentinel = false;
            while let Poll::Ready(Some(mut event)) = wire.a_txs.poll_recv(cx) {
                match &mut event {
                    NetworkTransactionEvent::IncomingTransactions { msg, .. } => {
                        reached_sentinel |= msg.0.iter().any(|tx| *tx.tx_hash() == sentinel_hash);
                        msg.0.retain(|tx| *tx.tx_hash() != sentinel_hash);
                        full_echoes += msg.0.len();
                        echoed.extend(msg.0.iter().map(|tx| *tx.tx_hash()));
                    }
                    NetworkTransactionEvent::IncomingPooledTransactionHashes { msg, .. } => {
                        hash_echoes += msg.len();
                        echoed.extend(msg.iter_hashes().copied())
                    }
                    _ => {}
                }
                source_input.try_send(event).unwrap();
            }
            if reached_sentinel {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;
        assert!(echoed.iter().all(|hash| original_hashes.contains(hash)));
        assert_eq!(echoed.len(), expected_echoes, "history={history}, batch={batch}");
        let source_handle = source_manager.handle();
        let mut dedup_barrier = Box::pin(source_handle.get_peer_transaction_hashes(target_peer));
        wire.drive(|_, cx| {
            assert!(Pin::new(&mut source_manager).poll(cx).is_pending());
            dedup_barrier.as_mut().poll(cx)
        })
        .await
        .unwrap();
        assert_eq!(source_pool.pool_size().pending, batch as usize);
        assert_eq!(target_pool.pool_size().pending, batch as usize);
        assert_eq!(source_pending.try_recv(), Err(mpsc::error::TryRecvError::Empty));
        println!("WIRE_TX history={history} batch={batch} chunk={chunk_size} target_pending={} echo_items={} full_echoes={full_echoes} hash_echoes={hash_echoes} source_new_pending=0", pending.len(), echoed.len());
    }
}
