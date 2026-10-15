//! Exercise the admission barrier against the actual upstream engine tree, not a query mock.

use super::*;
use crate::{
    chainspec::{bsc::bsc_mainnet, BscChainSpec},
    node::{consensus::BscConsensus, engine_api::validator::BscEngineValidator},
};
use alloy_consensus::Header;
use alloy_primitives::{Bytes, B256, U256};
use alloy_rpc_types_engine::{ForkchoiceState, PayloadStatusEnum};
use reth_chain_state::{CanonicalInMemoryState, ComputedTrieData};
use reth_engine_primitives::NoopInvalidBlockHook;
use reth_engine_tree::{
    engine::{EngineApiEvent, EngineApiKind},
    persistence::PersistenceHandle,
    tree::{payload_validator::BasicEngineValidator, EngineApiTreeHandler, TreeConfig},
};
use reth_primitives_traits::{RecoveredBlock, SealedHeader};
use reth_provider::test_utils::MockEthProvider;
use reth_trie_db::ChangesetCache;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use tokio::sync::mpsc;

#[tokio::test]
async fn real_engine_discards_competing_mined_block_and_barrier_prevents_download() {
    // Use the two incident heights. No execution/timeout response is stubbed: the upstream
    // handler must independently discard the insertion and return HeaderNotFound.
    for height in [33_209_169, 33_226_934] {
        let spec = BscChainSpec::from(bsc_mainnet());
        let provider = MockEthProvider::<BscPrimitives>::new().with_chain_spec(spec.clone());
        let spec = Arc::new(spec);
        let parent = Header { number: height - 1, difficulty: U256::from(2), ..Default::default() };
        let parent_hash = parent.hash_slow();
        let canonical = Header {
            number: height,
            parent_hash,
            difficulty: U256::from(2),
            extra_data: Bytes::from_static(b"competing canonical block"),
            ..Default::default()
        };
        provider.add_header(parent_hash, parent);
        provider.add_header(canonical.hash_slow(), canonical.clone());
        let local_header =
            Header { extra_data: Bytes::from_static(b"local off-turn block"), ..canonical.clone() };
        let local_hash = local_header.hash_slow();
        let block = ExecutedBlock::new(
            Arc::new(RecoveredBlock::new_unhashed(
                crate::BscBlock { header: local_header, body: Default::default() },
                Vec::new(),
            )),
            Arc::default(),
            ComputedTrieData::default(),
        );

        let runtime = reth_tasks::Runtime::test();
        let config = TreeConfig::default();
        let cache = ChangesetCache::new();
        let consensus = Arc::new(BscConsensus::new(spec.clone()));
        let evm = BscEvmConfig::new(spec.clone());
        let validator = BasicEngineValidator::new(
            provider.clone(),
            consensus.clone(),
            evm.clone(),
            BscEngineValidator::new(spec),
            config.clone(),
            Box::new(NoopInvalidBlockHook::default()),
            cache.clone(),
            runtime.clone(),
        );
        let (persistence_tx, _persistence_rx) = std::sync::mpsc::channel();
        let (payload_tx, _payload_rx) = mpsc::unbounded_channel();
        let (engine_tx, mut events) = EngineApiTreeHandler::spawn_new(
            provider,
            consensus,
            validator,
            PersistenceHandle::new(persistence_tx),
            reth::payload::PayloadBuilderHandle::new(payload_tx),
            CanonicalInMemoryState::with_head(SealedHeader::seal_slow(canonical), None, None),
            config,
            EngineApiKind::Ethereum,
            evm,
            cache,
            runtime,
        );
        let (tx, mut rx): (EngineTx<MockEthProvider<BscPrimitives, BscChainSpec>>, _) =
            mpsc::unbounded_channel();
        let engine_control = engine_tx.clone();
        let forwarding = tokio::spawn(async move {
            while let Some(request) = rx.recv().await {
                engine_tx.send(request.into()).unwrap();
            }
        });

        let selected = AtomicBool::new(false);
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            insert_then_update(Some(tx.clone()), block, |gate| async {
                gate.await?;
                selected.store(true, Ordering::SeqCst);
                Ok(())
            }),
        )
        .await
        .expect("real engine did not answer admission query");
        let error = result.expect_err("engine must discard local block at canonical height");
        assert!(format!("{error:?}").contains(&format!("{local_hash:?}")), "{error:?}");
        assert!(!selected.load(Ordering::SeqCst));
        while let Ok(event) = events.try_recv() {
            assert!(
                !matches!(event, EngineApiEvent::Download(_)),
                "fixed path requested a download"
            );
        }

        // Control: reproduce the old code continuing with FCU despite the discarded insertion.
        let (reply, response) = oneshot::channel();
        tx.send(EngineApiRequest::Beacon(BeaconEngineMessage::ForkchoiceUpdated {
            state: ForkchoiceState {
                head_block_hash: local_hash,
                safe_block_hash: parent_hash,
                finalized_block_hash: B256::ZERO,
            },
            payload_attrs: None,
            tx: reply,
        }))
        .unwrap();
        let response = tokio::time::timeout(Duration::from_secs(5), response)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .await
            .unwrap();
        assert!(matches!(response.payload_status.status, PayloadStatusEnum::Syncing));
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let event = events.recv().await.expect("engine stopped before download request");
                if let EngineApiEvent::Download(request) = event {
                    let reth_engine_tree::engine::DownloadRequest::BlockSet(hashes) = request
                    else {
                        panic!("expected a single-block download");
                    };
                    assert_eq!(hashes.len(), 1);
                    assert!(hashes.contains(&local_hash));
                    break;
                }
            }
        })
        .await
        .expect("old FCU path must request missing local block");
        drop(tx);
        forwarding.await.unwrap();
        let (stopped_tx, stopped_rx) = oneshot::channel();
        engine_control
            .send(reth_engine_tree::engine::FromEngine::Event(
                reth_engine_tree::chain::FromOrchestrator::Terminate { tx: stopped_tx },
            ))
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), stopped_rx).await.unwrap().unwrap();
    }
}
