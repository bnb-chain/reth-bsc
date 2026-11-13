//! Own the process-wide recovery gate so these signing checks cannot race other unit tests.

use alloy_consensus::Header;
use alloy_primitives::B256;
use alloy_rpc_types_engine::{PayloadStatus, PayloadStatusEnum};
use reth_bsc::{
    consensus::parlia::{bls_signer::BlsVoteSigner, vote::VoteData},
    node::{
        miner::signer::MinerSigner,
        network::BscNetworkPrimitives,
        startup_recovery::{self, BscRecoveryBackfill},
        startup_sync,
    },
};
use reth_engine_primitives::{BeaconEngineMessage, ConsensusEngineHandle, OnForkChoiceUpdated};
use reth_network::{
    eth_requests::EthRequestHandler, test_utils::Testnet, NetworkConfigBuilder, NetworkManager,
    PeersInfo,
};
use reth_network_api::Peers;
use reth_node_builder::sync::BackfillSyncBuilder;
use reth_primitives_traits::SealedHeader;
use reth_provider::{
    providers::BlockchainProvider,
    test_utils::{create_test_provider_factory, insert_headers, MockEthProvider},
    BlockNumReader, CanonChainTracker, ChainStateBlockWriter, DBProvider, DatabaseProviderFactory,
    RocksDBProviderFactory,
};
use reth_tasks::Runtime;
use secp256k1::SecretKey;
use std::time::Duration;
use tokio::sync::mpsc;

#[tokio::test]
async fn startup_hook_blocks_both_signers_until_engine_recovers_finalized_history() {
    let base = create_test_provider_factory();
    let static_dir = base.db_ref().path().join("bsc_static_files");
    std::fs::create_dir_all(&static_dir).unwrap();
    let factory = reth_provider::ProviderFactory::<
        reth::api::NodeTypesWithDBAdapter<reth_bsc::node::BscNode, _>,
    >::new(
        base.db_ref().clone(),
        std::sync::Arc::new(reth_bsc::chainspec::BscChainSpec::from(
            reth_bsc::chainspec::bsc::bsc_mainnet(),
        )),
        reth_provider::providers::StaticFileProvider::read_write(&static_dir).unwrap(),
        base.rocksdb_provider(),
        Runtime::test(),
    )
    .unwrap();
    let mut parent = B256::ZERO;
    let headers = (0..=3)
        .map(|number| {
            let header = SealedHeader::seal_slow(Header {
                number,
                parent_hash: parent,
                ..Default::default()
            });
            parent = header.hash();
            header
        })
        .collect::<Vec<_>>();
    insert_headers(&factory, &headers);
    let writer = factory.database_provider_rw().unwrap();
    writer.save_finalized_block_number(2).unwrap();
    writer.commit().unwrap();

    <BscRecoveryBackfill as BackfillSyncBuilder<_, ()>>::recover(
        &mut BscRecoveryBackfill,
        &factory,
    )
    .unwrap();
    let signer = loop {
        if let Ok(key) = SecretKey::from_slice(&rand::random::<[u8; 32]>()) {
            break MinerSigner::new(key);
        }
    };
    let bls_key = blst::min_pk::SecretKey::key_gen(&rand::random::<[u8; 32]>(), &[]).unwrap();
    let vote_signer = BlsVoteSigner::new_from_bytes(bls_key.to_bytes()).unwrap();
    let vote = VoteData {
        source_number: 1,
        source_hash: headers[1].hash(),
        target_number: 2,
        target_hash: headers[2].hash(),
    };
    let network = Testnet::create(2).await.spawn();
    tokio::time::timeout(Duration::from_secs(10), network.connect_peers()).await.unwrap();
    assert_eq!(network.peers_array::<2>()[0].network().num_connected_peers(), 1);
    assert_eq!(
        signer.seal_header(headers[3].header(), 714).unwrap_err().to_string(),
        "Signing failed: Startup history recovery and network head assessment are not complete"
    );
    assert_eq!(
        vote_signer.sign_vote(vote).unwrap_err().to_string(),
        "BLS signing failed: Startup history recovery and network head assessment are not complete"
    );
    drop(network.terminate().await);
    tokio::time::pause();

    // Model startup rewind below finality, with networking/Engine available immediately.
    let writer = factory.database_provider_rw().unwrap();
    writer.save_finalized_block_number(1).unwrap();
    writer.commit().unwrap();
    let provider = BlockchainProvider::with_latest(factory, headers[1].clone()).unwrap();
    let (sender, mut receiver) = mpsc::unbounded_channel();
    let engine = ConsensusEngineHandle::new(sender);
    let work = startup_recovery::run(provider.clone(), engine.clone());
    tokio::pin!(work);
    let request = tokio::select! {
        request = receiver.recv() => request.unwrap(),
        result = &mut work => panic!("premature recovery: {result:?}"),
    };
    let BeaconEngineMessage::ForkchoiceUpdated { state, tx, .. } = request else {
        panic!("expected FCU")
    };
    assert_eq!(state.finalized_block_hash, headers[2].hash());
    tx.send(Ok(OnForkChoiceUpdated::valid(PayloadStatus::new(PayloadStatusEnum::Syncing, None))))
        .unwrap();
    assert!(futures::poll!(&mut work).is_pending());
    tokio::time::advance(Duration::from_secs(60)).await;
    assert!(signer.seal_header(headers[3].header(), 714).is_err());
    assert!(vote_signer.sign_vote(vote).is_err());

    // Neither peer availability nor a long wait releases the signers. Only verified history does.
    provider.set_canonical_head(headers[2].clone());
    provider.set_finalized(headers[2].clone());
    work.await.unwrap();
    assert!(signer.seal_header(headers[3].header(), 714).is_err());
    assert!(vote_signer.sign_vote(vote).is_err());

    // A quiet peer ahead by one block is actively queried for its header and body before signing.
    tokio::time::resume();
    let remote = MockEthProvider::<reth_bsc::BscPrimitives>::new();
    for header in &headers {
        remote.add_block(
            header.hash(),
            reth_bsc::BscBlock { header: header.header().clone(), body: Default::default() },
        );
    }
    let mut handles = Vec::new();
    let mut tasks = Vec::new();
    for _ in 0..2 {
        let config = NetworkConfigBuilder::<BscNetworkPrimitives>::new(
            reth_network::config::rng_secret_key(),
            Runtime::test(),
        )
        .listener_addr("127.0.0.1:0".parse().unwrap())
        .disable_dns_discovery()
        .disable_discv4_discovery()
        .set_head(reth_ethereum_forks::Head {
            number: 3,
            hash: headers[3].hash(),
            ..Default::default()
        })
        .build(remote.clone());
        let mut manager = NetworkManager::new(config).await.unwrap();
        let (tx, rx) = mpsc::channel(16);
        manager.set_eth_request_handler(tx);
        tasks.push(tokio::spawn(EthRequestHandler::new(
            remote.clone(),
            manager.peers_handle(),
            rx,
        )));
        handles.push((manager.handle().clone(), manager.local_addr()));
        tasks.push(tokio::spawn(manager));
    }
    handles[0].0.add_peer(*handles[1].0.peer_id(), handles[1].1);
    let imported = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let imported_count = imported.clone();
    let canonical = provider.clone();
    let target = headers[3].clone();
    tasks.push(tokio::spawn(async move {
        while let Some(request) = receiver.recv().await {
            match request {
                BeaconEngineMessage::NewPayload { tx, .. } => {
                    imported_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    tx.send(Ok(PayloadStatus::new(PayloadStatusEnum::Valid, None))).unwrap();
                }
                BeaconEngineMessage::ForkchoiceUpdated { state, tx, .. } => {
                    if state.head_block_hash == target.hash() {
                        canonical.set_canonical_head(target.clone());
                    }
                    tx.send(Ok(OnForkChoiceUpdated::valid(PayloadStatus::new(
                        PayloadStatusEnum::Valid,
                        None,
                    ))))
                    .unwrap();
                }
                _ => panic!("unexpected Engine message"),
            }
        }
    }));
    let sync = tokio::spawn(startup_sync::run(
        provider.clone(),
        engine,
        handles[0].0.clone(),
        std::sync::Arc::new(reth_bsc::chainspec::BscChainSpec::from(
            reth_bsc::chainspec::bsc::bsc_mainnet(),
        )),
    ));
    tokio::time::timeout(Duration::from_secs(10), async {
        while signer.seal_header(headers[3].header(), 714).is_err() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(provider.chain_info().unwrap().best_hash, headers[3].hash());
    assert!(imported.load(std::sync::atomic::Ordering::Relaxed) > 0);
    sync.abort();
    for task in tasks {
        task.abort();
    }
    signer.seal_header(headers[3].header(), 714).unwrap();
    vote_signer.sign_vote(vote).unwrap();
}
