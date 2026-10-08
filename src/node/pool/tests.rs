use super::*;
use crate::rpc::miner::{BscMinerApiImpl, BscMinerApiServer};
use alloy_consensus::{TxEip1559, TxEip4844, TxEip7702, TxLegacy};
use alloy_primitives::{Address, Signature, TxKind, U256};
use reth_primitives_traits::Recovered;
use reth_transaction_pool::{
    blobstore::InMemoryBlobStore, noop::MockTransactionValidator, PoolConfig, TransactionPool,
    TransactionPoolExt,
};
use tip::meets_tip_floor;

// The production floor is process-wide. Keep tests isolated even if run without --test-threads=1.
static TIP_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
pub(crate) struct TipGuard {
    previous: u64,
    _lock: std::sync::MutexGuard<'static, ()>,
}
impl TipGuard {
    pub(crate) fn new(floor: u64) -> Self {
        let lock = TIP_TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let previous = crate::shared::get_miner_gas_tip().unwrap_or_default();
        crate::shared::init_miner_gas_tip_for_tests(previous);
        crate::shared::set_miner_gas_tip(floor);
        Self { previous, _lock: lock }
    }
}
impl Drop for TipGuard {
    fn drop(&mut self) {
        crate::shared::set_miner_gas_tip(self.previous);
    }
}

fn transaction(kind: u8, sender: u8, nonce: u64, cap: u128, tip: u128) -> EthPooledTransaction {
    let tx = match kind {
        0 => TxLegacy {
            nonce,
            gas_limit: 21_000,
            gas_price: cap,
            to: TxKind::Call(Address::ZERO),
            ..Default::default()
        }
        .into(),
        2 => TxEip1559 {
            nonce,
            gas_limit: 21_000,
            max_fee_per_gas: cap,
            max_priority_fee_per_gas: tip,
            ..Default::default()
        }
        .into(),
        3 => TxEip4844 {
            nonce,
            gas_limit: 21_000,
            max_fee_per_gas: cap,
            max_priority_fee_per_gas: tip,
            max_fee_per_blob_gas: 1,
            blob_versioned_hashes: vec![alloy_primitives::B256::repeat_byte(1)],
            ..Default::default()
        }
        .into(),
        4 => TxEip7702 {
            nonce,
            gas_limit: 100_000,
            max_fee_per_gas: cap,
            max_priority_fee_per_gas: tip,
            ..Default::default()
        }
        .into(),
        _ => unreachable!(),
    };
    EthPooledTransaction::new(
        Recovered::new_unchecked(
            EthTxSigned::new_unhashed(tx, Signature::new(U256::ZERO, U256::ZERO, false)),
            Address::repeat_byte(sender),
        ),
        200,
    )
}

type TestPool = Pool<
    BscTxValidator<MockTransactionValidator<EthPooledTransaction>>,
    CoinbaseTipOrdering<EthPooledTransaction>,
    InMemoryBlobStore,
>;
fn pool() -> TestPool {
    Pool::new(
        BscTxValidator::new(MockTransactionValidator::default(), false, None),
        CoinbaseTipOrdering::default(),
        InMemoryBlobStore::default(),
        PoolConfig::default().with_disabled_protocol_base_fee(),
    )
}

#[tokio::test]
async fn admission_uses_effective_tip_for_all_fee_types() {
    let _guard = TipGuard::new(10);
    let pool = pool();
    for kind in [0, 2, 3, 4] {
        for tip in [0, 9, 10, 11] {
            let cap = if kind == 0 { tip } else { 1000 };
            let tx = transaction(kind, kind + 1, 0, cap, tip);
            let outcome =
                pool.validator().validate_transaction(TransactionOrigin::External, tx).await;
            if tip < 10 {
                assert!(
                    matches!(
                        outcome,
                        TransactionValidationOutcome::Invalid(
                            _,
                            InvalidPoolTransactionError::Underpriced
                        )
                    ),
                    "kind={kind}, tip={tip}: {outcome:?}"
                );
            } else {
                assert!(outcome.is_valid(), "kind={kind}, tip={tip}: {outcome:?}");
            }
        }
    }
    assert!(pool
        .add_transaction(TransactionOrigin::External, transaction(2, 1, 0, 1000, 9))
        .await
        .is_err());
    assert!(pool
        .add_transaction(TransactionOrigin::External, transaction(2, 1, 0, 1000, 10))
        .await
        .is_ok());
    assert_eq!(pool.pool_size().pending, 1);
    crate::shared::set_miner_gas_tip(0);
    for kind in [0, 2, 3, 4] {
        let tx = transaction(kind, kind + 1, 0, 0, 0);
        assert!(pool
            .validator()
            .validate_transaction(TransactionOrigin::External, tx)
            .await
            .is_valid());
    }
}

#[test]
fn fee_calculation_failure_and_nonzero_base_fee() {
    for kind in [0, 2, 3, 4] {
        let tx = transaction(kind, 1, 0, 100, 20);
        let unavailable_tip = tx.effective_tip_per_gas(101);
        assert!(unavailable_tip.is_none());
        assert!(meets_tip_floor(unavailable_tip, 0));
        assert!(!meets_tip_floor(unavailable_tip, 1));
        assert!(meets_tip_floor(tx.effective_tip_per_gas(90), 10));
        assert!(!meets_tip_floor(tx.effective_tip_per_gas(90), 11));
    }
}

#[tokio::test]
async fn changing_floor_preserves_existing_transactions_and_updates_admission() {
    let _guard = TipGuard::new(1);
    let pool = pool();
    pool.set_block_info(reth_transaction_pool::BlockInfo {
        pending_basefee: 7,
        pending_blob_fee: Some(2),
        ..pool.block_info()
    });
    let txs = [
        transaction(2, 1, 0, 1000, 5),  // pending
        transaction(2, 1, 1, 1000, 10), // pending descendant
        transaction(2, 2, 3, 1000, 5),  // nonce gap
        transaction(2, 3, 0, 1000, 10), // qualifying pending
        transaction(2, 4, 3, 1000, 10), // qualifying queued
        transaction(3, 5, 0, 1000, 5),  // blob fee parked
        transaction(2, 6, 0, 5, 5),     // base fee parked
    ];
    for tx in &txs {
        pool.add_transaction(TransactionOrigin::External, tx.clone()).await.unwrap();
    }
    assert_eq!(pool.pool_size().pending, 3);
    let api = BscMinerApiImpl::new();
    assert!(api.set_gas_price(U256::from(10)).await.unwrap());
    for tx in &txs {
        assert!(pool.get(tx.hash()).is_some(), "changing the floor must not evict transactions");
    }
    assert_eq!(pool.pool_size().pending, 3);
    assert_eq!(pool.all_transaction_hashes().len(), txs.len());

    let new_tx = transaction(2, 7, 0, 1001, 5);
    let error =
        pool.add_transaction(TransactionOrigin::External, new_tx.clone()).await.unwrap_err();
    assert!(matches!(
        error.kind,
        reth_transaction_pool::error::PoolErrorKind::InvalidTransaction(
            InvalidPoolTransactionError::Underpriced
        )
    ));
    assert!(api.set_gas_price(U256::from(1)).await.unwrap());
    assert!(pool.add_transaction(TransactionOrigin::External, new_tx).await.is_ok());
    assert_eq!(pool.all_transaction_hashes().len(), txs.len() + 1);
}

#[tokio::test]
async fn ordinary_build_filter_skips_descendants_without_evicting_them() {
    let _guard = TipGuard::new(1);
    let pool = pool();
    let txs = [
        transaction(2, 1, 0, 1000, 5),
        transaction(2, 1, 1, 1000, 20),
        transaction(2, 2, 0, 1000, 10),
    ];
    for tx in &txs {
        pool.add_transaction(TransactionOrigin::External, tx.clone()).await.unwrap();
    }
    let mut best = pool.best_transactions();
    let mut included = vec![];
    while let Some(tx) = best.next() {
        if !meets_tip_floor(tx.effective_tip_per_gas(0), 10) {
            best.mark_invalid(&tx, &InvalidPoolTransactionError::Underpriced);
            continue;
        }
        included.push(*tx.hash());
    }
    assert_eq!(included, vec![*txs[2].hash()]);
    assert_eq!(
        pool.all_transaction_hashes().len(),
        3,
        "build filtering only affects this iterator"
    );
}
