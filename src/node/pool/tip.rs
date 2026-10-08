//! The fee floor shared by BSC admission, pool maintenance and block construction.

use reth_transaction_pool::{
    BestTransactions, PoolTransaction, TransactionPool, ValidPoolTransaction,
};
use std::{sync::Arc, time::Duration};

/// BSC currently has a protocol base fee of zero. Payloads use their actual EVM base fee.
pub(crate) const ADMISSION_BASE_FEE: u64 = 0;

/// A failed fee calculation must not pass, including when the floor is zero.
pub(crate) fn meets_tip_floor(effective_tip: Option<u128>, floor: u128) -> bool {
    effective_tip.is_some_and(|tip| tip >= floor)
}

pub(crate) fn current_tip_floor() -> u128 {
    crate::shared::get_miner_gas_tip().unwrap_or_default().into()
}

/// Remove only underpriced transactions; the pool parks their descendants until the gap is filled.
pub(crate) fn remove_underpriced<P: TransactionPool>(pool: &P) {
    let floor = current_tip_floor();
    // all_transactions() omits the blob subpool in the pinned upstream implementation.
    let hashes = pool
        .get_all(pool.all_transaction_hashes())
        .into_iter()
        .filter(|tx| !meets_tip_floor(tx.effective_tip_per_gas(ADMISSION_BASE_FEE), floor))
        .map(|tx| *tx.hash())
        .collect();
    pool.remove_transactions(hashes);
}

/// Reconcile transactions that finish insertion after a concurrent floor update and cleanup.
/// This is eventual consistency, not an atomic admission barrier. Builders also check the floor.
pub(crate) async fn maintain_tip_floor<P: TransactionPool>(pool: P) {
    let mut interval = tokio::time::interval(Duration::from_secs(5));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        // The first tick is immediate, so existing transactions are checked at startup as well.
        interval.tick().await;
        remove_underpriced(&pool);
    }
}

/// Skip an underpriced pool transaction and its dependent nonces for this build only.
pub(crate) fn enforce_pool_tip<T: PoolTransaction>(
    transactions: &mut dyn BestTransactions<Item = Arc<ValidPoolTransaction<T>>>,
    transaction: &Arc<ValidPoolTransaction<T>>,
    base_fee: u64,
    floor: u128,
) -> bool {
    if meets_tip_floor(transaction.effective_tip_per_gas(base_fee), floor) {
        return true;
    }
    transactions.mark_invalid(
        transaction,
        &reth_transaction_pool::error::InvalidPoolTransactionError::Underpriced,
    );
    false
}
