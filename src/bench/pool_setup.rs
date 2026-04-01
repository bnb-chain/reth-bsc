use alloy_consensus::transaction::Recovered;
use reth_primitives::TransactionSigned;
use reth_transaction_pool::blobstore::InMemoryBlobStore;
use reth_transaction_pool::noop::MockTransactionValidator;
use reth_transaction_pool::{
    CoinbaseTipOrdering, EthPooledTransaction, Pool, PoolConfig, TransactionOrigin, TransactionPool,
};

/// The concrete pool type used in the payload-job benchmark.
pub type BenchTxPool = Pool<
    MockTransactionValidator<EthPooledTransaction<TransactionSigned>>,
    CoinbaseTipOrdering<EthPooledTransaction<TransactionSigned>>,
    InMemoryBlobStore,
>;

/// Create a fresh, empty transaction pool for the benchmark.
///
/// Uses `minimal_protocol_basefee: 0` so low-fee benchmark transactions are accepted.
pub fn create_bench_pool() -> BenchTxPool {
    let config = PoolConfig::default().with_disabled_protocol_base_fee();
    Pool::new(
        MockTransactionValidator::default(),
        CoinbaseTipOrdering::default(),
        InMemoryBlobStore::default(),
        config,
    )
}

/// Populate the pool with pre-recovered transactions.
///
/// Transactions are already recovered (ecrecover done during pool generation),
/// matching production where txs arrive pre-recovered from P2P.
///
/// Returns the number of transactions that were successfully added.
pub async fn fill_pool(pool: &BenchTxPool, transactions: &[Recovered<TransactionSigned>]) -> usize {
    let pooled_txs: Vec<_> = transactions
        .iter()
        .map(|recovered| {
            let encoded_length = alloy_rlp::Encodable::length(recovered.inner());
            EthPooledTransaction::new(recovered.clone(), encoded_length)
        })
        .collect();

    let total = pooled_txs.len();
    let results = pool.add_transactions(TransactionOrigin::External, pooled_txs).await;

    let ok_count = results.iter().filter(|r| r.is_ok()).count();
    let err_count = results.iter().filter(|r| r.is_err()).count();
    if err_count > 0 {
        if let Some(Err(e)) = results.iter().find(|r| r.is_err()) {
            eprintln!("fill_pool: {}/{} txs failed to add. First error: {:?}", err_count, total, e);
        }
    }

    ok_count
}
