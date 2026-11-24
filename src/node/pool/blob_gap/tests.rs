use super::*;
use crate::node::pool::{tests::TipGuard, BscTxValidator};
use alloy_consensus::{Header, TxEip4844};
use alloy_eips::eip4844::{
    builder::{SidecarBuilder, SimpleCoder},
    BlobTransactionSidecar,
};
use alloy_primitives::{Signature, U256};
use reth_chainspec::MAINNET;
use reth_ethereum_primitives::{Block, TransactionSigned};
use reth_evm_ethereum::EthEvmConfig;
use reth_provider::test_utils::{ExtendedAccount, MockEthProvider};
use reth_transaction_pool::{
    blobstore::InMemoryBlobStore, validate::EthTransactionValidatorBuilder,
    EthBlobTransactionSidecar, EthTransactionValidator, PoolConfig,
};
use std::sync::OnceLock;

type Validator =
    BscTxValidator<EthTransactionValidator<MockEthProvider, EthPooledTransaction, EthEvmConfig>>;
type TestPool = BscTransactionPool<Validator, InMemoryBlobStore, MockEthProvider>;

fn pool() -> TestPool {
    let provider = MockEthProvider::default();
    let header = Header {
        number: 20_000_000,
        timestamp: 1_720_000_000,
        gas_limit: 30_000_000,
        base_fee_per_gas: Some(1),
        excess_blob_gas: Some(0),
        blob_gas_used: Some(0),
        ..Default::default()
    };
    provider.add_block(header.hash_slow(), Block { header, body: Default::default() });
    let store = InMemoryBlobStore::default();
    let validator =
        EthTransactionValidatorBuilder::new(provider.clone(), EthEvmConfig::new(MAINNET.clone()))
            .no_eip7594()
            .build(store.clone());
    let inner = Pool::new(
        BscTxValidator::new(validator, false, None),
        CoinbaseTipOrdering::default(),
        store,
        PoolConfig::default().with_disabled_protocol_base_fee(),
    );
    BscTransactionPool::new(inner, provider, TaskExecutor::test())
}

fn fund(pool: &TestPool, sender: u8, nonce: u64) {
    pool.provider.add_account(
        Address::repeat_byte(sender),
        ExtendedAccount::new(nonce, U256::from(10u64.pow(18))),
    );
}

fn blob(sender: u8, nonce: u64) -> EthPooledTransaction {
    static SIDECAR: OnceLock<BlobTransactionSidecar> = OnceLock::new();
    let sidecar = SIDECAR.get_or_init(|| {
        SidecarBuilder::<SimpleCoder>::from_slice(b"BSC nonce gap regression").build_4844().unwrap()
    });
    let tx = TxEip4844 {
        chain_id: 1,
        nonce,
        gas_limit: 50_000,
        max_fee_per_gas: 5_000_000_000,
        max_priority_fee_per_gas: 5_000_000_000,
        max_fee_per_blob_gas: 1_000_000,
        to: Address::repeat_byte(sender),
        blob_versioned_hashes: sidecar.versioned_hashes().collect(),
        ..Default::default()
    };
    let mut tx = EthPooledTransaction::new(
        Recovered::new_unchecked(
            TransactionSigned::new_unhashed(
                tx.into(),
                Signature::new(U256::from(1), U256::from(1), false),
            ),
            Address::repeat_byte(sender),
        ),
        200,
    );
    tx.blob_sidecar = EthBlobTransactionSidecar::Present(
        BlobTransactionSidecarVariant::Eip4844(sidecar.clone()).into(),
    );
    tx
}

#[test]
fn geth_allowance_boundaries() {
    for (nonce, expected) in
        [(0, 0), (8, 0), (9, 1), (10, 1), (98, 1), (99, 2), (999, 3), (u64::MAX, 16)]
    {
        assert_eq!(allowance(nonce, 0, 0), expected, "nonce {nonce}");
    }
    assert_eq!(allowance(999, 15, 0), 1);
    assert_eq!(allowance(999, 16, 0), 0);
    assert_eq!(allowance(999, 17, 0), 0);
    assert_eq!(allowance(999, 0, 3), 0);
    assert_eq!(allowance(10, 0, 2), 0);
}

#[tokio::test]
async fn nonce_11_is_held_then_automatically_promoted_after_nonce_10() {
    let _guard = TipGuard::new(0);
    for origin in
        [TransactionOrigin::External, TransactionOrigin::Local, TransactionOrigin::Private]
    {
        let pool = pool();
        fund(&pool, 1, 10);
        let later = blob(1, 11);
        let later_hash = *later.hash();
        // Regression: the unwrapped pinned Reth pool rejects the same validated transaction.
        assert!(is_nonce_gap(
            &pool.inner.add_transaction(origin, later.clone()).await.unwrap_err()
        ));
        let mut announcements =
            pool.pending_transactions_listener_for(TransactionListenerKind::PropagateOnly);
        let mut sidecars = pool.blob_transaction_sidecars_listener();
        let mut events = pool.add_transaction_and_subscribe(origin, later).await.unwrap();
        assert_eq!(events.next().await, Some(TransactionEvent::Queued));
        assert!(pool.contains(&later_hash));
        assert!(pool.get(&later_hash).is_none());
        assert!(pool.get_blob(later_hash).unwrap().is_none());
        assert!(pool.pooled_transaction_hashes().is_empty());
        assert!(pool.pending_transactions().is_empty());
        assert_eq!(pool.pending_and_queued_txn_count(), (0, 0));
        assert!(pool
            .get_highest_consecutive_transaction_by_sender(Address::repeat_byte(1), 10)
            .is_none());
        assert!(announcements.try_recv().is_err());
        assert!(sidecars.try_recv().is_err());
        let worker = pool.tasks.spawn_task(pool.clone().run());
        let filler = blob(1, 10);
        let filler_hash = *filler.hash();
        assert!(pool.add_transaction(origin, filler).await.unwrap().is_pending());
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), events.next()).await.unwrap(),
            Some(TransactionEvent::Pending)
        );
        assert_eq!(pool.pending_transactions().len(), 2);
        assert_eq!(
            pool.get_highest_consecutive_transaction_by_sender(Address::repeat_byte(1), 10)
                .unwrap()
                .nonce(),
            11
        );
        assert!(pool.get_blob(later_hash).unwrap().is_some());
        assert!(pool.buffer.lock().is_empty());
        let nonces: Vec<_> = pool.best_transactions().map(|tx| tx.nonce()).collect();
        assert_eq!(nonces, [10, 11]);
        if origin == TransactionOrigin::Private {
            assert!(announcements.try_recv().is_err());
            assert!(pool.pooled_transaction_hashes().is_empty());
        } else {
            assert_eq!(announcements.try_recv().unwrap(), filler_hash);
            assert_eq!(announcements.try_recv().unwrap(), later_hash);
        }
        pool.remove_transaction(later_hash);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), events.next()).await.unwrap(),
            Some(TransactionEvent::Discarded)
        );
        worker.abort();
    }
}

#[tokio::test]
async fn rejects_fresh_accounts_duplicates_and_excess_per_sender() {
    let _guard = TipGuard::new(0);
    let pool = pool();
    fund(&pool, 1, 0);
    assert!(is_nonce_gap(
        &pool.add_transaction(TransactionOrigin::Local, blob(1, 1)).await.unwrap_err()
    ));
    assert!(pool.buffer.lock().is_empty());
    fund(&pool, 1, 10);
    let tx = blob(1, 11);
    let hash = *tx.hash();
    assert_eq!(
        pool.add_transaction(TransactionOrigin::Local, tx.clone()).await.unwrap().state,
        AddedTransactionState::Queued(QueuedReason::NonceGap)
    );
    assert!(matches!(
        pool.add_transaction(TransactionOrigin::External, tx).await.unwrap_err().kind,
        PoolErrorKind::AlreadyImported
    ));
    assert!(is_nonce_gap(
        &pool.add_transaction(TransactionOrigin::Local, blob(1, 12)).await.unwrap_err()
    ));
    assert_eq!(pool.buffer.lock().len(), 1);
    assert!(pool.contains(&hash));
}

#[tokio::test]
async fn global_buffer_is_bounded_and_expiry_releases_capacity() {
    let _guard = TipGuard::new(0);
    let pool = pool();
    for sender in 1..=129 {
        fund(&pool, sender, 10);
        let result = pool.add_transaction(TransactionOrigin::External, blob(sender, 11)).await;
        if sender <= 128 {
            result.unwrap();
        } else {
            assert!(is_nonce_gap(&result.unwrap_err()));
        }
    }
    assert_eq!(pool.buffer.lock().len(), MAX_GAPPED);
    let old_hash = *pool.buffer.lock()[0].transaction.hash();
    let mut events = pool.transaction_event_listener(old_hash).unwrap();
    assert_eq!(events.next().await, Some(TransactionEvent::Queued));
    pool.buffer.lock()[0].received = Instant::now() - GAPPED_LIFETIME;
    assert!(!pool.contains(&old_hash));
    pool.add_transaction(TransactionOrigin::External, blob(129, 11)).await.unwrap();
    assert_eq!(events.next().await, Some(TransactionEvent::Discarded));
    assert_eq!(pool.buffer.lock().len(), MAX_GAPPED);
    assert!(!pool.contains(&old_hash));
}

#[tokio::test]
async fn multiple_gaps_promote_in_nonce_order_with_original_origins() {
    let _guard = TipGuard::new(0);
    let pool = pool();
    fund(&pool, 1, 99);
    let results = pool
        .add_transactions_with_origins(vec![
            (TransactionOrigin::Private, blob(1, 101)),
            (TransactionOrigin::External, blob(1, 100)),
            (TransactionOrigin::Local, blob(1, 99)),
        ])
        .await;
    assert!(results.iter().all(Result::is_ok));
    pool.promote().await;
    assert_eq!(pool.best_transactions().map(|tx| tx.nonce()).collect::<Vec<_>>(), [99, 100, 101]);
    assert_eq!(pool.get_pending_transactions_by_origin(TransactionOrigin::Private).len(), 1);
    assert_eq!(pool.get_pending_transactions_by_origin(TransactionOrigin::External).len(), 1);
    assert_eq!(pool.get_pending_transactions_by_origin(TransactionOrigin::Local).len(), 1);
    assert!(pool.buffer.lock().is_empty());
}

#[tokio::test]
async fn chain_progress_promotes_or_discards_buffered_transactions() {
    let _guard = TipGuard::new(0);
    let pool = pool();
    for sender in 1..=3 {
        fund(&pool, sender, 10);
        pool.add_transaction(TransactionOrigin::Local, blob(sender, 11)).await.unwrap();
    }
    fund(&pool, 1, 11); // missing nonce mined elsewhere
    fund(&pool, 2, 12); // buffered nonce already consumed
    pool.provider.add_account(Address::repeat_byte(3), ExtendedAccount::new(11, U256::ZERO));
    pool.promote().await;
    assert!(pool.get(blob(1, 11).hash()).is_some());
    assert!(!pool.contains(blob(2, 11).hash()));
    assert!(!pool.contains(blob(3, 11).hash()));
    assert!(pool.buffer.lock().is_empty());
}

#[tokio::test]
async fn expiry_and_explicit_removal_prevent_promotion() {
    let _guard = TipGuard::new(0);
    let pool = pool();
    for sender in 1..=3 {
        fund(&pool, sender, 10);
        pool.add_transaction(TransactionOrigin::Local, blob(sender, 11)).await.unwrap();
    }
    pool.buffer.lock()[0].received = Instant::now() - GAPPED_LIFETIME;
    pool.remove_transaction(*blob(2, 11).hash());
    pool.remove_transactions_by_sender(Address::repeat_byte(3));
    for sender in 1..=3 {
        pool.add_transaction(TransactionOrigin::Local, blob(sender, 10)).await.unwrap();
    }
    pool.promote().await;
    assert!(pool.buffer.lock().is_empty());
    assert_eq!(pool.pending_transactions().len(), 3);
    for sender in 1..=3 {
        assert!(!pool.contains(blob(sender, 11).hash()));
    }
}

#[tokio::test]
async fn invalid_sidecars_and_fee_caps_never_enter_buffer() {
    let _guard = TipGuard::new(0);
    let pool = pool();
    fund(&pool, 1, 10);
    let mut missing = blob(1, 11);
    missing.blob_sidecar = EthBlobTransactionSidecar::Missing;
    assert!(pool.add_transaction(TransactionOrigin::Local, missing).await.is_err());
    let mut invalid = blob(1, 11);
    let mut sidecar =
        SidecarBuilder::<SimpleCoder>::from_slice(b"wrong blob").build_4844().unwrap();
    sidecar.proofs[0] = Default::default();
    invalid.blob_sidecar =
        EthBlobTransactionSidecar::Present(BlobTransactionSidecarVariant::Eip4844(sidecar).into());
    assert!(pool.add_transaction(TransactionOrigin::Local, invalid).await.is_err());
    let mut zero_fee = blob(1, 11);
    let mut tx = zero_fee.transaction.as_eip4844().unwrap().tx().clone();
    tx.max_fee_per_blob_gas = 0;
    zero_fee.transaction = Recovered::new_unchecked(
        TransactionSigned::new_unhashed(
            tx.into(),
            Signature::new(U256::from(1), U256::from(1), false),
        ),
        Address::repeat_byte(1),
    );
    assert!(pool.add_transaction(TransactionOrigin::Local, zero_fee).await.is_err());
    assert!(pool.buffer.lock().is_empty());
}

#[tokio::test]
async fn gap_allowance_accounts_for_existing_blob_slots() {
    let _guard = TipGuard::new(0);
    let pool = pool();
    // This account earns 16 temporary slots, but 15 are occupied by normal pool entries.
    let nonce = 9_999_999_999_999_999;
    fund(&pool, 1, nonce);
    for offset in 0..15 {
        pool.add_transaction(TransactionOrigin::Local, blob(1, nonce + offset)).await.unwrap();
    }
    pool.add_transaction(TransactionOrigin::Local, blob(1, nonce + 16)).await.unwrap();
    assert!(is_nonce_gap(
        &pool.add_transaction(TransactionOrigin::Local, blob(1, nonce + 17)).await.unwrap_err()
    ));
    assert_eq!(pool.buffer.lock().len(), 1);
    pool.add_transaction(TransactionOrigin::Local, blob(1, nonce + 15)).await.unwrap();
    pool.promote().await;
    assert!(pool.buffer.lock().is_empty());
    assert_eq!(pool.pending_transactions().len(), 16);
    assert!(!pool.contains(blob(1, nonce + 16).hash()));
}

#[tokio::test]
async fn promotion_rechecks_cumulative_balance_and_replacement_rules() {
    let _guard = TipGuard::new(0);
    let pool = pool();
    let tx = blob(1, 11);
    // Each transaction is affordable on its own, but the pair is not.
    let balance = *tx.cost() + *tx.cost() / U256::from(2);
    pool.provider.add_account(Address::repeat_byte(1), ExtendedAccount::new(10, balance));
    pool.add_transaction(TransactionOrigin::Local, tx.clone()).await.unwrap();
    pool.add_transaction(TransactionOrigin::Local, blob(1, 10)).await.unwrap();
    pool.promote().await;
    assert!(!pool.contains(tx.hash()));
    assert_eq!(pool.pending_transactions().len(), 1);
    assert!(pool.buffer.lock().is_empty());

    fund(&pool, 2, 99);
    let first = blob(2, 100);
    let mut replacement = first.clone();
    let mut transaction = replacement.transaction.as_eip4844().unwrap().tx().clone();
    transaction.input = vec![1].into();
    replacement.transaction = Recovered::new_unchecked(
        TransactionSigned::new_unhashed(
            transaction.into(),
            Signature::new(U256::from(1), U256::from(1), false),
        ),
        Address::repeat_byte(2),
    );
    pool.add_transaction(TransactionOrigin::Local, first.clone()).await.unwrap();
    pool.add_transaction(TransactionOrigin::Local, replacement.clone()).await.unwrap();
    pool.add_transaction(TransactionOrigin::Local, blob(2, 99)).await.unwrap();
    pool.promote().await;
    // Equal-nonce arrivals retain order. The second must satisfy the normal replacement bump.
    assert!(pool.get(first.hash()).is_some());
    assert!(!pool.contains(replacement.hash()));
    assert!(pool.buffer.lock().is_empty());
}

#[derive(Debug, Default)]
struct ValidationGate {
    enabled: std::sync::atomic::AtomicBool,
    entered: Notify,
    release: Notify,
}

#[derive(Debug)]
struct PausingValidator {
    pool: InnerPool<Validator, InMemoryBlobStore>,
    gate: Arc<ValidationGate>,
}

impl TransactionValidator for PausingValidator {
    type Transaction = EthPooledTransaction;
    type Block = Block;
    async fn validate_transaction(
        &self,
        origin: TransactionOrigin,
        transaction: Self::Transaction,
    ) -> TransactionValidationOutcome<Self::Transaction> {
        if self.gate.enabled.load(std::sync::atomic::Ordering::Relaxed) {
            self.gate.entered.notify_one();
            self.gate.release.notified().await;
        }
        self.pool.validator().validate_transaction(origin, transaction).await
    }
}

#[tokio::test]
async fn removal_during_revalidation_does_not_resurrect_transaction() {
    let _guard = TipGuard::new(0);
    let template = pool();
    fund(&template, 1, 10);
    let gate = Arc::new(ValidationGate::default());
    let inner = Pool::new(
        PausingValidator { pool: template.inner.clone(), gate: gate.clone() },
        CoinbaseTipOrdering::default(),
        template.inner.blob_store().clone(),
        template.inner.config().clone(),
    );
    let pool = BscTransactionPool::new(inner, template.provider.clone(), template.tasks.clone());
    let tx = blob(1, 11);
    let hash = *tx.hash();
    pool.add_transaction(TransactionOrigin::Local, tx).await.unwrap();
    pool.add_transaction(TransactionOrigin::Local, blob(1, 10)).await.unwrap();
    gate.enabled.store(true, std::sync::atomic::Ordering::Relaxed);
    let promoting = pool.clone();
    let task = tokio::spawn(async move {
        promoting.promote().await;
    });
    tokio::time::timeout(Duration::from_secs(5), gate.entered.notified()).await.unwrap();
    assert!(pool.contains(&hash)); // still reserves a buffer slot during validation
    pool.remove_transaction(hash);
    gate.release.notify_one();
    tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
    assert!(!pool.contains(&hash));
    assert!(pool.get_blob(hash).unwrap().is_none());
}

#[tokio::test(start_paused = true)]
async fn idle_buffer_expires_without_new_transactions_or_blocks() {
    let _guard = TipGuard::new(0);
    let pool = pool();
    fund(&pool, 1, 10);
    let tx = blob(1, 11);
    let hash = *tx.hash();
    let mut events =
        pool.add_transaction_and_subscribe(TransactionOrigin::Local, tx).await.unwrap();
    assert_eq!(events.next().await, Some(TransactionEvent::Queued));
    let worker = pool.tasks.spawn_task(pool.clone().run());
    tokio::time::advance(GAPPED_LIFETIME + Duration::from_secs(1)).await;
    assert_eq!(events.next().await, Some(TransactionEvent::Discarded));
    assert!(!pool.contains(&hash));
    assert!(pool.buffer.lock().is_empty());
    assert!(pool.get_blob(hash).unwrap().is_none());
    worker.abort();
}

#[tokio::test]
async fn transaction_mined_elsewhere_finishes_buffered_subscription() {
    let _guard = TipGuard::new(0);
    let pool = pool();
    fund(&pool, 1, 10);
    let tx = blob(1, 11);
    let hash = *tx.hash();
    let mut events =
        pool.add_transaction_and_subscribe(TransactionOrigin::Local, tx).await.unwrap();
    assert_eq!(events.next().await, Some(TransactionEvent::Queued));
    let tip = reth_primitives_traits::SealedBlock::seal_slow(Block::default());
    pool.on_canonical_state_change(CanonicalStateUpdate {
        new_tip: &tip,
        pending_block_base_fee: 0,
        pending_block_blob_fee: Some(1),
        changed_accounts: vec![],
        mined_transactions: vec![hash],
        update_kind: reth_transaction_pool::PoolUpdateKind::Commit,
    });
    assert_eq!(events.next().await, Some(TransactionEvent::Mined(tip.hash())));
    assert!(!pool.contains(&hash));
    assert!(pool.buffer.lock().is_empty());
}
