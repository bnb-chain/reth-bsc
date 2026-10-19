use super::*;
use alloy_consensus::{SignableTransaction, Signed, TxEip4844, TxLegacy};
use alloy_eips::{
    eip2718::Encodable2718,
    eip4844::builder::{SidecarBuilder, SimpleCoder},
};
use alloy_primitives::{keccak256, Signature};

fn sign(tx: &impl SignableTransaction<Signature>) -> Signature {
    use secp256k1::{Message, SecretKey, SECP256K1};
    let key =
        SecretKey::from_slice(keccak256(b"legacy bid sidecar regression").as_slice()).unwrap();
    let (id, bytes) = SECP256K1
        .sign_ecdsa_recoverable(&Message::from_digest(tx.signature_hash().0), &key)
        .serialize_compact();
    Signature::new(
        U256::from_be_slice(&bytes[..32]),
        U256::from_be_slice(&bytes[32..]),
        i32::from(id) != 0,
    )
}

pub(crate) fn blob_fixture() -> (TransactionSigned, BlobTransactionSidecar) {
    let sidecar =
        SidecarBuilder::<SimpleCoder>::from_slice(b"legacy MEV bid blob").build_4844().unwrap();
    let tx = TxEip4844 {
        chain_id: 56,
        gas_limit: 21_000,
        max_fee_per_gas: 1_000_000_000,
        max_fee_per_blob_gas: 1,
        to: Address::repeat_byte(0x42),
        blob_versioned_hashes: sidecar.versioned_hashes().collect(),
        ..Default::default()
    };
    let signature = sign(&tx);
    (TransactionSigned::new_unhashed(tx.into(), signature), sidecar)
}

pub(crate) fn blob_wire(tx: &TransactionSigned, sidecar: BlobTransactionSidecar) -> Bytes {
    let signed = tx.as_eip4844().unwrap();
    Signed::new_unhashed(
        TxEip4844WithSidecar { tx: signed.tx().clone(), sidecar },
        *signed.signature(),
    )
    .encoded_2718()
    .into()
}

fn payment_wire() -> Bytes {
    let tx = TxLegacy { chain_id: Some(56), gas_limit: 21_000, ..Default::default() };
    let signature = sign(&tx);
    tx.into_signed(signature).encoded_2718().into()
}

fn args(txs: Vec<Bytes>, pay_bid_tx: Bytes) -> BidArgs {
    BidArgs {
        raw_bid: RawBid {
            block_number: U64::from(1),
            parent_hash: B256::ZERO,
            txs,
            un_revertible: vec![],
            gas_used: U64::from(42_000),
            gas_fee: U256::from(1),
            builder_fee: None,
        },
        signature: Bytes::new(),
        pay_bid_tx,
        pay_bid_tx_gas_used: U64::from(21_000),
    }
}

fn decode_bid(args: &BidArgs) -> Result<Bid, String> {
    let spec = BscChainSpec::from(crate::chainspec::bsc::bsc_mainnet());
    MevApiImpl::to_bid(args, Address::ZERO, &spec, B256::ZERO)
}

#[test]
fn legacy_bid_accepts_valid_blob_sidecars_and_non_blob_transactions() {
    let (tx, sidecar) = blob_fixture();
    let bid = decode_bid(&args(vec![blob_wire(&tx, sidecar.clone())], payment_wire())).unwrap();
    assert_eq!(bid.txs.len(), 2);
    assert_eq!(bid.blob_sidecars.get(tx.hash()), Some(&sidecar));
    let bid = decode_bid(&args(vec![payment_wire()], payment_wire())).unwrap();
    assert!(bid.blob_sidecars.is_empty());
}

#[test]
fn legacy_bid_rejects_bare_blob_transactions_in_both_fields() {
    let (tx, _) = blob_fixture();
    for wire in [Bytes::from(tx.encoded_2718()), Bytes::from(alloy_rlp::encode(&tx))] {
        assert!(decode_bid(&args(vec![wire.clone()], payment_wire())).is_err());
        assert!(decode_bid(&args(vec![], wire)).is_err());
    }
}

#[test]
fn legacy_bid_rejects_invalid_sidecars_in_both_fields() {
    let (tx, valid) = blob_fixture();
    let mut cases = Vec::new();
    let mut sidecar = valid.clone();
    sidecar.blobs.clear();
    cases.push(sidecar);
    let mut sidecar = valid.clone();
    sidecar.commitments.clear();
    cases.push(sidecar);
    let mut sidecar = valid.clone();
    sidecar.proofs.clear();
    cases.push(sidecar);
    let mut sidecar = valid.clone();
    sidecar.blobs.push(sidecar.blobs[0]);
    cases.push(sidecar);
    let mut sidecar = valid.clone();
    sidecar.commitments[0][0] ^= 1;
    cases.push(sidecar);
    let mut sidecar = valid.clone();
    sidecar.blobs[0][31] ^= 1;
    cases.push(sidecar);
    let mut sidecar = valid;
    sidecar.proofs[0][0] ^= 1;
    cases.push(sidecar);
    for (index, sidecar) in cases.into_iter().enumerate() {
        let wire = blob_wire(&tx, sidecar);
        for args in [args(vec![wire.clone()], payment_wire()), args(vec![], wire)] {
            assert!(decode_bid(&args).is_err(), "accepted invalid sidecar case {index}");
        }
    }
}

#[test]
fn legacy_bid_rejects_empty_blob_hashes_and_trailing_bytes() {
    let tx = TxEip4844 { chain_id: 56, ..Default::default() };
    let signature = sign(&tx);
    let tx = TransactionSigned::new_unhashed(tx.into(), signature);
    let empty = blob_wire(&tx, BlobTransactionSidecar::default());
    assert!(decode_bid(&args(vec![empty], payment_wire()))
        .err()
        .unwrap()
        .contains("no versioned hashes"));

    let (tx, sidecar) = blob_fixture();
    let mut trailing = blob_wire(&tx, sidecar).to_vec();
    trailing.push(0);
    assert!(decode_bid(&args(vec![trailing.into()], payment_wire())).is_err());
}
