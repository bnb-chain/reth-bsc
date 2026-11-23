//! Exercise both MEV RPC handlers in a fresh process, since their header provider,
//! mining configuration and queues are process-wide OnceLocks.

use alloy_consensus::{Header, SignableTransaction, TxLegacy};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{keccak256, Address, Bytes, Signature, B256, U256, U64};
use alloy_rlp::Encodable;
use reth_bsc::{
    chainspec::{bsc::bsc_mainnet, BscChainSpec},
    consensus::parlia::{Snapshot, SnapshotProvider},
    hardforks::bsc::BscHardfork,
    node::{
        miner::{
            bid_block::{BidBlock, BidBlockArgs},
            config::{set_global_mining_config, MiningConfig},
        },
        primitives::{BscBlock, BscPrimitives},
    },
    rpc::mev::{BidArgs, BscMevApiServer, MevApiImpl, RawBid},
    shared,
};
use reth_chainspec::ForkCondition;
use reth_provider::test_utils::MockEthProvider;
use secp256k1::{Message, PublicKey, SecretKey, SECP256K1};
use std::{
    sync::{atomic::AtomicBool, Arc, RwLock},
    time::{SystemTime, UNIX_EPOCH},
};

struct Snapshots(RwLock<Snapshot>);

impl SnapshotProvider for Snapshots {
    fn snapshot_by_hash(&self, hash: &B256) -> Option<Snapshot> {
        let snapshot = self.0.read().unwrap();
        (snapshot.block_hash == *hash).then(|| snapshot.clone())
    }

    fn insert(&self, snapshot: Snapshot) {
        *self.0.write().unwrap() = snapshot;
    }
}

fn sign(hash: B256, key: &SecretKey) -> Bytes {
    let (id, compact) =
        SECP256K1.sign_ecdsa_recoverable(&Message::from_digest(hash.0), key).serialize_compact();
    let mut bytes = compact.to_vec();
    bytes.push(i32::from(id) as u8);
    bytes.into()
}

fn legacy_tx(nonce: u64, key: &SecretKey) -> Bytes {
    let tx = TxLegacy {
        chain_id: Some(56),
        nonce,
        gas_price: 1_000_000_000,
        gas_limit: 21_000,
        to: Address::repeat_byte(0x42).into(),
        ..Default::default()
    };
    let bytes = sign(tx.signature_hash(), key);
    let signature = Signature::new(
        U256::from_be_slice(&bytes[..32]),
        U256::from_be_slice(&bytes[32..64]),
        bytes[64] != 0,
    );
    tx.into_signed(signature).encoded_2718().into()
}

fn legacy_bid(parent: &Header, marker: u64, key: &SecretKey) -> BidArgs {
    let raw_bid = RawBid {
        block_number: U64::from(parent.number + 1),
        parent_hash: parent.hash_slow(),
        txs: vec![legacy_tx(0, key)],
        un_revertible: vec![],
        gas_used: U64::from(42_000),
        gas_fee: U256::from(42_000_000_000_000u64 + marker),
        builder_fee: None,
    };
    let builder_fee = U256::ZERO;
    let fields: [&dyn Encodable; 7] = [
        &raw_bid.block_number,
        &raw_bid.parent_hash,
        &raw_bid.txs,
        &raw_bid.un_revertible,
        &raw_bid.gas_used,
        &raw_bid.gas_fee,
        &builder_fee,
    ];
    let mut encoded = Vec::new();
    alloy_rlp::Header {
        list: true,
        payload_length: fields.iter().map(|field| field.length()).sum(),
    }
    .encode(&mut encoded);
    for field in fields {
        field.encode(&mut encoded);
    }
    BidArgs {
        raw_bid,
        signature: sign(keccak256(encoded), key),
        pay_bid_tx: legacy_tx(1, key),
        pay_bid_tx_gas_used: U64::from(21_000),
    }
}

fn malformed_bid_block(parent: &Header, marker: u8, key: &SecretKey) -> BidBlockArgs {
    let bid_block = BidBlock {
        header: Header {
            number: parent.number + 1,
            parent_hash: parent.hash_slow(),
            timestamp: parent.timestamp,
            gas_used: 21_000,
            extra_data: vec![marker].into(), // Distinct signed bid hashes, not duplicate requests.
            ..Default::default()
        },
        transactions: vec![Bytes::from_static(&[0x01])], // Truncated typed transaction.
        sidecars: vec![],
    };
    let signature = sign(bid_block.hash(), key);
    BidBlockArgs { bid_block, signature }
}

#[tokio::test]
async fn bid_block_decode_failure_releases_shared_quota_for_send_bid() {
    let key = SecretKey::from_slice(keccak256(b"MEV quota regression builder").as_slice()).unwrap();
    let public = PublicKey::from_secret_key(SECP256K1, &key).serialize_uncompressed();
    let builder = Address::from_slice(&keccak256(&public[1..])[12..]);
    let validator = Address::repeat_byte(0x11);
    let mut spec = bsc_mainnet();
    for fork in [BscHardfork::Pasteur, BscHardfork::Jenner] {
        spec.hardforks.insert(fork, ForkCondition::Timestamp(0));
    }
    let spec = Arc::new(BscChainSpec::from(spec));
    let provider = MockEthProvider::<BscPrimitives, _>::new().with_chain_spec((*spec).clone());
    let mut parent = Header {
        number: 40_000_000,
        timestamp: SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs(),
        ..Default::default()
    };
    let hash = parent.hash_slow();
    provider.add_header(hash, parent.clone());
    provider.add_block(hash, BscBlock { header: parent.clone(), body: Default::default() });
    shared::set_header_provider(Arc::new(provider.clone())).unwrap();
    let mut snapshot = Snapshot::new(vec![validator], parent.number, hash, 200, None);
    // Leave ample admission time; this regression tests quota, not slot timing.
    snapshot.block_interval = 60_000;
    let snapshots = Arc::new(Snapshots(RwLock::new(snapshot.clone())));
    set_global_mining_config(MiningConfig {
        validator_address: Some(validator),
        allowed_builders: Some(vec![builder]),
        bid_block_enabled: true,
        max_bids_per_builder: Some(2),
        ..Default::default()
    })
    .unwrap();
    shared::set_mev_running(Arc::new(AtomicBool::new(true))).unwrap();
    shared::init_bid_package_queue();
    let api = MevApiImpl::new(snapshots.clone(), spec);

    for marker in [1, 2] {
        let error =
            api.send_bid_block(malformed_bid_block(&parent, marker, &key)).await.unwrap_err();
        assert_eq!(error.code(), -38001);
        assert!(error.message().contains("failed to decode bid block"), "{error}");
    }
    assert_eq!(shared::bid_block_queue_len(), 0);
    let first = legacy_bid(&parent, 1, &key);
    let hash = api.send_bid(first.clone()).await.expect("decode failures must not consume quota");
    let queued = shared::pop_bid_package().expect("accepted SendBid must reach the miner");
    assert_eq!(queued.bid_hash, hash);
    assert_eq!(queued.builder, builder);

    // Releasing a failed BidBlock must preserve an existing successful SendBid reservation.
    for marker in [1, 3] {
        // A retried failed hash must reach decoding again instead of being considered a duplicate.
        let error =
            api.send_bid_block(malformed_bid_block(&parent, marker, &key)).await.unwrap_err();
        assert!(error.message().contains("failed to decode bid block"), "{error}");
    }
    assert_eq!(api.send_bid(first).await.unwrap_err().message(), "bid already exists");
    api.send_bid(legacy_bid(&parent, 2, &key)).await.expect("one shared slot remains");
    let error = api.send_bid(legacy_bid(&parent, 3, &key)).await.unwrap_err();
    assert_eq!(error.code(), -32000);
    assert_eq!(error.message(), "too many bids: exceeded limit of 2 bids per builder per block");
    let error = api.send_bid_block(malformed_bid_block(&parent, 4, &key)).await.unwrap_err();
    assert_eq!(error.code(), -32000, "a full quota must reject before decoding");
    assert!(api.send_bid(legacy_bid(&parent, 3, &key)).await.is_err());

    // Advancing the head gives the same builder a fresh allowance, as in the reported control.
    parent.parent_hash = parent.hash_slow();
    parent.number += 1;
    let hash = parent.hash_slow();
    provider.add_header(hash, parent.clone());
    provider.add_block(hash, BscBlock { header: parent.clone(), body: Default::default() });
    snapshot.block_number = parent.number;
    snapshot.block_hash = hash;
    snapshots.insert(snapshot);
    api.send_bid(legacy_bid(&parent, 3, &key)).await.expect("next block has fresh quota");

    // Successfully decoded bids still consume quota if a later pre-seal check rejects them.
    let mut invalid = malformed_bid_block(&parent, 5, &key);
    invalid.bid_block.transactions = vec![legacy_tx(0, &key)];
    let error = api.send_bid_block(invalid).await.unwrap_err();
    assert_eq!(error.code(), -38007, "zero state root fails after transaction decoding");
    let error = api.send_bid(legacy_bid(&parent, 4, &key)).await.unwrap_err();
    assert_eq!(error.message(), "too many bids: exceeded limit of 2 bids per builder per block");
}
