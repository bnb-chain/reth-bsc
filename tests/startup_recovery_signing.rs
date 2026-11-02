//! Own the process-wide recovery gate so these signing checks cannot race other unit tests.

use alloy_consensus::Header;
use alloy_primitives::B256;
use alloy_rpc_types_engine::{PayloadStatus, PayloadStatusEnum};
use reth_bsc::{
    consensus::parlia::{bls_signer::BlsVoteSigner, vote::VoteData},
    node::{
        miner::signer::MinerSigner,
        startup_recovery::{self, BscRecoveryBackfill},
    },
};
use reth_engine_primitives::{BeaconEngineMessage, ConsensusEngineHandle, OnForkChoiceUpdated};
use reth_network::{test_utils::Testnet, PeersInfo};
use reth_node_builder::sync::BackfillSyncBuilder;
use reth_primitives_traits::SealedHeader;
use reth_provider::{
    providers::BlockchainProvider,
    test_utils::{create_test_provider_factory, insert_headers, MockNodeTypesWithDB},
    CanonChainTracker, ChainStateBlockWriter, DBProvider, DatabaseProviderFactory,
};
use secp256k1::SecretKey;
use std::time::Duration;
use tokio::sync::mpsc;

#[tokio::test]
async fn startup_hook_blocks_both_signers_until_engine_recovers_finalized_history() {
    let factory = create_test_provider_factory();
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

    <BscRecoveryBackfill as BackfillSyncBuilder<MockNodeTypesWithDB, ()>>::recover(
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
        "Signing failed: Persisted finalized history is not restored"
    );
    assert_eq!(
        vote_signer.sign_vote(vote).unwrap_err().to_string(),
        "BLS signing failed: Persisted finalized history is not restored"
    );
    drop(network.terminate().await);
    tokio::time::pause();

    // Model startup rewind below finality, with networking/Engine available immediately.
    let writer = factory.database_provider_rw().unwrap();
    writer.save_finalized_block_number(1).unwrap();
    writer.commit().unwrap();
    let provider = BlockchainProvider::with_latest(factory, headers[1].clone()).unwrap();
    let (sender, mut receiver) = mpsc::unbounded_channel();
    let work = startup_recovery::run(provider.clone(), ConsensusEngineHandle::new(sender));
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
    signer.seal_header(headers[3].header(), 714).unwrap();
    vote_signer.sign_vote(vote).unwrap();
}
