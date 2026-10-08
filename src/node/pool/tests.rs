use super::*;
use crate::rpc::miner::{BscMinerApiImpl, BscMinerApiServer};
use alloy_consensus::{TxEip1559, TxEip4844, TxEip7702, TxLegacy};
use alloy_primitives::{Address, Signature, TxKind, U256};
use reth_primitives_traits::Recovered;
use reth_transaction_pool::{
    blobstore::InMemoryBlobStore, noop::MockTransactionValidator, PoolConfig, TransactionPool, TransactionPoolExt,
};
use tip::{enforce_pool_tip, maintain_tip_floor, meets_tip_floor};

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
        assert!(!meets_tip_floor(tx.effective_tip_per_gas(101), 0));
        assert!(meets_tip_floor(tx.effective_tip_per_gas(90), 10));
        assert!(!meets_tip_floor(tx.effective_tip_per_gas(90), 11));
    }
}

#[tokio::test]
async fn raising_floor_cleans_all_subpools_and_parks_descendants() {
    let _guard = TipGuard::new(1);
    let pool = pool();
    pool.set_block_info(reth_transaction_pool::BlockInfo {
        pending_basefee: 7,
        pending_blob_fee: Some(2),
        ..pool.block_info()
    });
    let txs = [
        transaction(2, 1, 0, 1000, 5),  // pending, removed
        transaction(2, 1, 1, 1000, 10), // pending descendant, parked
        transaction(2, 2, 3, 1000, 5),  // nonce gap, removed
        transaction(2, 3, 0, 1000, 10), // retained pending
        transaction(2, 4, 3, 1000, 10), // retained queued
        transaction(3, 5, 0, 1000, 5),  // blob fee parked, removed
        transaction(2, 6, 0, 5, 5),     // base fee parked, removed
    ];
    for tx in &txs {
        pool.add_transaction(TransactionOrigin::External, tx.clone()).await.unwrap();
    }
    assert_eq!(pool.pool_size().pending, 3);
    let api = BscMinerApiImpl::new(pool.clone());
    assert!(api.set_gas_price(U256::from(10)).await.unwrap());
    for index in [0, 2, 5, 6] {
        assert!(pool.get(txs[index].hash()).is_none());
    }
    for index in [1, 3, 4] {
        assert!(pool.get(txs[index].hash()).is_some());
    }
    assert_eq!(pool.pool_size().pending, 1);
    assert_eq!(pool.all_transactions().queued.len(), 2);
    api.set_gas_price(U256::from(1)).await.unwrap();
    assert_eq!(
        pool.all_transaction_hashes().len(),
        3,
        "lowering does not resurrect removed transactions"
    );
    pool.add_transaction(TransactionOrigin::External, txs[0].clone()).await.unwrap();
    assert_eq!(pool.pool_size().pending, 3, "resubmitting the gap makes the descendant executable");
}

#[derive(Debug)]
struct PausedValidator {
    entered: tokio::sync::Notify,
    resume: tokio::sync::Notify,
}
impl TransactionValidator for PausedValidator {
    type Transaction = EthPooledTransaction;
    type Block = reth_ethereum_primitives::Block;
    async fn validate_transaction(
        &self,
        origin: TransactionOrigin,
        tx: Self::Transaction,
    ) -> TransactionValidationOutcome<Self::Transaction> {
        self.entered.notify_one();
        self.resume.notified().await;
        MockTransactionValidator::default().validate_transaction(origin, tx).await
    }
}

#[tokio::test(start_paused = true)]
async fn validation_rechecks_floor_after_await() {
    let _guard = TipGuard::new(1);
    let validator = BscTxValidator::new(
        PausedValidator { entered: Default::default(), resume: Default::default() },
        false,
        None,
    );
    let inner = validator.inner.clone();
    let task = tokio::spawn(async move {
        validator
            .validate_transaction(TransactionOrigin::External, transaction(2, 1, 0, 1000, 5))
            .await
    });
    inner.entered.notified().await;
    crate::shared::set_miner_gas_tip(10);
    inner.resume.notify_one();
    assert!(matches!(
        task.await.unwrap(),
        TransactionValidationOutcome::Invalid(_, InvalidPoolTransactionError::Underpriced)
    ));
}

#[tokio::test(start_paused = true)]
async fn maintenance_cleans_startup_and_late_insertions() {
    let _guard = TipGuard::new(1);
    // Bypass admission to model a transaction validated before the change but inserted after it.
    let pool = Pool::new(
        MockTransactionValidator::<EthPooledTransaction>::default(),
        CoinbaseTipOrdering::default(),
        InMemoryBlobStore::default(),
        PoolConfig::default().with_disabled_protocol_base_fee(),
    );
    let tx = transaction(2, 1, 0, 1000, 5);
    pool.add_transaction(TransactionOrigin::External, tx.clone()).await.unwrap();
    crate::shared::set_miner_gas_tip(10);
    let task = tokio::spawn(maintain_tip_floor(pool.clone()));
    tokio::task::yield_now().await;
    assert!(pool.get(tx.hash()).is_none(), "startup cleanup");
    pool.add_transaction(TransactionOrigin::External, tx.clone()).await.unwrap();
    tokio::time::advance(std::time::Duration::from_secs(4)).await;
    tokio::task::yield_now().await;
    assert!(pool.get(tx.hash()).is_some());
    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    tokio::task::yield_now().await;
    assert!(pool.get(tx.hash()).is_none(), "late insertion reconciled at the next tick");
    // Missed ticks coalesce and cleanup remains active after a long scheduling delay.
    pool.add_transaction(TransactionOrigin::External, tx.clone()).await.unwrap();
    tokio::time::advance(std::time::Duration::from_secs(60)).await;
    tokio::task::yield_now().await;
    assert!(pool.get(tx.hash()).is_none());
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
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
        if enforce_pool_tip(best.as_mut(), &tx, 0, 10) {
            included.push(*tx.hash());
        }
    }
    assert_eq!(included, vec![*txs[2].hash()]);
    assert_eq!(
        pool.all_transaction_hashes().len(),
        3,
        "build filtering only affects this iterator"
    );
}
